use std::cell::RefCell;
use std::rc::Rc;

use deno_core::{op2, v8, OpState};

use crate::denobridge::bridge::{MAX_FUNCTION_ARGS, MAX_OWNED_V8_STRING_SIZE};
use crate::luau::bridge::LuauObjectOp;
use crate::luau::objreg::LuauObjectRegistryID;
use super::value::ProxiedV8Value;
use super::inner::{CommonState, FunctionRunState};

// OP to bind arguments to a object by ID, returning a run ID
#[op2(fast)]
pub(super) fn __luabind(
    #[state] state: &CommonState,
    scope: &mut v8::PinScope,
    args: v8::Local<v8::Array>,
) -> Result<i32, deno_error::JsErrorBox> {
    if args.length() > MAX_FUNCTION_ARGS {
        return Err(deno_error::JsErrorBox::generic(format!("Too many function arguments passed to op bind"))); 
    }

    let mut args_proxied = Vec::with_capacity(args.length() as usize);
    let mut num_string_chars = 0;
    for i in 0..args.length() {
        let arg = args.get_index(scope, i).ok_or_else(|| deno_error::JsErrorBox::generic(format!("Failed to get argument {}", i)))?;
        match ProxiedV8Value::from_v8(scope, arg, &state, 0) {
            Ok(v) => {
                let sz = v.effective_size(0);
                if sz > 0 {
                    num_string_chars += sz;
                    if num_string_chars > MAX_OWNED_V8_STRING_SIZE {
                        return Err(deno_error::JsErrorBox::generic(format!("Too many string characters passed to op bind"))); 
                    }
                }
                args_proxied.push(v);
            },
            Err(e) => {
                return Err(deno_error::JsErrorBox::generic(format!("Failed to convert argument {}: {}", i, e)));
            }
        }
    }

    let mut funcs = state.list.borrow_mut();
    let run_id = funcs.len() as i32 + 1;
    funcs.insert(run_id, FunctionRunState::Created {
        args: args_proxied,
    });
    Ok(run_id)
}

// OP to execute a opcall by run ID
//
// Returns nothing
#[op2(async)]
pub(super) async fn __luarun(
    state_rc: Rc<RefCell<OpState>>,
    run_id: i32,
    #[bigint] obj_id: i64,
    op_id: u8,
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
        FunctionRunState::Created { args } => {
            let obj_reg_id = LuauObjectRegistryID::from_i64(obj_id);
            let op_id = LuauObjectOp::try_from(op_id)
                .map_err(|e| deno_error::JsErrorBox::generic(format!("Invalid op_id: {}", e)))?;
            let lua_resp = running_funcs.bridge.opcall(
                obj_reg_id,
                op_id,
                args,
            )
            .await
            .map_err(|e| deno_error::JsErrorBox::generic(format!("Bridge call failed: {}", e)))?;
            // Store the result in the run state
            let mut funcs = running_funcs.list.borrow_mut();
            funcs.insert(run_id, FunctionRunState::Executed { resp: lua_resp });
            return Ok(())
        }
        _ => {
            return Err(deno_error::JsErrorBox::generic("Run not in Created state".to_string()));
        }
    }
}

// OP to get the results of a opcall by run ID
#[op2]
pub(super) fn __luaret<'s>(
    #[state] state: &CommonState,
    scope: &'s mut v8::PinScope,
    run_id: i32,
) -> Result<v8::Local<'s, v8::Array>, deno_error::JsErrorBox> {
    let func_state = {
        let mut funcs = state.list.borrow_mut();
        let func_state = funcs.remove(&run_id)
            .ok_or_else(|| deno_error::JsErrorBox::generic("Run ID not found".to_string()))?;

        func_state
    }; // list borrow ends here

    match func_state {
        FunctionRunState::Executed { resp } => {
            // Proxy every return value to V8
            let mut results = vec![];
            for ret in resp {
                match ret.to_v8(scope, state, 0) {
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