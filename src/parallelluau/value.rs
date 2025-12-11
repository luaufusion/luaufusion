use serde::{Deserialize, Serialize};
use crate::luau::embedder_api::EmbedderDataContext;
use crate::base::Error;
use crate::luau::LuauObjectRegistryID;
use crate::luau::langbridge::ProxiedValue;
use crate::parallelluau::{ParallelLuaProxyBridge, ProxyPLuaClient};
use crate::parallelluau::foreignref::ForeignLuauValue;
use crate::parallelluau::objreg::PLuauObjectRegistryID;
use crate::parallelluau::primitives::ProxiedLuauPrimitive;
use crate::parallelluau::psuedoprimitive::ProxiedLuauPsuedoPrimitive;
use crate::luau::bridge::{
    LuaBridgeServiceClient, ObjectRegistryType, ProxyLuaClient
};
/// A Luau value proxied between Luau states
#[derive(Serialize, Deserialize)]
pub enum ProxiedLuauValue {
    /// Primitive values
    Primitive(ProxiedLuauPrimitive),

    /// Psuedoprimitive values
    Psuedoprimitive(ProxiedLuauPsuedoPrimitive),

    /// target-owned stuff
    TargetOwnedObject((ObjectRegistryType, PLuauObjectRegistryID)), // (Type, ID) of the target-owned object

    /// Source-owned stuff
    SourceOwnedObject((ObjectRegistryType, LuauObjectRegistryID)), // (Type, ID) of the source-owned object
}

