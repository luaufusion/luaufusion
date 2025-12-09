use serde::{Deserialize, Serialize};
use crate::{base::Error, luau::{bridge::{LuaBridgeServiceClient, ProxyLuaClient}, embedder_api::EmbedderDataContext}, parallelluau::{ParallelLuaProxyBridge, ProxyPLuaClient, value::ProxiedLuauValue}};

#[derive(Serialize, Deserialize)]
/// A psuedoprimitive is a type that is not fully primitive (not immutable/hashable across luau states) but is copied between luau states
pub enum ProxiedLuauPsuedoPrimitive {
    Number(f64),
    Vector((f32, f32, f32)),
    StringBytes(serde_bytes::ByteBuf), // For byte sequences that are not valid UTF-8
    StaticList(Vec<ProxiedLuauValue>),
    StaticMap(Vec<(ProxiedLuauValue, ProxiedLuauValue)>)
}

impl ProxiedLuauPsuedoPrimitive {
    /// Luau -> ProxiedLuauPsuedoPrimitive (host mode)
    pub(crate) fn from_luau_host(plc: &ProxyLuaClient, value: &mluau::Value, ed: &mut EmbedderDataContext) -> Result<Option<Self>, Error> {
        ed.add(1, "ProxiedLuauPsuedoPrimitive -> <base overhead>")?;
        match value {
            mluau::Value::Number(n) => Ok(Some(Self::Number(*n))),
            mluau::Value::Vector(v) => {
                Ok(Some(Self::Vector((v.x(), v.y(), v.z()))))
            },
            mluau::Value::String(s) => {
                ed.add(s.as_bytes().len(), "ProxiedLuauPsuedoPrimitive -> LuaString")?;
                Ok(Some(Self::StringBytes(serde_bytes::ByteBuf::from(s.as_bytes().to_vec()))))
            },
            mluau::Value::Table(t) => {
                let mt = t.metatable();
                if mt == Some(plc.array_mt.clone()) {
                    ed.add(t.raw_len(), "ProxiedLuauPsuedoPrimitive -> LuaArray")?;

                    let mut list = Vec::with_capacity(t.raw_len());
                    let mut inner_ed = ed.nest()?;
                    t.for_each(|_: mluau::Value, v| {
                        let v = ProxiedLuauValue::from_luau_host(plc, v, &mut inner_ed)
                            .map_err(|e| mluau::Error::external(e.to_string()))?;
                        list.push(v);
                        Ok(())
                    })
                    .map_err(|e| format!("Failed to iterate over array: {}", e))?;
                    
                    ed.merge(inner_ed)?;

                    return Ok(Some(Self::StaticList(list)));
                } else if mt.is_none() {
                    let mut smap = Vec::new();
                    let mut n = 0;
                    let mut inner_ed = ed.nest()?;
                    t.for_each(|k, v| {
                        let k = ProxiedLuauValue::from_luau_host(plc, k, &mut inner_ed)
                            .map_err(|e| mluau::Error::external(e.to_string()))?;

                        let v = ProxiedLuauValue::from_luau_host(plc, v, &mut inner_ed)
                            .map_err(|e| mluau::Error::external(e.to_string()))?;
                        
                        smap.push((k, v));
                        n += 1;
                        Ok(())
                    })
                    .map_err(|e| format!("Failed to iterate over table: {}", e))?;

                    ed.merge(inner_ed)?;
                    ed.add(n, "ProxiedLuauPsuedoPrimitive -> LuaMap")?;

                    Ok(Some(Self::StaticMap(smap)))
                } else {
                    Ok(None) // Not a recognized psuedoprimitive table
                }
            }
            _ => Ok(None),
        }
    }

