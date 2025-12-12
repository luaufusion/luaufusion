use std::{collections::HashMap, ops::Deref, rc::Rc, sync::Arc, time::Duration};

use concurrentlyexec::{ConcurrentExecutor, ConcurrentExecutorState, ConcurrentlyExecute, MultiSender, OneshotSender, ProcessOpts};
use mlua_scheduler::{ReturnTracker, TaskManager, taskmgr::NoopHooks};
use mluau::LuaSerdeExt;
use mluau_require::AssetRequirer;
use serde::{Deserialize, Serialize};

use crate::{base::{ProxyBridge, ProxyBridgeWithMultiprocessExt, ProxyBridgeWithStringExt, ShutdownTimeouts, StandardProxyBridge}, luau::{bridge::{LuaBridgeMessage, LuaBridgeService, LuaBridgeServiceClient, ObjectRegistryType, ProxyLuaClient}, embedder_api::{EmbedderData, EmbedderDataContext}}, parallelluau::{objreg::{PLuauObjectRegistryID, PObjRegistryLuau}, primitives::ProxiedLuauPrimitive, value::ProxiedLuauValue}};

// NOT WORKING YET: TO BE IMPLEMENTED LATER
mod objreg;
mod primitives;
mod psuedoprimitive;
mod value;
mod foreignref;
//use crate::{base::{ObjectRegistryID, ProxyBridge}, luau::bridge::{LuaBridgeObject, ProxyLuaClient}};

/// Memory limit for Luau isolates
const MIN_HEAP_LIMIT: usize = 5 * 1024 * 1024; // 5 MB
/// Magic number for Parallel Luau messages
/// 
/// Helpful to avoid common user errors like running a V8 isolate manager with a parallel luau child instead
pub const PLUAU_MESSAGE_MAGIC: usize = 2;

#[derive(Clone)]
pub struct ParallelLuaClient {}

impl ParallelLuaClient {
    fn spawn_function_async(
        lua: &mluau::Lua,
        scheduler: &TaskManager,
        common_state: &CommonState,
        func: mluau::Function,
        args: Vec<ProxiedLuauValue>,
        client_ctx: &concurrentlyexec::ClientContext,
        resp: OneshotSender<Result<Vec<ProxiedLuauValue>, String>>,
    ) {
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
            return;
        }

        let th = match lua.create_thread(func) {
            Ok(t) => t,
            Err(e) => {
                let _ = resp.client(&client_ctx).send(Err(format!("Failed to create thread for function call: {}", e)));
                return;
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
    }
}

#[derive(Serialize, Deserialize)]
/// Internal representation of a message that can be sent to the parallel luau instance
pub(super) enum ParallelLuaMessage {
    CodeExec {
        filename: String,
        resp: OneshotSender<Result<Vec<ProxiedLuauValue>, String>>,
        magic: usize,
    },
    GetProperty {
        obj_id: PLuauObjectRegistryID,
        property: ProxiedLuauValue,
        resp: OneshotSender<Result<ProxiedLuauValue, String>>,
        magic: usize,
    },
    CallFunctionChild {
        obj_id: PLuauObjectRegistryID,
        args: Vec<ProxiedLuauValue>,
        resp: OneshotSender<Result<Vec<ProxiedLuauValue>, String>>,
        magic: usize,
    },
    RequestDispose {
        obj_id: PLuauObjectRegistryID,
        resp: OneshotSender<Result<(), String>>,
        magic: usize,
    },
    Shutdown,
}

/// A message that can be sent to the Luau parallel process which can produce a response
pub(super) trait LuauSendableMessage: Send + 'static {
    type Response: Send + serde::Serialize + for<'de> serde::Deserialize<'de> + 'static;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> ParallelLuaMessage;
}

pub(super) struct CodeExecMessage {
    pub filename: String,
}

impl LuauSendableMessage for CodeExecMessage {
    type Response = Result<Vec<ProxiedLuauValue>, String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> ParallelLuaMessage {
        ParallelLuaMessage::CodeExec {
            filename: self.filename,
            resp,
            magic: PLUAU_MESSAGE_MAGIC,
        }
    }
}

pub struct GetPropertyMessage {
    pub obj_id: PLuauObjectRegistryID,
    pub property: ProxiedLuauValue,
}

