// TODO: Switch to using LuaProxyBridge to enable luau and v8 to be on different threads

pub(crate) mod extension;
pub(crate) mod base64_ops;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use mlua_scheduler::LuaSchedulerAsyncUserData;

//use deno_core::error::{CoreError, CoreErrorKind};
use deno_core::v8::CreateParams;
use deno_core::{op2, v8, Extension, OpState};
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
//use deno_core::{op2, OpState};
//use deno_error::JsErrorBox;
//use mluau::serde::de;
use tokio_util::sync::CancellationToken;

use crate::base::luau::{LuaProxyBridge, ObjectRegistryType, ProxiedLuaValue};
use crate::base::ValueArgs;
use crate::deno::extension::ExtensionTrait;

use crate::base::{StringAtomList, ObjectRegistry, deno::{ProxiedV8Value, ProxyV8Client}};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

const MAX_REFS: usize = 500;
const MIN_HEAP_LIMIT: usize = 10 * 1024 * 1024; // 10MB
const V8_MIN_STACK_SIZE: usize = 1024 * 1024 * 15; // 15MB minimum memory

/// Stores a pointer to a Rust struct and a V8 weak reference (with finalizer) to ensure the Rust struct is dropped when V8 garbage collects the associated object.
struct Finalizer<T> {
    ptr: *mut T,
    _weak: v8::Weak<v8::External>,
    finalized: Rc<RefCell<bool>>,
}

impl<T: 'static> Finalizer<T> {
    // Create a new Finalizer given scope and Rust struct
    //
    // Returns the V8 local `v8::External`
    // and the Finalizer struct itself which contains a weak reference to the `v8::External` with finalizer
    fn new<'s>(
        scope: &mut v8::HandleScope<'s, ()>,
        rust_obj: T,
        ext_cb: Option<Box<dyn FnOnce()>>,
    ) -> (*mut T, v8::Local<'s, v8::External>, Self) {
        let ptr = Box::into_raw(Box::new(rust_obj));
        let external = v8::External::new(scope, ptr as *mut std::ffi::c_void);
        let global_external = v8::Global::new(scope, external);
        
        let finalized = Rc::new(RefCell::new(false));

        let finalized_ref = finalized.clone();
        let weak = v8::Weak::with_guaranteed_finalizer(scope, global_external, Box::new(move || {
            // Finalizer callback when V8 garbage collects the object
            if !*finalized_ref.borrow() {
                *finalized_ref.borrow_mut() = true; // Mark as finalized before droping to avoid double free in Drop
                println!("Finalized V8 external and dropped Rust object at {:p}", ptr);
                unsafe {
                    drop(Box::from_raw(ptr));
                }

                if let Some(cb) = ext_cb {
                    cb();
                }
            }
        }));

        let finalizer = Self {
            ptr,
            _weak: weak,
            finalized,
        };

        (ptr, external, finalizer)
    }
}

impl<T> Drop for Finalizer<T> {
    fn drop(&mut self) {
        if !*self.finalized.borrow() {
            unsafe {
                drop(Box::from_raw(self.ptr));
            }
            *self.finalized.borrow_mut() = true;
        }
    }
}

pub struct FinalizerList<T> {
    list: Rc<RefCell<Vec<Finalizer<T>>>>,
    ptrs: Rc<RefCell<HashSet<usize>>>, // SAFETY: We store them as usizes to avoid improper use of T
}

impl<T: 'static> Default for FinalizerList<T> {
    fn default() -> Self {
        Self {
            list: Rc::default(),
            ptrs: Rc::default(),
        }
    }
}

impl<T: 'static> Clone for FinalizerList<T> {
    fn clone(&self) -> Self {
        Self {
            list: self.list.clone(),
            ptrs: self.ptrs.clone(),
        }
    }
}

impl<T: 'static> FinalizerList<T> {
    pub fn add<'s>(&self, scope: &mut v8::HandleScope<'s, ()>, rust_obj: T, ext_cb: Option<Box<dyn FnOnce()>>) -> Option<v8::Local<'s, v8::External>> {
        let (ptr, external, finalizer) = Finalizer::new(scope, rust_obj, ext_cb);
        let mut list = self.list.borrow_mut();
        if list.len() >= MAX_REFS {
            return None;
        }

        list.push(finalizer);

        self.ptrs.borrow_mut().insert(ptr as usize);

        Some(external)
    }

    pub fn contains(&self, ptr: *mut T) -> bool {
        let list = self.ptrs.borrow();
        list.contains(&(ptr as usize))
    }
}

/// Stores a lua function state
/// 
/// This is used internally to track async function call states
enum FunctionRunState {
    Created {
        fut: Pin<Box<dyn std::future::Future<Output = Result<Vec<ProxiedLuaValue>, Error>>>>,
    },
    Executed {
        lua_resp: Vec<ProxiedLuaValue>,
    },
}

