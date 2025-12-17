use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use concurrentlyexec::{ConcurrentExecutor, ConcurrentExecutorState, ConcurrentlyExecute, MultiSender, OneshotSender, ProcessOpts};
use deno_core::PollEventLoopOptions;
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::denobridge::inner::EventBridgeSetupData;
use crate::denobridge::modloader::FusionModuleLoader;
use crate::luau::bridge::{
    LuaBridgeMessage, LuaBridgeService, LuaBridgeServiceClient,
};

use crate::base::{Error, ProxyBridge, ProxyBridgeWithMultiprocessExt, ShutdownTimeouts};
use crate::luau::embedder_api::EmbedderData;
use super::inner::V8IsolateManagerInner;

/// Minimum heap size for V8 isolates
pub const MIN_HEAP_LIMIT: usize = 10 * 1024 * 1024; // 10MB
/// Magic number for V8 isolate manager messages
/// 
/// Helpful to avoid common user errors like running a parallel luau child with a v8 server instead of a v8 child
pub const V8_MESSAGE_MAGIC: usize = 1;

#[derive(Serialize, Deserialize)]
/// Internal representation of a message that can be sent to the V8 isolate manager
pub(super) enum V8IsolateManagerMessage {
    CodeExec {
        modname: String,
        resp: OneshotSender<Result<(), String>>,
        magic: usize,
    },
    SendText {
        msg: String,
        resp: OneshotSender<Result<(), String>>,
        magic: usize,
    },
    SendBinary {
        msg: serde_bytes::ByteBuf,
        resp: OneshotSender<Result<(), String>>,
        magic: usize,
    },
    Shutdown,
}

/// A message that can be sent to the V8 isolate manager
pub(super) trait V8IsolateSendableMessage: Send + 'static {
    type Response: Send + serde::Serialize + for<'de> serde::Deserialize<'de> + 'static;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage;
}

pub(super) struct CodeExecMessage {
    pub modname: String,
}

impl V8IsolateSendableMessage for CodeExecMessage {
    type Response = Result<(), String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::CodeExec {
            modname: self.modname,
            resp,
            magic: V8_MESSAGE_MAGIC,
        }
    }
}

pub(super) struct SendTextMessage {
    pub msg: String,
}

impl V8IsolateSendableMessage for SendTextMessage {
    type Response = Result<(), String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::SendText {
            msg: self.msg,
            resp,
            magic: V8_MESSAGE_MAGIC,
        }
    }
}

pub(super) struct SendBinaryMessage {
    pub msg: serde_bytes::ByteBuf,
}

impl V8IsolateSendableMessage for SendBinaryMessage {
    type Response = Result<(), String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::SendBinary {
            msg: self.msg,
            resp,
            magic: V8_MESSAGE_MAGIC,
        }
    }
}

pub(super) struct ShutdownMessage;

impl V8IsolateSendableMessage for ShutdownMessage {
    type Response = ();
    fn to_message(self, _resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::Shutdown
    }
}
 
#[derive(Clone)]
pub struct V8IsolateManagerClient {}

#[derive(Serialize, Deserialize)]
pub struct V8BootstrapData {
    ed: EmbedderData,
    messenger_tx: OneshotSender<MultiSender<V8IsolateManagerMessage>>,
    lua_bridge_tx: MultiSender<LuaBridgeMessage>,
    vfs: HashMap<String, String>,
}

