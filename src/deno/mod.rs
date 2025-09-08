pub(crate) mod extension;
pub(crate) mod base64_ops;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use mlua_scheduler::LuaSchedulerAsyncUserData;

//use deno_core::error::{CoreError, CoreErrorKind};
use deno_core::v8::CreateParams;
use deno_core::{op2, v8, Extension, OpState};
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use mluau::prelude::*;
//use deno_core::{op2, OpState};
//use deno_error::JsErrorBox;
//use mluau::serde::de;
use tokio_util::sync::CancellationToken;

use crate::deno::extension::ExtensionTrait;

pub type Error = Box<dyn std::error::Error>;

const MAX_REFS: usize = 500;
const MIN_HEAP_LIMIT: usize = 10 * 1024 * 1024; // 10MB

/// Stores a pointer to a Rust struct and a V8 weak reference (with finalizer) to ensure the Rust struct is dropped when V8 garbage collects the associated object.
struct Finalizer<T> {
    ptr: *mut T,
    weak: v8::Weak<v8::External>,
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
            weak,
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
pub struct FunctionState {
    func: Function,
    runs: Rc<RefCell<HashMap<i32, FunctionRunState>>>,
}

pub enum FunctionRunState {
    Bound {
        lua_args: LuaMultiValue,
    },
    Executed {
        lua_resp: LuaMultiValue,
    }
}

#[derive(Clone)]
pub struct CommonState {
    list: Rc<RefCell<HashMap<i32, FunctionState>>>,
    weak_lua: WeakLua,
    userdata_template: Rc<v8::Global<v8::ObjectTemplate>>, 
    func_template: Rc<v8::Global<v8::ObjectTemplate>>,
    finalizer_attachments: FinalizerAttachments,
}

/// Internal manager for a single V8 isolate with a minimal Deno runtime.
/// 
/// This should not be used directly, use V8IsolateManager instead
/// which uses a tokio task w/ channel to communicate with the isolate manager.
/// 
/// Alternatively, use a (WIP) IsolatePool to manage multiple isolates with drops
/// in LIFO order (needed for v8)
pub struct V8IsolateManagerInner {
    deno: deno_core::JsRuntime,
    cancellation_token: CancellationToken,
    common_state: CommonState
}

#[derive(Clone)]
pub struct FinalizerAttachments {
    ud: FinalizerList<UserData>,
    func: FinalizerList<Function>,
}

impl V8IsolateManagerInner {
    // Internal, use proxy_to_v8_safe to ensure finalizers are also set
    fn proxy_to_v8_impl(&mut self, value: LuaValue) -> Result<v8::Global<v8::Value>, Error> {
        let v8_ctx = self.deno.main_context();
        let isolate = self.deno.v8_isolate();
        let scope = &mut v8::HandleScope::new(isolate);
        let v8_ctx = v8::Global::new(scope, v8_ctx);
        let v8_ctx = v8::Local::new(scope, v8_ctx);
        let scope = &mut v8::ContextScope::new(scope, v8_ctx);
        let v8_value = Self::proxy_to_v8(scope, &self.common_state, value)?;
        Ok(v8::Global::new(scope, v8_value))
    }
    