#[derive(Clone)]
pub struct CommonState {
    list: Rc<RefCell<HashMap<i32, FunctionRunState>>>,
    bridge: LuaProxyBridge,
    obj_template: Rc<v8::Global<v8::ObjectTemplate>>,
    finalizer_attachments: FinalizerAttachments,
    proxy_client: ProxyV8Client
}

/// Internal manager for a single V8 isolate with a minimal Deno runtime.
/// 
/// This should not be used directly, use V8IsolateManager instead
/// which uses a tokio task w/ channel to communicate with the isolate manager.
/// 
/// It is unsafe to hold more than one V8IsolateManagerInner in the same thread at once.
pub struct V8IsolateManagerInner {
    deno: deno_core::JsRuntime,
    cancellation_token: CancellationToken,
    common_state: CommonState
}

#[derive(Clone)]
pub struct FinalizerAttachments {
    func_ids: FinalizerList<i32>,
}

pub const MAX_PROXY_DEPTH: usize = 10;

impl V8IsolateManagerInner {
    // Internal, use proxy_to_v8_safe to ensure finalizers are also set
    fn proxy_to_v8_impl(&mut self, value: ProxiedLuaValue) -> Result<v8::Global<v8::Value>, Error> {
        let v8_ctx = self.deno.main_context();
        let isolate = self.deno.v8_isolate();

        let v8_value = {
            let mut scope = v8::HandleScope::new(isolate);
            let v8_ctx = v8::Local::new(&mut scope, v8_ctx);
            let scope = &mut v8::ContextScope::new(&mut scope, v8_ctx);
            match Self::proxy_to_v8(scope, &self.common_state, value, 0) {
                Ok(v) => Ok(v8::Global::new(scope, v)),
                Err(e) => Err(e),
            }
        };

        println!("Proxied Lua value to V8");
        
        v8_value
    }

