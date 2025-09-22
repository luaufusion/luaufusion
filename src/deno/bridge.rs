use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use concurrentlyexec::{ConcurrentExecutor, ConcurrentExecutorState, ConcurrentlyExecute, MultiSender, OneshotSender, ProcessOpts};
//use deno_core::error::{CoreError, CoreErrorKind};
use deno_core::v8;
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
//use deno_core::{op2, OpState};
//use deno_error::JsErrorBox;
//use mluau::serde::de;

use crate::luau::bridge::{
    LuaBridgeMessage, LuaBridgeObject, LuaBridgeService, LuaBridgeServiceClient, ObjectRegistryType, ProxiedLuaValue, ProxyLuaClient,
    obj_registry_type_to_i32, i32_to_obj_registry_type
};

use crate::base::{ObjectRegistry, ObjectRegistryID, ProxyBridge};
use crate::deno::{CommonState, V8IsolateManagerInner};
use super::Error;

use crate::MAX_PROXY_DEPTH;

/// Minimum stack size for V8 isolates
pub const V8_MIN_STACK_SIZE: usize = 1024 * 1024 * 15; // 15MB minimum memory

pub(crate) struct BridgeVals {
    type_field: v8::Global<v8::String>,
    id_field: v8::Global<v8::String>,
    length_field: v8::Global<v8::String>,
    create_lua_object_from_data: v8::Global<v8::Function>,
}

impl BridgeVals {
    pub(crate) fn new<'s>(scope: &mut v8::HandleScope<'s>) -> Self {
        let id_field = v8::String::new(scope, "luaid").unwrap();
        let type_field = v8::String::new(scope, "luatype").unwrap();
        let length_field = v8::String::new(scope, "length").unwrap();

        // The createLuaObjectFromData function is stored in globalThis.lua.createLuaObjectFromData
        let create_lua_object_from_data = {
            let global = scope.get_current_context().global(scope);
            let lua_str = v8::String::new(scope, "lua").unwrap();
            let lua_obj = global.get(scope, lua_str.into()).unwrap();
            assert!(lua_obj.is_object());
            let lua_obj = lua_obj.to_object(scope).unwrap();
            let clofd = v8::String::new(scope, "createLuaObjectFromData").unwrap();
            let create_lua_object_from_data = lua_obj.get(scope, clofd.into()).unwrap();
            assert!(create_lua_object_from_data.is_function());
            let create_lua_object_from_data = v8::Local::<v8::Function>::try_from(create_lua_object_from_data).unwrap();
            create_lua_object_from_data
        };

        Self {
            id_field: v8::Global::new(scope, id_field),
            type_field: v8::Global::new(scope, type_field),
            length_field: v8::Global::new(scope, length_field),
            create_lua_object_from_data: v8::Global::new(scope, create_lua_object_from_data),
        }
    }
}

/// Marker struct for V8 objects in the object registry
#[derive(Clone, Copy)]
pub struct V8BridgeObject;

/// The core struct encapsulating a V8 object being proxied *to* luau
pub struct V8ObjectInner {
    pub id: ObjectRegistryID<V8BridgeObject>,
    pub typ: V8ObjectRegistryType,
    pub bridge: V8IsolateManagerServer,
}

impl V8ObjectInner {
    fn new(id: ObjectRegistryID<V8BridgeObject>, typ: V8ObjectRegistryType, bridge: V8IsolateManagerServer) -> Self {
        Self {
            id,
            typ,
            bridge,
        }
    }
}

#[derive(Clone)]
pub struct V8Object {
    pub inner: Rc<RefCell<Option<V8ObjectInner>>>,
}

impl V8Object {
    fn new(id: ObjectRegistryID<V8BridgeObject>, typ: V8ObjectRegistryType, bridge: V8IsolateManagerServer) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Some(V8ObjectInner::new(id, typ, bridge)))),
        }
    }

    fn get<R>(&self, func: impl FnOnce(&V8ObjectInner) -> R) -> Option<R> {
        if let Some(inner) = self.inner.borrow().as_ref() {
            func(inner).into()
        } else {
            None
        }
    }

    fn get_bridge(&self) -> Option<V8IsolateManagerServer> {
        self.get(|inner| inner.bridge.clone())
    }
}

