// TODO: Switch to using LuaBridge to enable luau and v8 to be on different threads

pub mod bridge;

use crate::deno::bridge::{BridgeVals, V8IsolateManager};

pub(crate) mod extension;
pub(crate) mod base64_ops;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::rc::Rc;

//use deno_core::error::{CoreError, CoreErrorKind};
use deno_core::v8::CreateParams;
use deno_core::{op2, v8, Extension, OpState};
use tokio_util::sync::CancellationToken;

use crate::luau::bridge::{LuaBridge, LuaBridgeObject, ProxiedLuaValue};
use crate::deno::extension::ExtensionTrait;

use crate::base::{ObjectRegistry, ObjectRegistryID};
use bridge::{ProxiedV8Value, ProxyV8Client};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

const MAX_REFS: usize = 500;
const MIN_HEAP_LIMIT: usize = 10 * 1024 * 1024; // 10MB

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
    bridge: LuaBridge<V8IsolateManager>,
    obj_template: Rc<v8::Global<v8::ObjectTemplate>>,
    finalizer_attachments: FinalizerAttachments,
    proxy_client: ProxyV8Client,
    bridge_vals: Rc<BridgeVals>
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
    func_ids: FinalizerList<ObjectRegistryID<LuaBridgeObject>>,
}

impl V8IsolateManagerInner {    
    pub fn new(bridge: LuaBridge<V8IsolateManager>, heap_limit: usize) -> Self {
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

        let bridge_vals = {
            let isolate = deno.v8_isolate();
            let scope = &mut v8::HandleScope::new(isolate);
            BridgeVals::new(scope)
        };

        let func_ids = FinalizerList::default();

        let common_state = CommonState {
            list: Rc::new(RefCell::new(HashMap::new())),
            obj_template,
            bridge,
            finalizer_attachments: FinalizerAttachments {
                func_ids,
            },
            proxy_client: ProxyV8Client {
                array_registry: ObjectRegistry::new(),
                func_registry: ObjectRegistry::new(),
                obj_registry: ObjectRegistry::new(),
                promise_registry: ObjectRegistry::new(),
            },
            bridge_vals: Rc::new(bridge_vals)
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

// OP to bind arguments to a function by ID, returning a run ID
#[op2(fast)]
fn __luadispatch(
    #[state] state: &CommonState,
    scope: &mut v8::HandleScope,
    #[bigint] func_id: i64,
    args: v8::Local<v8::Array>,
) -> Result<i32, deno_error::JsErrorBox> {
    let mut args_proxied = Vec::with_capacity(args.length() as usize);
    for i in 0..args.length() {
        let arg = args.get_index(scope, i).ok_or_else(|| deno_error::JsErrorBox::generic(format!("Failed to get argument {}", i)))?;
        match ProxiedV8Value::proxy_from_v8(scope, arg, &state, 0) {
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
            bridge.call_function(ObjectRegistryID::from_i64(func_id), args_proxied).await
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
