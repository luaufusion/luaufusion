use std::collections::HashMap;

use deno_core::v8;
use serde::{Deserialize, Serialize};
use crate::{MAX_PROXY_DEPTH, base::Error, denobridge::{V8IsolateManagerServer, bridge::MAX_OWNED_V8_STRING_SIZE, inner::CommonState, primitives::ProxiedV8Primitive, value::ProxiedV8Value}, luau::bridge::ProxyLuaClient};

#[derive(Serialize, Deserialize)]
/// A psuedoprimitive is a type that is not fully primitive (not immutable/hashable in both v8 and luau) but is copied between v8 and luau
pub enum ProxiedV8PsuedoPrimitive {
    Number(f64),
    Vector((f32, f32, f32)),
    StringBytes(serde_bytes::ByteBuf), // For byte sequences that are not valid UTF-8
    StaticList(Vec<ProxiedV8Value>),
    StaticMap(HashMap<ProxiedV8Primitive, ProxiedV8Value>)
}

impl ProxiedV8PsuedoPrimitive {
    /// Returns the number of bytes used by this psuedoprimitive
    ///
    /// Note that only stringbytes is counted here, as vectors are always 12 bytes
    pub fn effective_size(&self, depth: usize) -> usize {
        if depth >= MAX_PROXY_DEPTH {
            return MAX_PROXY_DEPTH; // Prevent excessively deep recursion
        }
        match self {
            Self::Number(_) => 1, // Always 8 bytes, so ignore
            Self::Vector(_) => 1, // Always 12 bytes, so ignore
            Self::StaticList(list) => {
                let mut size = 1; // Base overhead
                for v in list {
                    size += v.effective_size(depth+1);
                }
                size
            },
            Self::StaticMap(map) => {
                let mut size = 1; // Base overhead
                for (k, v) in map {
                    size += k.effective_size();
                    size += v.effective_size(depth+1);
                }
                size
            },
            Self::StringBytes(b) => b.len(),
        }
    }

    /// Luau -> ProxiedV8PsuedoPrimitive
    pub(crate) fn from_luau(plc: &ProxyLuaClient, value: &mluau::Value, depth: usize) -> Result<Option<Self>, Error> {
        if depth >= MAX_PROXY_DEPTH {
            return Err("Maximum proxy depth exceeded when converting Luau to psuedoprimitive".into());
        }
        match value {
            mluau::Value::Number(n) => Ok(Some(Self::Number(*n))),
            mluau::Value::Vector(v) => {
                Ok(Some(Self::Vector((v.x(), v.y(), v.z()))))
            },
            mluau::Value::String(s) => {
                if s.as_bytes().len() > MAX_OWNED_V8_STRING_SIZE {
                    return Err(format!("String too large to be a primitive (max {} bytes)", MAX_OWNED_V8_STRING_SIZE).into());
                }

                Ok(Some(Self::StringBytes(serde_bytes::ByteBuf::from(s.as_bytes().to_vec()))))
            },
            mluau::Value::Table(t) => {
                if !t.is_readonly() {
                    return Ok(None); // Non-readonly tables are not psuedoprimitives, but rather are references
                }

                if t.metatable() == Some(plc.array_mt.clone()) {
                    let mut list = Vec::new();
                    t.for_each(|_: mluau::Value, v| {
                        let v = ProxiedV8Value::from_luau(plc, v, depth+1)
                            .map_err(|e| mluau::Error::external(e.to_string()))?;
                        list.push(v);
                        Ok(())
                    })
                    .map_err(|e| format!("Failed to iterate over array: {}", e))?;

                    return Ok(Some(Self::StaticList(list)));
                }

                let mut smap = HashMap::new();
                t.for_each(|k, v| {
                    let k = ProxiedV8Primitive::from_luau(&k)
                        .map_err(|e| mluau::Error::external(e.to_string()))?;
                    let v = ProxiedV8Value::from_luau(plc, v, depth + 1)
                        .map_err(|e| mluau::Error::external(e.to_string()))?;
                    if let Some(k) = k {
                        smap.insert(k, v);
                    } else {
                        return Err(mluau::Error::external("Table key is not a ProxiedV8Primitive"));
                    }
                    Ok(())
                })
                .map_err(|e| format!("Failed to iterate over table: {}", e))?;

                Ok(Some(Self::StaticMap(smap)))
            }
            _ => Ok(None),
        }
    }

