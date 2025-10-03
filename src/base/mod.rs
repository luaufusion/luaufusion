use std::{collections::HashMap, sync::Arc};

use concurrentlyexec::{ConcurrentExecutor, ConcurrentExecutorState, ConcurrentlyExecute, ProcessOpts};
use serde::{Deserialize, Serialize};

use crate::luau::bridge::ProxyLuaClient;

pub const MAX_INTERN_SIZE: usize = 1024 * 512; // 512 KB
pub const MAX_OBJECT_REGISTRY_SIZE: usize = 2048; // 2048 objects
pub const MAX_BUFFER_SIZE: usize = 4096; // For now, 4096 bytes max buffer size

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[allow(async_fn_in_trait)]
pub trait ProxyBridge: Clone + 'static {
    type ValueType: Send + Sync + Serialize + for<'de> Deserialize<'de> + 'static;
    type ConcurrentlyExecuteClient: ConcurrentlyExecute;

    fn name() -> &'static str;

    async fn new(
        cs_state: ConcurrentExecutorState<Self::ConcurrentlyExecuteClient>, 
        heap_limit: usize, 
        process_opts: ProcessOpts,
        plc: ProxyLuaClient,
        vfs: HashMap<String, String>,
    ) -> Result<Self, Error>;

    /// Returns the executor for concurrently executing tasks on a separate process
    fn get_executor(&self) -> Arc<ConcurrentExecutor<Self::ConcurrentlyExecuteClient>>;

    /// Convert a value from the foreign language to a lua owned value
    fn to_source_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, plc: &ProxyLuaClient) -> Result<mluau::Value, Error>;

    /// Convert a value from the source lua to a foreign language owned value type
    fn from_source_lua_value(&self, lua: &mluau::Lua, plc: &ProxyLuaClient, value: mluau::Value) -> Result<Self::ValueType, Error>;

    /// Evaluates code (string) from the source Luau to the foreign language
    async fn eval_from_source(&self, modname: String) -> Result<Self::ValueType, Error>;

    /// Shuts down the bridge and its resources
    async fn shutdown(&self) -> Result<(), Error>;
}
