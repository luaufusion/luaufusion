use std::cell::RefCell;
use std::rc::Rc;

use deno_core::{op2, v8, OpState};
use crate::luau::bridge::i32_to_obj_registry_type;

use crate::base::ObjectRegistryID;
use super::value::ProxiedV8Value;
use super::inner::{CommonState, FunctionRunState};

// OP to request dropping an object by ID and type
#[op2(fast)]
pub(super) fn __dropluaobject(
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
pub(super) fn __luadispatch(
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
pub(super) async fn __luarun(
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
pub(super) fn __luaret<'s>(
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