    fn proxy_to_v8<'s>(
        scope: &mut v8::HandleScope<'s>, 
        common_state: &CommonState,
        value: ProxiedLuaValue,
        depth: usize,
    ) -> Result<v8::Local<'s, v8::Value>, Error> {
        if depth > MAX_PROXY_DEPTH {
            return Err("Maximum proxy depth exceeded".into());
        }

        let v8_value: v8::Local<v8::Value> = match value {
            ProxiedLuaValue::Nil => v8::null(scope).into(),
            ProxiedLuaValue::Boolean(b) => v8::Boolean::new(scope, b).into(),
            ProxiedLuaValue::Integer(i) => {
                if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                    v8::Integer::new(scope, i as i32).into()
                } else {
                    v8::Number::new(scope, i as f64).into()
                }
            },
            ProxiedLuaValue::Number(n) => v8::Number::new(scope, n).into(),
            ProxiedLuaValue::Vector((x,y,z)) => {
                let arr = v8::Array::new(scope, 3);
                let x = v8::Number::new(scope, x as f64);
                let y = v8::Number::new(scope, y as f64);
                let z = v8::Number::new(scope, z as f64);
                arr.set_index(scope, 0, x.into());
                arr.set_index(scope, 1, y.into());
                arr.set_index(scope, 2, z.into());
                arr.into()
            },
            ProxiedLuaValue::String(atom) => {
                let atom_bytes = atom.to_bytes();
                let s = std::str::from_utf8(&atom_bytes).map_err(|e| format!("Failed to convert string atom to UTF-8: {}", e))?;
                v8::String::new(scope, s).ok_or("Failed to create V8 string")?.into()
            }
            ProxiedLuaValue::Array(elems) => {
                if elems.len() > i32::MAX as usize {
                    return Err("Array too large to proxy to V8".into());
                }
                let arr = v8::Array::new(scope, elems.len() as i32);
                for (i, elem) in elems.into_iter().enumerate() {
                    let v8_elem = Self::proxy_to_v8(scope, common_state, elem, depth + 1)?;
                    arr.set_index(scope, i as u32, v8_elem);
                }
                arr.into()
            }
            ProxiedLuaValue::Table(elems) => {
                let obj = v8::Object::new(scope);
                for (k, v) in elems {
                    let k = Self::proxy_to_v8(scope, common_state, k, depth + 1)?;
                    let v = Self::proxy_to_v8(scope, common_state, v, depth + 1)?;
                    obj.set(scope, k, v);
                }
                obj.into()
            }
            ProxiedLuaValue::Function(func_id) => {
                let obj_template = common_state.obj_template.clone();
                
                let local_template = v8::Local::new(scope, (*obj_template).clone());
                
                let bridge_ref = common_state.bridge.clone();
                let external = common_state.finalizer_attachments.func_ids.add(scope, func_id, Some(Box::new(move || {
                    bridge_ref.request_drop_object(ObjectRegistryType::Function, func_id);
                })))
                    .ok_or("Maximum number of function references reached")?;

                let obj = local_template.new_instance(scope).ok_or("Failed to create V8 object")?;
                obj.set_internal_field(0, external.into());

                let func_id_val = v8::Integer::new(scope, func_id);
                let func_id_key = v8::String::new(scope, "__funcId").ok_or("Failed to create function ID string")?;
                obj.set(scope, func_id_key.into(), func_id_val.into());
                                
                let code = r#"
                    (function(dataToCapture) {
                        let func = async function(...args) {
                            let funcId = dataToCapture.__funcId;
                            console.log("Calling Lua function with ID", funcId);
                            let runId = Deno.core.ops.__luadispatch(funcId, args);
                            await Deno.core.ops.__luarun(runId);
                            let ret = Deno.core.ops.__luaresult(runId);
                            if (Array.isArray(ret) && ret.length <= 1) {
                                return ret[0]; // Cast to single value due to lua multivalue things
                            }
                            return ret;
                        };
                        func.__funcObj = dataToCapture;
                        return func;   
                    })
                "#;

                let try_catch = &mut v8::TryCatch::new(scope);

                let source = v8::String::new(try_catch, code).unwrap();
                let script = match v8::Script::compile(try_catch, source, None) {
                    Some(s) => s,
                    None => {
                        if try_catch.has_caught() {
                            let exception = try_catch.exception().unwrap();
                            let exception_string = exception.to_rust_string_lossy(try_catch);
                            return Err(format!("Failed to compile function proxy script: {}", exception_string).into());
                        }
                        return Err("Failed to compile function proxy script".into())
                    },
                };
                let result = script.run(try_catch).unwrap();
                let creator_fn: v8::Local<v8::Function> = result.try_into().unwrap();
                let global = try_catch.get_current_context().global(try_catch);
                let result = match creator_fn.call(try_catch, global.into(), &[obj.into()]) {
                    Some(r) => r,
                    None => {
                        if try_catch.has_caught() {
                            let exception = try_catch.exception().unwrap();
                            let exception_string = exception.to_rust_string_lossy(try_catch);
                            return Err(format!("Failed to run function proxy script: {}", exception_string).into());
                        }
                        return Err("Failed to run function proxy script".into())
                    },
                };
                let final_closure: v8::Local<v8::Function> = result.try_into().unwrap();

                final_closure.into()
            }
            ProxiedLuaValue::Thread(thread_id) => {
                let obj_template = common_state.obj_template.clone();
                
                let local_template = v8::Local::new(scope, (*obj_template).clone());
                
                let bridge_ref = common_state.bridge.clone();
                let external = common_state.finalizer_attachments.func_ids.add(scope, thread_id, Some(Box::new(move || {
                    bridge_ref.request_drop_object(ObjectRegistryType::Thread, thread_id);
                })))
                    .ok_or("Maximum number of thread references reached")?;
                let obj = local_template.new_instance(scope).ok_or("Failed to create V8 object")?;
                obj.set_internal_field(0, external.into());
                obj.into()
            }
            ProxiedLuaValue::UserData(ud_id) => {
                let obj_template = common_state.obj_template.clone();
                
                let local_template = v8::Local::new(scope, (*obj_template).clone());
                
                let bridge_ref = common_state.bridge.clone();
                let external = common_state.finalizer_attachments.func_ids.add(scope, ud_id, Some(Box::new(move || {
                    bridge_ref.request_drop_object(ObjectRegistryType::UserData, ud_id);
                })))
                    .ok_or("Maximum number of userdata references reached")?;
                let obj = local_template.new_instance(scope).ok_or("Failed to create V8 object")?;
                obj.set_internal_field(0, external.into());
                obj.into()
            }
            ProxiedLuaValue::Buffer(buf_id) => {
                let obj_template = common_state.obj_template.clone();
                
                let local_template = v8::Local::new(scope, (*obj_template).clone());
                
                let bridge_ref = common_state.bridge.clone();
                let external = common_state.finalizer_attachments.func_ids.add(scope, buf_id, Some(Box::new(move || {
                    bridge_ref.request_drop_object(ObjectRegistryType::Buffer, buf_id);
                })))
                    .ok_or("Maximum number of buffer references reached")?;
                let obj = local_template.new_instance(scope).ok_or("Failed to create V8 object")?;
                obj.set_internal_field(0, external.into());
                obj.into()
            }
        };

        Ok(v8_value)
    }
    
    pub fn new(bridge: LuaProxyBridge, heap_limit: usize) -> Self {
        let heap_limit = heap_limit.max(MIN_HEAP_LIMIT);

        // TODO: Support snapshots maybe
        let mut extensions = extension::all_extensions(false);

        // Add the __luadispatch, __luarun and __luaresult ops
        deno_core::extension!(
            init_lua_op,
            ops = [__luadispatch, __luarun, __luaresult],
        );
        impl ExtensionTrait<()> for init_lua_op {
            fn init((): ()) -> Extension {
                init_lua_op::init()
            }
        }

        extensions.push(init_lua_op::build((), false));

        let mut deno = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
            create_params: Some(
                CreateParams::default()
                .heap_limits(0, heap_limit)
            ),
            extensions,
            ..Default::default()
        });

        let isolate_handle = deno.v8_isolate().thread_safe_handle();
        let heap_exhausted_token = CancellationToken::new();

        // Add a callback to terminate the runtime if the max_heap_size limit is approached
        let heap_exhausted_token_ref = heap_exhausted_token.clone();
        deno.add_near_heap_limit_callback(move |current_value, _| {
            println!("V8 heap limit approached: {} bytes used", current_value);
            isolate_handle.terminate_execution();

            // Signal the outer runtime to cancel block_on future (avoid hanging) and return friendly error
            heap_exhausted_token_ref.cancel();

            // Spike the heap limit while terminating to avoid segfaulting
            // Callback may fire multiple times if memory usage increases quicker then termination finalizes
            5 * current_value
        });

        let obj_template = Rc::new({
            let isolate = deno.v8_isolate();
            let scope = &mut v8::HandleScope::new(isolate);
            let template = Self::create_obj_template(scope);
            v8::Global::new(scope, template)
        });

        let func_ids = FinalizerList::default();

        let common_state = CommonState {
            list: Rc::new(RefCell::new(HashMap::new())),
            obj_template,
            bridge,
            finalizer_attachments: FinalizerAttachments {
                func_ids,
            },
            proxy_client: ProxyV8Client {
                atom_list: StringAtomList::new(),
                func_registry: ObjectRegistry::new(),
                obj_registry: ObjectRegistry::new(),
                promise_registry: ObjectRegistry::new(),
            },
        };

        deno.op_state().borrow_mut().put(common_state.clone());

        Self {
            deno,
            cancellation_token: heap_exhausted_token,
            common_state
        }
    }

    pub fn thread_safe_handle(&mut self) -> deno_core::v8::IsolateHandle {
        self.deno.v8_isolate().thread_safe_handle()
    }

    fn create_obj_template<'s>(scope: &mut v8::HandleScope<'s, ()>) -> v8::Local<'s, v8::ObjectTemplate> {
        let template = v8::ObjectTemplate::new(scope);
        // Reserve space for the pointer to the Rust struct.
        template.set_internal_field_count(1);
        
        template
    }
}