impl mluau::UserData for V8Object {
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_, this, ()| {
            match this.inner.borrow().as_ref() {
                Some(inner) => Ok(inner.id.objid()),
                None => Err(mluau::Error::external("V8Object has already been dropped")),
            }
        });

        methods.add_method("typestr", |_, this, ()| {
            let typ = match this.inner.borrow().as_ref() {
                Some(inner) => inner.typ,
                None => return Err(mluau::Error::external("V8Object has already been dropped")),
            };
            let typ_str = match typ {
                V8ObjectRegistryType::ArrayBuffer => "ArrayBuffer",
                V8ObjectRegistryType::String => "String",
                V8ObjectRegistryType::Object => "Object",
                V8ObjectRegistryType::Array => "Array",
                V8ObjectRegistryType::Function => "Function",
                V8ObjectRegistryType::Promise => "Promise",
            };
            Ok(typ_str.to_string())
        });

        methods.add_method("type", |_, this, ()| {
            let typ = match this.inner.borrow().as_ref() {
                Some(inner) => inner.typ,
                None => return Err(mluau::Error::external("V8Object has already been dropped")),
            };
            Ok(v8_obj_registry_type_to_i32(typ))
        });

        methods.add_method("requestdispose", |_, this, ()| {
            if let Some(v) = this.inner.borrow_mut().take() {
                let bridge = v.bridge;
                bridge.request_drop_object(v.typ, v.id);
            }
            Ok(())
        }); 
    }
}

pub struct V8String {
    pub obj: V8Object,
    pub len: usize,
}

impl V8String {
    fn new(id: ObjectRegistryID<V8BridgeObject>, len: usize, bridge: V8IsolateManagerServer) -> Self {
        Self {
            obj: V8Object::new(id, V8ObjectRegistryType::String, bridge),
            len,
        }
    }
}

impl mluau::UserData for V8String {
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mluau::MetaMethod::Len, |_, this, ()| {
            Ok(this.len)
        });

        methods.add_method("object", |_, this, ()| {
            Ok(this.obj.clone())
        });
    }
}

macro_rules! impl_v8_obj_stub {
    ($name:ident, $typ:expr) => {
        pub struct $name {
            pub obj: V8Object,
        }

        impl $name {
            pub fn new(id: ObjectRegistryID<V8BridgeObject>, bridge: V8IsolateManagerServer) -> Self {
                Self {
                    obj: V8Object::new(id, $typ, bridge),
                }
            }
        }

        impl mluau::UserData for $name {
            fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
                methods.add_method("object", |_, this, ()| {
                    Ok(this.obj.clone())
                });
            }
        }
    };
}

// For now
impl_v8_obj_stub!(V8ArrayBuffer, V8ObjectRegistryType::ArrayBuffer);
impl_v8_obj_stub!(V8ObjectObj, V8ObjectRegistryType::Object);
impl_v8_obj_stub!(V8Array, V8ObjectRegistryType::Array);
impl_v8_obj_stub!(V8Function, V8ObjectRegistryType::Function);
impl_v8_obj_stub!(V8Promise, V8ObjectRegistryType::Promise);

/// A V8 value that can now be easily proxied to Luau
#[derive(Serialize, Deserialize)]
pub enum ProxiedV8Value {
    Nil,
    Undefined,
    Boolean(bool),
    Integer(i32),
    Number(f64),
    ArrayBuffer(ObjectRegistryID<V8BridgeObject>), // Buffer ID in the buffer registry
    String((ObjectRegistryID<V8BridgeObject>, usize)), // String ID in the string registry, length
    Object(ObjectRegistryID<V8BridgeObject>), // Object ID in the map registry
    Array(ObjectRegistryID<V8BridgeObject>), // Array ID in the array registry
    Function(ObjectRegistryID<V8BridgeObject>), // Function ID in the function registry
    Promise(ObjectRegistryID<V8BridgeObject>), // Promise ID in the function registry

