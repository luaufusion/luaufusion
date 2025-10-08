use deno_core::v8;
use serde::{Deserialize, Serialize};
use crate::{base::Error, denobridge::{V8IsolateManagerServer, inner::CommonState, value::ProxiedV8Value}, luau::{bridge::ProxyLuaClient, embedder_api::EmbedderDataContext}};

#[derive(Serialize, Deserialize)]
/// A psuedoprimitive is a type that is not fully primitive (not immutable/hashable in both v8 and luau) but is copied between v8 and luau
pub enum ProxiedV8PsuedoPrimitive {
    Number(f64),
    Vector((f32, f32, f32)),
    StringBytes(serde_bytes::ByteBuf), // For byte sequences that are not valid UTF-8
    StaticList(Vec<ProxiedV8Value>),
    StaticMap(Vec<(ProxiedV8Value, ProxiedV8Value)>)
}

impl ProxiedV8PsuedoPrimitive {
    /// Luau -> ProxiedV8PsuedoPrimitive
    pub(crate) fn from_luau(plc: &ProxyLuaClient, value: &mluau::Value, ed: &mut EmbedderDataContext) -> Result<Option<Self>, Error> {
        ed.add(1, "ProxiedV8PsuedoPrimitive -> <base overhead>")?;
        match value {
            mluau::Value::Number(n) => Ok(Some(Self::Number(*n))),
            mluau::Value::Vector(v) => {
                Ok(Some(Self::Vector((v.x(), v.y(), v.z()))))
            },
            mluau::Value::String(s) => {
                ed.add(s.as_bytes().len(), "ProxiedV8PsuedoPrimitive -> LuaString")?;
                Ok(Some(Self::StringBytes(serde_bytes::ByteBuf::from(s.as_bytes().to_vec()))))
            },
            mluau::Value::Table(t) => {
                let mt = t.metatable();
                if mt == Some(plc.array_mt.clone()) {
                    ed.add(t.raw_len(), "ProxiedV8PsuedoPrimitive -> LuaArray")?;

                    let mut list = Vec::with_capacity(t.raw_len());
                    let mut inner_ed = ed.nest()?;
                    t.for_each(|_: mluau::Value, v| {
                        let v = ProxiedV8Value::from_luau(plc, v, &mut inner_ed)
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
                        let k = ProxiedV8Value::from_luau(plc, k, &mut inner_ed)
                            .map_err(|e| mluau::Error::external(e.to_string()))?;

                        let v = ProxiedV8Value::from_luau(plc, v, &mut inner_ed)
                            .map_err(|e| mluau::Error::external(e.to_string()))?;
                        
                        smap.push((k, v));
                        n += 1;
                        Ok(())
                    })
                    .map_err(|e| format!("Failed to iterate over table: {}", e))?;

                    ed.merge(inner_ed)?;
                    ed.add(n, "ProxiedV8PsuedoPrimitive -> LuaMap")?;

                    Ok(Some(Self::StaticMap(smap)))
                } else {
                    Ok(None) // Not a recognized psuedoprimitive table
                }
            }
            _ => Ok(None),
        }
    }