pub(crate) enum V8IsolateManagerMessage {
    Shutdown,
    ErrSubscribe {
        resp: tokio::sync::oneshot::Sender<tokio::sync::watch::Receiver<Option<deno_core::error::CoreError>>>,
    },
    CodeExec {
        code: String,
        args: Vec<ProxiedLuaValue>,
        resp: tokio::sync::oneshot::Sender<Result<ProxiedV8Value, Error>>,
    },
    GetObjectProperty {
        obj_id: i32,
        key: ProxiedLuaValue,
        resp: tokio::sync::oneshot::Sender<Result<ProxiedV8Value, Error>>,
    },
}

/// Internal manager for a single V8 isolate with a minimal Deno runtime.
#[derive(Clone)]
pub enum V8IsolateManager {
    Threaded {
        tx: tokio::sync::mpsc::UnboundedSender<V8IsolateManagerMessage>,
        threadsafe_handle: deno_core::v8::IsolateHandle,
        thread_handle: Arc<std::thread::JoinHandle<Result<(), Box<dyn std::any::Any + Send + 'static>>>>
    },
    Local {
        tx: tokio::sync::mpsc::UnboundedSender<V8IsolateManagerMessage>,
        threadsafe_handle: deno_core::v8::IsolateHandle,
    }
}

impl Drop for V8IsolateManager {
    fn drop(&mut self) {
        let _ = match self.terminate() {
            Ok(_) => {},
            Err(e) => {
                eprintln!("Failed to send shutdown message to V8 isolate manager: {}", e);
            }
        }; 
    }
}

pub struct V8IsolateManagerThreadData {
    pub stack_size: usize,
    pub heap_limit: usize,
    pub id: String,
}

impl V8IsolateManager {
    /// Create a new isolate manager and spawn its task
    /// 
    /// Safety note: it is unsafe to create multiple isolates on the same thread
    /// As such, V8IsolateManager will create its own thread
    pub async fn new(
        thread_opts: V8IsolateManagerThreadData,
        cancellation_token: CancellationToken,
        bridge: LuaProxyBridge,
    ) -> Result<Self, Error> {
        let thread_stack_size = thread_opts.stack_size.max(V8_MIN_STACK_SIZE);
        let heap_limit = thread_opts.heap_limit;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        
        let (th_tx, th_rx) = tokio::sync::oneshot::channel();
        let tjh = std::thread::Builder::new()
        .stack_size(thread_stack_size)
        .name(thread_opts.id)
        .spawn(move || {
            let ct_ref = cancellation_token.clone();
            let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build_local(tokio::runtime::LocalOptions::default())
                    .expect("Failed to create tokio runtime");

                rt.block_on(async move {
                    let mut inner = V8IsolateManagerInner::new(bridge, heap_limit);
                    let _ = th_tx.send(inner.thread_safe_handle());
                    Self::run(inner, rx, cancellation_token).await;
                });
            }));

