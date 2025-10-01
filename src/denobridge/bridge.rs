use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use concurrentlyexec::{ConcurrentExecutor, ConcurrentExecutorState, ConcurrentlyExecute, MultiSender, OneshotSender, ProcessOpts};
use deno_core::v8::GetPropertyNamesArgs;
//use deno_core::error::{CoreError, CoreErrorKind};
use deno_core::{PollEventLoopOptions, v8};
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use mlua_scheduler::LuaSchedulerAsyncUserData;
use serde::{Deserialize, Serialize};
//use deno_core::{op2, OpState};
//use deno_error::JsErrorBox;
//use mluau::serde::de;

use crate::denobridge::modloader::FusionModuleLoader;
use crate::luau::bridge::{
    LuaBridgeMessage, LuaBridgeObject, LuaBridgeService, LuaBridgeServiceClient, ObjectRegistryType, ProxyLuaClient, StringRef, i32_to_obj_registry_type, obj_registry_type_to_i32
};

use crate::base::{ObjectRegistry, ObjectRegistryID, ProxyBridge};
use super::{CommonState, V8IsolateManagerInner};
use super::Error;

use crate::MAX_PROXY_DEPTH;

/// Max size for a owned v8 string
pub const MAX_OWNED_V8_STRING_SIZE: usize = 1024 * 16; // 16KB
/// Minimum stack size for V8 isolates
pub const V8_MIN_STACK_SIZE: usize = 1024 * 1024 * 15; // 15MB minimum memory

pub(crate) struct BridgeVals {
    type_field: v8::Global<v8::String>,
    id_field: v8::Global<v8::String>,
    length_field: v8::Global<v8::String>,
    create_lua_object_from_data: v8::Global<v8::Function>,
    string_ref_field: v8::Global<v8::Symbol>,
}

impl BridgeVals {
    pub(crate) fn new<'s>(scope: &mut v8::HandleScope<'s>) -> Self {
        let id_field = v8::String::new(scope, "luaid").unwrap();
        let type_field = v8::String::new(scope, "luatype").unwrap();
        let length_field = v8::String::new(scope, "length").unwrap();

        // The createLuaObjectFromData function is stored in globalThis.lua.createLuaObjectFromData
        let (string_ref_field, create_lua_object_from_data) = {
            let global = scope.get_current_context().global(scope);
            let lua_str = v8::String::new(scope, "lua").unwrap();
            let lua_obj = global.get(scope, lua_str.into()).unwrap();
            //println!("lua_obj: {:?}", lua_obj.to_rust_string_lossy(scope));
            assert!(lua_obj.is_object());
            let lua_obj = lua_obj.to_object(scope).unwrap();
            let clofd = v8::String::new(scope, "createLuaObjectFromData").unwrap();
            let create_lua_object_from_data = lua_obj.get(scope, clofd.into()).unwrap();
            assert!(create_lua_object_from_data.is_function());
            let create_lua_object_from_data = v8::Local::<v8::Function>::try_from(create_lua_object_from_data).unwrap();
            let srfk = v8::String::new(scope, "__stringref").unwrap();
            let string_ref_field = lua_obj.get(scope, srfk.into()).unwrap();
            assert!(string_ref_field.is_symbol());
            let string_ref_field = v8::Local::<v8::Symbol>::try_from(string_ref_field).unwrap();
            (string_ref_field, create_lua_object_from_data)
        };

        Self {
            id_field: v8::Global::new(scope, id_field),
            type_field: v8::Global::new(scope, type_field),
            length_field: v8::Global::new(scope, length_field),
            string_ref_field: v8::Global::new(scope, string_ref_field),
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
    pub plc: ProxyLuaClient,
    pub bridge: V8IsolateManagerServer,
}

impl V8ObjectInner {
    fn new(id: ObjectRegistryID<V8BridgeObject>, typ: V8ObjectRegistryType, plc: ProxyLuaClient, bridge: V8IsolateManagerServer) -> Self {
        Self {
            id,
            typ,
            plc,
            bridge,
        }
    }
}

#[derive(Clone)]
pub struct V8Object {
    pub inner: Rc<RefCell<Option<V8ObjectInner>>>,
}

impl V8Object {
    fn new(id: ObjectRegistryID<V8BridgeObject>, typ: V8ObjectRegistryType, plc: ProxyLuaClient, bridge: V8IsolateManagerServer) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Some(V8ObjectInner::new(id, typ, plc, bridge)))),
        }
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

        methods.add_scheduler_async_method("requestdispose", async move |_, this, ()| {
            if let Some(v) = this.inner.borrow_mut().take() {
                let bridge = v.bridge;
                bridge.send(DropObjectMessage {
                    obj_type: v.typ,
                    obj_id: v.id,
                }).await.map_err(|e| mluau::Error::external(format!("Failed to send DropObjectMessage: {}", e)))?;
            }
            Ok(())
        }); 
    }
}


/*impl mluau::UserData for V8String {
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mluau::MetaMethod::Len, |_, this, ()| {
            Ok(this.len)
        });

        methods.add_method("object", |_, this, ()| {
            Ok(this.obj.clone())
        });
    }
}*/

