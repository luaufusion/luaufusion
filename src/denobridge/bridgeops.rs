use std::cell::RefCell;
use std::rc::Rc;

use deno_core::parking_lot::Mutex;
use deno_core::{GarbageCollected, OpState, op2, v8};

use crate::denobridge::V8IsolateManagerServer;
use crate::luau::bridge::{LuaBridgeServiceClient, LuauObjectOp, ObjectRegistryType};
use crate::luau::embedder_api::EmbedderDataContext;
use crate::luau::LuauObjectRegistryID;
use super::value::ProxiedV8Value;
use super::inner::CommonState;

pub(super) struct LuaObject {
    pub(super) lua_type: ObjectRegistryType,
    pub(super) lua_id: LuauObjectRegistryID,
    pub(super) bridge: LuaBridgeServiceClient<V8IsolateManagerServer>,

}

pub(super) struct ExtractedLuaObject {
    pub(super) lua_type: ObjectRegistryType,
    pub(super) lua_id: LuauObjectRegistryID
}   

impl LuaObject {
    /// Extract out a LuaObject from a V8 value
    /// 
    /// Returns None if the value is not a LuaObject
    pub(super) fn from_v8<'a>(
        scope: &mut v8::PinScope<'a, '_>,
        value: v8::Local<'a, v8::Value>,
    ) -> Option<ExtractedLuaObject> {
        if let Some(cppgc_obj) = deno_core::cppgc::try_unwrap_cppgc_object::<LuaObject>(scope, value) {
            // Copy out what we need to ensure the UnsafePtr is only stored on the stack for minimal time
            Some(ExtractedLuaObject {
                lua_type: cppgc_obj.lua_type,
                lua_id: cppgc_obj.lua_id.clone(),
            })
        } else {
            None
        }
    }

    /// Creates a new LuaObject with the specified type and ID
    pub(super) fn new(lua_type: ObjectRegistryType, lua_id: LuauObjectRegistryID, bridge: LuaBridgeServiceClient<V8IsolateManagerServer>) -> Self {
        LuaObject {
            lua_type,
            lua_id,
            bridge,
        }
    }

    /// Creates a new LuaObject cppgc object in V8
    pub(super) fn to_v8<'a>(
        self,
        scope: &mut v8::PinScope<'a, '_>,
    ) -> v8::Local<'a, v8::Value> {
        let lua_object = deno_core::cppgc::make_cppgc_object::<LuaObject>(scope, self);

        lua_object.into()
    }

    /// Internal implementation of opcall
    /// that reuses a single ArgBuffer cppgc object
    /// 
    /// Verifies that the args passed in are valid for the opcall
    async fn opcall_impl(
        &self,
        state_rc: Rc<RefCell<OpState>>,
        bound_args: &ArgBuffer,
        op_id: LuauObjectOp,
    ) -> Result<(), deno_error::JsErrorBox> {
        let running_funcs = {
            let state = state_rc.try_borrow()
                .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
            
            state.try_borrow::<CommonState>()
                .ok_or_else(|| deno_error::JsErrorBox::generic("CommonState not found".to_string()))?
                .clone()
        };

        let args = bound_args.consume()?;

        match op_id {
            LuauObjectOp::Index => {
                if args.len() != 1 {
                    return Err(deno_error::JsErrorBox::generic(format!("Indexing a object requires exactly/only 1 argument, got {}", args.len())));
                }
            },
            _ => {}
        }

        let lua_resp = running_funcs.bridge.opcall(
            self.lua_id.clone(),
            op_id,
            args,
        )
        .await
        .map_err(|e| deno_error::JsErrorBox::generic(format!("Bridge call failed: {}", e)))?;

        bound_args.replace(lua_resp)?;
        Ok(())
    }
}

unsafe impl GarbageCollected for LuaObject {
    fn get_name(&self) -> &'static std::ffi::CStr {
        c"LuaObject"
    }

    fn trace(&self, _visitor: &mut v8::cppgc::Visitor) {
        // No fields to trace
    }
}

impl Drop for LuaObject {
    fn drop(&mut self) {
        // No fields to drop
        if cfg!(feature = "debug_message_print_enabled") {
            println!("LuaObject of type {} with ID {:?} dropped", self.lua_type.type_name(), self.lua_id);
        }
        self.bridge.fire_request_dispose(self.lua_id.clone());
    }
}

