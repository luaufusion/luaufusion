use serde::{Deserialize, Serialize};
use crate::base::Error;
use crate::denobridge::bridgeops::LuaObject;
use crate::{denobridge::bridge::V8ObjectRegistryType, luau::foreignref::ForeignRef};
use crate::luau::embedder_api::EmbedderDataContext;
use crate::denobridge::psuedoprimitive::ProxiedV8PsuedoPrimitive;
use crate::luau::langbridge::ProxiedValue;
use crate::denobridge::objreg::V8ObjectRegistryID;
use crate::luau::LuauObjectRegistryID;
use super::primitives::ProxiedV8Primitive;
use super::V8IsolateManagerServer;
use crate::luau::bridge::{
    ObjectRegistryType, ProxyLuaClient
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
    /// Returns the list of owned object ids contained within this ProxiedV8Value
    pub(crate) fn get_owned_object_ids(&self) -> (Vec<V8ObjectRegistryID>, Vec<LuauObjectRegistryID>) {
        match self {
            ProxiedV8Value::Primitive(_) => (Vec::with_capacity(0), Vec::with_capacity(0)),
            ProxiedV8Value::Psuedoprimitive(p) => {
                match p {
                    ProxiedV8PsuedoPrimitive::StaticList(v) => {
                        let mut v8_ids = Vec::new();
                        let mut luau_ids = Vec::new();
                        for item in v {
                            let (v8s, luaus) = item.get_owned_object_ids();
                            v8_ids.extend(v8s);
                            luau_ids.extend(luaus);
                        }
                        (v8_ids, luau_ids)
                    },
                    ProxiedV8PsuedoPrimitive::StaticMap(v) => {
                        let mut v8_ids = Vec::new();
                        let mut luau_ids = Vec::new();
                        for (key, value) in v {
                            let (v8s_key, luau_ids_key) = key.get_owned_object_ids();
                            let (v8s_value, luau_ids_value) = value.get_owned_object_ids();
                            v8_ids.extend(v8s_key);
                            v8_ids.extend(v8s_value);
                            luau_ids.extend(luau_ids_key);
                            luau_ids.extend(luau_ids_value);
                        }
                        (v8_ids, luau_ids)
                    },
                    _ => (Vec::with_capacity(0), Vec::with_capacity(0)),
                }
            },
            ProxiedV8Value::V8OwnedObject((_, id)) => (vec![*id], Vec::with_capacity(0)),
            ProxiedV8Value::SourceOwnedObject((_, id)) => (Vec::with_capacity(0), vec![id.clone()]),
        }
    }

    /// Proxies a Luau value to a ProxiedV8Value
    pub(crate) fn from_luau(plc: &ProxyLuaClient, value: mluau::Value, ed: &mut EmbedderDataContext) -> Result<Self, Error> {
        let mut inner_ed_a = ed.nest_in_depth();
        if let Some(prim) = ProxiedV8Primitive::from_luau(&value, &mut inner_ed_a)? {
            ed.merge(inner_ed_a)?;
            return Ok(ProxiedV8Value::Primitive(prim));
        }

        let mut inner_ed_b = ed.nest_in_depth();
        if let Some(psuedo) = ProxiedV8PsuedoPrimitive::from_luau(plc, &value, &mut inner_ed_b)? {
            ed.merge(inner_ed_b)?;
            return Ok(ProxiedV8Value::Psuedoprimitive(psuedo));
        }
        
        ed.add(1, "ProxiedV8Value -> <base overhead>")?;

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
                if let Ok(v8value) = ud.borrow::<ForeignRef<V8IsolateManagerServer>>() {
                    return Ok(ProxiedV8Value::V8OwnedObject((v8value.typ, v8value.id)));
                }

                // Handle LangTransferValue
                if let Ok(ev) = ud.take::<ProxiedValue<V8IsolateManagerServer>>() {
                    return Ok(ev.into_inner())
                }

                let userdata_id = plc.obj_registry.add(mluau::Value::UserData(ud))
                    .map_err(|e| format!("Failed to add object to registry: {}", e))?;
                Ok(ProxiedV8Value::SourceOwnedObject((ObjectRegistryType::UserData, userdata_id)))
            }
            mluau::Value::Error(e) => return Err(format!("Cannot proxy Lua error value: {}", e).into()),
            value => {
                let obj_type = match ObjectRegistryType::from_value(&value) {
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
    pub(crate) fn to_luau(self, lua: &mluau::Lua, plc: &ProxyLuaClient, bridge: &V8IsolateManagerServer, ed: &mut EmbedderDataContext) -> Result<mluau::Value, mluau::Error> {
        ed.add(1, "ProxiedV8Value -> <base overhead>")
            .map_err(|e| mluau::Error::external(e.to_string()))?;
        match self {
            ProxiedV8Value::Primitive(p) => {
                Ok(p.to_luau(lua, ed).map_err(|e| mluau::Error::external(format!("Failed to convert ProxiedV8Primitive to Luau: {}", e)))?)
            },
            ProxiedV8Value::Psuedoprimitive(p) => Ok(p.to_luau(lua, plc, bridge, ed).map_err(|e| mluau::Error::external(format!("Failed to convert ProxiedV8PsuedoPrimitive to Luau: {}", e)))?),
            // Target owned value
            ProxiedV8Value::V8OwnedObject((typ, id)) => {
                let ud = ForeignRef::new(id, plc.clone(), bridge.clone(), typ);
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

    /// Given a v8 value, convert it to a ProxiedV8Value
    pub(super) fn from_v8<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
        common_state: &CommonState,
        ed: &mut EmbedderDataContext,
    ) -> Result<Self, Error> {
        ed.add(1, "ProxiedV8Value -> <base overhead>")?;

        if let Some(prim) = ProxiedV8Primitive::from_v8(scope, value, ed)? {
            return Ok(Self::Primitive(prim));
        }

        if let Some(psuedo) = ProxiedV8PsuedoPrimitive::from_v8(scope, value, common_state, ed)? {
            return Ok(Self::Psuedoprimitive(psuedo));
        }

        let typ = if value.is_array_buffer() {
            V8ObjectRegistryType::ArrayBuffer
        } else if value.is_function() {
            V8ObjectRegistryType::Function
        } else if value.is_promise() {
            V8ObjectRegistryType::Promise
        } else if value.is_object() {
            // Handled source-owned objects
            if let Some(v) = LuaObject::from_v8(scope, value) {
                return Ok(Self::SourceOwnedObject((v.lua_type, v.lua_id)));
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
        scope: &mut v8::PinScope<'s, '_>,
        common_state: &CommonState,
        ed: &mut EmbedderDataContext,
    ) -> Result<v8::Local<'s, v8::Value>, Error> {
        ed.add(1, "ProxiedV8Value -> <base overhead>")?;
        match self {
            Self::Primitive(p) => Ok(p.to_v8(scope, ed)?),
            Self::Psuedoprimitive(p) => Ok(p.to_v8(scope, common_state, ed)?),
            Self::V8OwnedObject((_typ, id)) => {
                let obj = common_state.proxy_client.obj_registry.get(scope, id, |_scope, x| Ok(x))
                    .map_err(|e| format!("Object ID not found in registry: {}", e))?;
                Ok(obj.into())
            }
            Self::SourceOwnedObject((typ, id)) => {
                let lua_obj = LuaObject::new(typ, id, common_state.bridge.clone());
                Ok(lua_obj.to_v8(scope))
            },
        }
    }
}