macro_rules! impl_v8_obj {
    ($name:ident, $typ:expr, $ext_methods:expr) => {
        pub struct $name {
            pub obj: V8Object,
        }

        impl $name {
            pub fn new(id: ObjectRegistryID<V8BridgeObject>, plc: ProxyLuaClient, bridge: V8IsolateManagerServer) -> Self {
                Self {
                    obj: V8Object::new(id, $typ, plc, bridge),
                }
            }

            fn method_adder<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
                $ext_methods(methods);
            }
        }

        impl mluau::UserData for $name {
            fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
                methods.add_method("object", |_, this, ()| {
                    Ok(this.obj.clone())
                });

                Self::method_adder(methods);
            }
        }
    }
}

macro_rules! impl_v8_obj_with_len {
    ($name:ident, $typ:expr, $ext_methods:expr) => {
        pub struct $name {
            pub obj: V8Object,
            pub len: usize,
        }

        impl $name {
            pub fn new(id: ObjectRegistryID<V8BridgeObject>, plc: ProxyLuaClient, bridge: V8IsolateManagerServer, len: usize) -> Self {
                Self {
                    obj: V8Object::new(id, $typ, plc, bridge),
                    len,
                }
            }

            fn method_adder<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
                $ext_methods(methods);
            }
        }

        impl mluau::UserData for $name {
            fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
                methods.add_meta_method(mluau::MetaMethod::Len, |_, this, ()| {
                    Ok(this.len)
                });

                methods.add_method("object", |_, this, ()| {
                    Ok(this.obj.clone())
                });

                Self::method_adder(methods);
            }
        }
    }
}

impl_v8_obj_with_len!(V8String, V8ObjectRegistryType::String, |_m| {});

impl_v8_obj!(V8ArrayBuffer, V8ObjectRegistryType::ArrayBuffer, |_m| {});

impl_v8_obj!(V8ObjectObj, V8ObjectRegistryType::Object, __objmethods);
fn __objmethods<M: mluau::UserDataMethods<V8ObjectObj>>(methods: &mut M) {
    methods.add_scheduler_async_method("getproperties", async move |lua, this, has_own: Option<bool>| {
        let _g = this.obj.inner.try_borrow()
        .map_err(|e| mluau::Error::external(format!("Failed to borrow V8Object inner: {}", e)))?;
        let inner = match _g.as_ref() {
            Some(inner) => inner,
            None => return Err(mluau::Error::external("V8Object has already been dropped")),
        };
        match inner.bridge.send(ObjectPropertiesMessage {
            obj_id: inner.id,
            own_props: has_own.unwrap_or(true)
        })
        .await
        .map_err(|e| mluau::Error::external(format!("Failed to send ObjectPropertiesMessage: {}", e)))? {
            Ok(v) => {
                let val = v.proxy_to_src_lua(&lua, &inner.plc, &inner.bridge)?;
                Ok(val)
            },
            Err(e) => return Err(mluau::Error::external(format!("Failed to get properties: {}", e))),
        }
    });

    methods.add_scheduler_async_method("getproperty", async move |lua, this, key: mluau::Value| {
        let _g = this.obj.inner.try_borrow()
        .map_err(|e| mluau::Error::external(format!("Failed to borrow V8Object inner: {}", e)))?;
        let inner = match _g.as_ref() {
            Some(inner) => inner,
            None => return Err(mluau::Error::external("V8Object has already been dropped")),
        };
        let key = ProxiedV8Primitive::luau_to_primitive(&key)
        .map_err(|e| mluau::Error::external(format!("Failed to proxy key to ProxiedV8Value: {}", e)))?
        .ok_or(mluau::Error::external("Key is not a primitive value"))?;
        match inner.bridge.send(ObjectGetPropertyMessage {
            obj_id: inner.id,
            key
        })
        .await
        .map_err(|e| mluau::Error::external(format!("Failed to send ObjectGetPropertyMessage: {}", e)))? {
            Ok(v) => {
                let val = v.proxy_to_src_lua(&lua, &inner.plc, &inner.bridge)?;
                Ok(val)
            },
            Err(e) => return Err(mluau::Error::external(format!("Failed to get property: {}", e))),
        }
    });
}

//impl_v8_obj_stub!(V8ObjectObj, V8ObjectRegistryType::Object);
impl_v8_obj!(V8Array, V8ObjectRegistryType::Array, |_m| {});
impl_v8_obj!(V8Function, V8ObjectRegistryType::Function, |_m| {});
impl_v8_obj!(V8Promise, V8ObjectRegistryType::Promise, |_m| {});

#[derive(Serialize, Deserialize)]
/// A primitive value that is primitive across v8 and luau
pub enum ProxiedV8Primitive {
    Nil,
    Undefined,
    Boolean(bool),
    Integer(i32),
    BigInt(i64),
    Number(f64),
    String(String),
}

impl ProxiedV8Primitive {
    /// Luau -> ProxiedV8Primitive
    pub(crate) fn luau_to_primitive(value: &mluau::Value) -> Result<Option<Self>, Error> {
        match value {
            mluau::Value::Nil => Ok(Some(ProxiedV8Primitive::Nil)),
            mluau::Value::Boolean(b) => Ok(Some(ProxiedV8Primitive::Boolean(*b))),
            mluau::Value::LightUserData(s) => Ok(Some(ProxiedV8Primitive::BigInt(s.0 as i64))),
            mluau::Value::Integer(i) => {
                if i >= &(i32::MIN as i64) && i <= &(i32::MAX as i64) {
                    Ok(Some(ProxiedV8Primitive::Integer(*i as i32)))
                } else {
                    Ok(Some(ProxiedV8Primitive::BigInt(*i)))
                }
            },

            mluau::Value::Number(n) => Ok(Some(ProxiedV8Primitive::Number(*n))),
            mluau::Value::String(s) => {
                if s.as_bytes().len() > MAX_OWNED_V8_STRING_SIZE {
                    return Err(format!("String too large to be a primitive (max {} bytes)", MAX_OWNED_V8_STRING_SIZE).into());
                }
                Ok(Some(ProxiedV8Primitive::String(s.to_str().map_err(|e| format!("Failed to convert Lua string to Rust string: {}", e))?.to_string())))
            },
            mluau::Value::Other(r) => {
                let s = format!("unknown({r:?})");
                Ok(Some(ProxiedV8Primitive::String(s)))
            },
            _ => Ok(None),
        }
    }

