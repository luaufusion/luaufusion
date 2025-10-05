use serde::{Deserialize, Serialize};
use crate::denobridge::bridge::V8ObjectRegistryType;
use crate::denobridge::psuedoprimitive::ProxiedV8PsuedoPrimitive;
use crate::{base::Error, denobridge::luauobjs::V8Value};
use crate::denobridge::objreg::V8ObjectRegistryID;
use crate::luau::objreg::LuauObjectRegistryID;
use super::primitives::ProxiedV8Primitive;
use super::V8IsolateManagerServer;
use crate::luau::bridge::{
    ObjectRegistryType, ProxyLuaClient, i32_to_obj_registry_type, luau_value_to_obj_registry_type, obj_registry_type_to_i32
};
use super::inner::CommonState;
use deno_core::v8;

/// A V8 value that can now be easily proxied to Luau
#[derive(Serialize, Deserialize)]
pub enum ProxiedV8Value {
    /// Primitive values
    Primitive(ProxiedV8Primitive),

    /// Psuedoprimitive values
    Psuedoprimitive(ProxiedV8PsuedoPrimitive),

    /// v8-owned stuff
    V8OwnedObject((V8ObjectRegistryType, V8ObjectRegistryID)), // (Type, ID) of the v8-owned object

    /// Source-owned stuff
    SourceOwnedObject((ObjectRegistryType, LuauObjectRegistryID)), // (Type, ID) of the source-owned object
}

impl ProxiedV8Value {
    /// Returns the number of bytes used by this proxied value
    /// 
    /// Note that only large objects (primitive string and psuedoprimitive stringbyte)
    /// are counted here, as other types are always small
    pub fn effective_size(&self) -> usize {
        match self {
            Self::Primitive(p) => p.effective_size(),
            Self::Psuedoprimitive(p) => p.effective_size(),
            Self::V8OwnedObject(_) => 0, // Just a reference
            Self::SourceOwnedObject(_) => 0, // Just a reference
        }
    }

