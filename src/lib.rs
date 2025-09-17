pub mod base;
pub mod luau;
//pub mod parallelluau;
#[cfg(feature = "deno")]
pub mod deno;

pub const MAX_PROXY_DEPTH: usize = 10;

//use mluau::LuaSerdeExt;

/*
#[allow(async_fn_in_trait)]
pub trait LuaProxySerdeExt {
    async fn from_proxied_value<T: serde::de::DeserializeOwned>(&self, value: mluau::Value) -> mluau::Result<T>;
}

impl LuaProxySerdeExt for mluau::Lua {
    async fn from_proxied_value<T: serde::de::DeserializeOwned>(&self, value: mluau::Value) -> mluau::Result<T> {
        // Special proxy logic
        match value {
            mluau::Value::UserData(ref ud) => {
                #[cfg(feature = "rquickjs")]
                {
                    if let Ok(jsop) = ud.borrow::<rquickjs::JSObjectProxy>() {
                        let json = jsop.to_json().await?;
                        return Ok(serde_json::from_value::<T>(json).map_err(|e| mluau::Error::external(e.to_string()))?);
                    }
                }
            }
            _ => {}
        }

        self.from_value(value)
    }
}*/