    // Source-owned stuff
    SrcFunction(ObjectRegistryID<LuaBridgeObject>), // Function ID in the source lua's function registry
    SrcTable(ObjectRegistryID<LuaBridgeObject>), // Table ID in the source lua's table registry
    SrcThread(ObjectRegistryID<LuaBridgeObject>), // Thread ID in the source lua's thread registry
    SrcBuffer(ObjectRegistryID<LuaBridgeObject>), // Buffer ID in the source lua's buffer registry
    SrcUserData(ObjectRegistryID<LuaBridgeObject>), // Userdata ID in the source lua's userdata registry
    SrcString(ObjectRegistryID<LuaBridgeObject>), // String ID in the source lua's string registry
}

impl ProxiedV8Value {
    pub(crate) fn proxy_to_src_lua(self, lua: &mluau::Lua, plc: &ProxyLuaClient, bridge: &V8IsolateManagerServer, depth: usize) -> Result<mluau::Value, mluau::Error> {
        if depth > MAX_PROXY_DEPTH {
            return Err(mluau::Error::external("Maximum proxy depth exceeded"));
        }
        
        match self {
            ProxiedV8Value::Nil | ProxiedV8Value::Undefined => Ok(mluau::Value::Nil),
            ProxiedV8Value::Boolean(b) => Ok(mluau::Value::Boolean(b)),
            ProxiedV8Value::Integer(i) => Ok(mluau::Value::Integer(i as i64)),
            ProxiedV8Value::Number(n) => Ok(mluau::Value::Number(n)),

            // v8 values (v8 values being proxied from v8 to lua)
            ProxiedV8Value::ArrayBuffer(buf_id) => {
                let ud = V8ArrayBuffer::new(buf_id, bridge.clone());
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }
            ProxiedV8Value::String((string_id, len)) => {
                let ud = V8String::new(string_id, len, bridge.clone());
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }
            ProxiedV8Value::Object(obj_id) => {
                let ud = V8ObjectObj::new(obj_id, bridge.clone());
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }
            ProxiedV8Value::Array(arr_id) => {
                let ud = V8Array::new(arr_id, bridge.clone());
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }
            ProxiedV8Value::Function(func_id) => {
                let ud = V8Function::new(func_id, bridge.clone());
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }
            ProxiedV8Value::Promise(promise_id) => {
                let ud = V8Promise::new(promise_id, bridge.clone());
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }

            // Source-owned values (lua values being proxied back from v8 to lua)
            ProxiedV8Value::SrcString(str_id) => {
                let s = plc.string_registry.get(str_id)
                    .ok_or_else(|| mluau::Error::external(format!("String ID {} not found in registry", str_id)))?;
                Ok(mluau::Value::String(s))
            }
            ProxiedV8Value::SrcFunction(func_id) => {
                let func = plc.func_registry.get(func_id)
                    .ok_or_else(|| mluau::Error::external(format!("Function ID {} not found in registry", func_id)))?;
                Ok(mluau::Value::Function(func))
            }
            ProxiedV8Value::SrcTable(table_id) => {
                let table = plc.table_registry.get(table_id)
                    .ok_or_else(|| mluau::Error::external(format!("Table ID {} not found in registry", table_id)))?;
                Ok(mluau::Value::Table(table))
            }
            ProxiedV8Value::SrcThread(thread_id) => {
                let thread = plc.thread_registry.get(thread_id)
                    .ok_or_else(|| mluau::Error::external(format!("Thread ID {} not found in registry", thread_id)))?;
                Ok(mluau::Value::Thread(thread))
            }
            ProxiedV8Value::SrcBuffer(buf_id) => {
                let buffer = plc.buffer_registry.get(buf_id)
                    .ok_or_else(|| mluau::Error::external(format!("Buffer ID {} not found in registry", buf_id)))?;
                Ok(mluau::Value::Buffer(buffer))
            }
            ProxiedV8Value::SrcUserData(ud_id) => {
                let userdata = plc.userdata_registry.get(ud_id)
                    .ok_or_else(|| mluau::Error::external(format!("Userdata ID {} not found in registry", ud_id)))?;
                Ok(mluau::Value::UserData(userdata))
            }
        }
    }