    /// Luau -> ProxiedLuauPsuedoPrimitive (child mode)
    pub(crate) fn from_luau_child(plc: &ProxyPLuaClient, value: &mluau::Value, ed: &mut EmbedderDataContext) -> Result<Option<Self>, Error> {
        ed.add(1, "ProxiedLuauPsuedoPrimitive -> <base overhead>")?;
        match value {
            mluau::Value::Number(n) => Ok(Some(Self::Number(*n))),
            mluau::Value::Vector(v) => {
                Ok(Some(Self::Vector((v.x(), v.y(), v.z()))))
            },
            mluau::Value::String(s) => {
                ed.add(s.as_bytes().len(), "ProxiedLuauPsuedoPrimitive -> LuaString")?;
                Ok(Some(Self::StringBytes(serde_bytes::ByteBuf::from(s.as_bytes().to_vec()))))
            },
            mluau::Value::Table(t) => {
                let mt = t.metatable();
                if mt == Some(plc.array_mt.clone()) {
                    ed.add(t.raw_len(), "ProxiedLuauPsuedoPrimitive -> LuaArray")?;

                    let mut list = Vec::with_capacity(t.raw_len());
                    let mut inner_ed = ed.nest()?;
                    t.for_each(|_: mluau::Value, v| {
                        let v = ProxiedLuauValue::from_luau_child(plc, v, &mut inner_ed)
                            .map_err(|e| mluau::Error::external(e.to_string()))?;
                        list.push(v);
                        Ok(())
                    })
                    .map_err(|e| format!("Failed to iterate over array: {}", e))?;
                    
                    ed.merge(inner_ed)?;

                    return Ok(Some(Self::StaticList(list)));
                } else if mt.is_none() {
                    let mut smap = Vec::new();
                    let mut n = 0;
                    let mut inner_ed = ed.nest()?;
                    t.for_each(|k, v| {
                        let k = ProxiedLuauValue::from_luau_child(plc, k, &mut inner_ed)
                            .map_err(|e| mluau::Error::external(e.to_string()))?;

                        let v = ProxiedLuauValue::from_luau_child(plc, v, &mut inner_ed)
                            .map_err(|e| mluau::Error::external(e.to_string()))?;
                        
                        smap.push((k, v));
                        n += 1;
                        Ok(())
                    })
                    .map_err(|e| format!("Failed to iterate over table: {}", e))?;

                    ed.merge(inner_ed)?;
                    ed.add(n, "ProxiedLuauPsuedoPrimitive -> LuaMap")?;

                    Ok(Some(Self::StaticMap(smap)))
                } else {
                    Ok(None) // Not a recognized psuedoprimitive table
                }
            }
            _ => Ok(None),
        }
    }

    /// ProxiedLuauPsuedoPrimitive -> Luau (host mode)
    pub(crate) fn to_luau_host(self, lua: &mluau::Lua, plc: &ProxyLuaClient, bridge: &ParallelLuaProxyBridge, ed: &mut EmbedderDataContext) -> Result<mluau::Value, Error> {
        ed.add(1, "ProxiedLuauPsuedoPrimitive -> <base overhead>")?;
        match self {
            Self::Number(n) => Ok(mluau::Value::Number(n)),
            Self::Vector((x, y, z)) => {
                let vec = mluau::Vector::new(x, y, z);
                Ok(mluau::Value::Vector(vec))
            },
            Self::StringBytes(s) => {
                ed.add(s.len(), "ProxiedLuauPsuedoPrimitive -> StringBytes")?;
                let s = lua.create_string(s)
                .map_err(|e| format!("Failed to create Lua string: {}", e))?;
                Ok(mluau::Value::String(s))
            }
            Self::StaticList(list) => {
                ed.add(list.len(), "ProxiedLuauPsuedoPrimitive -> LuaArray")?;
                let array = lua.create_table().map_err(|e| format!("Failed to create Lua table: {}", e))?;
                
                let mut inner_ed = ed.nest()?;
                
                for (i, v) in list.into_iter().enumerate() {
                    let v = v.to_luau_host(lua, plc, bridge, &mut inner_ed).map_err(|e| format!("Failed to convert value to Lua: {}", e))?;
                    array.set(i + 1, v).map_err(|e| format!("Failed to set index/value in Lua array: {}", e))?;
                }
                ed.merge(inner_ed)?;

                array.set_metatable(Some(plc.array_mt.clone())).map_err(|e| format!("Failed to set metatable on Lua array: {}", e))?;
                Ok(mluau::Value::Table(array))
            },
            Self::StaticMap(smap) => {
                ed.add(smap.len(), "ProxiedLuauPsuedoPrimitive -> LuaMap")?;
                
                let mut inner_ed = ed.nest()?;

                let table = lua.create_table().map_err(|e| format!("Failed to create Lua table: {}", e))?;
                for (k, v) in smap {
                    let k = k.to_luau_host(lua, plc, bridge, &mut inner_ed).map_err(|e| e.to_string())?;
                    let v = v.to_luau_host(lua, plc, bridge, &mut inner_ed).map_err(|e| format!("Failed to convert value to Lua: {}", e))?;
                    table.set(k, v).map_err(|e| format!("Failed to set key/value in Lua table: {}", e))?;
                }

                ed.merge(inner_ed)?;

                Ok(mluau::Value::Table(table))
            }
        }
    }