impl ConcurrentlyExecute for V8IsolateManagerClient {
    type BootstrapData = V8BootstrapData;
    async fn run(
        data: Self::BootstrapData,
        client_ctx: concurrentlyexec::ClientContext
    ) {
        let (send_text_tx, send_text_rx) = unbounded_channel();
        let (send_binary_tx, send_binary_rx) = unbounded_channel();

        let (tx, mut rx) = client_ctx.multi();
        data.messenger_tx.client(&client_ctx).send(tx).unwrap();

        let mut inner = V8IsolateManagerInner::new(
            LuaBridgeServiceClient::new(client_ctx.clone(), data.lua_bridge_tx),
            data.ed,
            FusionModuleLoader::new(data.vfs.into_iter().map(|(x, y)| (x, y.into()))),
            EventBridgeSetupData {
                text_rx: send_text_rx,
                binary_rx: send_binary_rx,
            }
        );

        let mut evaluated_modules = HashMap::new();
        let mut module_evaluate_queue = FuturesUnordered::new();

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Ok(V8IsolateManagerMessage::CodeExec { modname, resp, magic }) => {
                            if magic != V8_MESSAGE_MAGIC {
                                let _ = resp.client(&client_ctx).send(Err("Invalid magic number in CodeExec message; are you sure you're running a V8 isolate manager and not a Parallel Luau child?".into()));
                                continue;
                            }

                            if evaluated_modules.contains_key(&modname) {
                                // Module already evaluated, just return early
                                let _ = resp.client(&client_ctx).send(Ok(()));
                                continue;
                            }

                            let url = match deno_core::url::Url::parse(&format!("file:///{modname}")) {
                                Ok(u) => u,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to parse module name as URL: {}", e).into()));
                                    continue;
                                }
                            };

                            let id = tokio::select! {
                                id = inner.deno.load_side_es_module(&url) => {
                                    match id {
                                        Ok(id) => id,
                                        Err(e) => {
                                            let _ = resp.client(&client_ctx).send(Err(format!("Failed to load module: {}", e).into()));
                                            continue;
                                        }
                                    }
                                }
                                _ = inner.cancellation_token.cancelled() => {
                                    return;
                                }
                            };

                            let fut = inner.deno.mod_evaluate(id);
                            module_evaluate_queue.push(async move {
                                let module_id = fut.await.map(|_| id);
                                (modname, module_id, resp)
                            });
                        },
                        Ok(V8IsolateManagerMessage::SendText { msg, resp, magic }) => {
                            if magic != V8_MESSAGE_MAGIC {
                                let _ = resp.client(&client_ctx).send(Err("Invalid magic number in CodeExec message; are you sure you're running a V8 child process?".into()));
                                continue;
                            }

                            let _ = send_text_tx.send(msg);
                            let _ = resp.client(&client_ctx).send(Ok(()));
                        },
                        Ok(V8IsolateManagerMessage::SendBinary { msg, resp, magic }) => {
                            if magic != V8_MESSAGE_MAGIC {
                                let _ = resp.client(&client_ctx).send(Err("Invalid magic number in CodeExec message; are you sure you're running a V8 child process?".into()));
                                continue;
                            }

                            let _ = send_binary_tx.send(msg);
                            let _ = resp.client(&client_ctx).send(Ok(()));
                        },
                        Ok(V8IsolateManagerMessage::Shutdown) => {
                            if cfg!(feature = "debug_message_print_enabled") {
                                println!("V8 isolate manager received shutdown message");
                            }
                            break;
                        }
                        Err(e) => {
                            eprintln!("Error receiving message in V8 isolate manager: {}", e);
                            //break;
                        }
                    }
                }
                _ = inner.cancellation_token.cancelled() => {
                    if cfg!(feature = "debug_message_print_enabled") {
                        println!("V8 isolate manager received shutdown message");
                    }
                    break;
                }
                _ = inner.deno.run_event_loop(PollEventLoopOptions {
                    wait_for_inspector: false,
                    pump_v8_message_loop: true,
                }) => {
                    tokio::task::yield_now().await;
                },
                Some((modname, result, resp)) = module_evaluate_queue.next() => {
                    let Ok(result) = result else {
                        let _ = resp.client(&client_ctx).send(Err("Failed to evaluate module".to_string().into()));
                        continue;
                    };
                    evaluated_modules.insert(modname, result);
                    let _ = resp.client(&client_ctx).send(Ok(()));
                }
            }
        }
    }
}

pub struct V8IsolateManagerServerInner {
    executor: Arc<ConcurrentExecutor<V8IsolateManagerClient>>,
    messenger: Arc<MultiSender<V8IsolateManagerMessage>>,
}

pub struct V8IsolateManagerServer {
    inner: Rc<V8IsolateManagerServerInner>,
    text_rx: Mutex<UnboundedReceiver<String>>,
    binary_rx: Mutex<UnboundedReceiver<serde_bytes::ByteBuf>>,
}