    /// ProxiedV8PsuedoPrimitive -> Luau
    pub(crate) fn to_luau(self, lua: &mluau::Lua, plc: &ProxyLuaClient, bridge: &V8IsolateManagerServer, depth: usize) -> Result<mluau::Value, Error> {
        match self {
            Self::Number(n) => Ok(mluau::Value::Number(n)),
            Self::Vector((x, y, z)) => {
                let vec = mluau::Vector::new(x, y, z);
                Ok(mluau::Value::Vector(vec))
            },
            Self::StringBytes(s) => {
                let s = lua.create_string(s)
                .map_err(|e| format!("Failed to create Lua string: {}", e))?;
                Ok(mluau::Value::String(s))
            }
            Self::StaticList(list) => {
                if depth >= MAX_PROXY_DEPTH {
                    return Err("Maximum proxy depth exceeded when converting psuedoprimitive to Lua".into());
                }
                let array = lua.create_table().map_err(|e| format!("Failed to create Lua table: {}", e))?;
                for (i, v) in list.into_iter().enumerate() {
                    let v = v.to_luau(lua, plc, bridge, depth + 1).map_err(|e| format!("Failed to convert value to Lua: {}", e))?;
                    array.set(i + 1, v).map_err(|e| format!("Failed to set index/value in Lua array: {}", e))?;
                }
                array.set_metatable(Some(plc.array_mt.clone())).map_err(|e| format!("Failed to set metatable on Lua array: {}", e))?;
                Ok(mluau::Value::Table(array))
            },
            Self::StaticMap(smap) => {
                if depth >= MAX_PROXY_DEPTH {
                    return Err("Maximum proxy depth exceeded when converting psuedoprimitive to Lua".into());
                }
                let table = lua.create_table().map_err(|e| format!("Failed to create Lua table: {}", e))?;
                for (k, v) in smap {
                    let k = k.to_luau(lua)?;
                    let v = v.to_luau(lua, plc, bridge, depth + 1).map_err(|e| format!("Failed to convert value to Lua: {}", e))?;
                    table.set(k, v).map_err(|e| format!("Failed to set key/value in Lua table: {}", e))?;
                }
                Ok(mluau::Value::Table(table))
            }
        }
    }

