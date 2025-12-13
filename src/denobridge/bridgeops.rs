use std::cell::RefCell;
use std::rc::Rc;

use deno_core::parking_lot::Mutex;
use deno_core::{GarbageCollected, OpState, op2, v8};

use crate::denobridge::primitives::ProxiedV8Primitive;
use crate::luau::bridge::{LuauObjectOp, ObjectRegistryType};
use crate::luau::embedder_api::EmbedderDataContext;
use crate::luau::LuauObjectRegistryID;
use super::value::ProxiedV8Value;
use super::inner::CommonState;

pub(super) struct LuaObject {
    pub(super) lua_type: ObjectRegistryType,
    pub(super) lua_id: LuauObjectRegistryID
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
            Some(ExtractedLuaObject {
                lua_type: cppgc_obj.lua_type,
                lua_id: cppgc_obj.lua_id.clone(),
            })
        } else {
            None
        }
    }

    /// Creates a new LuaObject with the specified type and ID
    pub(super) fn new(lua_type: ObjectRegistryType, lua_id: LuauObjectRegistryID) -> Self {
        LuaObject {
            lua_type,
            lua_id
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
    /// that reuses a single ProxiedValues cppgc object
    async fn opcall_impl(
        &self,
        state_rc: Rc<RefCell<OpState>>,
        bound_args: &ProxiedValues,
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

        let op_id = LuauObjectOp::try_from(op_id)
            .map_err(|e| deno_error::JsErrorBox::generic(format!("Invalid op_id: {}", e)))?;

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

    /// Internal implementation of opcall
    /// that creates a new ProxiedValues cppgc object for each call
    async fn opcall_impl_new_value(
        &self,
        state_rc: Rc<RefCell<OpState>>,
        args: Vec<ProxiedV8Value>,
        op_id: LuauObjectOp,
    ) -> Result<ProxiedValues, deno_error::JsErrorBox> {
        let running_funcs = {
            let state = state_rc.try_borrow()
                .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
            
            state.try_borrow::<CommonState>()
                .ok_or_else(|| deno_error::JsErrorBox::generic("CommonState not found".to_string()))?
                .clone()
        };

        let op_id = LuauObjectOp::try_from(op_id)
            .map_err(|e| deno_error::JsErrorBox::generic(format!("Invalid op_id: {}", e)))?;

        let lua_resp = running_funcs.bridge.opcall(
            self.lua_id.clone(),
            op_id,
            args,
        )
        .await
        .map_err(|e| deno_error::JsErrorBox::generic(format!("Bridge call failed: {}", e)))?;

        return Ok(ProxiedValues::new(lua_resp));
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
        println!("LuaObject of type {} with ID {:?} dropped", self.lua_type.type_name(), self.lua_id);
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
    #[rename("type")]
    #[string]
    pub fn get_lua_type(&self) -> &'static str {
        self.lua_type.type_name()
    }

    #[getter]
    #[rename("id")]
    #[bigint]
    pub fn get_lua_id(&self) -> i64 {
        self.lua_id.objid()
    }

    #[async_method]
    // Performs an opcall on this LuaObject
    pub async fn opcall(
        &self,
        state_rc: Rc<RefCell<OpState>>,
        #[cppgc] bound_args: &ProxiedValues,
        op_id: u8,
    ) -> Result<(), deno_error::JsErrorBox> {
        let op_id = LuauObjectOp::try_from(op_id)
            .map_err(|e| deno_error::JsErrorBox::generic(format!("Invalid op_id: {}", e)))?;

        self.opcall_impl(state_rc, bound_args, op_id).await
    }

    #[async_method]
    #[rename("callSync")]
    // Synchronous function call opcall
    pub async fn call_sync(
        &self,
        state_rc: Rc<RefCell<OpState>>,
        #[cppgc] bound_args: &ProxiedValues,
    ) -> Result<(), deno_error::JsErrorBox> {
        self.opcall_impl(state_rc, bound_args, LuauObjectOp::FunctionCallSync).await
    }

    #[async_method]
    #[rename("callAsync")]
    // Asynchronous function call opcall
    pub async fn call_async(
        &self,
        state_rc: Rc<RefCell<OpState>>,
        #[cppgc] bound_args: &ProxiedValues,
    ) -> Result<(), deno_error::JsErrorBox> {
        self.opcall_impl(state_rc, bound_args, LuauObjectOp::FunctionCallAsync).await
    }

    #[async_method]
    #[rename("get")]
    // Index/get opcall
    pub async fn get(
        &self,
        state_rc: Rc<RefCell<OpState>>,
        #[cppgc] bound_args: &ProxiedValues,
    ) -> Result<(), deno_error::JsErrorBox> {
        self.opcall_impl(state_rc, bound_args, LuauObjectOp::Index).await
    }

    #[async_method]
    #[rename("getProperty")]
    #[cppgc]
    // Get a property
    //
    // Similar to get, except works with a arg that is a String
    pub async fn get_property<'a>(
        &self,
        state_rc: Rc<RefCell<OpState>>,
        #[string] prop: String,
    ) -> Result<ProxiedValues, deno_error::JsErrorBox> {
        self.opcall_impl_new_value(state_rc, vec![ProxiedV8Value::Primitive(ProxiedV8Primitive::String(prop))], LuauObjectOp::Index).await
    }

    #[async_method]
    #[rename("getIndex")]
    #[cppgc]
    // Get a property
    //
    // Similar to get, except works with a arg that is a i32 index
    pub async fn get_index<'a>(
        &self,
        state_rc: Rc<RefCell<OpState>>,
        prop: i32,
    ) -> Result<ProxiedValues, deno_error::JsErrorBox> {
        self.opcall_impl_new_value(state_rc, vec![ProxiedV8Value::Primitive(ProxiedV8Primitive::Integer(prop))], LuauObjectOp::Index).await
    }
}