            ct_ref.cancel();
            res 
        })?;
        
        let handle = th_rx.await?;

        Ok(Self::Threaded {
            tx,
            threadsafe_handle: handle,
            thread_handle: Arc::new(tjh)
        })
    }

    /// Create a new isolate manager and spawn its task
    /// 
    /// Safety note: it is unsafe to create multiple isolates on the same thread
    pub fn new_local(
        mut inner: V8IsolateManagerInner,
        cancellation_token: CancellationToken,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                
        let handle = inner.thread_safe_handle();

        tokio::task::spawn_local(async move {
            let _ = Self::run(inner, rx, cancellation_token).await;
        });

        Self::Local {
            tx,
            threadsafe_handle: handle,
        }
    }

    /// Run the isolate manager task
    /// 
    /// This must be called for anything to work
    /// 
    /// Returns the inner isolate manager for any pool cleanup etc. after shutdown
    async fn run(
        mut inner: V8IsolateManagerInner, 
        mut rx: tokio::sync::mpsc::UnboundedReceiver<V8IsolateManagerMessage>, 
        shutdown_token: CancellationToken,
    ) {
        let (err_tx, err_rx) = tokio::sync::watch::channel(None);

        // Async futures unordered queue for code execution
        let mut code_exec_queue = FuturesUnordered::new();

        loop {
            tokio::select! {
                Some(msg) = rx.recv() => {
                    // Handle message
                    match msg {
                        V8IsolateManagerMessage::Shutdown => {
                            // Terminate isolate
                            break;
                        }
                        V8IsolateManagerMessage::ErrSubscribe { resp } => {
                            let _ = resp.send(err_rx.clone());
                        }
                        V8IsolateManagerMessage::CodeExec { code, args, resp } => {
                            let func = {
                                let main_ctx = inner.deno.main_context();
                                let isolate = inner.deno.v8_isolate();
                                let mut scope = v8::HandleScope::new(isolate);
                                let main_ctx = v8::Global::new(&mut scope, main_ctx);
                                let main_ctx = v8::Local::new(&mut scope, main_ctx);
                                let context_scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
                                let try_catch = &mut v8::TryCatch::new(context_scope);
                                let script = v8::String::new(try_catch, &code)
                                    .and_then(|s| v8::Script::compile(try_catch, s, None))
                                    .ok_or_else(|| {
                                        if try_catch.has_caught() {
                                            let exception = try_catch.exception().unwrap();
                                            let exception_string = exception.to_rust_string_lossy(try_catch);
                                            format!("Failed to compile script: {}", exception_string)
                                        } else {
                                            "Failed to compile script".to_string()
                                        }
                                    });

                                let script = match script {
                                    Ok(s) => s,
                                    Err(e) => {
                                        let _ = resp.send(Err(e.into()));
                                        continue;
                                    }
                                };

                                // Convert and proxy
                                let local_func = match script.run(try_catch) {
                                    Some(result) => {
                                        if result.is_function() {
                                            let v = v8::Local::<v8::Function>::try_from(result);
                                            match v {
                                                Ok(f) => f,
                                                Err(_) => {
                                                    let _ = resp.send(Err("Script did not return a function".to_string().into()));
                                                    continue;
                                                },
                                            }
                                        } else {
                                            let _ = resp.send(Err("Script did not return a function".to_string().into()));
                                            continue;
                                        }
                                    },
                                    None => {
                                        if try_catch.has_caught() {
                                            let exception = try_catch.exception().unwrap();
                                            let exception_string = exception.to_rust_string_lossy(try_catch);
                                            let _ = resp.send(Err(format!("Failed to run script: {}", exception_string).into()));
                                            continue;
                                        } else {
                                            let _ = resp.send(Err("Failed to run script".to_string().into()));
                                            continue;
                                        }
                                    }
                                };

                                v8::Global::new(try_catch, local_func)
                            };

                            // Call the function with args
                            // using denos' call_with_args
                            let args = args.into_iter().map(|v| inner.proxy_to_v8_impl(v)).collect::<Result<Vec<_>, _>>();
                            let args = match args {
                                Ok(a) => a,
                                Err(e) => {
                                    let _ = resp.send(Err(e));
                                    continue;
                                }
                            };
                            let fut = inner.deno.call_with_args(&func, &args);
                            code_exec_queue.push(async {
                                let result = fut.await;
                                (result, resp)
                            });
                        },
                        V8IsolateManagerMessage::GetObjectProperty { obj_id, key, resp } => {
                            let obj = match inner.common_state.proxy_client.obj_registry.get(obj_id) {
                                Some(v) => v,
                                None => {
                                    let _ = resp.send(Err("Object ID not found".into()));
                                    continue;
                                }
                            };

                            let key = match inner.proxy_to_v8_impl(key) {
                                Ok(k) => k,
                                Err(e) => {
                                    let _ = resp.send(Err(e));
                                    continue;
                                }
                            };

                            {
                                let main_ctx = inner.deno.main_context();
                                let isolate = inner.deno.v8_isolate();
                                let mut scope = v8::HandleScope::new(isolate);
                                let main_ctx = v8::Local::new(&mut scope, main_ctx);
                                let context_scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
                                let try_catch = &mut v8::TryCatch::new(context_scope);

                                let local_obj = v8::Local::new(try_catch, obj);
                                let local_key = v8::Local::new(try_catch, key);

                                let result = local_obj.get(try_catch, local_key);
                                let result = match result {
                                    Some(v) => v,
                                    None => {
                                        if try_catch.has_caught() {
                                            let exception = try_catch.exception().unwrap();
                                            let exception_string = exception.to_rust_string_lossy(try_catch);
                                            let _ = resp.send(Err(format!("Failed to get object property: {}", exception_string).into()));
                                            continue;
                                        } else {
                                            let _ = resp.send(Err("Failed to get object property".into()));
                                            continue;
                                        }
                                    }
                                };

                                // Convert to ProxiedV8Value
                                match ProxiedV8Value::proxy_from_v8(try_catch, result, &inner.common_state.proxy_client, 0) {
                                    Ok(v) => {
                                        let _ = resp.send(Ok(v));
                                    }
                                    Err(e) => {
                                        if try_catch.has_caught() {
                                            let exception = try_catch.exception().unwrap();
                                            let exception_string = exception.to_rust_string_lossy(try_catch);
                                            let _ = resp.send(Err(format!("Failed to convert object property to ProxiedV8Value: {} ({})", exception_string, e).into()));
                                            continue;
                                        }
                                        let _ = resp.send(Err(e.into()));
                                    }
                                };
                            }
                        }
                    }
                },
                _ = inner.cancellation_token.cancelled() => {
                    // Terminate isolate
                    break;
                }
                Some((result, resp)) = code_exec_queue.next() => {
                    // A code execution future has completed
                    let result = match result {
                        Ok(v) => {
                            // Convert v8::Global<v8::Value> to ProxiedV8Value
                            let main_ctx = inner.deno.main_context();
                            let isolate = inner.deno.v8_isolate();
                            let mut scope = v8::HandleScope::new(isolate);
                            let main_ctx = v8::Local::new(&mut scope, main_ctx);
                            let mut scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
                            let v = v8::Local::new(&mut scope, v);
                            match ProxiedV8Value::proxy_from_v8(
                                &mut scope, 
                                v,
                                &inner.common_state.proxy_client, 
                                0
                            ) {
                                Ok(pv) => Ok(pv),
                                Err(e) => Err(e),
                            }
                        }
                        Err(e) => Err(format!("JavaScript execution error: {}", e).into()),
                    };
                    let _ = resp.send(result.map_err(|e| e.into()));
                }
                res = inner.deno.run_event_loop(deno_core::PollEventLoopOptions::default()), if !code_exec_queue.is_empty() => {
                    // Continue running
                    match res {
                        Ok(_) => {},
                        Err(e) => {
                            err_tx.send_replace(Some(e));
                        }
                    };

                    tokio::task::yield_now().await; // Yield to allow other tasks to run
                }
            }
        }

        if !inner.thread_safe_handle().is_execution_terminating() {
            inner.thread_safe_handle().terminate_execution();
        }

        shutdown_token.cancel();
    }

    /// Sends a message to the isolate manager to subscribe to error events
    pub(crate) fn send(&self, msg: V8IsolateManagerMessage) -> Result<(), Error> {
        match self {
            V8IsolateManager::Threaded { tx, .. } => {
                tx.send(msg).map_err(|_| "Failed to send message to isolate manager".to_string().into())
            }
            V8IsolateManager::Local { tx, .. } => {
                tx.send(msg).map_err(|_| "Failed to send message to isolate manager".to_string().into())
            }
        }
    }

    /// Returns a watch receiver for errors from the isolates event loop
    pub async fn err_subscribe(&self) -> Result<tokio::sync::watch::Receiver<Option<deno_core::error::CoreError>>, Error> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let msg = V8IsolateManagerMessage::ErrSubscribe { resp: tx };
        self.send(msg)?;
        let rx = rx.await.map_err(|_| "Failed to receive ErrSubscribe response".to_string())?;
        Ok(rx)
    }

    pub(crate) async fn eval(&self, code: &str, args: Vec<ProxiedLuaValue>) -> Result<ProxiedV8Value, Error> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let msg = V8IsolateManagerMessage::CodeExec { code: code.to_string(), args, resp: tx };
        self.send(msg)?;
        let value = rx.await.map_err(|_| "Failed to receive CodeExec response".to_string())??;
        Ok(value)   
    }

    pub fn terminate(&self) -> Result<(), Error> {
        self.send(V8IsolateManagerMessage::Shutdown)
    }
}