#[op2]
impl LuaObject {
    //#[constructor]
    //#[cppgc]
    //pub fn constructor(_args: Option<v8::Local<'_, v8::Object>>) -> Result<LuaObject, deno_error::JsErrorBox> {
    //    Err(deno_error::JsErrorBox::generic("LuaObject cannot be constructed directly".to_string()))
    //}

    #[getter]
    #[rename("id")]
    #[bigint]
    pub fn get_lua_id(&self) -> i64 {
        self.lua_id.objid()
    }

    #[getter]
    #[rename("type")]
    #[string]
    pub fn get_lua_type(&self) -> &'static str {
        self.lua_type.type_name()
    }

    #[async_method]
    #[rename("callSync")]
    // Synchronous function call opcall
    pub async fn call_sync(
        &self,
        state_rc: Rc<RefCell<OpState>>,
        #[cppgc] bound_args: &ArgBuffer,
    ) -> Result<(), deno_error::JsErrorBox> {
        self.opcall_impl(state_rc, bound_args, LuauObjectOp::FunctionCallSync).await
    }

    #[async_method]
    #[rename("callAsync")]
    // Asynchronous function call opcall
    pub async fn call_async(
        &self,
        state_rc: Rc<RefCell<OpState>>,
        #[cppgc] bound_args: &ArgBuffer,
    ) -> Result<(), deno_error::JsErrorBox> {
        self.opcall_impl(state_rc, bound_args, LuauObjectOp::FunctionCallAsync).await
    }

    #[async_method]
    #[rename("get")]
    // Index/get a property from the Lua object
    pub async fn get(
        &self,
        state_rc: Rc<RefCell<OpState>>,
        #[cppgc] bound_args: &ArgBuffer,
    ) -> Result<(), deno_error::JsErrorBox> {
        self.opcall_impl(state_rc, bound_args, LuauObjectOp::Index).await
    }

    #[async_method]
    #[rename("requestDispose")]
    // Request disposal of the Lua object. Should be used with care as luaufusion usually handles this automatically
    pub async fn request_dispose(
        &self,
        state_rc: Rc<RefCell<OpState>>,
    ) -> Result<(), deno_error::JsErrorBox> {
        let running_funcs = {
            let state = state_rc.try_borrow()
                .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
            
            state.try_borrow::<CommonState>()
                .ok_or_else(|| deno_error::JsErrorBox::generic("CommonState not found".to_string()))?
                .clone()
        };

        running_funcs.bridge.request_dispose(
            self.lua_id.clone(),
        )
        .await
        .map_err(|e| deno_error::JsErrorBox::generic(format!("Bridge call failed: {}", e)))?;

        Ok(())
    }
}

// A cppgc struct to hold proxied values for passing between ops
//
// We use a cppgc struct to ensure that in the event of errors etc, the values
// are eventually garbage collected all the same by V8's cppgc system
// instead of leaking memory
pub(super) struct ArgBuffer {
    proxied_values: Mutex<Option<Vec<ProxiedV8Value>>>,
}

impl ArgBuffer {
    fn new(values: Vec<ProxiedV8Value>) -> Self {
        ArgBuffer {
            proxied_values: Mutex::new(Some(values)),
        }
    }

    /// Replace the current proxied values with the specified values
    fn replace(&self, values: Vec<ProxiedV8Value>) -> Result<(), deno_error::JsErrorBox> {
        let mut borrowed = self.proxied_values.lock();
        if borrowed.is_some() {
            Err(deno_error::JsErrorBox::generic("ArgBuffer already initialized".to_string()))
        } else {
            *borrowed = Some(values);
            Ok(())
        }
    }

    /// Consume the proxied values, leaving None in its place
    fn consume(&self) -> Result<Vec<ProxiedV8Value>, deno_error::JsErrorBox> {
        let mut borrowed = self.proxied_values.lock();
        borrowed.take().ok_or_else(|| deno_error::JsErrorBox::generic("ArgBuffer already consumed".to_string()))
    }
}

impl Drop for ArgBuffer {
    fn drop(&mut self) {
        // No fields to drop
        if cfg!(feature = "debug_message_print_enabled") {
            println!("ArgBuffer dropped");
        }
    }
}

unsafe impl GarbageCollected for ArgBuffer {
    fn get_name(&self) -> &'static std::ffi::CStr {
        c"ArgBuffer"
    }

    fn trace(&self, _visitor: &mut v8::cppgc::Visitor) {
        // No fields to trace
    }
}

