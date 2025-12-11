use std::collections::HashMap;

use crate::{base::{ProxyBridge, ShutdownTimeouts}, luau::{bridge::ProxyLuaClient, embedder_api::{EmbedderData, EmbedderDataContext}}};
use concurrentlyexec::{ConcurrentExecutorState, ProcessOpts};
use mlua_scheduler::LuaSchedulerAsyncUserData;

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
    pub fn from_luau_value(
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
            let result = this.bridge.eval_from_source(modname).await
                .map_err(|e| mluau::Error::external(e.to_string()))?;
            
            let mut mv = mluau::MultiValue::with_capacity(result.len());
            for val in result {
                mv.push_back(this.bridge.to_source_lua_value(&lua, val, &this.plc)
                    .map_err(|e| mluau::Error::external(format!("Failed to convert return value to Lua value: {}", e)))?);
            }

            Ok(mv)
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