    fn proxy_to_v8<'s>(
        scope: &mut v8::HandleScope<'s>, 
        common_state: &CommonState,
        value: LuaValue
    ) -> Result<v8::Local<'s, v8::Value>, Error> {
        let v8_value: v8::Local<v8::Value> = match value {
            LuaValue::Nil => v8::null(scope).into(),
            LuaValue::Boolean(b) => v8::Boolean::new(scope, b).into(),
            LuaValue::Integer(i) => {
                if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                    v8::Integer::new(scope, i as i32).into()
                } else {
                    v8::Number::new(scope, i as f64).into()
                }
            },
            LuaValue::Number(n) => v8::Number::new(scope, n).into(),
            mluau::Value::Vector(v) => {
                let arr = v8::Array::new(scope, 3);
                let x = v8::Number::new(scope, v.x() as f64);
                let y = v8::Number::new(scope, v.y() as f64);
                let z = v8::Number::new(scope, v.z() as f64);
                arr.set_index(scope, 0, x.into());
                arr.set_index(scope, 1, y.into());
                arr.set_index(scope, 2, z.into());
                arr.into()
            },
            LuaValue::String(s) => {
                let s = s.to_string_lossy();
                v8::String::new(scope, &s).ok_or("Failed to create V8 string")?.into()
            },
            LuaValue::Table(t) => {
                if let Some(lua) = t.weak_lua().try_upgrade() {
                    if t.metatable() == Some(lua.array_metatable()) {
                        // Convert to array
                        let len = t.raw_len();
                        if len <= i32::MAX as usize {
                            let arr = v8::Array::new(scope, len as i32);
                            for i in 1..=len {
                                let v = t.raw_get(i)
                                    .map_err(|e| mluau::Error::external(format!("Failed to get array element {}: {}", i, e)))?;
                                let v = Self::proxy_to_v8(scope, common_state, v)?;
                                arr.set_index(scope, (i - 1) as u32, v);
                            }
                            return Ok(arr.into());
                        }
                    }
                }

                let obj = v8::Object::new(scope);
                t.for_each(|k, v| {
                    let v8_key = Self::proxy_to_v8(scope, common_state, k).map_err(|e| LuaError::external(e.to_string()))?;
                    let v8_val = Self::proxy_to_v8(scope, common_state, v).map_err(|e| LuaError::external(e.to_string()))?;
                    obj.set(scope, v8_key, v8_val);
                    Ok(())
                })?;
                obj.into()
            },
            // TODO: Function, Thread, Buffer
            mluau::Value::LightUserData(ptr) => {
                let num = ptr.0 as usize as u64;
                v8::BigInt::new_from_u64(scope, num).into()
            },
            mluau::Value::UserData(ud) => {
                let obj_template = common_state.userdata_template.clone();
                
                let local_template = v8::Local::new(scope, (*obj_template).clone());
                
                let external = common_state.finalizer_attachments.ud.add(scope, UserData::new(ud, common_state.clone()), None)
                    .ok_or("Maximum number of userdata references reached")?;
                let obj = local_template.new_instance(scope).ok_or("Failed to create V8 object")?;
                obj.set_internal_field(0, external.into());
                obj.into()
            },
            mluau::Value::Function(func) => {
                // We can emulate functions using both a function object (so it can be proxied back to Lua)
                // and a function which uses a function ID to perform the call. The function object is also used
                // for finalizers
                // 
                // TODO: Generate function proxy using Deno ops

                let func_id = loop {
                    let id = rand::random::<i32>();
                    let list = common_state.list.borrow();

                    if list.len() >= MAX_REFS {
                        return Err("Maximum number of running functions reached".into());
                    }

                    if !list.contains_key(&id) {
                        break id;
                    }
                };

                let func = Function { func };
                let obj_template = common_state.func_template.clone();
                
                let local_template = v8::Local::new(scope, (*obj_template).clone());
                
                let func_state_ref = common_state.list.clone();
                let external = common_state.finalizer_attachments.func.add(scope, func.clone(), Some(Box::new(move || {
                    // Remove from running functions list
                    let mut list = func_state_ref.borrow_mut();
                    list.remove(&func_id);
                })))
                    .ok_or("Maximum number of function references reached")?;
                let obj = local_template.new_instance(scope).ok_or("Failed to create V8 object")?;
                obj.set_internal_field(0, external.into());

                // NOTE: We've already gotten a free function id
                {
                    let mut list = common_state.list.borrow_mut();
                    list.insert(func_id, FunctionState {
                        func,
                        runs: Rc::new(RefCell::new(HashMap::new())),
                    });
                }

                let func_id_val = v8::Integer::new(scope, func_id);
                let func_id_key = v8::String::new(scope, "__funcId").ok_or("Failed to create function ID string")?;
                obj.set(scope, func_id_key.into(), func_id_val.into());
                                
                let code = r#"
                    (function(dataToCapture) {
                        let func = async function(...args) {
                            let funcId = dataToCapture.__funcId;
                            console.log("Calling Lua function with ID", funcId);
                            let runId = Deno.core.ops.__luabind(funcId, args);
                            await Deno.core.ops.__luaexecute(funcId, runId);
                            let ret = Deno.core.ops.__luaresult(funcId, runId);
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
            },
            mluau::Value::Buffer(buf) => {
                let bytes = buf.to_vec();
                let bs = v8::ArrayBuffer::new_backing_store_from_boxed_slice(bytes.into_boxed_slice()).make_shared();
                let array = v8::ArrayBuffer::with_backing_store(scope, &bs);
                array.into()
            },
            mluau::Value::Error(e) => return Err(format!("Cannot proxy Lua error: {}", e).into()),
            mluau::Value::Other(_) => return Err("Cannot proxy unknown/other Lua value".into()),
            _ => v8::undefined(scope).into(),
        };

        Ok(v8_value)
    }

    fn proxy_from_v8<'s>(
        scope: &mut v8::HandleScope<'s>, 
        lua: &mluau::Lua,
        value: v8::Local<'s, v8::Value>,
        finalizers: FinalizerAttachments,
    ) -> Result<LuaValue, Error> {
        if value.is_null() || value.is_undefined() {
            return Ok(LuaValue::Nil);
        } else if value.is_boolean() {
            let b = value.to_boolean(scope).is_true();
            return Ok(LuaValue::Boolean(b));
        } else if value.is_int32() {
            let i = value.to_int32(scope).ok_or("Failed to convert to int32")?.value();
            return Ok(LuaValue::Integer(i as i64));
        } else if value.is_number() {
            let n = value.to_number(scope).ok_or("Failed to convert to number")?.value();
            return Ok(LuaValue::Number(n));
        } else if value.is_string() {
            let s = value.to_string(scope).ok_or("Failed to convert to string")?;
            let s = s.to_rust_string_lossy(scope);
            return lua.create_string(s).map(LuaValue::String).map_err(|e| e.into());
        } else if value.is_array() {
            let arr = value.to_object(scope).ok_or("Failed to convert to object")?;
            let length_str = v8::String::new(scope, "length").ok_or("Failed to create length string")?;
            let length_val = arr.get(scope, length_str.into()).ok_or("Failed to get length property")?;
            let length = length_val.to_uint32(scope).ok_or("Failed to convert length to uint32")?.value() as usize;
            let table = lua.create_table_with_capacity(length, 0)?;
            for i in 0..length {
                let elem = arr.get_index(scope, i as u32).ok_or(format!("Failed to get array element {}", i))?;
                let lua_elem = Self::proxy_from_v8(scope, lua, elem, finalizers.clone())?;
                table.raw_set(i + 1, lua_elem)?; // Lua arrays are 1-based
            }
            return Ok(LuaValue::Table(table));
        } else if value.is_array_buffer() {
            let ab = v8::Local::<v8::ArrayBuffer>::try_from(value).map_err(|_| "Failed to convert to ArrayBuffer")?;
            let bs = ab.get_backing_store();
            let Some(data) = bs.data() else {
                return lua.create_buffer_with_capacity(0).map(LuaValue::Buffer).map_err(|e| e.into());
            };
            let slice = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, bs.byte_length()) };
            let buf = lua.create_buffer(slice)?;
            return Ok(LuaValue::Buffer(buf));
        } else if value.is_object() {
            let obj = value.to_object(scope).ok_or("Failed to convert to object")?;
            let num_internal_fields = obj.internal_field_count();
            if num_internal_fields == 1 {
                let maybe_external = obj.get_internal_field(scope, 0);
                if let Some(internal) = maybe_external {
                    if let Ok(external) = v8::Local::<v8::External>::try_from(internal) {
                        
                        // Case 1: UserData proxy
                        if finalizers.ud.contains(external.value() as *mut UserData) {
                            let ptr = external.value() as *mut UserData;
                            if !ptr.is_null() {
                                let ud = unsafe { &*ptr };
                                return Ok(LuaValue::UserData(ud.ud.clone()));
                            }
                        }
                    }
                }
            }

            // Generic object to table
            let table = lua.create_table()?;
            let props = obj.get_own_property_names(scope, Default::default()).ok_or("Failed to get object properties")?;
            let length = props.length();
            for i in 0..length {
                let key = props.get_index(scope, i).ok_or(format!("Failed to get property at index {}", i))?;
                let val = obj.get(scope, key).ok_or("Failed to get property value")?;
                let lua_key = Self::proxy_from_v8(scope, lua, key, finalizers.clone())?;
                let lua_val = Self::proxy_from_v8(scope, lua, val, finalizers.clone())?;
                table.raw_set(lua_key, lua_val)?;
            }
            return Ok(LuaValue::Table(table));
        }

        Ok(LuaValue::Nil) // TODO: Implement
    }

    pub fn new(lua: &Lua, heap_limit: usize) -> Self {
        let heap_limit = heap_limit.max(MIN_HEAP_LIMIT);

        // TODO: Support snapshots maybe
        let mut extensions = extension::all_extensions(false);

        // Add the __luabind, __luaexecute and __luaresult ops
        deno_core::extension!(
            init_lua_op,
            ops = [__luabind, __luaexecute, __luaresult],
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
            isolate_handle.terminate_execution();

            // Signal the outer runtime to cancel block_on future (avoid hanging) and return friendly error
            heap_exhausted_token_ref.cancel();

            // Spike the heap limit while terminating to avoid segfaulting
            // Callback may fire multiple times if memory usage increases quicker then termination finalizes
            5 * current_value
        });

        let userdata_template = Rc::new({
            let isolate = deno.v8_isolate();
            let scope = &mut v8::HandleScope::new(isolate);
            let template = Self::create_ud_proxy_template(scope);
            v8::Global::new(scope, template)
        });

        let function_template = Rc::new({
            let isolate = deno.v8_isolate();
            let scope = &mut v8::HandleScope::new(isolate);
            let template = Self::create_func_proxy_template(scope);
            v8::Global::new(scope, template)
        });

        let uds = FinalizerList::default();
        let funcs = FinalizerList::default();

        let common_state = CommonState {
            list: Rc::new(RefCell::new(HashMap::new())),
            weak_lua: lua.weak(),
            userdata_template,
            func_template: function_template,
            finalizer_attachments: FinalizerAttachments {
                ud: uds.clone(),
                func: funcs.clone(),
            },
        };

        deno.op_state().borrow_mut().put(common_state.clone());

        Self {
            deno,
            cancellation_token: heap_exhausted_token,
            common_state
        }
    }

    fn create_ud_proxy_template<'s>(scope: &mut v8::HandleScope<'s, ()>) -> v8::Local<'s, v8::ObjectTemplate> {
        let template = v8::ObjectTemplate::new(scope);
        // Reserve space for the pointer to the Rust struct.
        template.set_internal_field_count(1);
        
        // 4. Configure the template to use our getter and setter callbacks.
        let named_handler = v8::NamedPropertyHandlerConfiguration::new()
            .getter(UserData::named_property_getter)
            .setter(UserData::named_property_setter);
        template.set_named_property_handler(named_handler);

        template
    }

    fn create_func_proxy_template<'s>(scope: &mut v8::HandleScope<'s, ()>) -> v8::Local<'s, v8::ObjectTemplate> {
        let template = v8::ObjectTemplate::new(scope);
        // Reserve space for the pointer to the Rust struct.
        template.set_internal_field_count(1);
        
        template
    }
}