    /// Proxies a Luau value to a ProxiedV8Value
    pub(crate) fn from_luau(plc: &ProxyLuaClient, value: mluau::Value) -> Result<Self, Error> {
        if let Some(prim) = ProxiedV8Primitive::from_luau(&value)? {
            return Ok(ProxiedV8Value::Primitive(prim));
        }

        if let Some(psuedo) = ProxiedV8PsuedoPrimitive::from_luau(plc, &value)? {
            return Ok(ProxiedV8Value::Psuedoprimitive(psuedo));
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
                "String should have been handled as a primitive/psuedoprimitive"
            ), // is a primitive
            mluau::Value::Other(r) => unreachable!(
                "Other({r:?}) should have been handled as a primitive"
            ), // is a primitive 
            mluau::Value::Vector(v) => unreachable!(
                "Vector({v:?}) should have been handled as a psuedoprimitive"
            ),
            mluau::Value::UserData(ud) => {
                // Handle v8 objects
                if let Ok(v8value) = ud.borrow::<V8Value>() {
                    return Ok(ProxiedV8Value::V8OwnedObject((v8value.typ, v8value.id)));
                }

                let userdata_id = plc.obj_registry.add(mluau::Value::UserData(ud))
                    .map_err(|e| format!("Failed to add object to registry: {}", e))?;
                Ok(ProxiedV8Value::SourceOwnedObject((ObjectRegistryType::UserData, userdata_id)))
            }
            mluau::Value::Error(e) => return Err(format!("Cannot proxy Lua error value: {}", e).into()),
            value => {
                let obj_type = match luau_value_to_obj_registry_type(&value) {
                    Some(t) => t,
                    None => return Err(format!("Cannot proxy Luau value of type {:?}", value).into()),
                };

                let id = plc.obj_registry.add(value)
                    .map_err(|e| format!("Failed to add object to registry: {}", e))?; 

                Ok(ProxiedV8Value::SourceOwnedObject((obj_type, id)))
            }
        }
    }

    /// Proxy a ProxiedV8Value to a Luau value
    pub(crate) fn to_luau(self, lua: &mluau::Lua, plc: &ProxyLuaClient, bridge: &V8IsolateManagerServer) -> Result<mluau::Value, mluau::Error> {
        match self {
            ProxiedV8Value::Primitive(p) => Ok(p.to_luau(lua).map_err(|e| mluau::Error::external(format!("Failed to convert ProxiedV8Primitive to Luau: {}", e)))?),
            ProxiedV8Value::Psuedoprimitive(p) => Ok(p.to_luau(lua, plc, bridge).map_err(|e| mluau::Error::external(format!("Failed to convert ProxiedV8PsuedoPrimitive to Luau: {}", e)))?),
            // Target owned value
            ProxiedV8Value::V8OwnedObject((typ, id)) => {
                let ud = V8Value::new(id, plc.clone(), bridge.clone(), typ);
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }

            // Source-owned values (lua values being proxied back from v8 to lua)
            ProxiedV8Value::SourceOwnedObject((_typ, id)) => {
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
        let typ_key = v8::Local::new(scope, &common_state.bridge_vals.lua_type_symbol);
        let typ_val = obj.get(scope, typ_key.into());
        if let Some(typ_val) = typ_val {
            if typ_val.is_int32() {
                let typ_i32 = typ_val.to_int32(scope).ok_or("Failed to convert lua type to int32")?.value();
                if let Some(typ) = i32_to_obj_registry_type(typ_i32) {
                    
                    // Look for luaid
                    let lua_id = {
                        let p_obj_key = v8::Local::new(scope, &common_state.bridge_vals.lua_id_symbol);
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

                    return Ok(Some(Self::SourceOwnedObject((typ, LuauObjectRegistryID::from_i64(lua_id)))))
                }
            }
        }

        Ok(None)
    }

    /// Given a v8 value, convert it to a ProxiedV8Value
    pub(super) fn from_v8<'s>(
        scope: &mut v8::HandleScope<'s>, 
        value: v8::Local<'s, v8::Value>,
        common_state: &CommonState,
    ) -> Result<Self, Error> {
        if let Some(prim) = ProxiedV8Primitive::from_v8(scope, value)? {
            return Ok(Self::Primitive(prim));
        }

        if let Some(psuedo) = ProxiedV8PsuedoPrimitive::from_v8(scope, value, common_state)? {
            return Ok(Self::Psuedoprimitive(psuedo));
        }

        let typ = if value.is_array() {
            V8ObjectRegistryType::Array
        } else if value.is_array_buffer() {
            V8ObjectRegistryType::ArrayBuffer
        } else if value.is_function() {
            V8ObjectRegistryType::Function
        } else if value.is_promise() {
            V8ObjectRegistryType::Promise
        } else if value.is_object() {
            let obj = value.to_object(scope).ok_or("Failed to convert to object")?;

            // Handled source-owned objects
            if let Some(v) = Self::proxy_source_owned_object_from_v8(scope, &common_state, obj)? {
                return Ok(v);
            }

            V8ObjectRegistryType::Object
        } else {
            V8ObjectRegistryType::Object
        };

        // Handle source-owned objects
        let obj_id = common_state.proxy_client.obj_registry.add(scope, value)
            .map_err(|e| format!("Failed to register array: {}", e))?;

        return Ok(Self::V8OwnedObject((typ, obj_id)));
    }

    /// Proxy a ProxiedV8Value to a V8 value
    pub(super) fn to_v8<'s>(
        self,
        scope: &mut v8::HandleScope<'s>, 
        common_state: &CommonState,
    ) -> Result<v8::Local<'s, v8::Value>, Error> {
        match self {
            Self::Primitive(p) => Ok(p.to_v8(scope)?),
            Self::Psuedoprimitive(p) => Ok(p.to_v8(scope, common_state)?),
            Self::V8OwnedObject((_typ, id)) => {
                let obj = common_state.proxy_client.obj_registry.get(scope, id, |_scope, x| Ok(x))
                    .map_err(|e| format!("Object ID not found in registry: {}", e))?;
                Ok(obj.into())
            }
            Self::SourceOwnedObject((typ, id)) => {
                let oid_key = v8::Local::new(scope, &common_state.bridge_vals.lua_id_symbol);
                let otype_key = v8::Local::new(scope, &common_state.bridge_vals.lua_type_symbol);
                
                let local_template = v8::Local::new(scope, (*common_state.obj_template).clone());
                
                let obj = local_template.new_instance(scope).ok_or("Failed to create V8 proxy object")?;

                let id_val = v8::BigInt::new_from_i64(scope, id.objid());
                obj.set(scope, oid_key.into(), id_val.into());
                let type_val = v8::Integer::new(scope, obj_registry_type_to_i32(typ));
                obj.set(scope, otype_key.into(), type_val.into());
                Ok(obj.into())
            },
        }
    }
}