    pub(crate) fn proxy_from_v8_get_proxied<'s>(
        scope: &mut v8::HandleScope<'s>,
        common_state: &CommonState,
        obj: v8::Local<'s, v8::Object>,
    ) -> Result<Option<Self>, Error> {
        let typ_key = v8::Local::new(scope, &common_state.bridge_vals.type_field);
        let typ_val = obj.get(scope, typ_key.into());
        if let Some(typ_val) = typ_val {
            if typ_val.is_int32() {
                let typ_i32 = typ_val.to_int32(scope).ok_or("Failed to convert lua type to int32")?.value();
                if let Some(typ) = i32_to_obj_registry_type(typ_i32) {
                    
                    // Look for __luaid
                    let lua_id = {
                        let p_obj_key = v8::Local::new(scope, &common_state.bridge_vals.id_field);
                        let p_obj_val = obj.get(scope, p_obj_key.into());
                        if let Some(p_obj_val) = p_obj_val {
                            if p_obj_val.is_big_int() {
                                let id = p_obj_val.to_big_int(scope).ok_or("Failed to convert lua function id to int32")?.i64_value().0;
                                Some(id)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };

                    let Some(lua_id) = lua_id else {
                        return Ok(None);
                    };

                    match typ {
                        ObjectRegistryType::Function => {
                            return Ok(Some(Self::SrcFunction(ObjectRegistryID::from_i64(lua_id))));
                        }
                        ObjectRegistryType::Table => {
                            return Ok(Some(Self::SrcTable(ObjectRegistryID::from_i64(lua_id))));
                        }
                        ObjectRegistryType::Thread => {
                            return Ok(Some(Self::SrcThread(ObjectRegistryID::from_i64(lua_id))));
                        }
                        ObjectRegistryType::Buffer => {
                            return Ok(Some(Self::SrcBuffer(ObjectRegistryID::from_i64(lua_id))));
                        }
                        ObjectRegistryType::UserData => {
                            return Ok(Some(Self::SrcUserData(ObjectRegistryID::from_i64(lua_id))));
                        }
                        ObjectRegistryType::String => {
                            return Ok(Some(Self::SrcString(ObjectRegistryID::from_i64(lua_id))));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    pub(crate) fn proxy_from_v8<'s>(
        scope: &mut v8::HandleScope<'s>, 
        value: v8::Local<'s, v8::Value>,
        common_state: &CommonState,
        depth: usize,
    ) -> Result<Self, Error> {
        if depth > MAX_PROXY_DEPTH {
            return Err("Maximum proxy depth exceeded".into());
        }

        if value.is_null() {
            return Ok(Self::Nil);
        } else if value.is_undefined() {
            return Ok(Self::Undefined)
        } else if value.is_boolean() {
            let b = value.to_boolean(scope).is_true();
            return Ok(Self::Boolean(b));
        } else if value.is_int32() {
            let i = value.to_int32(scope).ok_or("Failed to convert to int32")?.value();
            return Ok(Self::Integer(i));
        } else if value.is_number() {
            let n = value.to_number(scope).ok_or("Failed to convert to number")?.value();
            return Ok(Self::Number(n));
        } else if value.is_string() {
            let s = value.to_string(scope).ok_or("Failed to convert to string")?;
            let s_len = s.length();
            let global_str = v8::Global::new(scope, s);
            let sid = common_state.proxy_client.string_registry.add(global_str)
                .ok_or("Failed to register string: too many string references")?;
            return Ok(Self::String((sid, s_len)));
        } else if value.is_array() {
            let arr = v8::Local::<v8::Array>::try_from(value).map_err(|_| "Failed to convert to array")?;
            let global_obj = v8::Global::new(scope, arr);
            let obj_id = common_state.proxy_client.array_registry.add(global_obj)
                .ok_or("Failed to register array: too many array references")?;
            return Ok(Self::Array(obj_id));
        } else if value.is_array_buffer() {
            let ab = v8::Local::<v8::ArrayBuffer>::try_from(value).map_err(|_| "Failed to convert to ArrayBuffer")?;
            let ab = v8::Global::new(scope, ab);
            let ab_id = common_state.proxy_client.array_buffer_registry.add(ab)
                .ok_or("Failed to register ArrayBuffer: too many ArrayBuffer references")?;
            return Ok(Self::ArrayBuffer(ab_id));
        } else if value.is_function() {
            let func = v8::Local::<v8::Function>::try_from(value).map_err(|_| "Failed to convert to function")?;
            let global_func = v8::Global::new(scope, func);
            let func_id = common_state.proxy_client.func_registry.add(global_func)
                .ok_or("Failed to register function: too many function references")?;
            return Ok(Self::Function(func_id));
        } else if value.is_promise() {
            let promise = v8::Local::<v8::Promise>::try_from(value).map_err(|_| "Failed to convert to promise")?;
            let global_promise = v8::Global::new(scope, promise);
            let promise_id = common_state.proxy_client.promise_registry.add(global_promise)
                .ok_or("Failed to register promise: too many promise references")?;
            return Ok(Self::Promise(promise_id));
        } else if value.is_object() {
            let obj = value.to_object(scope).ok_or("Failed to convert to object")?;

            // Handled source-proxied objects
            if let Some(v) = Self::proxy_from_v8_get_proxied(scope, &common_state, obj)? {
                return Ok(v);
            }

            let global_obj = v8::Global::new(scope, obj);
            let obj_id = common_state.proxy_client.obj_registry.add(global_obj)
                .ok_or("Failed to register object: too many object references")?;
            return Ok(Self::Object(obj_id));
        } else {
            return Err("Unsupported V8 value type".into());
        }
    }
}

impl V8IsolateManagerInner {
    /// Proxy a ProxiedLuaValue to a V8 value
    pub(crate) fn proxy_to_v8_impl(&mut self, value: ProxiedLuaValue) -> Result<v8::Global<v8::Value>, Error> {
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

    fn proxy_objreg_from_lua<'s>(
        scope: &mut v8::HandleScope<'s>,
        typ: ObjectRegistryType,
        id: ObjectRegistryID<LuaBridgeObject>,
        len: Option<usize>,
        common_state: &CommonState
    ) -> Result<v8::Local<'s, v8::Value>, Error> {
        let oid_key = v8::Local::new(scope, &common_state.bridge_vals.id_field);
        let otype_key = v8::Local::new(scope, &common_state.bridge_vals.type_field);

        let obj_template = common_state.obj_template.clone();
        
        let local_template = v8::Local::new(scope, (*obj_template).clone());
        
        let obj = local_template.new_instance(scope).ok_or("Failed to create V8 proxy object")?;

        let id_val = v8::BigInt::new_from_i64(scope, id.objid());
        obj.set(scope, oid_key.into(), id_val.into());
        let type_val = v8::Integer::new(scope, obj_registry_type_to_i32(typ));
        obj.set(scope, otype_key.into(), type_val.into());

        if let Some(len) = len {
            let len_key = v8::Local::new(scope, &common_state.bridge_vals.length_field);
            let len_val = v8::Integer::new(scope, len as i32);
            obj.set(scope, len_key.into(), len_val.into());
        }
        
        let try_catch = &mut v8::TryCatch::new(scope);

        let clfd = v8::Local::new(try_catch, &common_state.bridge_vals.create_lua_object_from_data);
        let global = try_catch.get_current_context().global(try_catch);
        let result = match clfd.call(try_catch, global.into(), &[obj.into()]) {
            Some(r) => r,
            None => {
                if try_catch.has_caught() {
                    let exception = try_catch.exception().unwrap();
                    let exception_string = exception.to_rust_string_lossy(try_catch);
                    return Err(format!("Failed to run createLuaObjectFromData: {}", exception_string).into());
                }
                return Err("Failed to run createLuaObjectFromData".into())
            },
        };
        Ok(result)
    }

    // Internal implementation to convert a ProxiedLuaValue to a V8 value
    pub(crate) fn proxy_to_v8<'s>(
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
            ProxiedLuaValue::String((string_id, len)) => {
                Self::proxy_objreg_from_lua(scope, ObjectRegistryType::String, string_id, Some(len), common_state)?
            }
            ProxiedLuaValue::Table(table_id) => {
                Self::proxy_objreg_from_lua(scope, ObjectRegistryType::Table, table_id, None, common_state)?
            }
            ProxiedLuaValue::Function(func_id) => {
                Self::proxy_objreg_from_lua(scope, ObjectRegistryType::Function, func_id, None, common_state)?
            }
            ProxiedLuaValue::Thread(thread_id) => {
                Self::proxy_objreg_from_lua(scope, ObjectRegistryType::Thread, thread_id, None, common_state)?
            }
            ProxiedLuaValue::UserData(ud_id) => {
                Self::proxy_objreg_from_lua(scope, ObjectRegistryType::UserData, ud_id, None, common_state)?
            }
            ProxiedLuaValue::Buffer(buf_id) => {
                Self::proxy_objreg_from_lua(scope, ObjectRegistryType::Buffer, buf_id, None, common_state)?
            }
        };

        Ok(v8_value)
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum V8ObjectRegistryType {
    ArrayBuffer,
    String,
    Object,
    Array,
    Function,
    Promise,
}

pub fn v8_obj_registry_type_to_i32(typ: V8ObjectRegistryType) -> i32 {
    match typ {
        V8ObjectRegistryType::ArrayBuffer => 0,
        V8ObjectRegistryType::String => 1,
        V8ObjectRegistryType::Object => 2,
        V8ObjectRegistryType::Array => 3,
        V8ObjectRegistryType::Function => 4,
        V8ObjectRegistryType::Promise => 5,
    }
}

pub fn i32_to_v8_obj_registry_type(i: i32) -> Option<V8ObjectRegistryType> {
    match i {
        0 => Some(V8ObjectRegistryType::ArrayBuffer),
        1 => Some(V8ObjectRegistryType::String),
        2 => Some(V8ObjectRegistryType::Object),
        3 => Some(V8ObjectRegistryType::Array),
        4 => Some(V8ObjectRegistryType::Function),
        5 => Some(V8ObjectRegistryType::Promise),
        _ => None,
    }
}

#[derive(Clone)]
/// The client side state for proxying Lua values
/// 
/// This struct is not thread safe and must be kept on the Lua side
pub struct ProxyV8Client {
    pub array_buffer_registry: ObjectRegistry<v8::Global<v8::ArrayBuffer>, V8BridgeObject>,
    pub string_registry: ObjectRegistry<v8::Global<v8::String>, V8BridgeObject>,
    pub array_registry: ObjectRegistry<v8::Global<v8::Array>, V8BridgeObject>,
    pub obj_registry: ObjectRegistry<v8::Global<v8::Object>, V8BridgeObject>,
    pub func_registry: ObjectRegistry<v8::Global<v8::Function>, V8BridgeObject>,
    pub promise_registry: ObjectRegistry<v8::Global<v8::Promise>, V8BridgeObject>,
}

#[derive(Serialize, Deserialize)]
pub enum V8IsolateManagerMessage {
    CodeExec {
        code: String,
        args: Vec<ProxiedLuaValue>,
        resp: OneshotSender<Result<ProxiedV8Value, String>>,
    },
    GetObjectProperty {
        obj_id: ObjectRegistryID<V8BridgeObject>,
        key: ProxiedLuaValue,
        resp: OneshotSender<Result<ProxiedV8Value, String>>,
    },
    DropObject {
        obj_type: V8ObjectRegistryType,
        obj_id: ObjectRegistryID<V8BridgeObject>,
    },
    Shutdown,
}

#[derive(Clone)]
pub struct V8IsolateManagerClient {}

impl V8IsolateManagerClient {
    fn compile_code(
        inner: &mut V8IsolateManagerInner,
        code: String,
    ) -> Result<v8::Global<v8::Function>, Error> {
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
                })?;

            // Convert and proxy
            let local_func = match script.run(try_catch) {
                Some(result) => {
                    if result.is_function() {
                        v8::Local::<v8::Function>::try_from(result)
                        .map_err(|e| format!("Failed to convert script result to function: {}", e))?
                    } else {
                        return Err("Script did not return a function".to_string().into());
                    }
                },
                None => {
                    if try_catch.has_caught() {
                        let exception = try_catch.exception().unwrap();
                        let exception_string = exception.to_rust_string_lossy(try_catch);
                        return Err(format!("Failed to run script: {}", exception_string).into());
                    } else {
                        return Err("Failed to run script".to_string().into())
                    }
                }
            };

            v8::Global::new(try_catch, local_func)
        };

        Ok(func)
    }
}

#[derive(Serialize, Deserialize)]
pub struct V8BootstrapData {
    pub heap_limit: usize,
    pub messenger_tx: OneshotSender<MultiSender<V8IsolateManagerMessage>>,
    pub lua_bridge_tx: MultiSender<LuaBridgeMessage<V8IsolateManagerServer>>
}

impl ConcurrentlyExecute for V8IsolateManagerClient {
    type BootstrapData = V8BootstrapData;
    async fn run(
        data: Self::BootstrapData,
        client_ctx: concurrentlyexec::ClientContext
    ) {
        let (tx, mut rx) = client_ctx.multi();
        data.messenger_tx.client(&client_ctx).send(tx).unwrap();

        let mut inner = V8IsolateManagerInner::new(
            LuaBridgeServiceClient::new(client_ctx.clone(), data.lua_bridge_tx),
            data.heap_limit,
        );

        let mut code_exec_queue = FuturesUnordered::new();

        loop {
            tokio::select! {
                Ok(msg) = rx.recv() => {
                    match msg {
                        V8IsolateManagerMessage::CodeExec { code, args, resp } => {
                            let func = match Self::compile_code(&mut inner, code) {
                                Ok(f) => f,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(e.to_string()));
                                    continue;
                                }
                            };
                            // Call the function with args
                            // using denos' call_with_args
                            let args = args.into_iter().map(|v| inner.proxy_to_v8_impl(v)).collect::<Result<Vec<_>, _>>();
                            let args = match args {
                                Ok(a) => a,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(e.to_string()));
                                    continue;
                                }
                            };
                            let fut = inner.deno.call_with_args(&func, &args);
                            code_exec_queue.push(async {
                                let result = fut.await;
                                (result, resp)
                            });
                        },
                        V8IsolateManagerMessage::GetObjectProperty { obj_id: _, key: _, resp: _ } => {
                        },
                        V8IsolateManagerMessage::Shutdown => {
                            break;
                        }
                        V8IsolateManagerMessage::DropObject { obj_type, obj_id } => {
                            match obj_type {
                                V8ObjectRegistryType::ArrayBuffer => {
                                    inner.common_state.proxy_client.array_buffer_registry.remove(obj_id);
                                }
                                V8ObjectRegistryType::String => {
                                    inner.common_state.proxy_client.string_registry.remove(obj_id);
                                }
                                V8ObjectRegistryType::Array => {
                                    inner.common_state.proxy_client.array_registry.remove(obj_id);
                                }
                                V8ObjectRegistryType::Object => {
                                    inner.common_state.proxy_client.obj_registry.remove(obj_id);
                                }
                                V8ObjectRegistryType::Function => {
                                    inner.common_state.proxy_client.func_registry.remove(obj_id);
                                }
                                V8ObjectRegistryType::Promise => {
                                    inner.common_state.proxy_client.promise_registry.remove(obj_id);
                                }
                            }
                        }
                    }
                }
                _ = inner.cancellation_token.cancelled() => {
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
                                &inner.common_state, 
                                0
                            ) {
                                Ok(pv) => Ok(pv),
                                Err(e) => Err(e),
                            }
                        }
                        Err(e) => Err(format!("JavaScript execution error: {}", e).into()),
                    };
                    let _ = resp.client(&client_ctx).send(result.map_err(|e| e.to_string()));
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct V8IsolateManagerServer {
    pub executor: Arc<ConcurrentExecutor<V8IsolateManagerClient>>,
    pub messenger: Arc<MultiSender<V8IsolateManagerMessage>>,
}

