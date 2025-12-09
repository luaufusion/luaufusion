use crate::{luau::{LuauObjectRegistryID, bridge::{LuaBridgeServiceClient, LuauObjectOp, ObjectRegistryType, ProxyLuaClient}, embedder_api::EmbedderDataContext}, parallelluau::{ParallelLuaProxyBridge, ProxyPLuaClient, objreg::PLuauObjectRegistryID, value::ProxiedLuauValue}};
use mlua_scheduler::LuaSchedulerAsyncUserData;
use mluau::prelude::*;

/// The core struct encapsulating a host luau object proxied to the child luau state
pub enum ForeignLuauValue {
    // host-owned stuff
    Host {
        id: LuauObjectRegistryID,
        typ: ObjectRegistryType,
        plc: ProxyPLuaClient,
        bridge: LuaBridgeServiceClient<ParallelLuaProxyBridge>
    },
    Child {
        id: PLuauObjectRegistryID,
        typ: ObjectRegistryType,
        plc: ProxyLuaClient,
        bridge: ParallelLuaProxyBridge,
    },
}

impl ForeignLuauValue {
    /// Create a new ForeignLuauValue (host mode)
    pub fn new_host(id: LuauObjectRegistryID, plc: ProxyPLuaClient, bridge: LuaBridgeServiceClient<ParallelLuaProxyBridge>, typ: ObjectRegistryType) -> Self {
        Self::Host {
            id,
            typ,
            plc,
            bridge,
        }
    }

    /// Create a new ForeignLuauValue (child mode)
    pub fn new_child(id: PLuauObjectRegistryID, plc: ProxyLuaClient, bridge: ParallelLuaProxyBridge, typ: ObjectRegistryType) -> Self {
        Self::Child {
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
            match this {
                ForeignLuauValue::Host { typ, .. } => Ok(typ.type_name()),
                ForeignLuauValue::Child { typ, .. } => Ok(typ.type_name()),
            }
        });

        methods.add_scheduler_async_method("requestdispose", async move |_lua, this, ()| {
            match &*this {
                ForeignLuauValue::Host { id, bridge, .. } => {
                    bridge.opcall(
                        id.clone(),
                        LuauObjectOp::Drop,
                        vec![]
                    )
                    .await
                    .map_err(|e| mluau::Error::external(format!("Bridge call failed: {}", e)))?;
                }
                ForeignLuauValue::Child { id, bridge, .. } => {
                    bridge.send(
                        super::RequestDisposeMessage {
                            obj_id: id.clone(),
                        }
                    )
                    .await
                    .map_err(|e| mluau::Error::external(format!("Bridge call send failed: {}", e)))?
                    .map_err(|e| mluau::Error::external(format!("Bridge call failed: {}", e)))?;
                }
            }

            Ok(())
        }); 

        methods.add_scheduler_async_method("callasync", async move |lua, this, args: mluau::MultiValue| {
            match &*this {
                ForeignLuauValue::Host { id, bridge, plc, .. } => {
                    let mut p_args = Vec::with_capacity(args.len());
                    let mut ed = EmbedderDataContext::new(&plc.ed);
                    for arg in args {
                        let p_arg = ProxiedLuauValue::from_luau_child(&plc, arg, &mut ed)
                        .map_err(|e| mluau::Error::external(format!("Failed to proxy argument to ProxiedLuauValue: {}", e)))?;
                        p_args.push(p_arg);
                    }
                    let ret = bridge.opcall(
                        id.clone(),
                        LuauObjectOp::FunctionCallAsync,
                        p_args
                    )
                    .await
                    .map_err(|e| mluau::Error::external(format!("Bridge call failed: {}", e)))?;

                    let mut proxied = mluau::MultiValue::with_capacity(ret.len());
                    let mut ed = EmbedderDataContext::new(&plc.ed);
                    for v in ret {
                        let v = v.to_luau_child(&lua, &plc, &bridge, &mut ed)
                        .map_err(|e| mluau::Error::external(format!("Failed to convert return value to Lua: {}", e)))?;
                        proxied.push_back(v);
                    }

                    Ok(proxied)
                }
                ForeignLuauValue::Child { id, bridge, plc, .. } => {
                    let mut p_args = Vec::with_capacity(args.len());
                    let mut ed = EmbedderDataContext::new(&plc.ed);
                    for arg in args {
                        let p_arg = ProxiedLuauValue::from_luau_host(&plc, arg, &mut ed)
                        .map_err(|e| mluau::Error::external(format!("Failed to proxy argument to ProxiedLuauValue: {}", e)))?;
                        p_args.push(p_arg);
                    }
                    let ret = bridge.send(
                        super::CallFunctionChildMessage {
                            obj_id: id.clone(),
                            args: p_args,
                        }
                    )
                    .await
                    .map_err(|e| mluau::Error::external(format!("Bridge call send failed: {}", e)))?
                    .map_err(|e| mluau::Error::external(format!("Bridge call failed: {}", e)))?;

                    let mut proxied = mluau::MultiValue::with_capacity(ret.len());
                    let mut ed = EmbedderDataContext::new(&plc.ed);
                    for v in ret {
                        let v = v.to_luau_host(&lua, plc, bridge, &mut ed)
                        .map_err(|e| mluau::Error::external(format!("Failed to convert return value to Lua: {}", e)))?;
                        proxied.push_back(v);   
                    }

                    Ok(proxied)
                }
            }
        }); 
    }
}