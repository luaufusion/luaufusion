use serde::{Deserialize, Serialize};
use crate::base::Error;
use crate::denobridge::objreg::V8ObjectRegistryID;
use crate::luau::objreg::LuauObjectRegistryID;
use super::primitives::ProxiedV8Primitive;
use super::V8IsolateManagerServer;
use super::luauobjs::{V8Array, V8ArrayBuffer, V8Function, V8ObjectObj, V8Promise, V8String};
use crate::luau::bridge::{
    ObjectRegistryType, ProxyLuaClient, StringRef, i32_to_obj_registry_type, luau_value_to_obj_registry_type, obj_registry_type_to_i32
};
use super::inner::CommonState;
use deno_core::v8;
use crate::MAX_PROXY_DEPTH;

/// A V8 value that can now be easily proxied to Luau
#[derive(Serialize, Deserialize)]
pub enum ProxiedV8Value {
    Primitive(ProxiedV8Primitive),
    Vector((f32, f32, f32)), 
    ArrayBuffer(V8ObjectRegistryID), // Buffer ID in the buffer registry
    StringRef((V8ObjectRegistryID, usize)), // String ID in the string registry, length
    Object(V8ObjectRegistryID), // Object ID in the map registry
    Array(V8ObjectRegistryID), // Array ID in the array registry
    Function(V8ObjectRegistryID), // Function ID in the function registry
    Promise(V8ObjectRegistryID), // Promise ID in the function registry

    // Source-owned stuff
    SourceOwnedObject((ObjectRegistryType, LuauObjectRegistryID, Option<usize>)), // (Type, ID, <optional length>) of the source-owned object
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
            mluau::Value::Vector(v) => Ok(ProxiedV8Value::Vector((v.x(), v.y(), v.z()))),
            mluau::Value::UserData(ud) => {
                if let Ok(ref_wrapper) = ud.borrow::<StringRef<V8IsolateManagerServer>>() {
                    let s_len = ref_wrapper.value.as_bytes().len();
                    let string_id = plc.obj_registry.add(mluau::Value::String(ref_wrapper.value.clone()))
                        .map_err(|e| format!("Failed to add string to registry: {}", e))?;
                    return Ok(ProxiedV8Value::SourceOwnedObject((ObjectRegistryType::String, string_id, Some(s_len))))
                }

                let userdata_id = plc.obj_registry.add(mluau::Value::UserData(ud))
                    .map_err(|e| format!("Failed to add object to registry: {}", e))?;
                Ok(ProxiedV8Value::SourceOwnedObject((ObjectRegistryType::UserData, userdata_id, None)))
            }
            mluau::Value::Error(e) => return Err(format!("Cannot proxy Lua error value: {}", e).into()),
            value => {
                let obj_type = match luau_value_to_obj_registry_type(&value) {
                    Some(t) => t,
                    None => return Err(format!("Cannot proxy Luau value of type {:?}", value).into()),
                };

                let id = plc.obj_registry.add(value)
                    .map_err(|e| format!("Failed to add object to registry: {}", e))?; 

                Ok(ProxiedV8Value::SourceOwnedObject((obj_type, id, None)))
            }
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
            ProxiedV8Value::SourceOwnedObject((_typ, id, _len)) => {
                let value = plc.obj_registry.get(id)
                    .map_err(|e| mluau::Error::external(format!("Failed to get object from registry: {}", e)))?;
                Ok(value)
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

                    return Ok(Some(Self::SourceOwnedObject((typ, LuauObjectRegistryID::from_i64(lua_id), len))))
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

            let obj_id = common_state.proxy_client.obj_registry.add(scope, arr.into())
                .map_err(|e| format!("Failed to register array: {}", e))?;
            return Ok(Self::Array(obj_id));
        } else if value.is_array_buffer() {
            let ab = v8::Local::<v8::ArrayBuffer>::try_from(value).map_err(|_| "Failed to convert to ArrayBuffer")?;
            let ab_id = common_state.proxy_client.obj_registry.add(scope, ab.into())
                .map_err(|e| format!("Failed to register ArrayBuffer: {}", e))?;
            return Ok(Self::ArrayBuffer(ab_id));
        } else if value.is_function() {
            let func = v8::Local::<v8::Function>::try_from(value).map_err(|_| "Failed to convert to function")?;
            let func_id = common_state.proxy_client.obj_registry.add(scope, func.into())
                .map_err(|e| format!("Failed to register function: {}", e))?;
            return Ok(Self::Function(func_id));
        } else if value.is_promise() {
            let promise = v8::Local::<v8::Promise>::try_from(value).map_err(|_| "Failed to convert to promise")?;
            let promise_id = common_state.proxy_client.obj_registry.add(scope, promise.into())
                .map_err(|e| format!("Failed to register promise: {}", e))?;
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
                        let sid = common_state.proxy_client.obj_registry.add(scope, s.into())
                            .map_err(|e| format!("Failed to register string: {}", e))?;
                        return Ok(Self::StringRef((sid, s_len)));
                    } 
                    return Err("string_ref field is not a string".into());
                }
            }

            let obj_id = common_state.proxy_client.obj_registry.add(scope, obj.into())
                .map_err(|e| format!("Failed to register object: {}", e))?;
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
            Self::Array(id) | 
            Self::ArrayBuffer(id) | 
            Self::Object(id) | 
            Self::Function(id) |
            Self::Promise(id) | 
            Self::StringRef((id, _)) => {
                let obj = common_state.proxy_client.obj_registry.get(scope, id, |_scope, x| Ok(x))
                    .map_err(|e| format!("Object ID not found in registry: {}", e))?;
                Ok(obj.into())
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
                
                let local_template = v8::Local::new(scope, (*common_state.obj_template).clone());
                
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