pub enum V8IsolateManagerMessage {
    Shutdown,
    ProxyToV8 {
        value: LuaValue,
        resp: tokio::sync::oneshot::Sender<Result<v8::Global<v8::Value>, Error>>,
    },
    ProxyMultipleToV8 {
        values: LuaMultiValue,
        resp: tokio::sync::oneshot::Sender<Result<Vec<v8::Global<v8::Value>>, Error>>,
    },
    ProxyFromV8 {
        value: v8::Global<v8::Value>,
        resp: tokio::sync::oneshot::Sender<Result<LuaValue, Error>>,
    },
    ErrSubscribe {
        resp: tokio::sync::oneshot::Sender<tokio::sync::watch::Receiver<Option<deno_core::error::CoreError>>>,
    },
    CodeExec {
        code: String,
        args: Vec<v8::Global<v8::Value>>,
        resp: tokio::sync::oneshot::Sender<Result<v8::Global<v8::Value>, Error>>,
    }
}

/// Internal manager for a single V8 isolate with a minimal Deno runtime.
#[derive(Clone)]
pub struct V8IsolateManager {
    tx: tokio::sync::mpsc::UnboundedSender<V8IsolateManagerMessage>,
}

impl Drop for V8IsolateManager {
    fn drop(&mut self) {
        let _ = self.tx.send(V8IsolateManagerMessage::Shutdown);
    }
}

