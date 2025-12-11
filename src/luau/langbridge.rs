use std::collections::HashMap;

use crate::{base::{ProxyBridge, ProxyBridgeWithStringExt, ShutdownTimeouts}, luau::{bridge::ProxyLuaClient, embedder_api::{EmbedderData, EmbedderDataContext}}};
use concurrentlyexec::{ConcurrentExecutorState, ProcessOpts};
use mlua_scheduler::LuaSchedulerAsyncUserData;

/// A value userdata that holds a opaque value
/// 
/// This can be used by embedders to create userdata that holds foreign language values
/// in a way that cannot be tampered with from Luau side (e.g. large events that would otherwise run into proxy bridge limits can be sent in this way)
pub struct ProxiedValue<T: ProxyBridge> {
    value: T::ValueType,
}

impl <T: ProxyBridge> ProxiedValue<T> {
    pub fn new(value: T::ValueType) -> Self {
        Self { value }
    }

    pub fn into_inner(self) -> T::ValueType {
        self.value
    }

    /// Creates a new ProxiedValue from a raw string
    pub fn from_str(s: String) -> Self 
    where T: ProxyBridgeWithStringExt,
    {
        Self::new(
            T::from_string(s)
        )
    }
}

impl<T: ProxyBridge> mluau::UserData for ProxiedValue<T> {}

/// Common userdata for language bridges
pub struct LangBridge<T: ProxyBridge> {
    bridge: T,
    plc: ProxyLuaClient,
    from_luau_shutdown_timeouts: ShutdownTimeouts,
}

impl<T: ProxyBridge> LangBridge<T> {
    /// Creates a new language bridge
    pub fn new(bridge: T, plc: ProxyLuaClient, from_luau_shutdown_timeouts: ShutdownTimeouts) -> Self {
        Self { bridge, plc, from_luau_shutdown_timeouts }
    }

    pub fn bridge(&self) -> &T {
        &self.bridge
    }

    /// Converts a Luau value to the foreign language value type
    pub fn create_luau_value(
        &self,
        lv: mluau::Value,
        disable_limits: bool,
    ) -> Result<T::ValueType, crate::base::Error> {
        let Some(lua) = self.plc.weak_lua.try_upgrade() else {
            return Err("Lua state has been dropped".into());
        };

        let mut ed = EmbedderDataContext::new(&self.plc.ed);
        if disable_limits {
            ed.disable_limits();
        }
        return self.bridge.from_source_lua_value(&lua, &self.plc, lv, &mut ed);
    }

    /// Converts a Luau value to a opaque ProxiedValue userdata
    /// that can be sent across the bridge with/without being subject to payload limits
    /// depending on the disable_limits flag
    pub fn create_proxied_value(
        &self,
        lv: mluau::Value,
        disable_limits: bool,
    ) -> Result<ProxiedValue<T>, crate::base::Error> {
        let value = self.create_luau_value(lv, disable_limits)?;
        Ok(ProxiedValue::new(value))
    }

    /// Creates a new language bridge from Luau state and other parameters
    pub async fn new_from_bridge(
        lua: &mluau::Lua,
        ed: EmbedderData,
        process_opts: ProcessOpts,
        cs_state: ConcurrentExecutorState<T::ConcurrentlyExecuteClient>,
        vfs: HashMap<String, String>,
        from_luau_shutdown_timeouts: ShutdownTimeouts
    ) -> Result<Self, crate::base::Error> {
        let plc = ProxyLuaClient::new(lua, ed.clone())
            .map_err(|e| format!("Failed to create ProxyLuaClient: {}", e))?;
        let bridge_vals = T::new(
            cs_state,
            ed,
            process_opts,
            plc.clone(),
            vfs
        ).await?;

        Ok(Self {
            bridge: bridge_vals,
            plc,
            from_luau_shutdown_timeouts,
        })
    }

    pub async fn run(&self, lua: &mluau::Lua, modname: String) -> Result<mluau::MultiValue, mluau::Error> {
        let result = self.bridge.eval_from_source(modname).await
            .map_err(|e| mluau::Error::external(e.to_string()))?;
        
        let mut mv = mluau::MultiValue::with_capacity(result.len());
        for val in result {
            mv.push_back(self.bridge.to_source_lua_value(&lua, val, &self.plc)
                .map_err(|e| mluau::Error::external(format!("Failed to convert return value to Lua value: {}", e)))?);
        }

        Ok(mv)
    }
}

impl<T: ProxyBridge> mluau::UserData for LangBridge<T> {
    fn add_fields<F: mluau::UserDataFields<Self>>(fields: &mut F) {
        fields.add_field("lang", T::name());
    }
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        // For convenience, expose the array metatable
        methods.add_method("array_metatable", |_, this, ()| {
            Ok(this.plc.array_mt.clone())
        });
        // For convenience, expose null
        methods.add_method("null", |_, _, ()| {
            Ok(mluau::Value::NULL)
        });

        methods.add_scheduler_async_method("run", async move |lua, this, modname: String| {
            this.run(&lua, modname).await
        });

        methods.add_scheduler_async_method("shutdown", async move |_, this, ()| {
            this.bridge.shutdown(this.from_luau_shutdown_timeouts).await
                .map_err(|e| mluau::Error::external(format!("Failed to shutdown foreign language bridge: {}", e)))
        });

        methods.add_method("isshutdown", |_, this, ()| {
            Ok(this.bridge.is_shutdown())
        });
    }
}