    /// ProxiedV8Primitive -> Luau
    pub(crate) fn to_luau(&self, lua: &mluau::Lua) -> Result<mluau::Value, Error> {
        match self {
            ProxiedV8Primitive::Nil => Ok(mluau::Value::Nil),
            ProxiedV8Primitive::Boolean(b) => Ok(mluau::Value::Boolean(*b)),
            ProxiedV8Primitive::Integer(i) => Ok(mluau::Value::Integer(*i as i64)),
            ProxiedV8Primitive::BigInt(i) => Ok(mluau::Value::Integer(*i)),
            ProxiedV8Primitive::Number(n) => Ok(mluau::Value::Number(*n)),
            ProxiedV8Primitive::Undefined => Ok(mluau::Value::Nil), // Luau does not have undefined, so we map it to nil
            ProxiedV8Primitive::String(s) => {
                let s = lua.create_string(s)
                .map_err(|e| format!("Failed to create Lua string: {}", e))?;
                Ok(mluau::Value::String(s))
            }
        }
    }

    /// ProxiedV8Primitive -> V8
    pub(crate) fn to_v8<'s>(&self, scope: &mut v8::HandleScope<'s>) -> Result<v8::Local<'s, v8::Value>, Error> {
        match self {
            ProxiedV8Primitive::Nil => Ok(v8::null(scope).into()),
            ProxiedV8Primitive::Undefined => Ok(v8::undefined(scope).into()),
            ProxiedV8Primitive::Boolean(b) => Ok(v8::Boolean::new(scope, *b).into()),
            ProxiedV8Primitive::Integer(i) => Ok(v8::Integer::new(scope, *i).into()),
            ProxiedV8Primitive::BigInt(i) => Ok(v8::BigInt::new_from_i64(scope, *i).into()),
            ProxiedV8Primitive::Number(n) => Ok(v8::Number::new(scope, *n).into()),
            ProxiedV8Primitive::String(s) => {
                let mut try_catch = v8::TryCatch::new(scope);
                let s = v8::String::new(try_catch.as_mut(), s);
                match s {
                    Some(s) => Ok(s.into()),
                    None => {
                        if try_catch.has_caught() {
                            let exception = try_catch.exception().unwrap();
                            let exception_str = exception.to_rust_string_lossy(try_catch.as_mut());
                            return Err(format!("Failed to create V8 string from ProxiedV8Primitive: {}", exception_str).into());
                        } 
                        return Err("Failed to create V8 string from ProxiedV8Primitive".into());
                    },
                }
            }
        }
    }

    /// V8 -> ProxiedV8Primitive
    pub(crate) fn v8_to_primitive<'s>(
        scope: &mut v8::HandleScope<'s>,
        value: v8::Local<'s, v8::Value>,
    ) -> Result<Option<Self>, Error> {
        if value.is_null() {
            return Ok(Some(ProxiedV8Primitive::Nil));
        }
        if value.is_undefined() {
            return Ok(Some(ProxiedV8Primitive::Undefined));
        }
        if value.is_boolean() {
            let b = value.to_boolean(scope).is_true();
            return Ok(Some(ProxiedV8Primitive::Boolean(b)));
        }
        if value.is_int32() {
            let i = value.to_int32(scope).ok_or("Failed to convert V8 value to int32")?.value();
            return Ok(Some(ProxiedV8Primitive::Integer(i)));
        }
        if value.is_big_int() {
            let bi = value.to_big_int(scope).ok_or("Failed to convert V8 value to BigInt")?;
            let (i, lossless) = bi.i64_value();
            if !lossless {
                // BigInt too large to fit in i64, so return string representation instead
                // as strings are also primitive values
                let s = value.to_string(scope).ok_or("Failed to convert to string")?;
                let string = s.to_rust_string_lossy(scope);
                return Ok(Some(ProxiedV8Primitive::String(string)));

            }
            return Ok(Some(ProxiedV8Primitive::BigInt(i)));
        }
        if value.is_number() {
            let n = value.to_number(scope).unwrap().value();
            return Ok(Some(ProxiedV8Primitive::Number(n)));
        }
        if value.is_string() {
            let s = value.to_string(scope).ok_or("Failed to convert to string")?;
            let s_len = s.length();
            if s_len > MAX_OWNED_V8_STRING_SIZE {
                return Err(format!("String too large to be a primitive (max {} bytes)", MAX_OWNED_V8_STRING_SIZE).into());
            }
            let string = s.to_rust_string_lossy(scope);
            return Ok(Some(Self::String(string)));
        }

        // Not a primitive
        Ok(None)
    }
}