impl Drop for V8IsolateManagerServer {
    fn drop(&mut self) {
        let _ = self.messenger.server(self.executor.server_context()).send(V8IsolateManagerMessage::Shutdown);
        let _ = self.executor.shutdown();
    }
}

impl V8IsolateManagerServer {
    pub async fn new(
        cs_state: ConcurrentExecutorState<V8IsolateManagerClient>, 
        heap_limit: usize, 
        process_opts: ProcessOpts,
        plc: ProxyLuaClient,
    ) -> Result<Self, crate::base::Error> {
        let (executor, (lua_bridge_rx, ms_rx)) = ConcurrentExecutor::new(
            cs_state,
            process_opts,
            move |cei| {
                let (tx, rx) = cei.create_multi();
                let (msg_tx, msg_rx) = cei.create_oneshot();
                (V8BootstrapData {
                    heap_limit,
                    messenger_tx: msg_tx,
                    lua_bridge_tx: tx,
                }, (rx, msg_rx))
            }
        ).await.map_err(|e| format!("Failed to create V8 isolate manager executor: {}", e))?;
        let messenger = ms_rx.recv().await.map_err(|e| format!("Failed to receive messenger: {}", e))?;

        let self_ret = Self { executor: Arc::new(executor), messenger: Arc::new(messenger) };
        let self_ref = self_ret.clone();
        tokio::task::spawn_local(async move {
            let lua_bridge_service = LuaBridgeService::new(
                self_ref,
                lua_bridge_rx,
            );
            lua_bridge_service.run(plc).await;
        });

        Ok(self_ret)
    }