/*impl LuaUserData for V8IsolateManager {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("isrunning", |_, this, _: ()| {
            Ok(!this.tx.is_closed())
        });

        // TODO: load function for ES modules
        methods.add_scheduler_async_method("eval", async move |_, this, (code, args): (String, LuaMultiValue)| {
            // First proxy args to V8
            let (res_tx, res_rx) = tokio::sync::oneshot::channel();
            this.tx.send(V8IsolateManagerMessage::ProxyMultipleToV8 {
                values: args,
                resp: res_tx,
            }).map_err(|e| mluau::Error::external(format!("Failed to send event to runtime: {}", e)))?;
            let args = res_rx.await
                .map_err(|e| mluau::Error::external(e.to_string()))?
                .map_err(|e| mluau::Error::external(e.to_string()))?;
            
            // Push to the runtime event queue
            let (res_tx, res_rx) = tokio::sync::oneshot::channel();
            this.tx.send(V8IsolateManagerMessage::CodeExec {
                args,
                code,
                resp: res_tx,
            }).map_err(|e| mluau::Error::external(format!("Failed to send event to runtime: {}", e)))?;

            let res = res_rx.await
                .map_err(|e| mluau::Error::external(e.to_string()))?
                .map_err(|e| mluau::Error::external(e.to_string()))?;

            let (res_tx, res_rx) = tokio::sync::oneshot::channel();
            this.tx.send(V8IsolateManagerMessage::ProxyFromV8 {
                value: res,
                resp: res_tx,
            }).map_err(|e| mluau::Error::external(format!("Failed to send event to runtime: {}", e)))?;
            let res = res_rx.await
                .map_err(|e| mluau::Error::external(e.to_string()))?
                .map_err(|e| mluau::Error::external(e.to_string()))?;

            Ok(res)
        });
    }
}*/