impl V8IsolateManager {
    // Create a new isolate manager and spawn its task
    pub fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<V8IsolateManagerMessage>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        (Self {
            tx
        }, rx)
    }

    /// Run the isolate manager task
    /// 
    /// This must be called for anything to work
    /// 
    /// Returns the inner isolate manager for any pool cleanup etc. after shutdown
    pub async fn run(mut inner: V8IsolateManagerInner, mut rx: tokio::sync::mpsc::UnboundedReceiver<V8IsolateManagerMessage>, start_tx: tokio::sync::oneshot::Sender<()>) -> V8IsolateManagerInner {
        let (err_tx, err_rx) = tokio::sync::watch::channel(None);

        // Async futures unordered queue for code execution
        let mut code_exec_queue = FuturesUnordered::new();

        let _ = start_tx.send(());

        loop {
            tokio::select! {
                Some(msg) = rx.recv() => {
                    // Handle message
                    match msg {
                        V8IsolateManagerMessage::Shutdown => {
                            // Terminate isolate
                            break;
                        }
                        V8IsolateManagerMessage::ProxyToV8 { value, resp } => {
                            let result = inner.proxy_to_v8_impl(value);
                            let _ = resp.send(result);
                        }
                        V8IsolateManagerMessage::ProxyMultipleToV8 { values, resp } => {
                            let mut results = Vec::with_capacity(values.len());
                            let mut error = None;
                            for v in values {
                                match inner.proxy_to_v8_impl(v) {
                                    Ok(v8_val) => results.push(v8_val),
                                    Err(e) => {
                                        error = Some(e);
                                        break;
                                    }
                                }
                            }
                            match error {
                                Some(e) => {
                                    let _ = resp.send(Err(e));
                                }
                                None => {
                                    let _ = resp.send(Ok(results));
                                }
                            }
                        }
                        V8IsolateManagerMessage::ProxyFromV8 { value, resp } => {
                            let result = {
                                let v8_ctx = inner.deno.main_context();
                                let isolate = inner.deno.v8_isolate();
                                let scope = &mut v8::HandleScope::new(isolate);
                                let v8_ctx = v8::Local::new(scope, v8_ctx);
                                let scope = &mut v8::ContextScope::new(scope, v8_ctx);
                                let local_value = v8::Local::new(scope, value);
                                let lua = inner.common_state.weak_lua.try_upgrade().ok_or("Lua state has been dropped".to_string());
                                match lua {
                                    Ok(lua) => {
                                        V8IsolateManagerInner::proxy_from_v8(scope, &lua, local_value, inner.common_state.finalizer_attachments.clone())
                                    }
                                    Err(e) => {
                                        let _ = resp.send(Err(e.into()));
                                        continue;
                                    },
                                }
                            };
                            let _ = resp.send(result);
                        }
                        V8IsolateManagerMessage::ErrSubscribe { resp } => {
                            let _ = resp.send(err_rx.clone());
                        }
                        V8IsolateManagerMessage::CodeExec { code, args, resp } => {
                            let func = {
                                let main_ctx = inner.deno.main_context();
                                let isolate = inner.deno.v8_isolate();
                                let scope = &mut v8::HandleScope::new(isolate);
                                let main_ctx = v8::Local::new(scope, main_ctx);
                                let context_scope = &mut v8::ContextScope::new(scope, main_ctx);
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
                            let fut = inner.deno.call_with_args(&func, &args);
                            code_exec_queue.push(async {
                                let result = fut.await;
                                let _ = resp.send(result.map_err(|e| e.into()));
                            });
                        }
                    }
                },
                _ = inner.cancellation_token.cancelled() => {
                    // Terminate isolate
                    break;
                }
                Some(_) = code_exec_queue.next() => {
                    // A code execution future has completed
                    // We don't need to do anything here as the future already sent the response
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

        inner // Return the inner isolate manager for any pool cleanup etc.
    }

    /// Proxies a Lua value to V8
    pub async fn proxy_to_v8(&self, value: LuaValue) -> Result<v8::Global<v8::Value>, Error> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let msg = V8IsolateManagerMessage::ProxyToV8 { value, resp: tx };
        self.tx.send(msg).map_err(|_| "Failed to send ProxyToV8 message".to_string())?;
        let value = rx.await.map_err(|_| "Failed to receive ProxyToV8 response".to_string())??;
        Ok(value)
    }

    /// Returns a watch receiver for errors from the isolates event loop
    pub async fn err_subscribe(&self) -> Result<tokio::sync::watch::Receiver<Option<deno_core::error::CoreError>>, Error> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let msg = V8IsolateManagerMessage::ErrSubscribe { resp: tx };
        self.tx.send(msg).map_err(|_| "Failed to send ErrSubscribe message".to_string())?;
        let rx = rx.await.map_err(|_| "Failed to receive ErrSubscribe response".to_string())?;
        Ok(rx)
    }

    pub async fn eval(&self, code: &str, args: Vec<v8::Global<v8::Value>>) -> Result<v8::Global<v8::Value>, Error> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let msg = V8IsolateManagerMessage::CodeExec { code: code.to_string(), args, resp: tx };
        self.tx.send(msg).map_err(|_| "Failed to send CodeExec message".to_string())?;
        let value = rx.await.map_err(|_| "Failed to receive CodeExec response".to_string())??;
        Ok(value)   
    }
}

