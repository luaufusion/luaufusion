use std::{collections::HashMap, time::Duration};

use concurrentlyexec::{ConcurrentExecutor, ConcurrentExecutorState, ConcurrentlyExecute, ProcessOpts};

use crate::luau::embedder_api::EmbedderData;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Copy, Debug)]
pub struct ShutdownTimeouts {
    pub bridge_shutdown: Duration,
    pub executor_shutdown: Duration,
}

#[allow(async_fn_in_trait)]
pub trait ProxyBridge: 'static + Sized {
    type ConcurrentlyExecuteClient: ConcurrentlyExecute;

    fn name() -> &'static str;

    async fn new(
        cs_state: ConcurrentExecutorState<Self::ConcurrentlyExecuteClient>, 
        ed: EmbedderData, 
        process_opts: ProcessOpts,
        vfs: HashMap<String, String>,
    ) -> Result<Self, Error>;

    /// Push a text message to the foreign language
    async fn send_text(&self, msg: String) -> Result<(), Error>;

    /// Push a binary message to the foreign language
    async fn send_binary(&self, msg: serde_bytes::ByteBuf) -> Result<(), Error>;

    /// Receive a text message from the foreign language
    async fn receive_text(&self) -> Result<String, Error>;

    /// Receive a binary message from the foreign language
    async fn receive_binary(&self) -> Result<serde_bytes::ByteBuf, Error>;

    /// Evaluates code (string) from the source Luau to the foreign language
    /// 
    /// The specified modname is either a module name or the file name to load (depending on the underlying implementation)
    /// 
    /// May return one or one+ values depending on the foreign language semantics for modules (parallel luau may return multiple values while deno/v8 returns a single value)
    async fn eval_from_source(&self, modname: String) -> Result<(), Error>;

    /// Shuts down the bridge and its resources
    async fn shutdown(&self, timeouts: ShutdownTimeouts) -> Result<(), Error>;

    /// Fires a shutdown request without waiting for the result
    fn fire_shutdown(&self);

    /// Returns true if the bridge has been shutdown
    fn is_shutdown(&self) -> bool;
}

/// Extension trait for ProxyBridge's that support multiprocess execution via concurrentlyexec
pub trait ProxyBridgeWithMultiprocessExt: ProxyBridge {
    /// Returns the executor for concurrently executing tasks on a separate process
    fn get_executor(&self) -> &ConcurrentExecutor<Self::ConcurrentlyExecuteClient>;
}