use deno_core::v8;
use serde::{Deserialize, Serialize};
use crate::{base::Error, denobridge::bridge::MAX_OWNED_V8_STRING_SIZE};

#[derive(Serialize, Deserialize)]
/// A psuedoprimitive is a type that is not fully primitive (not immutable in both v8 and luau) but is copied between v8 and luau
pub enum ProxiedV8PsuedoPrimitive {
    Vector((f32, f32, f32)),
    StringBytes(serde_bytes::ByteBuf), // For byte sequences that are not valid UTF-8
}

impl ProxiedV8PsuedoPrimitive {
    /// Returns the number of bytes used by this psuedoprimitive
    ///
    /// Note that only stringbytes is counted here, as vectors are always 12 bytes
    pub fn effective_size(&self) -> usize {
        match self {
            Self::Vector(_) => 0, // Always 12 bytes, so ignore
            Self::StringBytes(b) => b.len(),
        }
    }

    /// Luau -> ProxiedV8PsuedoPrimitive
    pub(crate) fn from_luau(value: &mluau::Value) -> Result<Option<Self>, Error> {
        match value {
            mluau::Value::Vector(v) => {
                Ok(Some(Self::Vector((v.x(), v.y(), v.z()))))
            },
            mluau::Value::String(s) => {
                if s.as_bytes().len() > MAX_OWNED_V8_STRING_SIZE {
                    return Err(format!("String too large to be a primitive (max {} bytes)", MAX_OWNED_V8_STRING_SIZE).into());
                }

                Ok(Some(Self::StringBytes(serde_bytes::ByteBuf::from(s.as_bytes().to_vec()))))
            },
            _ => Ok(None),
        }
    }

    /// ProxiedV8PsuedoPrimitive -> Luau
    pub(crate) fn to_luau(self, lua: &mluau::Lua) -> Result<mluau::Value, Error> {
        match self {
            Self::Vector((x, y, z)) => {
                let vec = mluau::Vector::new(x, y, z);
                Ok(mluau::Value::Vector(vec))
            },
            Self::StringBytes(s) => {
                let s = lua.create_string(s)
                .map_err(|e| format!("Failed to create Lua string: {}", e))?;
                Ok(mluau::Value::String(s))
            }
        }
    }

    /// ProxiedV8PsuedoPrimitive -> V8
    pub(crate) fn to_v8<'s>(self, scope: &mut v8::HandleScope<'s>) -> Result<v8::Local<'s, v8::Value>, Error> {
        match self {
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
                let bs = v8::ArrayBuffer::new_backing_store_from_bytes(b.into_vec());
                let array_buffer = v8::ArrayBuffer::with_backing_store(scope, &bs.make_shared());
                let uint8_array = v8::Uint8Array::new(scope, array_buffer, 0, array_buffer.byte_length()).ok_or("Failed to create Uint8Array from ArrayBuffer")?;
                return Ok(uint8_array.into());
            }
        }
    }

    /// V8 -> ProxiedV8PsuedoPrimitive
    pub(crate) fn from_v8<'s>(
        scope: &mut v8::HandleScope<'s>,
        value: v8::Local<'s, v8::Value>,
    ) -> Result<Option<Self>, Error> {
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
        }

        // Not a psuedoprimitive
        Ok(None)
    }
}