impl LuaUserData for V8IsolateManager {
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
}

#[derive(Clone)]
struct Function {
    func: mluau::Function,
}

// OP to bind arguments to a function by ID, returning a run ID
#[op2(fast)]
fn __luabind(
    #[state] state: &CommonState,
    scope: &mut v8::HandleScope,
    func_id: i32,
    args: v8::Local<v8::Array>,
) -> Result<i32, deno_error::JsErrorBox> {
    println!("__luabind called with func_id: {}, args len: {}", func_id, args.length());
    let Some(lua) = state.weak_lua.try_upgrade() else {
        return Err(deno_error::JsErrorBox::generic("Lua state has been dropped".to_string()));
    };
    // Proxy every argument to Lua
    let args_count = args.length();
    let mut lua_args = LuaMultiValue::with_capacity(args_count as usize);
    for i in 0..args_count {
        let arg = args.get_index(scope, i).ok_or_else(|| deno_error::JsErrorBox::generic(format!("Failed to get argument {}", i)))?;
        match V8IsolateManagerInner::proxy_from_v8(scope, &lua, arg, state.finalizer_attachments.clone()) {
            Ok(v) => lua_args.push_back(v),
            Err(e) => {
                return Err(deno_error::JsErrorBox::generic(format!("Failed to convert argument {}: {}", i, e)));
            }
        }
    }

    loop {
        let list = state.list.borrow_mut();
        if list.len() >= MAX_REFS {
            return Err(deno_error::JsErrorBox::generic("Maximum number of running functions reached".to_string()));
        } else {
            let run_id = rand::random::<i32>();
            if list.contains_key(&run_id) {
                continue; // Collision, try again
            }

            let func = list.get(&func_id).ok_or_else(|| deno_error::JsErrorBox::generic("Function ID not found".to_string()))?;
            func.runs.borrow_mut().insert(run_id, FunctionRunState::Bound { lua_args });

            return Ok(run_id);
        }
    }
}

