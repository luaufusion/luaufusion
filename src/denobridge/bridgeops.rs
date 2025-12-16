use std::sync::atomic::AtomicBool;

use deno_core::{GarbageCollected, op2, v8};

use crate::denobridge::V8IsolateManagerServer;
use crate::luau::bridge::{LuaBridgeServiceClient, ObjectRegistryType};
use crate::luau::LuauObjectRegistryID;
use super::value::ProxiedV8Value;

pub(super) struct LuaObject {
    pub(super) lua_type: ObjectRegistryType,
    pub(super) lua_id: LuauObjectRegistryID,
    pub(super) bridge: LuaBridgeServiceClient<V8IsolateManagerServer>,
    pub(super) dropped: AtomicBool,
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
                lua_id: cppgc_obj.lua_id,
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
            dropped: AtomicBool::new(false),
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
        if self.dropped.swap(true, std::sync::atomic::Ordering::SeqCst) {
            // Already dropped
            return;
        }
        self.bridge.fire_request_disposes(vec![self.lua_id]);
    }
}

#[op2]
impl LuaObject {
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
    #[to_v8]
    // Synchronous function call opcall
    pub async fn call_sync(
        &self,
        #[from_v8] args: Option<Vec<ProxiedV8Value>>,
    ) -> Result<Vec<ProxiedV8Value>, deno_error::JsErrorBox> {
        let args = args.unwrap_or_default();

        // NOTE: It is highly tempting to put a scope guard and just GC all v8 refs however this is *usually*
        // wrong. As long as the luau call finishes the ProxiedV8Value::to_luau correctly, the Luau side will automatically
        // GC the v8 references by itself. In short, we don't need to do anything here regarding GC/object drop.

        let resp = self.bridge.call_function_sync(
            self.lua_id,
            args,
        )
        .await
        .map_err(|e| deno_error::JsErrorBox::generic(format!("Bridge call failed: {}", e)))?;

        Ok(resp)
    }

    #[async_method]
    #[rename("callAsync")]
    #[to_v8]
    // Asynchronous function call
    pub async fn call_async(
        &self,
        #[from_v8] args: Option<Vec<ProxiedV8Value>>,
    ) -> Result<Vec<ProxiedV8Value>, deno_error::JsErrorBox> {
        let args = args.unwrap_or_default();

        // NOTE: It is highly tempting to put a scope guard and just GC all v8 refs however this is *usually*
        // wrong. As long as the luau call finishes the ProxiedV8Value::to_luau correctly, the Luau side will automatically
        // GC the v8 references by itself. In short, we don't need to do anything here regarding GC/object drop.

        let resp = self.bridge.call_function_async(
            self.lua_id,
            args,
        )
        .await
        .map_err(|e| deno_error::JsErrorBox::generic(format!("Bridge call failed: {}", e)))?;

        Ok(resp)
    }

    #[async_method]
    #[rename("get")]
    #[to_v8]
    // Index/get a property from the Lua object
    pub async fn get(
        &self,
        #[from_v8] key: ProxiedV8Value,
    ) -> Result<ProxiedV8Value, deno_error::JsErrorBox> {
        // NOTE: It is highly tempting to put a scope guard and just GC all v8 refs however this is *usually*
        // wrong. As long as the luau call finishes the ProxiedV8Value::to_luau correctly, the Luau side will automatically
        // GC the v8 references by itself. In short, we don't need to do anything here regarding GC/object drop.

        let resp = self.bridge.index(
            self.lua_id,
            key
        )
        .await
        .map_err(|e| deno_error::JsErrorBox::generic(format!("Bridge call failed: {}", e)))?;

        Ok(resp)
    }

    #[async_method]
    #[rename("requestDispose")]
    // Request disposal of the Lua object. Should be used with care as luaufusion usually handles this automatically
    //
    // If set, stores a drop flag to avoid double disposal. It is implementation-defined behavior whether other operations can
    // be performed after this however these other operations are guaranteed to be safely error if not possible.
    pub async fn request_dispose(
        &self,
    ) -> Result<(), deno_error::JsErrorBox> {
        self.bridge.request_dispose(
            self.lua_id,
        )
        .await
        .map_err(|e| deno_error::JsErrorBox::generic(format!("Bridge call failed: {}", e)))?;

        self.dropped.store(true, std::sync::atomic::Ordering::SeqCst);

        Ok(())
    }
}