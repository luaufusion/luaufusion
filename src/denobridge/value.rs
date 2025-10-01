use serde::{Deserialize, Serialize};
use crate::base::{Error, ObjectRegistryID};
use super::primitives::ProxiedV8Primitive;
use super::V8IsolateManagerServer;
use super::bridge::V8BridgeObject;
use super::luauobjs::{V8Array, V8ArrayBuffer, V8Function, V8ObjectObj, V8Promise, V8String};
use crate::luau::bridge::{
    ObjectRegistryType, LuaBridgeObject, ProxyLuaClient, StringRef,
    i32_to_obj_registry_type, obj_registry_type_to_i32
};
use super::inner::CommonState;
use deno_core::v8;
use crate::MAX_PROXY_DEPTH;

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
    pub(super) fn proxy_from_v8<'s>(
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
    pub(super) fn proxy_to_v8<'s>(
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