    /// ProxiedV8PsuedoPrimitive -> Luau
    pub(crate) fn to_luau(self, lua: &mluau::Lua, plc: &ProxyLuaClient, bridge: &V8IsolateManagerServer, ed: &mut EmbedderDataContext) -> Result<mluau::Value, Error> {
        ed.add(1, "ProxiedV8PsuedoPrimitive -> <base overhead>")?;
        match self {
            Self::Number(n) => Ok(mluau::Value::Number(n)),
            Self::Vector((x, y, z)) => {
                let vec = mluau::Vector::new(x, y, z);
                Ok(mluau::Value::Vector(vec))
            },
            Self::StringBytes(s) => {
                ed.add(s.len(), "ProxiedV8Primitive -> StringBytes")?;
                let s = lua.create_string(s)
                .map_err(|e| format!("Failed to create Lua string: {}", e))?;
                Ok(mluau::Value::String(s))
            }
            Self::StaticList(list) => {
                ed.add(list.len(), "ProxiedV8PsuedoPrimitive -> LuaArray")?;
                let array = lua.create_table().map_err(|e| format!("Failed to create Lua table: {}", e))?;
                
                let mut inner_ed = ed.nest()?;
                
                for (i, v) in list.into_iter().enumerate() {
                    let v = v.to_luau(lua, plc, bridge, &mut inner_ed).map_err(|e| format!("Failed to convert value to Lua: {}", e))?;
                    array.set(i + 1, v).map_err(|e| format!("Failed to set index/value in Lua array: {}", e))?;
                }
                ed.merge(inner_ed)?;

                array.set_metatable(Some(plc.array_mt.clone())).map_err(|e| format!("Failed to set metatable on Lua array: {}", e))?;
                Ok(mluau::Value::Table(array))
            },
            Self::StaticMap(smap) => {
                ed.add(smap.len(), "ProxiedV8PsuedoPrimitive -> LuaMap")?;
                
                let mut inner_ed = ed.nest()?;

                let table = lua.create_table().map_err(|e| format!("Failed to create Lua table: {}", e))?;
                for (k, v) in smap {
                    let k = k.to_luau(lua, plc, bridge, &mut inner_ed).map_err(|e| e.to_string())?;
                    let v = v.to_luau(lua, plc, bridge, &mut inner_ed).map_err(|e| format!("Failed to convert value to Lua: {}", e))?;
                    table.set(k, v).map_err(|e| format!("Failed to set key/value in Lua table: {}", e))?;
                }

                ed.merge(inner_ed)?;

                Ok(mluau::Value::Table(table))
            }
        }
    }

    /// ProxiedV8PsuedoPrimitive -> V8
    pub(crate) fn to_v8<'s>(self, scope: &mut v8::PinScope<'s, '_>, common_state: &CommonState, ed: &mut EmbedderDataContext) -> Result<v8::Local<'s, v8::Value>, Error> {
        ed.add(1, "ProxiedV8PsuedoPrimitive -> <base overhead>")?;
        match self {
            Self::Number(n) => {
                let num = v8::Number::new(scope, n);
                Ok(num.into())
            },
            Self::Vector((x, y, z)) => {
                let array = v8::Array::new(scope, 3);
                let x = v8::Number::new(scope, x as f64);
                let y = v8::Number::new(scope, y as f64);
                let z = v8::Number::new(scope, z as f64);
                array.set_index(scope, 0, x.into());
                array.set_index(scope, 1, y.into());
                array.set_index(scope, 2, z.into());
                Ok(array.into())
            },
            Self::StringBytes(b) => {
                // Proxy as a Uint8Array to v8
                ed.add(b.len(), "ProxiedV8Primitive -> StringBytes")?;

                let bs = v8::ArrayBuffer::new_backing_store_from_bytes(b.into_vec());
                let array_buffer = v8::ArrayBuffer::with_backing_store(scope, &bs.make_shared());
                let uint8_array = v8::Uint8Array::new(scope, array_buffer, 0, array_buffer.byte_length()).ok_or("Failed to create Uint8Array from ArrayBuffer")?;
                return Ok(uint8_array.into());
            }
            Self::StaticList(list) => {
                ed.add(list.len(), "ProxiedV8PsuedoPrimitive -> V8Array")?;
                let array = v8::Array::new(scope, list.len() as i32);
                let mut inner_ed = ed.nest()?;
                for (i, v) in list.into_iter().enumerate() {
                    let v = v.to_v8(scope, common_state, &mut inner_ed)?;
                    array.set_index(scope, i as u32, v);
                }
                ed.merge(inner_ed)?;
                Ok(array.into())
            },
            Self::StaticMap(smap) => {
                ed.add(smap.len(), "ProxiedV8PsuedoPrimitive -> V8Map")?;
                let map = v8::Map::new(scope);
                let mut inner_ed = ed.nest()?;
                for (k, v) in smap {
                    let k = k.to_v8(scope, common_state, &mut inner_ed)?;
                    let v = v.to_v8(scope, common_state, &mut inner_ed)?;
                    map.set(scope, k, v)
                    .ok_or("Failed to set key/value in V8 Map")?;
                }
                ed.merge(inner_ed)?;
                Ok(map.into())
            }
        }
    }

