use crate::luau::embedder_api::EmbedderDataContext;

/// Place this inside a Lua state to convert UserData to MessageEvent
/// if you desire extra logic here as an embedder
/// 
/// It is the embedders job to check lengths. The converted value will not be considered
/// in any length checks.
pub struct LuauUserDataBinaryConverter(
    pub fn(mluau::AnyUserData, &mluau::Lua) -> mluau::Result<serde_bytes::ByteBuf>,
);

/// Place this inside a Lua state to convert UserData to MessageEvent
/// if you desire extra logic here as an embedder
/// 
/// It is the embedders job to check lengths. The converted value will not be considered
/// in any length checks.
pub struct LuauUserDataTextConverter(
    pub fn(mluau::AnyUserData, &mluau::Lua) -> mluau::Result<String>,
);

/// Converts a Luau value to a byte buffer for sending as a message
pub(super) fn from_lua_binary(value: mluau::Value, lua: &mluau::Lua, ed: &mut EmbedderDataContext) -> mluau::Result<serde_bytes::ByteBuf> {
    match value {
        mluau::Value::String(s) => {
            ed.add(s.as_bytes().len(), "MessageEvent::Binary")
                .map_err(|e| mluau::Error::external(e.to_string()))?;

            let bytes = s.as_bytes();
            Ok(bytes.to_vec().into())
        },
        mluau::Value::UserData(ud) => {
            match lua.try_app_data_ref::<LuauUserDataBinaryConverter>()
                .map_err(|e| mluau::Error::external(format!("failed to get LuauUserDataBinaryConverter from app data: {}", e)))? {
                Some(converter) => {
                    let bytes = (converter.0)(ud, lua)?;
                    Ok(bytes)
                },
                None => {
                    Err(mluau::Error::FromLuaConversionError {
                        from: "userdata",
                        to: "MessageEvent::Binary".to_string(),
                        message: Some("No LuauUserDataBinaryConverter found to convert this userdata".to_string()),
                    })
                }
            }
        },
        mluau::Value::Buffer(b) => {
            ed.add(b.len(), "MessageEvent::Binary")
                .map_err(|e| mluau::Error::external(e.to_string()))?;

            let bytes = b.to_vec();
            Ok(bytes.into())
        },
        _ => Err(mluau::Error::FromLuaConversionError {
            from: value.type_name(),
            to: "MessageEvent".to_string(),
            message: Some("Expected string, userdata or buffer containing bytes".to_string()),
        }),
    }
}

/// Converts a Luau value to a string for sending as a message
pub(super) fn from_lua_text(value: mluau::Value, lua: &mluau::Lua, ed: &mut EmbedderDataContext) -> mluau::Result<String> {
    match value {
        mluau::Value::String(s) => {
            ed.add(s.as_bytes().len(), "MessageEvent::Text")
                .map_err(|e| mluau::Error::external(e.to_string()))?;

            let s = s.to_string_lossy();
            Ok(s)
        },
        mluau::Value::UserData(ud) => {
            match lua.try_app_data_ref::<LuauUserDataTextConverter>()
                .map_err(|e| mluau::Error::external(format!("failed to get LuauUserDataTextConverter from app data: {}", e)))? {
                Some(converter) => {
                    let s = (converter.0)(ud, lua)?;
                    Ok(s)
                },
                None => {
                    Err(mluau::Error::FromLuaConversionError {
                        from: "userdata",
                        to: "MessageEvent::Text".to_string(),
                        message: Some("No LuauUserDataTextConverter found to convert this userdata".to_string()),
                    })
                }
            }
        },
        _ => Err(mluau::Error::FromLuaConversionError {
            from: value.type_name(),
            to: "MessageEvent".to_string(),
            message: Some("Expected String or UserData containing a string".to_string()),
        }),
    }
}