// OP to execute a bound function by func ID/run ID
//
// Returns nothing
#[op2(async)]
async fn __luaexecute(
    state_rc: Rc<RefCell<OpState>>,
    func_id: i32,
    run_id: i32,
) -> Result<(), deno_error::JsErrorBox> {
    let running_funcs = {
        let state = state_rc.try_borrow()
            .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
        
        state.try_borrow::<CommonState>()
            .ok_or_else(|| deno_error::JsErrorBox::generic("CommonState not found".to_string()))?
            .clone()
    };

    let (func_state, func) = {
        let list = running_funcs.list.borrow();
        let func_state = list.get(&func_id)
            .ok_or_else(|| deno_error::JsErrorBox::generic("Function ID not found".to_string()))?;

        // We can remove here because we are executing it now
        let run_state = func_state.runs.borrow_mut().remove(&run_id)
            .ok_or_else(|| deno_error::JsErrorBox::generic("Run ID not found".to_string()))?;

        (run_state, func_state.func.func.clone())
    };

    let lua_args = match func_state {
        FunctionRunState::Bound { lua_args } => lua_args,
        FunctionRunState::Executed { .. } => {
            return Err(deno_error::JsErrorBox::generic("Function has already been executed".to_string()));
        }
    };

    let Some(lua) = running_funcs.weak_lua.try_upgrade() else {
        return Err(deno_error::JsErrorBox::generic("Lua state has been dropped".to_string()));
    };

    let th = lua.create_thread(func)
        .map_err(|e| deno_error::JsErrorBox::generic(format!("Failed to create Lua thread: {}", e)))?;

    let scheduler = mlua_scheduler::taskmgr::get(&lua);
    match scheduler.spawn_thread_and_wait(th, lua_args).await {
        Ok(ret) => {
            match ret {
                Some(v) => {
                    let v = v.map_err(|e| deno_error::JsErrorBox::generic(format!("Lua function error: {}", e)))?;
                    // Store the result in the run state
                    let mut list = running_funcs.list.borrow_mut();
                    let func_state = list.get_mut(&func_id)
                        .ok_or_else(|| deno_error::JsErrorBox::generic("Function ID not found".to_string()))?;
                    func_state.runs.borrow_mut().insert(run_id, FunctionRunState::Executed { lua_resp: v });
                    Ok(())
                },
                None => Err(deno_error::JsErrorBox::generic("Lua thread returned no values".to_string())),
            }
        }
        Err(e) => Err(deno_error::JsErrorBox::generic(format!("Lua thread error: {}", e))),
    }
}