    /// ProxiedV8PsuedoPrimitive -> V8
    pub(crate) fn to_v8<'s>(self, scope: &mut v8::PinScope<'s, '_>, common_state: &CommonState, depth: usize) -> Result<v8::Local<'s, v8::Value>, Error> {
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
                if b.len() > MAX_OWNED_V8_STRING_SIZE {
                    return Err(format!("[to_v8] Byte sequence too large to be a primitive (max {} bytes)", MAX_OWNED_V8_STRING_SIZE).into());
                }

                let bs = v8::ArrayBuffer::new_backing_store_from_bytes(b.into_vec());
                let array_buffer = v8::ArrayBuffer::with_backing_store(scope, &bs.make_shared());
                let uint8_array = v8::Uint8Array::new(scope, array_buffer, 0, array_buffer.byte_length()).ok_or("Failed to create Uint8Array from ArrayBuffer")?;
                return Ok(uint8_array.into());
            }
            Self::StaticList(list) => {
                if depth >= MAX_PROXY_DEPTH {
                    return Err("Maximum proxy depth exceeded when converting psuedoprimitive to V8".into());
                }
                let array = v8::Array::new(scope, list.len() as i32);
                for (i, v) in list.into_iter().enumerate() {
                    let v = v.to_v8(scope, common_state, depth+1)?;
                    array.set_index(scope, i as u32, v);
                }
                Ok(array.into())
            },
            Self::StaticMap(smap) => {
                if depth >= MAX_PROXY_DEPTH {
                    return Err("Maximum proxy depth exceeded when converting psuedoprimitive to V8".into());
                }
                let map = v8::Map::new(scope);
                for (k, v) in smap {
                    let k = k.to_v8(scope)?;
                    let v = v.to_v8(scope, common_state, depth+1)?;
                    map.set(scope, k, v)
                    .ok_or("Failed to set key/value in V8 Map")?;
                }
                Ok(map.into())
            }
        }
    }

    /// V8 -> ProxiedV8PsuedoPrimitive
    pub(crate) fn from_v8<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
        common_state: &CommonState,
        depth: usize
    ) -> Result<Option<Self>, Error> {
        if depth >= MAX_PROXY_DEPTH {
            return Err("Maximum proxy depth exceeded when converting V8 to psuedoprimitive".into());
        }

        if value.is_number() {
            let n = value.to_number(scope).unwrap().value();
            return Ok(Some(Self::Number(n)));
        }

        if value.is_uint8_array() {
            let uint8_array = v8::Local::<v8::Uint8Array>::try_from(value)
                .map_err(|_| "Failed to convert V8 value to Uint8Array")?;
            let length = uint8_array.byte_length();
            if length > MAX_OWNED_V8_STRING_SIZE {
                return Err(format!("Uint8Array too large to be a primitive (max {} bytes)", MAX_OWNED_V8_STRING_SIZE).into());
            }
            let mut buf = vec![0u8; length as usize];
            uint8_array.copy_contents(&mut buf);
            return Ok(Some(Self::StringBytes(serde_bytes::ByteBuf::from(buf))));
        }

        if value.is_array() {
            let array = v8::Local::<v8::Array>::try_from(value)
                .map_err(|_| "Failed to convert V8 value to Array")?;
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
            let mut num_string_chars = 0;
            for i in 0..array.length() {
                let v = array.get_index(scope, i).ok_or("Failed to get index of array")?;
                let v = ProxiedV8Value::from_v8(scope, v, common_state, depth+1)?;
                num_string_chars += v.effective_size(0);
                if num_string_chars > MAX_OWNED_V8_STRING_SIZE {
                    return Err(format!("Too many string characters in array when converting to psuedoprimitive (max {} bytes)", MAX_OWNED_V8_STRING_SIZE).into());
                }
                list.push(v);
            }
            return Ok(Some(Self::StaticList(list)));
        }

        if value.is_map() {
            let map = v8::Local::<v8::Map>::try_from(value)
                .map_err(|_| "Failed to convert V8 value to Map")?;
            let mut smap = HashMap::new();
            let entries = map.as_array(scope);
            let length = entries.length();
            let mut num_string_chars = 0;
            for i in (0..length).step_by(2) {
                let key = entries.get_index(scope, i).ok_or("Failed to get index of map entries")?;
                let value = entries.get_index(scope, i + 1).ok_or("Failed to get value of map entries")?;
                let k = ProxiedV8Primitive::from_v8(scope, key)?.ok_or("Map key is not a ProxiedV8Primitive")?;
                num_string_chars += k.effective_size(); // Check key size first
                if num_string_chars > MAX_OWNED_V8_STRING_SIZE {
                    return Err(format!("Too many string characters in map keys when converting to psuedoprimitive (max {} bytes)", MAX_OWNED_V8_STRING_SIZE).into());
                }
                let v = ProxiedV8Value::from_v8(scope, value, common_state, depth+1)?;
                num_string_chars += v.effective_size(0); // Then check value size
                if num_string_chars > MAX_OWNED_V8_STRING_SIZE {
                    return Err(format!("Too many string characters in map when converting to psuedoprimitive (max {} bytes)", MAX_OWNED_V8_STRING_SIZE).into());
                }
                smap.insert(k, v);
            }
            return Ok(Some(Self::StaticMap(smap)));
        }

        // Not a psuedoprimitive
        Ok(None)
    }
}