#[op2]
impl ArgBuffer {
    #[constructor]
    #[cppgc]
    pub fn constructor<'a>(
        op_state: &OpState, 
        scope: &mut v8::PinScope<'a, '_>,
        #[varargs] fn_args: Option<&v8::FunctionCallbackArguments<'a>>,
    ) -> Result<ArgBuffer, deno_error::JsErrorBox> {
        let Some(args) = fn_args else {
            return Ok(ArgBuffer::new(vec![]))
        };

        let state = op_state.try_borrow::<CommonState>()
        .ok_or_else(|| deno_error::JsErrorBox::generic("CommonState not found".to_string()))?;

        let mut ed = EmbedderDataContext::new(&state.ed);

        let mut args_proxied = Vec::with_capacity(args.length() as usize);
        for i in 0..args.length() {
            let arg = args.get(i);
            match ProxiedV8Value::from_v8(scope, arg, &state, &mut ed) {
                Ok(v) => {
                    args_proxied.push(v);
                },
                Err(e) => {
                    return Err(deno_error::JsErrorBox::generic(format!("Failed to convert argument {}: {}", i, e)));
                }
            }
        }

        let proxied_values = ArgBuffer::new(args_proxied);
        Ok(proxied_values)
    }

    #[fast]
    #[method]
    // Returns the length of the proxied values
    pub fn size(&self) -> Result<u32, deno_error::JsErrorBox> {
        let borrowed = self.proxied_values.lock();
        match &*borrowed {
            Some(vals) => {
                if vals.len() > (u32::MAX as usize) {
                    Err(deno_error::JsErrorBox::generic("ArgBuffer length exceeds u32 max, use sizeBigint instead".to_string()))
                } else {
                    Ok(vals.len() as u32)
                }
            },
            None => Err(deno_error::JsErrorBox::generic("ArgBuffer already consumed".to_string())),
        }
    }

    #[fast]
    #[method]
    #[rename("sizeBigint")]
    #[bigint]
    // Returns the length of the proxied values as bigint
    pub fn size_bigint(&self) -> Result<usize, deno_error::JsErrorBox> {
        let borrowed = self.proxied_values.lock();
        match &*borrowed {
            Some(vals) => {
                Ok(vals.len())
            },
            None => Err(deno_error::JsErrorBox::generic("ArgBuffer already consumed".to_string())),
        }
    }

    #[method]
    // After a function call or otherwise, take the values out of the ArgBuffer
    pub fn take<'a>(
        &self,
        scope: &mut v8::PinScope<'a, '_>,
        op_state: &OpState,
    ) -> Result<v8::Local<'a, v8::Array>, deno_error::JsErrorBox> {
        let state = op_state.try_borrow::<CommonState>()
            .ok_or_else(|| deno_error::JsErrorBox::generic("CommonState not found".to_string()))?;

        // Proxy every return value to V8
        let mut results = vec![];
        let mut ed = EmbedderDataContext::new(&state.ed);
        let args = self.consume()?;
        for arg in args {
            match arg.to_v8(scope, state, &mut ed) {
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

    #[method]
    // Similar to take, but flattens single-valued results into just that value
    // and returns undefined for zero-length results
    pub fn extract<'a>(
        &self,
        scope: &mut v8::PinScope<'a, '_>,
        op_state: &OpState,
    ) -> Result<v8::Local<'a, v8::Value>, deno_error::JsErrorBox> {
        let state = op_state.try_borrow::<CommonState>()
            .ok_or_else(|| deno_error::JsErrorBox::generic("CommonState not found".to_string()))?;

        // Proxy every return value to V8
        let mut results = vec![];
        let mut ed = EmbedderDataContext::new(&state.ed);
        let args = self.consume()?;

        match args.len() {
            0 => return Ok(v8::undefined(scope).into()),
            1 => {
                let arg = args.into_iter().next().ok_or(deno_error::JsErrorBox::generic("Failed to extract single value".to_string()))?;
                let v8_ret = arg.to_v8(scope, state, &mut ed)
                    .map_err(|e| deno_error::JsErrorBox::generic(format!("Failed to convert return value: {}", e)))?;
                return Ok(v8_ret);
            },
            _ => {
                for arg in args {
                    match arg.to_v8(scope, state, &mut ed) {
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

                Ok(arr.into())
            }
        }
    }
}