// OP to bind arguments to a function by ID, returning a run ID
#[op2(fast)]
fn __luadispatch(
    #[state] state: &CommonState,
    scope: &mut v8::HandleScope,
    func_id: i32,
    args: v8::Local<v8::Array>,
) -> Result<i32, deno_error::JsErrorBox> {
    let mut args_proxied = Vec::with_capacity(args.length() as usize);
    for i in 0..args.length() {
        let arg = args.get_index(scope, i).ok_or_else(|| deno_error::JsErrorBox::generic(format!("Failed to get argument {}", i)))?;
        match ProxiedV8Value::proxy_from_v8(scope, arg, &state.proxy_client, 0) {
            Ok(v) => args_proxied.push(v),
            Err(e) => {
                return Err(deno_error::JsErrorBox::generic(format!("Failed to convert argument {}: {}", i, e)));
            }
        }
    }

    let mut funcs = state.list.borrow_mut();
    let run_id = funcs.len() as i32 + 1;
    let bridge = state.bridge.clone();
    funcs.insert(run_id, FunctionRunState::Created {
        fut: Box::pin(async move { 
            bridge.call_function(func_id, ValueArgs::V8(args_proxied)).await
        })
    });
    Ok(run_id)
}

// OP to execute a bound function by run ID
//
// Returns nothing
#[op2(async)]
async fn __luarun(
    state_rc: Rc<RefCell<OpState>>,
    run_id: i32,
) -> Result<(), deno_error::JsErrorBox> {
    let running_funcs = {
        let state = state_rc.try_borrow()
            .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
        
        state.try_borrow::<CommonState>()
            .ok_or_else(|| deno_error::JsErrorBox::generic("CommonState not found".to_string()))?
            .clone()
    };

    let func_state = {
        let mut funcs = running_funcs.list.borrow_mut();
        let func_state = funcs.remove(&run_id)
            .ok_or_else(|| deno_error::JsErrorBox::generic("Run ID not found".to_string()))?;

        func_state
    }; // list borrow ends here

    match func_state {
        FunctionRunState::Created { fut } => {
            let lua_resp = fut.await
                .map_err(|e| deno_error::JsErrorBox::generic(format!("Function execution error: {}", e)))?;
            // Store the result in the run state
            let mut funcs = running_funcs.list.borrow_mut();
            funcs.insert(run_id, FunctionRunState::Executed { lua_resp });
            return Ok(())
        }
        _ => {
            return Err(deno_error::JsErrorBox::generic("Run not in Created state".to_string()));
        }
    }
}