impl std::ops::Deref for V8IsolateManagerServer {
    type Target = V8IsolateManagerServerInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl V8IsolateManagerServer {
    /// Create a new V8 isolate manager server
    async fn new(
        cs_state: ConcurrentExecutorState<V8IsolateManagerClient>, 
        ed: EmbedderData, 
        process_opts: ProcessOpts,
        vfs: HashMap<String, String>,
    ) -> Result<Self, crate::base::Error> {
        let (executor, (lua_bridge_rx, ms_rx)) = ConcurrentExecutor::new(
            cs_state,
            process_opts,
            move |cei| {
                let (tx, rx) = cei.create_multi();
                let (msg_tx, msg_rx) = cei.create_oneshot();
                (V8BootstrapData {
                    ed,
                    messenger_tx: msg_tx,
                    lua_bridge_tx: tx,
                    vfs
                }, (rx, msg_rx))
            }
        ).await.map_err(|e| format!("Failed to create V8 isolate manager executor: {}", e))?;
        let messenger = ms_rx.recv().await.map_err(|e| format!("Failed to receive messenger: {}", e))?;

        let (lua_bridge_service, handle) = LuaBridgeService::new(lua_bridge_rx);

        let ctoken = executor.get_state().cancel_token.clone();
        tokio::task::spawn_local(async move {
            lua_bridge_service.run(ctoken).await;
        });

        let self_ret = Self { 
            inner: Rc::new(V8IsolateManagerServerInner {
                executor: Arc::new(executor), 
                messenger: Arc::new(messenger),
            }),
            text_rx: Mutex::new(handle.text_rx),
            binary_rx: Mutex::new(handle.binary_rx),
        };

        Ok(self_ret)
    }

    /// Send a message to the V8 isolate process and wait for a response
    pub(super) async fn send<T: V8IsolateSendableMessage>(&self, msg: T) -> Result<T::Response, crate::base::Error> {
        let (resp_tx, resp_rx) = self.executor.create_oneshot();
        self.messenger.server(self.executor.server_context()).send(msg.to_message(resp_tx))
            .map_err(|e| format!("Failed to send message to V8 isolate: {}", e))?;
        
        let resp = resp_rx.recv().await;

        if self.is_shutdown() {
            return Err("V8 isolate manager is shut down (likely due to timeout)".into());
        }
        
        resp.map_err(|e| format!("Failed to receive response from V8 isolate: {}", e).into())
    }

    /// Send a message to the V8 isolate process with a timeout and wait for a response
    pub(super) async fn send_timeout<T: V8IsolateSendableMessage>(&self, msg: T, timeout: Duration) -> Result<T::Response, crate::base::Error> {
        let (resp_tx, resp_rx) = self.executor.create_oneshot();
        self.messenger.server(self.executor.server_context()).send(msg.to_message(resp_tx))
            .map_err(|e| format!("Failed to send message to V8 isolate: {}", e))?;
        
        let resp = tokio::time::timeout(timeout, resp_rx.recv()).await
            .map_err(|e| format!("Timeout waiting for response from V8 isolate: {}", e))?
            .map_err(|e| format!("Failed to receive response from V8 isolate: {}", e))?;

        if self.is_shutdown() {
            return Err("V8 isolate manager is shut down (likely due to timeout)".into());
        }
        
        Ok(resp)
    }
}


impl ProxyBridge for V8IsolateManagerServer {
    type ConcurrentlyExecuteClient = V8IsolateManagerClient;

    fn name() -> &'static str {
        "v8"
    }

    async fn new(
        cs_state: ConcurrentExecutorState<Self::ConcurrentlyExecuteClient>, 
        ed: EmbedderData,
        process_opts: ProcessOpts,
        vfs: HashMap<String, String>,
    ) -> Result<Self, crate::base::Error> {
        Self::new(cs_state, ed, process_opts, vfs).await        
    }

    async fn send_text(&self, msg: String) -> Result<(), Error> {
        self.send(SendTextMessage { msg }).await??;
        Ok(())
    }

    async fn send_binary(&self, msg: serde_bytes::ByteBuf) -> Result<(), Error> {
        self.send(SendBinaryMessage { msg }).await??;
        Ok(())
    }

    async fn receive_text(&self) -> Result<String, Error> {
        if self.text_rx.try_lock().is_err() {
            return Err("Lua bridge text receiver is already locked".into());
        }
        let mut text_rx = self.text_rx.lock().await;
        text_rx.recv().await.ok_or_else(|| "Lua bridge text channel closed".into())
    }

    async fn receive_binary(&self) -> Result<serde_bytes::ByteBuf, Error> {
        if self.binary_rx.try_lock().is_err() {
            return Err("Lua bridge binary receiver is already locked".into());
        }
        let mut binary_rx = self.binary_rx.lock().await;
        binary_rx.recv().await.ok_or_else(|| "Lua bridge binary channel closed".into())
    }

    async fn eval_from_source(&self, modname: String) -> Result<(), crate::base::Error> {
        Ok(self.send(CodeExecMessage { modname }).await??)
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

    fn fire_shutdown(&self) {
        let _ = self.messenger.server(self.executor.server_context()).send(
            V8IsolateManagerMessage::Shutdown
        );
        let _ = self.executor.shutdown_in_task();
    }

    fn is_shutdown(&self) -> bool {
        self.executor.get_state().cancel_token.is_cancelled()
    }
}

impl ProxyBridgeWithMultiprocessExt for V8IsolateManagerServer {
    /// Returns the executor for concurrently executing tasks on a separate process
    fn get_executor(&self) -> &ConcurrentExecutor<Self::ConcurrentlyExecuteClient> {
        self.executor.as_ref()
    }
}

/// Helper method to run the process client
pub async fn run_v8_process_client() {
    ConcurrentExecutor::<<V8IsolateManagerServer as ProxyBridge>::ConcurrentlyExecuteClient>::run_process_client().await;
}