use crate::{luau::{LuauObjectRegistryID, bridge::{LuaBridgeServiceClient, LuauObjectOp, ObjectRegistryType}, embedder_api::EmbedderDataContext}, parallelluau::{ParallelLuaProxyBridge, ProxyPLuaClient, value::ProxiedLuauValue}};
use mlua_scheduler::LuaSchedulerAsyncUserData;
use mluau::prelude::*;

/// The core struct encapsulating a host luau object proxied to the child luau state
pub struct ForeignLuauValue {
    pub id: LuauObjectRegistryID,
    pub typ: ObjectRegistryType,
    plc: ProxyPLuaClient,
    bridge: LuaBridgeServiceClient<ParallelLuaProxyBridge>

}

impl Drop for ForeignLuauValue {
    fn drop(&mut self) {
        self.bridge.fire_request_disposes(vec![self.id]);
    }
}

impl ForeignLuauValue {
    /// Create a new ForeignLuauValue (host mode)
    pub fn new(id: LuauObjectRegistryID, plc: ProxyPLuaClient, bridge: LuaBridgeServiceClient<ParallelLuaProxyBridge>, typ: ObjectRegistryType) -> Self {
        Self {
            id,
            typ,
            plc,
            bridge,
        }
    }
}

// Empty impl for now for userdata
impl LuaUserData for ForeignLuauValue {
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("typename", |_, this, ()| {
            Ok(this.typ.type_name())
        });

        methods.add_scheduler_async_method("requestdispose", async move |_lua, this, ()| {
            this.bridge.request_dispose(
                this.id,
            )
            .await
            .map_err(|e| mluau::Error::external(format!("Bridge call failed: {}", e)))?;
            
            Ok(())
        }); 

        methods.add_scheduler_async_method("callasync", async move |lua, this, args: mluau::MultiValue| {
            let mut p_args = Vec::with_capacity(args.len());
            let mut ed = EmbedderDataContext::new(this.plc.ed);
            for arg in args {
                let p_arg = ProxiedLuauValue::from_luau_child(&this.plc, arg, &mut ed)
                .map_err(|e| mluau::Error::external(format!("Failed to proxy argument to ProxiedLuauValue: {}", e)))?;
                p_args.push(p_arg);
            }
            let ret = this.bridge.opcall(
                this.id,
                LuauObjectOp::FunctionCallAsync,
                p_args
            )
            .await
            .map_err(|e| mluau::Error::external(format!("Bridge call failed: {}", e)))?;

            let mut proxied = mluau::MultiValue::with_capacity(ret.len());
            let mut ed = EmbedderDataContext::new(this.plc.ed);
            for v in ret {
                let v = v.to_luau_child(&lua, &this.plc, &this.bridge, &mut ed)
                .map_err(|e| mluau::Error::external(format!("Failed to convert return value to Lua: {}", e)))?;
                proxied.push_back(v);
            }

            Ok(proxied)
        }); 
    }
}