// OP to get the results of a function by func ID/run ID
#[op2]
fn __luaresult<'s>(
    #[state] state: &CommonState,
    scope: &mut v8::HandleScope<'s>,
    run_id: i32,
) -> Result<v8::Local<'s, v8::Array>, deno_error::JsErrorBox> {
    let func_state = {
        let mut funcs = state.list.borrow_mut();
        let func_state = funcs.remove(&run_id)
            .ok_or_else(|| deno_error::JsErrorBox::generic("Run ID not found".to_string()))?;

        func_state
    }; // list borrow ends here

    match func_state {
        FunctionRunState::Executed { lua_resp } => {
            // Proxy every return value to V8
            let mut results = vec![];
            for ret in lua_resp {
                match V8IsolateManagerInner::proxy_to_v8(scope, state, ret, 0) {
                    Ok(v8_ret) => results.push(v8_ret),
                    Err(e) => {
                        return Err(deno_error::JsErrorBox::generic(format!("Failed to convert return value: {}", e)));
                    }
                }
            }

            let arr = v8::Array::new(scope, results.len() as i32);
            for (i, v) in results.into_iter().enumerate() {
                arr.set_index(scope, i as u32, v);
            }

            Ok(arr)
        }
        FunctionRunState::Created { .. } => {
            Err(deno_error::JsErrorBox::generic("Function has not been executed yet".to_string()))
        }
    }
}

/*#[cfg(test)]
mod tests {
    use mlua_scheduler::{taskmgr::NoopHooks, LuaSchedulerAsync, XRc};
    use mluau::IntoLua;
    use tokio_util::sync::CancellationToken;

    use crate::deno::V8IsolateManager;
    #[test]
    fn test_v8_isolate_manager() {
        println!("Starting V8 isolate manager test");
        let lua = mluau::Lua::new();
        let compiler = mluau::Compiler::new().set_optimization_level(2);
        lua.set_compiler(compiler);

        let manager_i = super::V8IsolateManagerInner::new(&lua, super::MIN_HEAP_LIMIT);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            println!("Creating Lua scheduler and task manager");
            let returns_tracker = mlua_scheduler::taskmgr::ReturnTracker::new();

            let mut wildcard_sender = returns_tracker.track_wildcard_thread();

            tokio::task::spawn_local(async move {
                while let Some((thread, result)) = wildcard_sender.recv().await {
                    if let Err(e) = result {
                        eprintln!("Error in thread {e:?}: {:?}", thread.to_pointer());
                    }
                }
            });

            let task_mgr = mlua_scheduler::taskmgr::TaskManager::new(&lua, returns_tracker, XRc::new(NoopHooks {})).await.expect("Failed to create task manager");

            lua.globals()
                .set(
                    "_PANIC",
                    lua.create_scheduler_async_function(|_lua, n: i32| async move {
                        panic!("Panic test: {n}");
                        #[allow(unreachable_code)]
                        Ok(())
                    })
                    .expect("Failed to create async function"),
                )
                .expect("Failed to set _OS global");

            lua.globals()
                .set(
                    "task",
                    mlua_scheduler::userdata::task_lib(&lua).expect("Failed to create table"),
                )
                .expect("Failed to set task global");

            lua.sandbox(true).expect("Sandboxed VM"); // Sandbox VM

            let (manager, rx) = V8IsolateManager::new();
            tokio::task::spawn_local(async move {
                let _inner = V8IsolateManager::run(manager_i, rx, CancellationToken::new()).await;
            });

            // Call the v8 function now as a async script
            let lua_code = r#"
local v8 = ...
local result = v8:eval([[
  (function() {
    async function f(waiter, v8ud, buf) {
        // Allocate a large array to test heap limits
        //let arr = new Array(1e6).fill(0).map((_, i) => i);
        //console.log('Array allocated with length:', arr.length);
        let v = await v8ud.isrunning(v8ud);
        console.log('V8 userdata:', v8ud, v, typeof v);
        console.log('Buffer:', buf, typeof buf.buffer, buf.buffer.byteLength);
        let waited = await waiter();
        return [waited, buf, v8ud]
    }

    return f;
  })()
]], function() print('am here'); return task.wait(1) end, v8, buffer.create(10))
assert(result[3] == v8)
return result[1], result[2], result[3]
"#;

            let func = lua.load(lua_code).into_function().expect("Failed to load Lua code");
            let th = lua.create_thread(func).expect("Failed to create Lua thread");
            
            let mut args = mluau::MultiValue::new();
            args.push_back(manager.into_lua(&lua).expect("Failed to push QuickJS runtime to Lua"));

            let output = task_mgr
                .spawn_thread_and_wait(th, args)
                .await
                .expect("Failed to run Lua thread")
                .expect("Lua thread returned no value")
                .expect("Lua thread returned an error");
            
            println!("Output: {:?}", output);
        });
    }
}*/

mod asserter {
    use super::V8IsolateManager;
    //use super::LuaProxyBridge;

    const fn assert_send_const<T: Send>() {}
    const _: () = assert_send_const::<V8IsolateManager>(); 
    //const _: () = assert_send_const::<LuaProxyBridge>();
}