/// A V8 value that can now be easily proxied to Luau
#[derive(Serialize, Deserialize)]
pub enum ProxiedV8Value {
    Primitive(ProxiedV8Primitive),
    Vector((f32, f32, f32)), 
    ArrayBuffer(ObjectRegistryID<V8BridgeObject>), // Buffer ID in the buffer registry
    StringRef((ObjectRegistryID<V8BridgeObject>, usize)), // String ID in the string registry, length
    Object(ObjectRegistryID<V8BridgeObject>), // Object ID in the map registry
    Array(ObjectRegistryID<V8BridgeObject>), // Array ID in the array registry
    Function(ObjectRegistryID<V8BridgeObject>), // Function ID in the function registry
    Promise(ObjectRegistryID<V8BridgeObject>), // Promise ID in the function registry

    // Source-owned stuff
    SourceOwnedObject((ObjectRegistryType, ObjectRegistryID<LuaBridgeObject>, Option<usize>)), // (Type, ID, <optional length>) of the source-owned object
}

impl ProxiedV8Value {
    /// Proxies a Luau value to a ProxiedV8Value
    pub(crate) fn proxy_from_src_lua(plc: &ProxyLuaClient, value: mluau::Value) -> Result<Self, Error> {
        if let Some(prim) = ProxiedV8Primitive::luau_to_primitive(&value)? {
            return Ok(ProxiedV8Value::Primitive(prim));
        }
        
        match value {
            mluau::Value::Nil => unreachable!(
                "Nil should have been handled as a primitive"
            ), // is a primitive
            mluau::Value::LightUserData(_s) => unreachable!(
                "LightUserData should have been handled as a primitive"
            ), // is a primitive
            mluau::Value::Boolean(_b) => unreachable!(
                "Boolean should have been handled as a primitive"
            ), // is a primitive
            mluau::Value::Integer(_i) => unreachable!(
                "Integer should have been handled as a primitive"
            ), // is a primitive
            mluau::Value::Number(_n) => unreachable!(
                "Number should have been handled as a primitive"
            ), // is a primitive
            mluau::Value::String(_s) => unreachable!(
                "String should have been handled as a primitive"
            ), // is a primitive
            mluau::Value::Other(r) => unreachable!(
                "Other({r:?}) should have been handled as a primitive"
            ), // is a primitive 
            mluau::Value::Table(t) => {
                let table_id = plc.table_registry.add(t)
                    .ok_or_else(|| "Table registry is full".to_string())?;

                Ok(ProxiedV8Value::SourceOwnedObject((ObjectRegistryType::Table, table_id, None)))
            }
            mluau::Value::Function(f) => {
                let func_id = plc.func_registry.add(f)
                    .ok_or_else(|| "Function registry is full".to_string())?;
                Ok(ProxiedV8Value::SourceOwnedObject((ObjectRegistryType::Function, func_id, None)))
            }
            mluau::Value::UserData(ud) => {
                if let Ok(ref_wrapper) = ud.borrow::<StringRef<V8IsolateManagerServer>>() {
                    let s_len = ref_wrapper.value.as_bytes().len();
                    let string_id = plc.string_registry.add(ref_wrapper.value.clone())
                        .ok_or_else(|| "String registry is full".to_string())?;
                    return Ok(ProxiedV8Value::SourceOwnedObject((ObjectRegistryType::String, string_id, Some(s_len))))
                }

                let userdata_id = plc.userdata_registry.add(ud)
                    .ok_or_else(|| "UserData registry is full".to_string())?;
                Ok(ProxiedV8Value::SourceOwnedObject((ObjectRegistryType::UserData, userdata_id, None)))
            }
            mluau::Value::Vector(v) => Ok(ProxiedV8Value::Vector((v.x(), v.y(), v.z()))),
            mluau::Value::Buffer(b) => {
                let buffer_len = b.len();
                let buffer_id = plc.buffer_registry.add(b)
                    .ok_or_else(|| "Buffer registry is full".to_string())?;
                Ok(ProxiedV8Value::SourceOwnedObject((ObjectRegistryType::Buffer, buffer_id, Some(buffer_len))))
            }
            mluau::Value::Thread(th) => {
                let thread_id = plc.thread_registry.add(th)
                    .ok_or_else(|| "Thread registry is full".to_string())?;
                Ok(ProxiedV8Value::SourceOwnedObject((ObjectRegistryType::Thread, thread_id, None)))
            }
            mluau::Value::Error(e) => return Err(format!("Cannot proxy Lua error value: {}", e).into()),
        }
    }

