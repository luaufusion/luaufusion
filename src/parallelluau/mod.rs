use std::{collections::HashMap, ops::Deref, rc::Rc, sync::Arc};

use concurrentlyexec::{ConcurrentExecutor, ConcurrentExecutorState, ConcurrentlyExecute, MultiSender, OneshotSender, ProcessOpts};
use mlua_scheduler::{ReturnTracker, TaskManager, taskmgr::NoopHooks};
use mluau::LuaSerdeExt;
use serde::{Deserialize, Serialize};

use crate::{base::ProxyBridge, luau::{bridge::{LuaBridgeMessage, LuaBridgeService, LuaBridgeServiceClient, ProxyLuaClient}, embedder_api::{EmbedderData, EmbedderDataContext}}, parallelluau::{objreg::{PLuauObjectRegistryID, PObjRegistryLuau}, value::ProxiedLuauValue}};

// NOT WORKING YET: TO BE IMPLEMENTED LATER
mod objreg;
mod primitives;
mod psuedoprimitive;
mod value;
mod foreignref;
//use crate::{base::{ObjectRegistryID, ProxyBridge}, luau::bridge::{LuaBridgeObject, ProxyLuaClient}};

const MIN_HEAP_LIMIT: usize = 5 * 1024 * 1024; // 5 MB

#[derive(Clone)]
pub struct ParallelLuaClient {}

#[derive(Serialize, Deserialize)]
/// Internal representation of a message that can be sent to the parallel luau instance
pub(super) enum ParallelLuaMessage {
    CodeExec {
        modname: String,
        resp: OneshotSender<Result<ProxiedLuauValue, String>>,
    },
    CallFunctionChild {
        obj_id: PLuauObjectRegistryID,
        args: Vec<ProxiedLuauValue>,
        resp: OneshotSender<Result<Vec<ProxiedLuauValue>, String>>,
    },
    RequestDispose {
        obj_id: PLuauObjectRegistryID,
        resp: OneshotSender<Result<(), String>>,
    },
    Shutdown,
}

/// A message that can be sent to the Luau parallel process which can produce a response
pub(super) trait LuauSendableMessage: Send + 'static {
    type Response: Send + serde::Serialize + for<'de> serde::Deserialize<'de> + 'static;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> ParallelLuaMessage;
}

pub(super) struct CodeExecMessage {
    pub modname: String,
}

impl LuauSendableMessage for CodeExecMessage {
    type Response = Result<ProxiedLuauValue, String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> ParallelLuaMessage {
        ParallelLuaMessage::CodeExec {
            modname: self.modname,
            resp,
        }
    }
}

pub struct CallFunctionChildMessage {
    pub obj_id: PLuauObjectRegistryID,
    pub args: Vec<ProxiedLuauValue>,
}

impl LuauSendableMessage for CallFunctionChildMessage {
    type Response = Result<Vec<ProxiedLuauValue>, String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> ParallelLuaMessage {
        ParallelLuaMessage::CallFunctionChild {
            obj_id: self.obj_id,
            args: self.args,
            resp,
        }
    }
}

pub struct RequestDisposeMessage {
    pub obj_id: PLuauObjectRegistryID,
}

impl LuauSendableMessage for RequestDisposeMessage {
    type Response = Result<(), String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> ParallelLuaMessage {
        ParallelLuaMessage::RequestDispose {
            obj_id: self.obj_id,
            resp,
        }
    }
}

pub(super) struct ShutdownMessage;

impl LuauSendableMessage for ShutdownMessage {
    type Response = ();
    fn to_message(self, _resp: OneshotSender<Self::Response>) -> ParallelLuaMessage {
        ParallelLuaMessage::Shutdown
    }
}

#[derive(Serialize, Deserialize)]
pub struct LuauBootstrapData {
    ed: EmbedderData,
    messenger_tx: OneshotSender<MultiSender<ParallelLuaMessage>>,
    lua_bridge_tx: MultiSender<LuaBridgeMessage<ParallelLuaProxyBridge>>,
    vfs: HashMap<String, String>,
}

#[derive(Clone)]
pub(crate) struct CommonState {
    pub(super) bridge: LuaBridgeServiceClient<ParallelLuaProxyBridge>,
    pub(super) proxy_client: ProxyPLuaClient,
    pub(super) ed: EmbedderData,
}

impl ConcurrentlyExecute for ParallelLuaClient {
    type BootstrapData = LuauBootstrapData;