// A cppgc struct to hold proxied values for passing between ops
//
// We use a cppgc struct to ensure that in the event of errors etc, the values
// are eventually garbage collected all the same by V8's cppgc system
// instead of leaking memory
pub(super) struct ProxiedValues {
    proxied_values: Mutex<Option<Vec<ProxiedV8Value>>>,
}

impl ProxiedValues {
    fn new(values: Vec<ProxiedV8Value>) -> Self {
        ProxiedValues {
            proxied_values: Mutex::new(Some(values)),
        }
    }

    /// Replace the current proxied values with the specified values
    fn replace(&self, values: Vec<ProxiedV8Value>) -> Result<(), deno_error::JsErrorBox> {
        let mut borrowed = self.proxied_values.lock();
        if borrowed.is_some() {
            Err(deno_error::JsErrorBox::generic("ProxiedValues already initialized".to_string()))
        } else {
            *borrowed = Some(values);
            Ok(())
        }
    }

    /// Consume the proxied values, leaving None in its place
    fn consume(&self) -> Result<Vec<ProxiedV8Value>, deno_error::JsErrorBox> {
        let mut borrowed = self.proxied_values.lock();
        borrowed.take().ok_or_else(|| deno_error::JsErrorBox::generic("ProxiedValues already consumed".to_string()))
    }
}

impl Drop for ProxiedValues {
    fn drop(&mut self) {
        // No fields to drop
        println!("ProxiedValues dropped");
    }
}

unsafe impl GarbageCollected for ProxiedValues {
    fn get_name(&self) -> &'static std::ffi::CStr {
        c"ProxiedValues"
    }

    fn trace(&self, _visitor: &mut v8::cppgc::Visitor) {
        // No fields to trace
    }
}

#[op2]
impl ProxiedValues {
    #[constructor]
    #[cppgc]
    pub fn constructor<'a>(
        op_state: &OpState, 
        scope: &mut v8::PinScope<'a, '_>,
        #[varargs] fn_args: Option<&v8::FunctionCallbackArguments<'a>>,
    ) -> Result<ProxiedValues, deno_error::JsErrorBox> {
        let Some(args) = fn_args else {
            return Ok(ProxiedValues::new(vec![]))
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

        let proxied_values = ProxiedValues::new(args_proxied);
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
                    Err(deno_error::JsErrorBox::generic("ProxiedValues length exceeds u32 max, use sizeBigint instead".to_string()))
                } else {
                    Ok(vals.len() as u32)
                }
            },
            None => Err(deno_error::JsErrorBox::generic("ProxiedValues already consumed".to_string())),
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
            None => Err(deno_error::JsErrorBox::generic("ProxiedValues already consumed".to_string())),
        }
    }

    #[method]
    // After a function call, take the values out of the ProxiedValues
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
}