// OP to get the results of a function by func ID/run ID
#[op2]
fn __luaresult<'s>(
    #[state] state: &CommonState,
    scope: &mut v8::HandleScope<'s>,
    func_id: i32,
    run_id: i32,
) -> Result<v8::Local<'s, v8::Array>, deno_error::JsErrorBox> {
    let lua_resp = {
        let list = state.list.borrow();
        let func_state = list.get(&func_id)
            .ok_or_else(|| deno_error::JsErrorBox::generic("Function ID not found".to_string()))?;

        let mut runs_g = func_state.runs.borrow_mut();
        let run_state = runs_g.remove(&run_id)
            .ok_or_else(|| deno_error::JsErrorBox::generic("Run ID not found".to_string()))?;

        match run_state {
            FunctionRunState::Executed { lua_resp } => lua_resp,
            FunctionRunState::Bound { .. } => {
                return Err(deno_error::JsErrorBox::generic("Function has not been executed yet".to_string()));
            }
        }
    };

    // Proxy every return value to V8
    let mut results = vec![];
    for ret in lua_resp {
        match V8IsolateManagerInner::proxy_to_v8(scope, state, ret) {
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

struct UserData {
    pub ud: mluau::AnyUserData,
    pub common_state: CommonState,
}

impl UserData {
    pub fn new(ud: mluau::AnyUserData, common_state: CommonState) -> Self {
        Self { ud, common_state }
    }

    fn named_property_getter<'s>(
        scope: &mut v8::HandleScope<'s>,
        key: v8::Local<'s, v8::Name>,
        args: v8::PropertyCallbackArguments,
        mut rv: v8::ReturnValue,
    ) -> v8::Intercepted {
        let this = args.this();
        let external = v8::Local::<v8::External>::try_from(this.get_internal_field(scope, 0).unwrap()).unwrap();
        if external.value().is_null() {
            if let Some(message) = v8::String::new(scope, "Cannot access external value") {
                let exception = v8::Exception::type_error(scope, message);
                scope.throw_exception(exception);
                return v8::Intercepted::No;
            }
            return v8::Intercepted::No;
        }
        let rust_obj = unsafe { &*(external.value() as *mut UserData) };
        println!("UD: getting property {:?}, ud: {:?}", key, rust_obj.ud);
        // Get the property from the userdata
        let key = {
            let Some(lua) = rust_obj.common_state.weak_lua.try_upgrade() else {
                if let Some(message) = v8::String::new(scope, "Lua state has been dropped") {
                    let exception = v8::Exception::type_error(scope, message);
                    scope.throw_exception(exception);
                }
                return v8::Intercepted::No;
            };

            match V8IsolateManagerInner::proxy_from_v8(scope, &lua, key.into(), rust_obj.common_state.finalizer_attachments.clone()) {
                Ok(v) => v,
                Err(e) => {
                    if let Some(message) = v8::String::new(scope, &format!("Failed to convert key: {}", e)) {
                        let exception = v8::Exception::type_error(scope, message);
                        scope.throw_exception(exception);
                    }
                    return v8::Intercepted::No;
                }
            }
        };

        let val = match rust_obj.ud.get::<LuaValue>(key) {
            Ok(v) => v,
            Err(e) => {
                if let Some(message) = v8::String::new(scope, &format!("Failed to get property from userdata: {}", e)) {
                    let exception = v8::Exception::type_error(scope, message);
                    scope.throw_exception(exception);
                }
                return v8::Intercepted::No;
            }
        };

        let v8_val = match V8IsolateManagerInner::proxy_to_v8(scope, &rust_obj.common_state, val) {
            Ok(v) => v,
            Err(e) => {
                if let Some(message) = v8::String::new(scope, &format!("Failed to convert property to V8: {}", e)) {
                    let exception = v8::Exception::type_error(scope, message);
                    scope.throw_exception(exception);
                }
                return v8::Intercepted::No;
            }
        };

        rv.set(v8_val);
        return v8::Intercepted::Yes;
    }

    fn named_property_setter(
        scope: &mut v8::HandleScope,
        _key: v8::Local<v8::Name>,
        _value: v8::Local<v8::Value>,
        args: v8::PropertyCallbackArguments,
        mut _rv: v8::ReturnValue<()>,
    ) -> v8::Intercepted {
        let this = args.this();
        let external = v8::Local::<v8::External>::try_from(this.get_internal_field(scope, 0).unwrap()).unwrap();
        if external.value().is_null() {
            if let Some(message) = v8::String::new(scope, "Cannot access external value") {
                let exception = v8::Exception::type_error(scope, message);
                scope.throw_exception(exception);
                return v8::Intercepted::No;
            }
            return v8::Intercepted::No;
        }
        let _rust_obj = unsafe { &*(external.value() as *mut UserData) };
        return v8::Intercepted::No; // TODO: Implement
    }
} 


#[cfg(test)]
mod tests {
    use deno_core::v8;
    use mlua_scheduler::{taskmgr::NoopHooks, LuaSchedulerAsync, XRc};
    use mluau::IntoLua;

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
            let (start_tx, start_rx) = tokio::sync::oneshot::channel();
            tokio::task::spawn_local(async move {
                let _inner = V8IsolateManager::run(manager_i, rx, start_tx).await;
            });
            start_rx.await.expect("Failed to start V8 isolate manager");
            println!("V8 isolate manager started");

            // Call the v8 function now as a async script
            let lua_code = r#"
local v8 = ...
local result = v8:eval([[
  (function() {
    async function f(waiter) {
        return await waiter();
    }

    return f;
  })()
]], function() print('am here'); return task.wait(1) end)
return result
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
}