    async fn run(
        data: Self::BootstrapData,
        client_ctx: concurrentlyexec::ClientContext
    ) {
        let (tx, mut rx) = client_ctx.multi();
        data.messenger_tx.client(&client_ctx).send(tx).unwrap();

        let heap_limit = data.ed.heap_limit.max(MIN_HEAP_LIMIT);
        let lua = mluau::Lua::new_with(
            mluau::StdLib::ALL,
            mluau::LuaOptions::default()
            .catch_rust_panics(true)
            .disable_error_userdata(true)
        )
        .expect("Failed to create Luau state");

        let scheduler = TaskManager::new(
            &lua,
            ReturnTracker::new(),
            Rc::new(NoopHooks {}),
        )
        .await
        .expect("Failed to create task manager");

        lua.globals().set("task", mlua_scheduler::userdata::task_lib(&lua).expect("Failed to create task library"))
        .expect("Failed to set task library");

        lua.set_memory_limit(heap_limit)
        .expect("Failed to set Luau memory limit");

        // TODO: Some way of post-init

        lua.sandbox(true)
        .expect("Failed to sandbox Luau state");

        let lbsc = LuaBridgeServiceClient::new(client_ctx.clone(), data.lua_bridge_tx);

        let common_state = CommonState {
            bridge: lbsc,
            proxy_client: ProxyPLuaClient {
                //weak_lua: lua.weak(),
                array_mt: lua.array_metatable(),
                obj_registry: PObjRegistryLuau::new(&lua).expect("Failed to create Luau object registry"),
                ed: data.ed.clone(),
            },
            ed: data.ed.clone(),
        };

        loop {
            tokio::select! {
                Ok(msg) = rx.recv() => {
                    match msg {
                        ParallelLuaMessage::CodeExec { modname, resp } => {
                            todo!() // TODO: Implement module loading and execution
                        }
                        ParallelLuaMessage::CallFunctionChild { obj_id, args, resp } => {
                            let value = match common_state.proxy_client.obj_registry.get(obj_id) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to get object for CallFunctionChild: {}", e)));
                                    continue;
                                }
                            };

                            let func = match value {
                                mluau::Value::Function(f) => f,
                                _ => {
                                    let _ = resp.client(&client_ctx).send(Err("Object is not a function".to_string()));
                                    continue;
                                }
                            };

                            // Convert args to LuaMultiValue
                            let mut lua_args = mluau::MultiValue::with_capacity(args.len());
                            let mut ed = EmbedderDataContext::new(&common_state.ed);
                            let mut err = None;
                            for arg in args {
                                let lua_val = match arg.to_luau_child(&lua, &common_state.proxy_client, &common_state.bridge, &mut ed) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        err = Some(format!("Failed to convert argument to Lua value: {}", e));
                                        break;
                                    }
                                };
                                lua_args.push_back(lua_val);
                            }

                            if let Some(e) = err {
                                let _ = resp.client(&client_ctx).send(Err(e));
                                continue;
                            }

                            let th = match lua.create_thread(func) {
                                Ok(t) => t,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to create thread for function call: {}", e)));
                                    continue;
                                }
                            };

                            let scheduler_ref = scheduler.clone();
                            let proxy_client_ref = common_state.proxy_client.clone();
                            let client_ctx_ref = client_ctx.clone();
                            tokio::task::spawn_local(async move {
                                let res = match scheduler_ref.spawn_thread_and_wait(th, lua_args).await {
                                    Ok(Some(v)) => v,
                                    Ok(None) => {
                                        let _ = resp.client(&client_ctx_ref).send(Err("Function yielded unexpectedly".to_string()));
                                        return;
                                    },
                                    Err(e) => {
                                        let _ = resp.client(&client_ctx_ref).send(Err(format!("Function call failed: {}", e)));
                                        return;
                                    }
                                };

                                let ret = match res {
                                    Ok(v) => v,
                                    Err(e) => {
                                        let _ = resp.client(&client_ctx_ref).send(Err(format!("Function call error: {}", e)));
                                        return;
                                    }
                                };

                                // Convert return values to ProxiedLuauValue
                                let mut proxied_rets = Vec::with_capacity(ret.len());
                                for rv in ret {
                                    let proxied = match ProxiedLuauValue::from_luau_child(&proxy_client_ref, rv, &mut ed) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            let _ = resp.client(&client_ctx_ref).send(Err(format!("Failed to convert return value to ProxiedLuauValue: {}", e)));
                                            return;
                                        }
                                    };
                                    proxied_rets.push(proxied);
                                }

                                let _ = resp.client(&client_ctx_ref).send(Ok(proxied_rets));
                            });
                        },
                        ParallelLuaMessage::RequestDispose { obj_id, resp } => {
                            match common_state.proxy_client.obj_registry.remove(obj_id).map_err(|e| e.to_string()) {
                                Ok(_) => {
                                    let _ = resp.client(&client_ctx).send(Ok(()));
                                },
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to dispose object: {}", e)));
                                }
                            };
                        },
                        ParallelLuaMessage::Shutdown => {
                            break;
                        },
                    }
                }
            }
        }
    }
}

pub struct ParallelLuaProxyBridgeInner {
    executor: Arc<ConcurrentExecutor<ParallelLuaClient>>,
    messenger: Arc<MultiSender<ParallelLuaMessage>>,
}