impl LuauSendableMessage for GetPropertyMessage {
    type Response = Result<ProxiedLuauValue, String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> ParallelLuaMessage {
        ParallelLuaMessage::GetProperty {
            obj_id: self.obj_id,
            property: self.property,
            resp,
            magic: PLUAU_MESSAGE_MAGIC,
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
            magic: PLUAU_MESSAGE_MAGIC,
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
            magic: PLUAU_MESSAGE_MAGIC,
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

        let compiler = mluau::Compiler::new()
            .set_optimization_level(2)
            .set_type_info_level(1);

        lua.set_compiler(compiler.clone());

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

        let vfs = mluau_require::create_vfs_from_map(&data.vfs)
            .expect("Failed to create VFS from map");
        let controller = AssetRequirer::new(vfs.clone(), "pluau_main_client".to_string(), lua.globals());

        lua.globals()
            .set("require", lua.create_require_function(controller).expect("Failed to create require function"))
            .expect("Failed to set require function");

        /*
        // Custom print function that writes to a table to ensure stdout isnt spammed 
        let print_table = lua.create_table().expect("Failed to create print table");
        let print_table_ref = print_table.clone();
        lua.globals().set("print", lua.create_function(move |lua, msg: mluau::MultiValue| {
            let insert_tab = lua.create_table()?;
            for v in msg.into_iter() {
                insert_tab.raw_push(v)?;
            }

            print_table_ref.raw_push(insert_tab)?;
            Ok(())
        }).expect("Failed to create print function"))
        .expect("Failed to set print function");

        lua.globals().set("printoutput", lua.create_function(move |_, ()| {
            Ok(print_table.clone())
        }).expect("Failed to create printoutput function"))
        .expect("Failed to set printoutput function");
        */

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

        let mut function_cache: HashMap<String, mluau::Function> = HashMap::new();
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Ok(ParallelLuaMessage::CodeExec { filename, resp, magic }) => {
                            if magic != PLUAU_MESSAGE_MAGIC {
                                let _ = resp.client(&client_ctx).send(Err("Invalid magic number in CodeExec message; are you sure you're running a parallel luau child process?".into()));
                                continue;
                            }

                            let code = match vfs
                            .get_file(filename.clone()) {
                                Ok(c) => c,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to get file from VFS: {}", e)));
                                    continue;
                                }
                            };

                            let func = if let Some(f) = function_cache.get(&filename) {
                                f.clone() // f is cheap to clone
                            } else {
                                let bytecode = match compiler.compile(code) {
                                    Ok(bc) => bc,
                                    Err(e) => {
                                        let _ = resp.client(&client_ctx).send(Err(format!("Failed to compile Luau code: {}", e)));
                                        continue;
                                    }
                                };

                                let function = match lua
                                    .load(&bytecode)
                                    .set_name(&filename)
                                    .set_mode(mluau::ChunkMode::Binary) // Ensure auto-detection never selects binary mode
                                    //.set_environment(self.global_table.clone())
                                    .into_function()
                                {
                                    Ok(f) => f,
                                    Err(e) => {
                                        let _ = resp.client(&client_ctx).send(Err(format!("Failed to load function from bytecode: {}", e)));
                                        continue;
                                    }
                                };

                                function_cache.insert(filename.to_string(), function.clone());
                                function
                            };

                            Self::spawn_function_async(
                                &lua,
                                &scheduler,
                                &common_state,
                                func,
                                vec![],
                                &client_ctx,
                                resp,
                            );
                        }
                        Ok(ParallelLuaMessage::GetProperty { obj_id, property, resp, magic }) => {
                            if magic != PLUAU_MESSAGE_MAGIC {
                                let _ = resp.client(&client_ctx).send(Err("Invalid magic number in GetProperty message; are you sure you're running a parallel luau child process?".into()));
                                continue;
                            }

                            let value = match common_state.proxy_client.obj_registry.get(obj_id) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to get object for GetProperty: {}", e)));
                                    continue;
                                }
                            };

