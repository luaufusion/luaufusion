use std::{collections::HashMap, pin::Pin, sync::LazyLock};

use concurrentlyexec::{ConcurrentExecutor, ConcurrentExecutorState, ConcurrentlyExecute, MultiReceiver, MultiSender, OneshotSender, ProcessOpts};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{base::{ClientMessage, ClientTransport, InitialData, ServerMessage, ServerTransport}, luau::embedder_api::EmbedderData};

type ConcurrentExecuteCb = Box<dyn Fn(ConcurrentlyExecuteClient, InitialData) -> Pin<Box<dyn std::future::Future<Output = ()>>> + Send + Sync>;
static CONCURRENT_EXECUTE_CB: LazyLock<Mutex<Option<ConcurrentExecuteCb>>> = LazyLock::new(|| Mutex::new(None));

pub async fn set_concurrently_execute_callback(cb: ConcurrentExecuteCb) {
    let mut guard = CONCURRENT_EXECUTE_CB.lock().await;
    *guard = Some(Box::new(cb));
}

pub async fn set_use_v8() {
    set_concurrently_execute_callback(Box::new(|ctx, id| Box::pin(async move {
        crate::denobridge::bridge::run_v8_client(ctx, id).await;
    }))).await;
}

pub struct ConcurrentlyExecuteServer {
    executor: ConcurrentExecutor<ConcurrentlyExecuteTransportClient>,
    tx: MultiSender<ServerMessage>,
    rx: Mutex<MultiReceiver<ClientMessage>>,
}

pub struct ConcurrentlyExecuteServerData {
    pub ed: EmbedderData,
    pub vfs: HashMap<String, String>,
    pub module_name: String,
}

impl ConcurrentlyExecuteServer {
    pub async fn new(cs_state: ConcurrentExecutorState<ConcurrentlyExecuteTransportClient>, process_opts: ProcessOpts, data: ConcurrentlyExecuteServerData) -> Result<Self, crate::base::Error> {
        let (executor, (lua_bridge_rx, ms_rx)) = ConcurrentExecutor::new(
            cs_state,
            process_opts,
            move |cei| {
                let (tx, rx) = cei.create_multi();
                let (msg_tx, msg_rx) = cei.create_oneshot();
                (BootstrapData {
                    ed: data.ed,
                    messenger_tx: msg_tx,
                    lua_bridge_tx: tx,
                    vfs: data.vfs,
                    module_name: data.module_name,
                }, (rx, msg_rx))
            }
        ).await.map_err(|e| format!("Failed to create V8 isolate manager executor: {}", e))?;
        let messenger = ms_rx.recv().await.map_err(|e| format!("Failed to receive messenger: {}", e))?;

        Ok(Self {
            executor,
            tx: messenger,
            rx: Mutex::new(lua_bridge_rx),
        })
    }
}

impl ServerTransport for ConcurrentlyExecuteServer {
    fn send_to_foreign(&self, msg: ServerMessage) -> Result<(), crate::base::Error> {
        self.tx.server(self.executor.server_context()).send(msg)
            .map_err(|e| format!("Failed to send message to target language: {}", e))?;
        Ok(())
    }

    async fn receive_from_foreign(&self) -> Result<ClientMessage, crate::base::Error> {
        if self.rx.try_lock().is_err() {
            return Err("Lua bridge binary receiver is already locked".into());
        }
        let mut rx = self.rx.lock().await;
        let msg = rx.recv()
            .await
            .map_err(|e| format!("Failed to send message to target language: {}", e))?;
        Ok(msg)
    }

    async fn shutdown(&self) -> Result<(), crate::base::Error> {
        self.executor.shutdown().await?;

        // Not strictly needed as shutdown waits for the process to exit, but good to be explicit
        self.executor.wait().await
            .map_err(|e| format!("Failed to wait for target language to shut down: {}", e))?;
        
        Ok(())
    }

    fn fire_shutdown(&self) {
        let _ = self.executor.shutdown_in_task();
    }

    fn is_shutdown(&self) -> bool {
        self.executor.get_state().cancel_token.is_cancelled()
    }
}

pub struct ConcurrentlyExecuteTransportClient {}

impl ConcurrentlyExecute for ConcurrentlyExecuteTransportClient {
    type BootstrapData = BootstrapData;
    
    async fn run(
        data: Self::BootstrapData,
        client_ctx: concurrentlyexec::ClientContext
    ) {
        let (tx, rx) = client_ctx.multi();
        data.messenger_tx.client(&client_ctx).send(tx).unwrap();

        let cec = ConcurrentlyExecuteClient {
            client_context: client_ctx,
            tx: data.lua_bridge_tx,
            rx: Mutex::new(rx),
        };

        let guard = CONCURRENT_EXECUTE_CB.lock().await;
        if guard.is_none() {
            panic!("Concurrently execute callback not set");
        }
        let cb = guard.as_ref().unwrap();
        cb(cec, InitialData {
            vfs: data.vfs,
            ed: data.ed,
            module_name: data.module_name,
        }).await;
    }
}

#[derive(Serialize, Deserialize)]
pub struct BootstrapData {
    ed: EmbedderData,
    vfs: HashMap<String, String>,
    messenger_tx: OneshotSender<MultiSender<ServerMessage>>,
    lua_bridge_tx: MultiSender<ClientMessage>,
    module_name: String,
}

pub struct ConcurrentlyExecuteClient {
    client_context: concurrentlyexec::ClientContext,
    tx: MultiSender<ClientMessage>,
    rx: Mutex<MultiReceiver<ServerMessage>>,
}

impl ClientTransport for ConcurrentlyExecuteClient {
    fn send_to_host(&self, msg: ClientMessage) -> Result<(), crate::base::Error> {
        self.tx.client(&self.client_context).send(msg)
            .map_err(|e| format!("Failed to send message to host language: {}", e))?;
        Ok(())
    }

    async fn receive_from_host(&self) -> Result<ServerMessage, crate::base::Error> {
        if self.rx.try_lock().is_err() {
            return Err("Lua bridge binary receiver is already locked".into());
        }
        let mut rx = self.rx.lock().await;
        let msg = rx.recv()
            .await
            .map_err(|e| format!("Failed to receive message from host language: {}", e))?;
        Ok(msg)
    }
}

/// Helper method to run the process client
pub async fn run_process_client() {
    ConcurrentExecutor::<ConcurrentlyExecuteTransportClient>::run_process_client().await;
}