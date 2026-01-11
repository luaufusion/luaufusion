use crate::{base::{ClientMessage, ServerMessage, ServerTransport}, luau::{embedder_api::EmbedderData, event::{from_lua_binary, from_lua_text}}};
use mlua_scheduler::LuaSchedulerAsyncUserData;

/// Common userdata for language bridges
pub struct LangBridge<T: ServerTransport> {
    bridge: T,
    ed: EmbedderData,
}

impl<T: ServerTransport> LangBridge<T> {
    /// Creates a new language bridge
    pub fn new(bridge: T, ed: EmbedderData) -> Self {
        Self { bridge, ed }
    }

    pub fn bridge(&self) -> &T {
        &self.bridge
    }
}

impl<T: ServerTransport> mluau::UserData for LangBridge<T> {
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        // For convenience, expose null
        methods.add_method("null", |_, _, ()| {
            Ok(mluau::Value::NULL)
        });

        methods.add_method("sendtext", move |lua, this, msg: mluau::Value| {
            let v = from_lua_text(msg, &lua, this.ed)
                .map_err(|e| mluau::Error::external(format!("Failed to convert argument to string: {}", e)))?;
            this.bridge.send_to_foreign(ServerMessage::SendText { msg: v })
                .map_err(|e| mluau::Error::external(format!("Failed to send text message: {}", e)))?;
            Ok(())
        });

        methods.add_method("sendbinary", move |lua, this, msg: mluau::Value| {
            let v = from_lua_binary(msg, &lua, this.ed)
                .map_err(|e| mluau::Error::external(format!("Failed to convert argument to binary: {}", e)))?;
            this.bridge.send_to_foreign(ServerMessage::SendBinary { msg: v })
                .map_err(|e| mluau::Error::external(format!("Failed to send binary message: {}", e)))?;
            Ok(())
        });

        methods.add_scheduler_async_method("receive", async move |lua, this, _: ()| {
            let msg = this.bridge.receive_from_foreign().await
                .map_err(|e| mluau::Error::external(format!("Failed to receive text message: {}", e)))?;
            match msg {
                ClientMessage::SendText { msg } => {
                    let s = lua.create_string(&msg)?;
                    Ok(mluau::Value::String(s))
                },
                ClientMessage::SendBinary { msg } => {
                    let buf = lua.create_buffer(msg)?;
                    Ok(mluau::Value::Buffer(buf))
                },
            }
        });

        methods.add_scheduler_async_method("shutdown", async move |_, this, ()| {
            this.bridge.shutdown().await
                .map_err(|e| mluau::Error::external(format!("Failed to shutdown foreign language bridge: {}", e)))
        });

        methods.add_method("isshutdown", |_, this, ()| {
            Ok(this.bridge.is_shutdown())
        });
    }
}

impl<T: ServerTransport> Drop for LangBridge<T> {
    fn drop(&mut self) {
        // Ensure the bridge is shutdown on drop
        if !self.bridge.is_shutdown() {
            self.bridge.fire_shutdown();
        }
    }
}