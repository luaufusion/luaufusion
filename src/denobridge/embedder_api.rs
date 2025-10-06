#[cfg(feature = "embedder_json")]
use serde_json::Value as JsonValue;
#[cfg(feature = "embedder_json")]
use serde_json::value::RawValue as JsonRawValue;

#[cfg(feature = "embedder_json")]
use crate::{MAX_PROXY_DEPTH, base::Error, denobridge::value::ProxiedV8Value, denobridge::primitives::ProxiedV8Primitive};

/// Converts a serde_json::value::JsonRawValue to a ProxiedV8Value directly
/// 
/// If `check_chars_limit` is true, will check that the total number of string characters
/// does not exceed MAX_OWNED_V8_STRING_SIZE (this is useful if the JSON is coming from
/// an untrusted source, such as over the network).
#[cfg(feature = "embedder_json")]
pub fn json_raw_to_proxied_v8(value: &JsonRawValue, check_chars_limit: bool, depth: usize) -> Result<ProxiedV8Value, Error> {
    if depth >= MAX_PROXY_DEPTH {
        return Err("Maximum proxy depth exceeded when converting JSON to ProxiedV8Value".into());
    }

    let v: JsonValue = serde_json::from_str(value.get())
        .map_err(|e| format!("Failed to parse JsonRawValue: {}", e))?;
    json_to_proxied_v8(v, check_chars_limit, depth)
}

/// Converts a serde_json::Value to a ProxiedV8Value directly
/// 
/// If `check_chars_limit` is true, will check that the total number of string characters
/// does not exceed MAX_OWNED_V8_STRING_SIZE (this is useful if the JSON is coming from
/// an untrusted source, such as over the network).
#[cfg(feature = "embedder_json")]
pub fn json_to_proxied_v8(value: JsonValue, check_chars_limit: bool, depth: usize) -> Result<ProxiedV8Value, Error> {
    if depth >= MAX_PROXY_DEPTH {
        return Err("Maximum proxy depth exceeded when converting JSON to ProxiedV8Value".into());
    }

    match value {
        JsonValue::Null => Ok(ProxiedV8Value::Primitive(ProxiedV8Primitive::Null)),
        JsonValue::Bool(b) => Ok(ProxiedV8Value::Primitive(ProxiedV8Primitive::Boolean(b))),
        JsonValue::Array(a) => {
            let mut parray = Vec::with_capacity(a.len());
            let mut num_string_chars = 0;
            for v in a {
                let pv = json_to_proxied_v8(v, check_chars_limit, depth + 1)?;

                if check_chars_limit {
                    let sz = pv.effective_size(0);
                    if sz > 0 {
                        num_string_chars += sz;
                        if num_string_chars > crate::denobridge::bridge::MAX_OWNED_V8_STRING_SIZE {
                            return Err(format!("Too many string characters in array when converting JSON to ProxiedV8Value (max {} bytes)", crate::denobridge::bridge::MAX_OWNED_V8_STRING_SIZE).into());
                        }
                    }
                }
                parray.push(pv);
            }
            Ok(ProxiedV8Value::Psuedoprimitive(crate::denobridge::psuedoprimitive::ProxiedV8PsuedoPrimitive::StaticList(parray)))
        }
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i >= (i32::MIN as i64) && i <= (i32::MAX as i64) {
                    Ok(ProxiedV8Value::Primitive(ProxiedV8Primitive::Integer(i as i32)))
                } else {
                    Ok(ProxiedV8Value::Primitive(ProxiedV8Primitive::BigInt(i)))
                }
            } else if let Some(f) = n.as_f64() {
                Ok(ProxiedV8Value::Psuedoprimitive(crate::denobridge::psuedoprimitive::ProxiedV8PsuedoPrimitive::Number(f)))
            } else {
                Err("JSON number is neither integer nor float".into())
            }
        },
        JsonValue::String(s) => {
            if s.len() > crate::denobridge::bridge::MAX_OWNED_V8_STRING_SIZE {
                return Err(format!("String too large to be a primitive (max {} bytes)", crate::denobridge::bridge::MAX_OWNED_V8_STRING_SIZE).into());
            }
            Ok(ProxiedV8Value::Primitive(ProxiedV8Primitive::String(s)))
        }
        JsonValue::Object(o) => {
            let mut smap = std::collections::HashMap::new();
            let mut s_chars = 0;
            for (k, v) in o {
                let pk = ProxiedV8Primitive::String(k);
                if check_chars_limit {
                    s_chars += pk.effective_size();
                    if s_chars > crate::denobridge::bridge::MAX_OWNED_V8_STRING_SIZE {
                        return Err(format!("Too many string characters in object keys when converting JSON to ProxiedV8Value (max {} bytes)", crate::denobridge::bridge::MAX_OWNED_V8_STRING_SIZE).into());
                    }
                }
                
                let pv = json_to_proxied_v8(v, check_chars_limit, depth + 1)?;

                if check_chars_limit {
                    s_chars += pv.effective_size(0);
                    if s_chars > crate::denobridge::bridge::MAX_OWNED_V8_STRING_SIZE {
                        return Err(format!("Too many string characters in object when converting JSON to ProxiedV8Value (max {} bytes)", crate::denobridge::bridge::MAX_OWNED_V8_STRING_SIZE).into());
                    }
                }
                smap.insert(pk, pv);
            }
            Ok(ProxiedV8Value::Psuedoprimitive(crate::denobridge::psuedoprimitive::ProxiedV8PsuedoPrimitive::StaticMap(smap)))
        },
    }
}