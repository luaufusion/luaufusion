// TODO: Switch to using LuaBridge to enable luau and v8 to be on different threads

pub mod bridge;
pub mod modloader;

use bridge::{BridgeVals, V8IsolateManagerServer};

pub(crate) mod denoexts;

use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;
use std::rc::Rc;

//use deno_core::error::{CoreError, CoreErrorKind};
use deno_core::v8::CreateParams;
use deno_core::{op2, v8, OpState};
use tokio_util::sync::CancellationToken;

use crate::denobridge::modloader::FusionModuleLoader;
use crate::luau::bridge::{i32_to_obj_registry_type, LuaBridgeServiceClient};

use crate::base::{ObjectRegistry, ObjectRegistryID};
use bridge::{ProxiedV8Value, ProxyV8Client};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

const MIN_HEAP_LIMIT: usize = 10 * 1024 * 1024; // 10MB

    
/// Stores a lua function state
/// 
/// This is used internally to track async function call states
enum FunctionRunState {
    Created {
        fut: Pin<Box<dyn std::future::Future<Output = Result<Vec<ProxiedV8Value>, Error>>>>,
    },
    Executed {
        lua_resp: Vec<ProxiedV8Value>,
    },
}

#[derive(Clone)]
pub struct CommonState {
    list: Rc<RefCell<HashMap<i32, FunctionRunState>>>,
    bridge: LuaBridgeServiceClient<V8IsolateManagerServer>,
    obj_template: Rc<v8::Global<v8::ObjectTemplate>>,
    proxy_client: ProxyV8Client,
    bridge_vals: Rc<BridgeVals>,
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

impl V8IsolateManagerInner {    
    pub fn new(bridge: LuaBridgeServiceClient<V8IsolateManagerServer>, heap_limit: usize, loader: FusionModuleLoader) -> Self {
        let heap_limit = heap_limit.max(MIN_HEAP_LIMIT);

        // TODO: Support snapshots maybe
        let extensions = denoexts::extension::all_extensions(false);

        let mut deno = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
            create_params: Some(
                CreateParams::default()
                .heap_limits(0, heap_limit)
            ),
            extensions,
            module_loader: Some(Rc::new(loader)),
            inspector: false,
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
            let main_ctx = deno.main_context();
            let isolate = deno.v8_isolate();
            let scope = &mut v8::HandleScope::new(isolate);
            let main_ctx = v8::Local::new(scope, main_ctx);
            let scope = &mut v8::ContextScope::new(scope, main_ctx);
            BridgeVals::new(scope)
        };

        let common_state = CommonState {
            list: Rc::new(RefCell::new(HashMap::new())),
            obj_template,
            bridge,
            proxy_client: ProxyV8Client {
                string_registry: ObjectRegistry::new(),
                array_registry: ObjectRegistry::new(),
                func_registry: ObjectRegistry::new(),
                obj_registry: ObjectRegistry::new(),
                array_buffer_registry: ObjectRegistry::new(),
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
        
        template
    }
}

// OP to request dropping an object by ID and type
#[op2(fast)]
fn __dropluaobject(
    #[state] state: &CommonState,
    typ: i32,
    #[bigint] id: i64,
) {
    if let Some(typ) = i32_to_obj_registry_type(typ) {            
        let id = ObjectRegistryID::from_i64(id);
        state.bridge.request_drop_object(typ, id);
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
fn __luaret<'s>(
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
                match ret.proxy_to_v8(scope, state) {
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