impl ProxiedLuauValue {
    /// Proxies a Luau value to a ProxiedLuauValue host-side
    pub(crate) fn from_luau_host(plc: &ProxyLuaClient, value: mluau::Value, ed: &mut EmbedderDataContext) -> Result<Self, Error> {
        let mut inner_ed_a = ed.nest_in_depth();
        if let Some(prim) = ProxiedLuauPrimitive::from_luau(&value, &mut inner_ed_a)? {
            ed.merge(inner_ed_a)?;
            return Ok(Self::Primitive(prim));
        }

        let mut inner_ed_b = ed.nest_in_depth();
        if let Some(psuedo) = ProxiedLuauPsuedoPrimitive::from_luau_host(plc, &value, &mut inner_ed_b)? {
            ed.merge(inner_ed_b)?;
            return Ok(Self::Psuedoprimitive(psuedo));
        }
        
        ed.add(1, "ProxiedLuauValue -> <base overhead>")?;

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
                // Handle luau objects
                if let Ok(flvalue) = ud.borrow::<ForeignLuauValue>() {
                    match &*flvalue {
                        ForeignLuauValue::Host { id, typ, .. } => {
                            return Ok(Self::SourceOwnedObject((*typ, id.clone())));
                        },
                        ForeignLuauValue::Child { id, typ, .. } => {
                            return Ok(Self::TargetOwnedObject((*typ, id.clone())));
                        }
                    }
                }

                // Handle LangTransferValue
                if let Ok(ev) = ud.take::<ProxiedValue<ParallelLuaProxyBridge>>() {
                    return Ok(ev.into_inner())
                }

                let userdata_id = plc.obj_registry.add(mluau::Value::UserData(ud))
                    .map_err(|e| format!("Failed to add object to registry: {}", e))?;
                Ok(Self::SourceOwnedObject((ObjectRegistryType::UserData, userdata_id)))
            }
            mluau::Value::Error(e) => return Err(format!("Cannot proxy Lua error value: {}", e).into()),
            value => {
                let obj_type = match ObjectRegistryType::from_value(&value) {
                    Some(t) => t,
                    None => return Err(format!("Cannot proxy Luau value of type {:?}", value).into()),
                };

                let id = plc.obj_registry.add(value)
                    .map_err(|e| format!("Failed to add object to registry: {}", e))?; 

                Ok(Self::SourceOwnedObject((obj_type, id)))
            }
        }
    }

    /// Proxies a Luau value to a ProxiedLuauValue child-side
    pub(crate) fn from_luau_child(plc: &ProxyPLuaClient, value: mluau::Value, ed: &mut EmbedderDataContext) -> Result<Self, Error> {
        let mut inner_ed_a = ed.nest_in_depth();
        if let Some(prim) = ProxiedLuauPrimitive::from_luau(&value, &mut inner_ed_a)? {
            ed.merge(inner_ed_a)?;
            return Ok(Self::Primitive(prim));
        }

        let mut inner_ed_b = ed.nest_in_depth();
        if let Some(psuedo) = ProxiedLuauPsuedoPrimitive::from_luau_child(plc, &value, &mut inner_ed_b)? {
            ed.merge(inner_ed_b)?;
            return Ok(Self::Psuedoprimitive(psuedo));
        }
        
        ed.add(1, "ProxiedLuauValue -> <base overhead>")?;

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
                // Handle luau objects
                if let Ok(flvalue) = ud.borrow::<ForeignLuauValue>() {
                    match &*flvalue {
                        ForeignLuauValue::Host { id, typ, .. } => {
                            return Ok(Self::SourceOwnedObject((*typ, id.clone())));
                        },
                        ForeignLuauValue::Child { id, typ, .. } => {
                            return Ok(Self::TargetOwnedObject((*typ, id.clone())));
                        }
                    }
                }

                let userdata_id = plc.obj_registry.add(mluau::Value::UserData(ud))
                    .map_err(|e| format!("Failed to add object to registry: {}", e))?;
                Ok(Self::TargetOwnedObject((ObjectRegistryType::UserData, userdata_id)))
            }
            mluau::Value::Error(e) => return Err(format!("Cannot proxy Lua error value: {}", e).into()),
            value => {
                let obj_type = match ObjectRegistryType::from_value(&value) {
                    Some(t) => t,
                    None => return Err(format!("Cannot proxy Luau value of type {:?}", value).into()),
                };

                let id = plc.obj_registry.add(value)
                    .map_err(|e| format!("Failed to add object to registry: {}", e))?; 

                Ok(Self::TargetOwnedObject((obj_type, id)))
            }
        }
    }

    /// Proxy a ProxiedLuauValue to a Luau value host-side
    pub(crate) fn to_luau_host(self, lua: &mluau::Lua, plc: &ProxyLuaClient, bridge: &ParallelLuaProxyBridge, ed: &mut EmbedderDataContext) -> Result<mluau::Value, mluau::Error> {
        ed.add(1, "ProxiedLuauValue -> <base overhead>")
            .map_err(|e| mluau::Error::external(e.to_string()))?;
        match self {
            Self::Primitive(p) => {
                Ok(p.to_luau(lua, ed).map_err(|e| mluau::Error::external(format!("Failed to convert ProxiedLuauPrimitive to Luau: {}", e)))?)
            },
            Self::Psuedoprimitive(p) => Ok(p.to_luau_host(lua, plc, bridge, ed).map_err(|e| mluau::Error::external(format!("Failed to convert ProxiedLuauPsuedoPrimitive to Luau: {}", e)))?),
            // Target owned value
            Self::TargetOwnedObject((typ, id)) => {
                let ud = ForeignLuauValue::new_child(id, plc.clone(), bridge.clone(), typ);
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))
            }
            // Source-owned values (lua values being proxied back from child luau to host luau)
            Self::SourceOwnedObject((_typ, id)) => {
                let value = plc.obj_registry.get(id)
                    .map_err(|e| mluau::Error::external(format!("Failed to get object from registry: {}", e)))?;
                Ok(value)
            }
        }
    }

    /// Proxy a ProxiedLuauValue to a Luau value child-side
    pub(crate) fn to_luau_child(self, lua: &mluau::Lua, plc: &ProxyPLuaClient, bridge: &LuaBridgeServiceClient<ParallelLuaProxyBridge>, ed: &mut EmbedderDataContext) -> Result<mluau::Value, mluau::Error> {
        ed.add(1, "ProxiedLuauValue -> <base overhead>")
            .map_err(|e| mluau::Error::external(e.to_string()))?;
        match self {
            Self::Primitive(p) => {
                Ok(p.to_luau(lua, ed).map_err(|e| mluau::Error::external(format!("Failed to convert ProxiedLuauPrimitive to Luau: {}", e)))?)
            },
            Self::Psuedoprimitive(p) => Ok(p.to_luau_child(lua, plc, bridge, ed).map_err(|e| mluau::Error::external(format!("Failed to convert ProxiedLuauPsuedoPrimitive to Luau: {}", e)))?),
            // Target owned value
            Self::TargetOwnedObject((_typ, id)) => {
                let value = plc.obj_registry.get(id)
                    .map_err(|e| mluau::Error::external(format!("Failed to get object from registry: {}", e)))?;
                Ok(value)
            }
            // Source-owned values (lua values being proxied between host luau and child luau)
            Self::SourceOwnedObject((typ, id)) => {
                let ud = ForeignLuauValue::new_host(id, plc.clone(), bridge.clone(), typ);
                let ud = lua.create_userdata(ud)?;
                Ok(mluau::Value::UserData(ud))

            }
        }
    }
}