#[derive(Clone)]
pub struct ParallelLuaProxyBridge {
    inner: Rc<ParallelLuaProxyBridgeInner>,
}

impl ParallelLuaProxyBridge {
    /// Create a new ParallelLuaProxyBridge
    async fn new(
        cs_state: ConcurrentExecutorState<ParallelLuaClient>, 
        ed: EmbedderData, 
        process_opts: ProcessOpts,
        plc: ProxyLuaClient,
        vfs: HashMap<String, String>,
    ) -> Result<Self, crate::base::Error> {
        let (executor, (lua_bridge_rx, ms_rx)) = ConcurrentExecutor::new(
            cs_state,
            process_opts,
            move |cei| {
                let (tx, rx) = cei.create_multi();
                let (msg_tx, msg_rx) = cei.create_oneshot();
                (LuauBootstrapData {
                    ed,
                    messenger_tx: msg_tx,
                    lua_bridge_tx: tx,
                    vfs
                }, (rx, msg_rx))
            }
        ).await.map_err(|e| format!("Failed to create parallel luau executor: {}", e))?;
        let messenger = ms_rx.recv().await.map_err(|e| format!("Failed to receive messenger: {}", e))?;

        let self_ret = Self { 
            inner: Rc::new(ParallelLuaProxyBridgeInner {
                executor: Arc::new(executor), 
                messenger: Arc::new(messenger)
            })
         };
        let self_ref = self_ret.clone();
        tokio::task::spawn_local(async move {
            let lua_bridge_service = LuaBridgeService::new(
                self_ref,
                lua_bridge_rx,
            );
            lua_bridge_service.run(plc).await;
        });

        Ok(self_ret)
    }

    /// Send a message to the parallel luau process and wait for a response
    pub(super) async fn send<T: LuauSendableMessage>(&self, msg: T) -> Result<T::Response, crate::base::Error> {
        let (resp_tx, resp_rx) = self.executor.create_oneshot();
        self.messenger.server(self.executor.server_context()).send(msg.to_message(resp_tx))
            .map_err(|e| format!("Failed to send message to parallel luau: {}", e))?;
        
        let resp = resp_rx.recv().await;

        if self.is_shutdown() {
            return Err("Parallel luau is shut down (likely due to timeout)".into());
        }
        
        resp.map_err(|e| format!("Failed to receive response from parallel luau: {}", e).into())
    }
}

impl Deref for ParallelLuaProxyBridge {
    type Target = ParallelLuaProxyBridgeInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl ProxyBridge for ParallelLuaProxyBridge {
    type ValueType = ProxiedLuauValue;
    type ConcurrentlyExecuteClient = ParallelLuaClient;

    fn name() -> &'static str {
        "parallelluau"
    }

    async fn new(
        cs_state: ConcurrentExecutorState<Self::ConcurrentlyExecuteClient>, 
        ed: crate::luau::embedder_api::EmbedderData, 
        process_opts: concurrentlyexec::ProcessOpts,
        plc: crate::luau::bridge::ProxyLuaClient,
        vfs: std::collections::HashMap<String, String>,
    ) -> Result<Self, crate::base::Error> {
        Self::new(cs_state, ed, process_opts, plc, vfs).await
    }

    fn get_executor(&self) -> std::sync::Arc<ConcurrentExecutor<Self::ConcurrentlyExecuteClient>> {
        self.executor.clone()
    }

    fn to_source_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, plc: &ProxyLuaClient) -> Result<mluau::Value, crate::base::Error> {
        let mut ed = EmbedderDataContext::new(&plc.ed);
        Ok(value.to_luau_host(lua, plc, self, &mut ed).map_err(|e| e.to_string())?)
    }

    fn from_source_lua_value(&self, _lua: &mluau::Lua, plc: &ProxyLuaClient, value: mluau::Value, ed: &mut EmbedderDataContext) -> Result<Self::ValueType, crate::base::Error> {
        Ok(ProxiedLuauValue::from_luau_host(plc, value, ed).map_err(|e| e.to_string())?)
    }

    async fn eval_from_source(&self, modname: String) -> Result<Self::ValueType, crate::base::Error> {
        Ok(self.send(CodeExecMessage { modname }).await??)
    }

    async fn shutdown(&self) -> Result<(), crate::base::Error> {
        let _ = self.send(ShutdownMessage).await;
        self.executor.shutdown().await?;
        self.executor.wait().await?;
        Ok(())
    }

    fn is_shutdown(&self) -> bool {
        self.executor.get_state().cancel_token.is_cancelled()
    }
}

#[derive(Clone)]
/// The client side state for proxying Lua values
/// 
/// This struct is not thread safe and must be kept on the Lua side
pub struct ProxyPLuaClient {
    pub array_mt: mluau::Table,
    pub obj_registry: PObjRegistryLuau,
    pub ed: EmbedderData,
}

