use std::collections::HashMap;

use crate::{base::{ProxyBridge, ShutdownTimeouts}, luau::{bridge::ProxyLuaClient, embedder_api::{EmbedderData, EmbedderDataContext}, event::{from_lua_binary, from_lua_text}}};
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

    /// Creates a new language bridge from Luau state and other parameters
    pub async fn new_from_bridge(
        lua: &mluau::Lua,
        ed: EmbedderData,
        process_opts: ProcessOpts,
        cs_state: ConcurrentExecutorState<T::ConcurrentlyExecuteClient>,
        vfs: HashMap<String, String>,
        from_luau_shutdown_timeouts: ShutdownTimeouts
    ) -> Result<Self, crate::base::Error> {
        let plc = ProxyLuaClient::new(lua, ed)
            .map_err(|e| format!("Failed to create ProxyLuaClient: {}", e))?;
        let bridge_vals = T::new(
            cs_state,
            ed,
            process_opts,
            vfs
        ).await?;

        Ok(Self {
            bridge: bridge_vals,
            plc,
            from_luau_shutdown_timeouts,
        })
    }

    pub async fn run(&self, modname: String) -> Result<(), mluau::Error> {
        self.bridge.eval_from_source(modname).await
            .map_err(|e| mluau::Error::external(e.to_string()))?;
        
        Ok(())
    }
}

impl<T: ProxyBridge> mluau::UserData for LangBridge<T> {
    fn add_fields<F: mluau::UserDataFields<Self>>(fields: &mut F) {
        fields.add_field("lang", T::name());
    }
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        // For convenience, expose null
        methods.add_method("null", |_, _, ()| {
            Ok(mluau::Value::NULL)
        });

        methods.add_scheduler_async_method("run", async move |_lua, this, modname: String| {
            this.run(modname).await
        });

        methods.add_scheduler_async_method("sendtext", async move |lua, this, msg: mluau::Value| {
            let mut ed = EmbedderDataContext::new(this.plc.ed);
            let v = from_lua_text(msg, &lua, &mut ed)
                .map_err(|e| mluau::Error::external(format!("Failed to convert argument to string: {}", e)))?;
            this.bridge.send_text(v)
                .await
                .map_err(|e| mluau::Error::external(format!("Failed to send text message: {}", e)))?;
            Ok(())
        });

        methods.add_scheduler_async_method("sendbinary", async move |lua, this, msg: mluau::Value| {
            let mut ed = EmbedderDataContext::new(this.plc.ed);
            let v = from_lua_binary(msg, &lua, &mut ed)
                .map_err(|e| mluau::Error::external(format!("Failed to convert argument to binary: {}", e)))?;
            this.bridge.send_binary(v)
                .await
                .map_err(|e| mluau::Error::external(format!("Failed to send binary message: {}", e)))?;
            Ok(())
        });

        methods.add_scheduler_async_method("receivetext", async move |_lua, this, _: ()| {
            let msg = this.bridge.receive_text().await
                .map_err(|e| mluau::Error::external(format!("Failed to receive text message: {}", e)))?;
            Ok(msg)
        });

        methods.add_scheduler_async_method("receivebinary", async move |lua, this, _: ()| {
            let msg = this.bridge.receive_binary().await
                .map_err(|e| mluau::Error::external(format!("Failed to receive text message: {}", e)))?;
            let buf = match msg {
                Some(msg) => mluau::Value::Buffer(lua.create_buffer(msg.into_vec())?),
                None => mluau::Value::Nil,
            };
            Ok(buf)
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

impl<T: ProxyBridge> Drop for LangBridge<T> {
    fn drop(&mut self) {
        // Ensure the bridge is shutdown on drop
        if !self.bridge.is_shutdown() {
            self.bridge.fire_shutdown();
        }
    }
}