    /// Proxy a ProxiedV8Value to a Luau value
    pub(crate) fn proxy_to_src_lua(self, lua: &mluau::Lua, plc: &ProxyLuaClient, bridge: &V8IsolateManagerServer) -> Result<mluau::Value, mluau::Error> {
        match self {
            ProxiedV8Value::Primitive(p) => Ok(p.to_luau(lua).map_err(|e| mluau::Error::external(format!("Failed to convert ProxiedV8Primitive to Luau: {}", e)))?),
            ProxiedV8Value::Vector((x,y,z)) => {
                let vec = mluau::Vector::new(x, y, z);
                Ok(mluau::Value::Vector(vec))
            }

            // v8 values (v8 values being proxied from v8 to lua)
            ProxiedV8Value::ArrayBuffer(buf_id) => {
                let ud = V8ArrayBuffer::new(buf_id, plc.clone(), bridge.clone());
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }
            ProxiedV8Value::StringRef((string_id, len)) => {
                let ud = V8String::new(string_id, plc.clone(), bridge.clone(), len);
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }
            ProxiedV8Value::Object(obj_id) => {
                let ud = V8ObjectObj::new(obj_id, plc.clone(), bridge.clone());
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }
            ProxiedV8Value::Array(arr_id) => {
                let ud = V8Array::new(arr_id, plc.clone(), bridge.clone());
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }
            ProxiedV8Value::Function(func_id) => {
                let ud = V8Function::new(func_id, plc.clone(), bridge.clone());
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }
            ProxiedV8Value::Promise(promise_id) => {
                let ud = V8Promise::new(promise_id, plc.clone(), bridge.clone());
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }

            // Source-owned values (lua values being proxied back from v8 to lua)
            ProxiedV8Value::SourceOwnedObject((typ, id, _len)) => {
                match typ {
                    ObjectRegistryType::String => {
                        let s = plc.string_registry.get(id)
                            .ok_or_else(|| mluau::Error::external(format!("String ID {} not found in registry", id)))?;
                        return Ok(mluau::Value::String(s));
                    }
                    ObjectRegistryType::Function => {
                        let func = plc.func_registry.get(id)
                            .ok_or_else(|| mluau::Error::external(format!("Function ID {} not found in registry", id)))?;
                        return Ok(mluau::Value::Function(func));
                    }
                    ObjectRegistryType::Table => {
                        let table = plc.table_registry.get(id)
                            .ok_or_else(|| mluau::Error::external(format!("Table ID {} not found in registry", id)))?;
                        return Ok(mluau::Value::Table(table));
                    }
                    ObjectRegistryType::Thread => {
                        let thread = plc.thread_registry.get(id)
                            .ok_or_else(|| mluau::Error::external(format!("Thread ID {} not found in registry", id)))?;
                        return Ok(mluau::Value::Thread(thread));
                    }
                    ObjectRegistryType::Buffer => {
                        let buffer = plc.buffer_registry.get(id)
                            .ok_or_else(|| mluau::Error::external(format!("Buffer ID {} not found in registry", id)))?;
                        return Ok(mluau::Value::Buffer(buffer));
                    }
                    ObjectRegistryType::UserData => {
                        let userdata = plc.userdata_registry.get(id)
                            .ok_or_else(|| mluau::Error::external(format!("Userdata ID {} not found in registry", id)))?;
                        return Ok(mluau::Value::UserData(userdata));
                    }
                }
            }
        }
    }

    // Helper function to extract a SourceOwnedObject from a V8 object, if it is one
    fn proxy_source_owned_object_from_v8<'s>(
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
                    
                    // Look for luaid
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

