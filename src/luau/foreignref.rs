use mlua_scheduler::LuaSchedulerAsyncUserData;
use crate::base::{Error, StandardProxyBridge};
use crate::luau::bridge::ProxyLuaClient;
use crate::luau::embedder_api::EmbedderDataContext;

/// A struct encapsulating a foreign object being proxied *to* host luau (child object handle on luau, e.g. v8value being proxied to luau or parallel luau value being proxied to host luau)
pub struct ForeignRef<T: StandardProxyBridge> {
    pub id: T::ObjectRegistryID,
    pub typ: T::ObjectRegistryType,
    pub plc: ProxyLuaClient,
    pub bridge: T,
}

impl<T: StandardProxyBridge> Drop for ForeignRef<T> {
    fn drop(&mut self) {
        if cfg!(feature = "debug_message_print_enabled") {
            println!("ForeignRef ({}) of type {} with ID {} dropped", std::any::type_name::<T>(), self.typ.into(), self.id.into());
        }
        self.bridge.fire_request_disposes(vec![self.id]);
    }
}

impl<T: StandardProxyBridge> ForeignRef<T> {
    /// Create a new V8Value
    pub fn new(id: T::ObjectRegistryID, plc: ProxyLuaClient, bridge: T, typ: T::ObjectRegistryType) -> Self {
        Self {
            id,
            typ,
            plc,
            bridge,
        }
    }

    async fn get_property(&self, lua: &mluau::Lua, key: mluau::Value) -> Result<mluau::Value, Error> {
        let mut ed = EmbedderDataContext::new(self.plc.ed);
        let key_proxied = self.bridge.from_source_lua_value(lua, &self.plc, key, &mut ed)
        .map_err(|e| format!("Failed to proxy argument to ProxiedValue: {}", e))?;
        match self.bridge.get_property(self.id, key_proxied)
        .await {
            Ok(v) => {
                let ret = self.bridge.to_source_lua_value(lua, v, &self.plc) 
                .map_err(|e| format!("Failed to convert returned property to Lua: {}", e))?;

                Ok(ret)
            },
            Err(e) => Err(e.into()),
        }
    }

    async fn function_call(&self, lua: &mluau::Lua, args: mluau::MultiValue) -> Result<mluau::MultiValue, Error> {
        let mut ed = EmbedderDataContext::new(self.plc.ed);
        
        let mut args_proxied = Vec::with_capacity(args.len());
        for arg in args {
            let proxied = self.bridge.from_source_lua_value(lua, &self.plc, arg, &mut ed)
            .map_err(|e| format!("Failed to proxy argument to ProxiedValue: {}", e))?;
            args_proxied.push(proxied);
        }
        match self.bridge.function_call(self.id, args_proxied)
        .await {
            Ok(v) => {
                let mut proxied = mluau::MultiValue::with_capacity(v.len());
                for ret in v {
                    let ret = self.bridge.to_source_lua_value(lua, ret, &self.plc) 
                    .map_err(|e| format!("Failed to convert return value to Lua: {}", e))?;
                    proxied.push_back(ret);
                }
                Ok(proxied)
            },
            Err(e) => Err(e.into()),
        }
    }

    async fn request_dispose(&self) -> Result<(), Error> {
        self.bridge.request_dispose(self.id).await
    }

    fn objid(&self) -> i64 {
        Into::<i64>::into(self.id)
    }

    fn objtype(&self) -> &'static str {
        Into::<&'static str>::into(self.typ)
    }
}

impl<T: StandardProxyBridge> mluau::UserData for ForeignRef<T> {
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_scheduler_async_method("requestdispose", async move |_lua, this, ()| {
            this.request_dispose().await
            .map_err(|e| mluau::Error::external(format!("Failed to request dispose: {}", e)))?;
            Ok(())
        }); 

        methods.add_method("id", |_, this, ()| {
            Ok(this.objid())
        });

        // Underlying JS type name
        methods.add_method("typename", |_, this, ()| {
            Ok(this.objtype())
        });

        methods.add_scheduler_async_method("call", async move |lua, this, args: mluau::MultiValue| {
            let resp = this.function_call(&lua, args).await
            .map_err(|e| mluau::Error::external(format!("Failed to call function: {}", e)))?;

            Ok(resp)
        });

        methods.add_scheduler_async_method("getproperty", async move |lua, this, key: mluau::Value| {        
            let resp = this.get_property(&lua, key).await
            .map_err(|e| mluau::Error::external(format!("Failed to get property: {}", e)))?;

            Ok(resp)
        });
    }
}