    pub async fn exec_code(&self, code: String, args: Vec<ProxiedLuaValue>) -> Result<ProxiedV8Value, String> {
        let (resp_tx, resp_rx) = self.executor.create_oneshot();
        let msg = V8IsolateManagerMessage::CodeExec {
            code,
            args,
            resp: resp_tx,
        };
        self.messenger.server(self.executor.server_context()).send(msg).map_err(|e| format!("Failed to send code exec message: {}", e))?;
        resp_rx.recv().await.map_err(|e| format!("Failed to receive code exec response: {}", e))?
    }

    pub fn request_drop_object(&self, obj_type: V8ObjectRegistryType, obj_id: ObjectRegistryID<V8BridgeObject>) {
        let msg = V8IsolateManagerMessage::DropObject {
            obj_type,
            obj_id,
        };
        let _ = self.messenger.server(self.executor.server_context()).send(msg);
    }

    pub async fn shutdown(&self) {
        let _ = self.messenger.server(self.executor.server_context()).send(V8IsolateManagerMessage::Shutdown);
        let _ = self.executor.shutdown();
    }
}


impl ProxyBridge for V8IsolateManagerServer {
    type ValueType = ProxiedV8Value;
    type ConcurrentlyExecuteClient = V8IsolateManagerClient;

    fn get_executor(&self) -> Arc<ConcurrentExecutor<Self::ConcurrentlyExecuteClient>> {
        self.executor.clone()
    }

    fn to_source_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, plc: &ProxyLuaClient, depth: usize) -> Result<mluau::Value, Error> {
        Ok(value.proxy_to_src_lua(lua, plc, self, depth).map_err(|e| e.to_string())?)
    }

    async fn eval_from_source(&self, code: String, args: Vec<ProxiedLuaValue>) -> Result<Self::ValueType, crate::base::Error> {
        self.exec_code(code, args).await
            .map_err(|e| e.into())
    }
}