                    // Look for optional length field
                    let len = {
                        let len_key = v8::Local::new(scope, &common_state.bridge_vals.length_field);
                        let len_val = obj.get(scope, len_key.into());
                        if let Some(len_val) = len_val {
                            if len_val.is_int32() {
                                Some(len_val.to_int32(scope).ok_or("Failed to convert length to int32")?.value() as usize)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };

                    return Ok(Some(Self::SourceOwnedObject((typ, ObjectRegistryID::from_i64(lua_id), len))))
                }
            }
        }

        Ok(None)
    }

    /// Given a v8 value, convert it to a ProxiedV8Value
    pub(crate) fn proxy_from_v8<'s>(
        scope: &mut v8::HandleScope<'s>, 
        value: v8::Local<'s, v8::Value>,
        common_state: &CommonState,
        depth: usize,
    ) -> Result<Self, Error> {
        if depth > MAX_PROXY_DEPTH {
            return Err("Maximum proxy depth exceeded".into());
        }

        if let Some(prim) = ProxiedV8Primitive::v8_to_primitive(scope, value)? {
            return Ok(Self::Primitive(prim));
        }

        if value.is_array() {
            let arr = v8::Local::<v8::Array>::try_from(value).map_err(|_| "Failed to convert to array")?;
            if arr.length() == 3 {
                let x = arr.get_index(scope, 0).ok_or("Failed to get array index 0")?;
                let y = arr.get_index(scope, 1).ok_or("Failed to get array index 1")?;
                let z = arr.get_index(scope, 2).ok_or("Failed to get array index 2")?;
                if x.is_number() && y.is_number() && z.is_number() {
                    let x = x.to_number(scope).ok_or("Failed to convert x to number")?.value() as f32;
                    let y = y.to_number(scope).ok_or("Failed to convert y to number")?.value() as f32;
                    let z = z.to_number(scope).ok_or("Failed to convert z to number")?.value() as f32;
                    return Ok(Self::Vector((x, y, z)));
                }
            }
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

            // Handled source-owned objects
            if let Some(v) = Self::proxy_source_owned_object_from_v8(scope, &common_state, obj)? {
                return Ok(v);
            }

            // Handle string ref cases
            {
                let string_ref_field = v8::Local::new(scope, &common_state.bridge_vals.string_ref_field);
                if let Some(s) = obj.get(scope, string_ref_field.into()) && !s.is_undefined() {
                    if s.is_string() {
                        let s = s.to_string(scope).ok_or("Failed to convert to string")?;
                        let s_len = s.length();
                        let global_str = v8::Global::new(scope, s);
                        let sid = common_state.proxy_client.string_registry.add(global_str)
                            .ok_or("Failed to register string: too many string references")?;
                        return Ok(Self::StringRef((sid, s_len)));
                    } 
                    return Err("string_ref field is not a string".into());
                }
            }

            let global_obj = v8::Global::new(scope, obj);
            let obj_id = common_state.proxy_client.obj_registry.add(global_obj)
                .ok_or("Failed to register object: too many object references")?;
            return Ok(Self::Object(obj_id));
        } else {
            return Err("Unsupported V8 value type".into());
        }
    }

    /// Proxy a ProxiedV8Value to a V8 value
    pub(crate) fn proxy_to_v8<'s>(
        self,
        scope: &mut v8::HandleScope<'s>, 
        common_state: &CommonState,
    ) -> Result<v8::Local<'s, v8::Value>, Error> {
        match self {
            Self::Primitive(p) => Ok(p.to_v8(scope)?),
            Self::Array(arr_id) => {
                let arr = common_state.proxy_client.array_registry.get(arr_id)
                    .ok_or("Array ID not found in registry")?;
                let arr = v8::Local::new(scope, arr.clone());
                Ok(arr.into())
            }
            Self::ArrayBuffer(buf_id) => {
                let ab = common_state.proxy_client.array_buffer_registry.get(buf_id)
                    .ok_or("ArrayBuffer ID not found in registry")?;
                let ab = v8::Local::new(scope, ab.clone());
                Ok(ab.into())
            }
            Self::StringRef((string_id, _len)) => {
                let s = common_state.proxy_client.string_registry.get(string_id)
                    .ok_or("String ID not found in registry")?;
                let s = v8::Local::new(scope, s.clone());
                Ok(s.into())
            }
            Self::Object(obj_id) => {
                let obj = common_state.proxy_client.obj_registry.get(obj_id)
                    .ok_or("Object ID not found in registry")?;
                let obj = v8::Local::new(scope, obj.clone());
                Ok(obj.into())
            }
            Self::Function(func_id) => {
                let func = common_state.proxy_client.func_registry.get(func_id)
                    .ok_or("Function ID not found in registry")?;
                let func = v8::Local::new(scope, func.clone());
                Ok(func.into())
            }
            Self::Promise(promise_id) => {
                let promise = common_state.proxy_client.promise_registry.get(promise_id)
                    .ok_or("Promise ID not found in registry")?;
                let promise = v8::Local::new(scope, promise.clone());
                Ok(promise.into())
            }
            Self::Vector((x,y, z)) => {
                let arr = v8::Array::new(scope, 3);
                let x = v8::Number::new(scope, x as f64);
                let y = v8::Number::new(scope, y as f64);
                let z = v8::Number::new(scope, z as f64);
                arr.set_index(scope, 0, x.into());
                arr.set_index(scope, 1, y.into());
                arr.set_index(scope, 2, z.into());
                Ok(arr.into())
            }
            Self::SourceOwnedObject((typ, id, len)) => {
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
            },
        }
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
/// Internal representation of a message that can be sent to the V8 isolate manager
enum V8IsolateManagerMessage {
    CodeExec {
        modname: String,
        resp: OneshotSender<Result<ProxiedV8Value, String>>,
    },
    ObjectProperties {
        obj_id: ObjectRegistryID<V8BridgeObject>,
        own_props: bool,
        resp: OneshotSender<Result<ProxiedV8Value, String>>,
    },
    ObjectGetProperty {
        obj_id: ObjectRegistryID<V8BridgeObject>,
        key: ProxiedV8Primitive,
        resp: OneshotSender<Result<ProxiedV8Value, String>>,
    },
    DropObject {
        obj_type: V8ObjectRegistryType,
        obj_id: ObjectRegistryID<V8BridgeObject>,
    },
    Shutdown,
}

/// A message that can be sent to the V8 isolate manager
trait V8IsolateSendableMessage: Send + 'static {
    type Response: Send + serde::Serialize + for<'de> serde::Deserialize<'de> + 'static;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage;
}

struct CodeExecMessage {
    modname: String,
}

impl V8IsolateSendableMessage for CodeExecMessage {
    type Response = Result<ProxiedV8Value, String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::CodeExec {
            modname: self.modname,
            resp,
        }
    }
}

struct ObjectPropertiesMessage {
    obj_id: ObjectRegistryID<V8BridgeObject>,
    own_props: bool,
}

impl V8IsolateSendableMessage for ObjectPropertiesMessage {
    type Response = Result<ProxiedV8Value, String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::ObjectProperties {
            obj_id: self.obj_id,
            own_props: self.own_props,
            resp,
        }
    }
}

struct ObjectGetPropertyMessage {
    obj_id: ObjectRegistryID<V8BridgeObject>,
    key: ProxiedV8Primitive,
}

impl V8IsolateSendableMessage for ObjectGetPropertyMessage {
    type Response = Result<ProxiedV8Value, String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::ObjectGetProperty {
            obj_id: self.obj_id,
            key: self.key,
            resp,
        }
    }
}

struct DropObjectMessage {
    obj_type: V8ObjectRegistryType,
    obj_id: ObjectRegistryID<V8BridgeObject>,
}

impl V8IsolateSendableMessage for DropObjectMessage {
    type Response = ();
    fn to_message(self, _resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::DropObject {
            obj_type: self.obj_type,
            obj_id: self.obj_id,
        }
    }
}

struct ShutdownMessage;

impl V8IsolateSendableMessage for ShutdownMessage {
    type Response = ();
    fn to_message(self, _resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::Shutdown
    }
}

#[derive(Clone)]
pub struct V8IsolateManagerClient {}