                            let mut ed = EmbedderDataContext::new(&common_state.ed);
                            let prop_lua = match property.to_luau_child(&&lua, &common_state.proxy_client, &common_state.bridge, &mut ed) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to convert property to Lua value: {}", e)));
                                    continue;
                                }
                            };

                            let tab = match value {
                                mluau::Value::Table(t) => t,
                                _ => {
                                    let _ = resp.client(&client_ctx).send(Err("Object is not a table".to_string()));
                                    continue;
                                }
                            };

                            let prop_value = match tab.get(prop_lua) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to get property from Lua table: {}", e)));
                                    continue;
                                }
                            };

                            let proxied_prop = match ProxiedLuauValue::from_luau_child(&common_state.proxy_client, prop_value, &mut ed) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to convert property value to ProxiedLuauValue: {}", e)));
                                    continue;
                                }
                            };

                            let _ = resp.client(&client_ctx).send(Ok(proxied_prop));
                        },
                        Ok(ParallelLuaMessage::CallFunctionChild { obj_id, args, resp, magic }) => {
                            if magic != PLUAU_MESSAGE_MAGIC {
                                let _ = resp.client(&client_ctx).send(Err("Invalid magic number in CallFunctionChild message; are you sure you're running a parallel luau child process?".into()));
                                continue;
                            }

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

                            Self::spawn_function_async(
                                &lua,
                                &scheduler,
                                &common_state,
                                func,
                                args,
                                &client_ctx,
                                resp,
                            );
                        },
                        Ok(ParallelLuaMessage::RequestDispose { obj_id, resp, magic }) => {
                            if magic != PLUAU_MESSAGE_MAGIC {
                                let _ = resp.client(&client_ctx).send(Err("Invalid magic number in RequestDispose message; are you sure you're running a parallel luau child process?".into()));
                                continue;
                            }

                            match common_state.proxy_client.obj_registry.remove(obj_id).map_err(|e| e.to_string()) {
                                Ok(_) => {
                                    let _ = resp.client(&client_ctx).send(Ok(()));
                                },
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to dispose object: {}", e)));
                                }
                            };
                        },
                        Ok(ParallelLuaMessage::Shutdown) => {
                            break;
                        },
                        Err(e) => {
                            eprintln!("Error receiving message in ParallelLuaClient: {}", e);
                            //break;
                        }
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

    /// Send a message to the parallel luau process with a timeout and wait for a response
    pub(super) async fn send_timeout<T: LuauSendableMessage>(&self, msg: T, timeout: Duration) -> Result<T::Response, crate::base::Error> {
        let (resp_tx, resp_rx) = self.executor.create_oneshot();
        self.messenger.server(self.executor.server_context()).send(msg.to_message(resp_tx))
            .map_err(|e| format!("Failed to send message to parallel luau: {}", e))?;
        
        let resp = tokio::time::timeout(timeout, resp_rx.recv()).await
            .map_err(|e| format!("Timeout waiting for response from parallel luau: {}", e))?
            .map_err(|e| format!("Failed to receive response from parallel luau: {}", e))?;

        if self.is_shutdown() {
            return Err("parallel luau is shut down (likely due to timeout)".into());
        }
        
        Ok(resp)
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

    fn to_source_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, plc: &ProxyLuaClient) -> Result<mluau::Value, crate::base::Error> {
        let mut ed = EmbedderDataContext::new(&plc.ed);
        Ok(value.to_luau_host(lua, plc, self, &mut ed).map_err(|e| e.to_string())?)
    }

    fn from_source_lua_value(&self, _lua: &mluau::Lua, plc: &ProxyLuaClient, value: mluau::Value, ed: &mut EmbedderDataContext) -> Result<Self::ValueType, crate::base::Error> {
        Ok(ProxiedLuauValue::from_luau_host(plc, value, ed).map_err(|e| e.to_string())?)
    }

    async fn eval_from_source(&self, filename: String) -> Result<Vec<Self::ValueType>, crate::base::Error> {
        Ok(self.send(CodeExecMessage { filename }).await??)
    }

    async fn shutdown(&self, timeouts: ShutdownTimeouts) -> Result<(), crate::base::Error> {
        let _ = self.send_timeout(ShutdownMessage, timeouts.bridge_shutdown).await;
        tokio::time::timeout(timeouts.executor_shutdown, self.executor.shutdown())
            .await
            .map_err(|e| format!("Timeout shutting down V8 isolate manager executor: {}", e))??;
        
        // Not strictly needed as shutdown waits for the process to exit, but good to be explicit
        tokio::time::timeout(timeouts.executor_shutdown, self.executor.wait())
            .await
            .map_err(|e| format!("Timeout waiting for V8 isolate manager to shut down: {}", e))??;

        Ok(())
    }

    fn is_shutdown(&self) -> bool {
        self.executor.get_state().cancel_token.is_cancelled()
    }
}

impl ProxyBridgeWithMultiprocessExt for ParallelLuaProxyBridge {
    /// Returns the executor for concurrently executing tasks on a separate process
    fn get_executor(&self) -> Arc<ConcurrentExecutor<Self::ConcurrentlyExecuteClient>> {
        self.executor.clone()
    }
}

impl ProxyBridgeWithStringExt for ParallelLuaProxyBridge {
    /// Creates the foreign language value type from a string
    fn from_string(s: String) -> Self::ValueType {
        ProxiedLuauValue::Primitive(ProxiedLuauPrimitive::String(s))
    }
}

impl StandardProxyBridge for ParallelLuaProxyBridge {
    type ObjectRegistryID = PLuauObjectRegistryID;
    type ObjectRegistryType = ObjectRegistryType;

    async fn get_property(
        &self,
        id: Self::ObjectRegistryID,
        property: Self::ValueType,
    ) -> Result<Self::ValueType, crate::base::Error> {
        Ok(self.send(GetPropertyMessage { obj_id: id, property }).await??)
    }

    async fn function_call(
        &self,
        id: Self::ObjectRegistryID,
        args: Vec<Self::ValueType>,
    ) -> Result<Vec<Self::ValueType>, crate::base::Error> {
        Ok(self.send(CallFunctionChildMessage { obj_id: id, args }).await??)
    }

    async fn request_dispose(
        &self,
        id: Self::ObjectRegistryID,
    ) -> Result<(), crate::base::Error> {
        Ok(self.send(RequestDisposeMessage { obj_id: id }).await??)
    }
}

/// Helper method to run the process client
pub async fn run_luau_process_client() {
    ConcurrentExecutor::<<ParallelLuaProxyBridge as ProxyBridge>::ConcurrentlyExecuteClient>::run_process_client().await;
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

