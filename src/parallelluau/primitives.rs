use serde::{Deserialize, Serialize};
use crate::{base::Error, luau::embedder_api::EmbedderDataContext};

#[derive(Serialize, Deserialize, PartialEq, Eq, Hash, Clone, Debug)]
/// A primitive value that is primitive across two luau vms (immutable and can be cloned between two luau states)
/// 
/// Note that primitives do not have to be cheap to clone (for example, strings are primitives but can be
/// fairly large). However, they must be immutable and clonable (see psuedoprimitives for types that are cloneable but may 
/// not be immutable on either side of the proxy bridge such as non-UTF-8 strings and vectors).
/// 
/// Only primitives can be used with object.getProperty etc.
pub enum ProxiedLuauPrimitive {
    Null,
    Nil,
    Boolean(bool),
    Integer(i64),
    String(String),
}

impl ProxiedLuauPrimitive {
    /// Luau -> ProxiedLuauPrimitive
    pub(crate) fn from_luau(value: &mluau::Value, ed: &mut EmbedderDataContext) -> Result<Option<Self>, Error> {
        ed.add(1, "ProxiedLuauPrimitive -> <base overhead>")?;
        if value.is_null() {
            return Ok(Some(ProxiedLuauPrimitive::Null));
        }
        match value {
            mluau::Value::Nil => Ok(Some(ProxiedLuauPrimitive::Nil)),
            mluau::Value::Boolean(b) => Ok(Some(ProxiedLuauPrimitive::Boolean(*b))),
            mluau::Value::LightUserData(s) => Ok(Some(ProxiedLuauPrimitive::Integer(s.0 as i64))),
            mluau::Value::Integer(i) => Ok(Some(ProxiedLuauPrimitive::Integer(*i))),
            mluau::Value::String(s) => {
                ed.add(s.as_bytes().len(), "ProxiedLuauPrimitive -> LuaString")?;

                if let Ok(s) = s.to_str() {
                    return Ok(Some(ProxiedLuauPrimitive::String(s.to_string())));
                }

                Ok(None) // Not valid UTF-8, so not a primitive
            },
            mluau::Value::Other(r) => {
                let s = format!("unknown({r:?})");
                ed.add(s.len(), "ProxiedLuauPrimitive -> LuaOther")?;
                Ok(Some(ProxiedLuauPrimitive::String(s)))
            },
            _ => Ok(None),
        }
    }

    /// ProxiedLuauPrimitive -> Luau
    pub(crate) fn to_luau(self, lua: &mluau::Lua, ed: &mut EmbedderDataContext) -> Result<mluau::Value, Error> {
        ed.add(1, "ProxiedLuauPrimitive -> <base overhead>")?;
        match self {
            ProxiedLuauPrimitive::Null => Ok(mluau::Value::NULL),
            ProxiedLuauPrimitive::Nil => Ok(mluau::Value::Nil),
            ProxiedLuauPrimitive::Boolean(b) => Ok(mluau::Value::Boolean(b)),
            ProxiedLuauPrimitive::Integer(i) => Ok(mluau::Value::Integer(i as i64)),
            ProxiedLuauPrimitive::String(s) => {
                ed.add(s.len(), "ProxiedLuauPrimitive -> LuaString")?;
                let s = lua.create_string(s)
                .map_err(|e| format!("Failed to create Lua string: {}", e))?;
                Ok(mluau::Value::String(s))
            }
        }
    }
}