impl V8IsolateManagerClient {
    fn obj_op<'s, R>(
        inner: &'s mut V8IsolateManagerInner, 
        obj_id: ObjectRegistryID<V8BridgeObject>,
        func: impl FnOnce(&mut v8::HandleScope<'s>, &CommonState, v8::Local<'s, v8::Object>) -> Result<R, crate::base::Error>,
    ) -> Result<R, crate::base::Error> {
        let obj = match inner.common_state.proxy_client.obj_registry.get(obj_id) {
            Some(o) => o,
            None => {
                return Err(format!("Object ID {} not found", obj_id).into());
            }
        };
        let result = {
            let main_ctx = inner.deno.main_context();
            let isolate = inner.deno.v8_isolate();
            let mut scope = v8::HandleScope::new(isolate);
            let main_ctx = v8::Local::new(&mut scope, main_ctx);
            let scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
            let obj = v8::Local::new(scope, obj.clone());
            func(scope, &inner.common_state, obj)
        };
        result.map_err(|e| format!("V8 object operation failed: {}", e).into())
    }
}

#[derive(Serialize, Deserialize)]
pub struct V8BootstrapData {
    heap_limit: usize,
    messenger_tx: OneshotSender<MultiSender<V8IsolateManagerMessage>>,
    lua_bridge_tx: MultiSender<LuaBridgeMessage<V8IsolateManagerServer>>,
    vfs: HashMap<String, String>,
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
            FusionModuleLoader::new(data.vfs.into_iter().map(|(x, y)| (x, y.into())))
        );

        let mut evaluated_modules = HashMap::new();
        let mut module_evaluate_queue = FuturesUnordered::new();

        loop {
            tokio::select! {
                Ok(msg) = rx.recv() => {
                    match msg {
                        V8IsolateManagerMessage::CodeExec { modname, resp } => {
                            /*let nargs = {
                                let mut nargs = Vec::with_capacity(args.len());

                                let main_ctx = inner.deno.main_context();
                                let isolate = inner.deno.v8_isolate();
                                let mut scope = v8::HandleScope::new(isolate);
                                let main_ctx = v8::Global::new(&mut scope, main_ctx);
                                let main_ctx = v8::Local::new(&mut scope, main_ctx);
                                let context_scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
                                let mut err = None;
                                for arg in args {
                                    match arg.proxy_to_v8(context_scope, &inner.common_state) {
                                        Ok(arg) => {
                                            let global_arg = v8::Global::new(context_scope, arg);
                                            nargs.push(global_arg);
                                        },
                                        Err(e) => {
                                            err = Some(e);
                                            break;
                                        }
                                    };
                                }

                                if let Some(err) = err {
                                    let _ = resp.client(&client_ctx).send(Err(err.to_string()));
                                    continue;
                                }

                                nargs
                            };*/

                            if let Some(module_id) = evaluated_modules.get(&modname) {
                                // Module already evaluated, just return the namespace object
                                let namespace_obj = match inner.deno.get_module_namespace(*module_id) {
                                    Ok(obj) => obj,
                                    Err(e) => {
                                        let _ = resp.client(&client_ctx).send(Err(format!("Failed to get module namespace: {}", e).into()));
                                        continue;
                                    }
                                };
                                // Proxy the namespace object to a ProxiedV8Value
                                let proxied = {
                                    let main_ctx = inner.deno.main_context();
                                    let isolate = inner.deno.v8_isolate();
                                    let mut scope = v8::HandleScope::new(isolate);
                                    let main_ctx = v8::Local::new(&mut scope, main_ctx);
                                    let context_scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
                                    let namespace_obj = v8::Local::new(context_scope, namespace_obj);
                                    match ProxiedV8Value::proxy_from_v8(context_scope, namespace_obj.into(), &inner.common_state, 0) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            let _ = resp.client(&client_ctx).send(Err(format!("Failed to proxy module namespace: {}", e).into()));
                                            continue;
                                        }
                                    }
                                };

                                let _ = resp.client(&client_ctx).send(Ok(proxied));
                                continue;
                            }

                            let url = match deno_core::url::Url::parse(&format!("file:///{modname}")) {
                                Ok(u) => u,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to parse module name as URL: {}", e).into()));
                                    continue;
                                }
                            };

                            let id = match inner.deno.load_side_es_module(&url).await {
                                Ok(id) => id,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to load module: {}", e).into()));
                                    continue;
                                }
                            };
                            let fut = inner.deno.mod_evaluate(id);
                            module_evaluate_queue.push(async move {
                                let module_id = fut.await.map(|_| id);
                                (modname, module_id, resp)
                            });
                        },
                        V8IsolateManagerMessage::ObjectProperties { obj_id, own_props, resp } => {
                            match Self::obj_op(&mut inner, obj_id, |scope, common_state, obj| {
                                // TODO: Allow customizing GetPropertyNamesArgs 
                                let props = if own_props {
                                    obj.get_own_property_names(scope, GetPropertyNamesArgs::default())
                                    .ok_or("Failed to get object property names")?
                                } else {
                                    obj.get_property_names(scope, GetPropertyNamesArgs::default())
                                    .ok_or("Failed to get object property names")?
                                };
                                ProxiedV8Value::proxy_from_v8(scope, props.into(), common_state, 0)
                            }) {
                                Ok(v) => {
                                    let _ = resp.client(&client_ctx).send(Ok(v));
                                }
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(e.to_string()));
                                }
                            }
                        },
                        V8IsolateManagerMessage::ObjectGetProperty { obj_id, key, resp } => {
                            match Self::obj_op(&mut inner, obj_id, |scope, common_state, obj| {
                                let key = key.to_v8(scope)?;
                                let key = v8::Local::new(scope, key);
                                match obj.get(scope, key) {
                                    Some(v) => ProxiedV8Value::proxy_from_v8(scope, v, common_state, 0),
                                    None => Err("Failed to get object property".into()),
                                }
                            }) {
                                Ok(v) => {
                                    let _ = resp.client(&client_ctx).send(Ok(v));
                                }
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(e.to_string()));
                                }
                            }
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
                _ = inner.deno.run_event_loop(PollEventLoopOptions {
                    wait_for_inspector: false,
                    pump_v8_message_loop: true,
                }) => {
                    tokio::task::yield_now().await;
                },
                Some((modname, result, resp)) = module_evaluate_queue.next() => {
                    let Ok(result) = result else {
                        let _ = resp.client(&client_ctx).send(Err("Failed to evaluate module".to_string().into()));
                        continue;
                    };
                    evaluated_modules.insert(modname, result);
                    let namespace_obj = match inner.deno.get_module_namespace(result) {
                        Ok(obj) => obj,
                        Err(e) => {
                            let _ = resp.client(&client_ctx).send(Err(format!("Failed to get module namespace: {}", e).into()));
                            continue;
                        }
                    };
                    // Proxy the namespace object to a ProxiedV8Value
                    let proxied = {
                        let main_ctx = inner.deno.main_context();
                        let isolate = inner.deno.v8_isolate();
                        let mut scope = v8::HandleScope::new(isolate);
                        let main_ctx = v8::Local::new(&mut scope, main_ctx);
                        let context_scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
                        let namespace_obj = v8::Local::new(context_scope, namespace_obj);
                        {
                            let props = namespace_obj.get_own_property_names(context_scope, GetPropertyNamesArgs::default()).unwrap();
                            println!("Got namespace object: {:?}", props.to_rust_string_lossy(context_scope));
                        }
                        match ProxiedV8Value::proxy_from_v8(context_scope, namespace_obj.into(), &inner.common_state, 0) {
                            Ok(v) => v,
                            Err(e) => {
                                let _ = resp.client(&client_ctx).send(Err(format!("Failed to proxy module namespace: {}", e).into()));
                                continue;
                            }
                        }
                    };

                    let _ = resp.client(&client_ctx).send(Ok(proxied));
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct V8IsolateManagerServer {
    executor: Arc<ConcurrentExecutor<V8IsolateManagerClient>>,
    messenger: Arc<MultiSender<V8IsolateManagerMessage>>,
}

impl Drop for V8IsolateManagerServer {
    fn drop(&mut self) {
        let _ = self.messenger.server(self.executor.server_context()).send(V8IsolateManagerMessage::Shutdown);
        let _ = self.executor.shutdown();
    }
}

impl V8IsolateManagerServer {
    /// Create a new V8 isolate manager server
    async fn new(
        cs_state: ConcurrentExecutorState<V8IsolateManagerClient>, 
        heap_limit: usize, 
        process_opts: ProcessOpts,
        plc: ProxyLuaClient,
        vfs: HashMap<String, String>,
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
                    vfs
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

    /// Send a message to the V8 isolate process and wait for a response
    async fn send<T: V8IsolateSendableMessage>(&self, msg: T) -> Result<T::Response, crate::base::Error> {
        let (resp_tx, resp_rx) = self.executor.create_oneshot();
        self.messenger.server(self.executor.server_context()).send(msg.to_message(resp_tx))
            .map_err(|e| format!("Failed to send message to V8 isolate: {}", e))?;
        resp_rx.recv().await.map_err(|e| format!("Failed to receive response from V8 isolate: {}", e).into())
    }
}


impl ProxyBridge for V8IsolateManagerServer {
    type ValueType = ProxiedV8Value;
    type ConcurrentlyExecuteClient = V8IsolateManagerClient;

    fn name() -> &'static str {
        "v8"
    }

    async fn new(
        cs_state: ConcurrentExecutorState<Self::ConcurrentlyExecuteClient>, 
        heap_limit: usize, 
        process_opts: ProcessOpts,
        plc: ProxyLuaClient,
        vfs: HashMap<String, String>,
    ) -> Result<Self, crate::base::Error> {
        Self::new(cs_state, heap_limit, process_opts, plc, vfs).await        
    }

    fn get_executor(&self) -> Arc<ConcurrentExecutor<Self::ConcurrentlyExecuteClient>> {
        self.executor.clone()
    }

    fn to_source_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, plc: &ProxyLuaClient) -> Result<mluau::Value, Error> {
        Ok(value.proxy_to_src_lua(lua, plc, self).map_err(|e| e.to_string())?)
    }

    fn from_source_lua_value(&self, _lua: &mluau::Lua, plc: &ProxyLuaClient, value: mluau::Value) -> Result<Self::ValueType, crate::base::Error> {
        Ok(ProxiedV8Value::proxy_from_src_lua(plc, value).map_err(|e| e.to_string())?)
    }

    async fn eval_from_source(&self, modname: String) -> Result<Self::ValueType, crate::base::Error> {
        Ok(self.send(CodeExecMessage { modname }).await??)
    }

    async fn shutdown(&self) -> Result<(), crate::base::Error> {
        self.send(ShutdownMessage).await
    }
}

