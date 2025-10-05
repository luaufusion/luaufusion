use serde::{Deserialize, Serialize};
use crate::{base::Error, denobridge::bridge::MAX_OWNED_V8_STRING_SIZE};
use deno_core::v8;

#[derive(Serialize, Deserialize)]
/// A primitive value that is primitive across v8 and luau (immutable and can be cloned between luau and v8)
/// 
/// Note that primitives do not have to be cheap to clone (for example, strings are primitives but can be
/// fairly large). However, they must be immutable and clonable (see psuedoprimitives for types that are cloneable but may 
/// not be immutable on either side of the proxy bridge such as non-UTF-8 strings and vectors).
/// 
/// Only primitives can be used with object.getProperty etc.
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
/// Returns the number of bytes used by this psuedoprimitive
    ///
    /// Note that only string is counted here, as other types are always small
    pub fn effective_size(&self) -> usize {
        match self {
            Self::String(b) => b.len(),
            _ => 0,
        }
    }

    /// Luau -> ProxiedV8Primitive
    pub(crate) fn from_luau(value: &mluau::Value) -> Result<Option<Self>, Error> {
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

                if let Ok(s) = s.to_str() {
                    return Ok(Some(ProxiedV8Primitive::String(s.to_string())));
                }

                Ok(None) // Not valid UTF-8, so not a primitive
            },
            mluau::Value::Other(r) => {
                let s = format!("unknown({r:?})");
                Ok(Some(ProxiedV8Primitive::String(s)))
            },
            _ => Ok(None),
        }
    }

    /// ProxiedV8Primitive -> Luau
    pub(crate) fn to_luau(self, lua: &mluau::Lua) -> Result<mluau::Value, Error> {
        match self {
            ProxiedV8Primitive::Nil => Ok(mluau::Value::Nil),
            ProxiedV8Primitive::Boolean(b) => Ok(mluau::Value::Boolean(b)),
            ProxiedV8Primitive::Integer(i) => Ok(mluau::Value::Integer(i as i64)),
            ProxiedV8Primitive::BigInt(i) => Ok(mluau::Value::Integer(i)),
            ProxiedV8Primitive::Number(n) => Ok(mluau::Value::Number(n)),
            ProxiedV8Primitive::Undefined => Ok(mluau::Value::Nil), // Luau does not have undefined, so we map it to nil
            ProxiedV8Primitive::String(s) => {
                let s = lua.create_string(s)
                .map_err(|e| format!("Failed to create Lua string: {}", e))?;
                Ok(mluau::Value::String(s))
            }
        }
    }

    /// ProxiedV8Primitive -> V8
    pub(crate) fn to_v8<'s>(self, scope: &mut v8::HandleScope<'s>) -> Result<v8::Local<'s, v8::Value>, Error> {
        match self {
            ProxiedV8Primitive::Nil => Ok(v8::null(scope).into()),
            ProxiedV8Primitive::Undefined => Ok(v8::undefined(scope).into()),
            ProxiedV8Primitive::Boolean(b) => Ok(v8::Boolean::new(scope, b).into()),
            ProxiedV8Primitive::Integer(i) => Ok(v8::Integer::new(scope, i).into()),
            ProxiedV8Primitive::BigInt(i) => Ok(v8::BigInt::new_from_i64(scope, i).into()),
            ProxiedV8Primitive::Number(n) => Ok(v8::Number::new(scope, n).into()),
            ProxiedV8Primitive::String(s) => {
                let mut try_catch = v8::TryCatch::new(scope);
                let s = v8::String::new(try_catch.as_mut(), &s);
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
    pub(crate) fn from_v8<'s>(
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