    /// ProxiedLuauPsuedoPrimitive -> Luau (child mode)
    pub(crate) fn to_luau_child(self, lua: &mluau::Lua, plc: &ProxyPLuaClient, bridge: &LuaBridgeServiceClient<ParallelLuaProxyBridge>, ed: &mut EmbedderDataContext) -> Result<mluau::Value, Error> {
        ed.add(1, "ProxiedLuauPsuedoPrimitive -> <base overhead>")?;
        match self {
            Self::Number(n) => Ok(mluau::Value::Number(n)),
            Self::Vector((x, y, z)) => {
                let vec = mluau::Vector::new(x, y, z);
                Ok(mluau::Value::Vector(vec))
            },
            Self::StringBytes(s) => {
                ed.add(s.len(), "ProxiedLuauPsuedoPrimitive -> StringBytes")?;
                let s = lua.create_string(s)
                .map_err(|e| format!("Failed to create Lua string: {}", e))?;
                Ok(mluau::Value::String(s))
            }
            Self::StaticList(list) => {
                ed.add(list.len(), "ProxiedLuauPsuedoPrimitive -> LuaArray")?;
                let array = lua.create_table().map_err(|e| format!("Failed to create Lua table: {}", e))?;
                
                let mut inner_ed = ed.nest()?;
                
                for (i, v) in list.into_iter().enumerate() {
                    let v = v.to_luau_child(lua, plc, bridge, &mut inner_ed).map_err(|e| format!("Failed to convert value to Lua: {}", e))?;
                    array.set(i + 1, v).map_err(|e| format!("Failed to set index/value in Lua array: {}", e))?;
                }
                ed.merge(inner_ed)?;

                array.set_metatable(Some(plc.array_mt.clone())).map_err(|e| format!("Failed to set metatable on Lua array: {}", e))?;
                Ok(mluau::Value::Table(array))
            },
            Self::StaticMap(smap) => {
                ed.add(smap.len(), "ProxiedLuauPsuedoPrimitive -> LuaMap")?;
                
                let mut inner_ed = ed.nest()?;

                let table = lua.create_table().map_err(|e| format!("Failed to create Lua table: {}", e))?;
                for (k, v) in smap {
                    let k = k.to_luau_child(lua, plc, bridge, &mut inner_ed).map_err(|e| e.to_string())?;
                    let v = v.to_luau_child(lua, plc, bridge, &mut inner_ed).map_err(|e| format!("Failed to convert value to Lua: {}", e))?;
                    table.set(k, v).map_err(|e| format!("Failed to set key/value in Lua table: {}", e))?;
                }

                ed.merge(inner_ed)?;

                Ok(mluau::Value::Table(table))
            }
        }
    }
}