    /// V8 -> ProxiedV8PsuedoPrimitive
    pub(crate) fn from_v8<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
        common_state: &CommonState,
        ed: &mut EmbedderDataContext
    ) -> Result<Option<Self>, Error> {
        ed.add(1, "ProxiedV8PsuedoPrimitive -> <base overhead>")?;

        if value.is_number() {
            let n = value.to_number(scope).unwrap().value();
            return Ok(Some(Self::Number(n)));
        }

        if value.is_uint8_array() {
            let uint8_array = v8::Local::<v8::Uint8Array>::try_from(value)
                .map_err(|_| "Failed to convert V8 value to Uint8Array")?;
            let length = uint8_array.byte_length();
            ed.add(length, "ProxiedV8Primitive -> V8Uint8Array")?;

            let mut buf = vec![0u8; length as usize];
            uint8_array.copy_contents(&mut buf);
            return Ok(Some(Self::StringBytes(serde_bytes::ByteBuf::from(buf))));
        }

        if value.is_array() {
            let array = v8::Local::<v8::Array>::try_from(value)
                .map_err(|_| "Failed to convert V8 value to Array")?;
            ed.add(array.length() as usize, "ProxiedV8Primitive -> V8Array")?;

            if array.length() == 3 {
                let x = array.get_index(scope, 0).ok_or("Failed to get index 0 of array")?;
                let y = array.get_index(scope, 1).ok_or("Failed to get index 1 of array")?;
                let z = array.get_index(scope, 2).ok_or("Failed to get index 2 of array")?;
                if x.is_number() && y.is_number() && z.is_number() {
                    let x = x.to_number(scope).unwrap().value() as f32;
                    let y = y.to_number(scope).unwrap().value() as f32;
                    let z = z.to_number(scope).unwrap().value() as f32;
                    return Ok(Some(Self::Vector((x, y, z))));
                }
            }

            let mut list = Vec::new();
            let mut inner_ed = ed.nest()?;
            for i in 0..array.length() {
                let v = array.get_index(scope, i).ok_or("Failed to get index of array")?;
                let v = ProxiedV8Value::from_v8(scope, v, common_state, &mut inner_ed)?;
                list.push(v);
            }
            ed.merge(inner_ed)?;
            return Ok(Some(Self::StaticList(list)));
        }

        if value.is_map() {
            let map = v8::Local::<v8::Map>::try_from(value)
                .map_err(|_| "Failed to convert V8 value to Map")?;
            let mut smap = Vec::new();
            let entries = map.as_array(scope);
            let length = entries.length();
            ed.add(length as usize, "ProxiedV8Primitive -> V8Map")?;

            let mut inner_ed = ed.nest()?;
            for i in (0..length).step_by(2) {
                let key = entries.get_index(scope, i).ok_or("Failed to get index of map entries")?;
                let value = entries.get_index(scope, i + 1).ok_or("Failed to get value of map entries")?;
                let k = ProxiedV8Value::from_v8(scope, key, common_state, &mut inner_ed)?;
                let v = ProxiedV8Value::from_v8(scope, value, common_state, &mut inner_ed)?;

                smap.push((k, v));
            }
            ed.merge(inner_ed)?;
            return Ok(Some(Self::StaticMap(smap)));
        }

        // Not a psuedoprimitive
        Ok(None)
    }
}