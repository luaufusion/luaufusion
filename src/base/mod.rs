use std::{collections::HashMap, sync::Arc, time::Duration};

use concurrentlyexec::{ConcurrentExecutor, ConcurrentExecutorState, ConcurrentlyExecute, ProcessOpts};
use serde::{Deserialize, Serialize};

use crate::luau::{bridge::ProxyLuaClient, embedder_api::{EmbedderData, EmbedderDataContext}};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Copy, Debug)]
pub struct ShutdownTimeouts {
    pub bridge_shutdown: Duration,
    pub executor_shutdown: Duration,
}

#[allow(async_fn_in_trait)]
pub trait ProxyBridge: Clone + 'static {
    type ValueType: Send + Sync + Serialize + for<'de> Deserialize<'de> + 'static;
    type ConcurrentlyExecuteClient: ConcurrentlyExecute;

    fn name() -> &'static str;

    async fn new(
        cs_state: ConcurrentExecutorState<Self::ConcurrentlyExecuteClient>, 
        ed: EmbedderData, 
        process_opts: ProcessOpts,
        plc: ProxyLuaClient,
        vfs: HashMap<String, String>,
    ) -> Result<Self, Error>;

    /// Convert a value from the foreign language to a lua owned value
    fn to_source_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, plc: &ProxyLuaClient) -> Result<mluau::Value, Error>;

    /// Convert a value from the source lua to a foreign language owned value type
    fn from_source_lua_value(&self, lua: &mluau::Lua, plc: &ProxyLuaClient, value: mluau::Value, ed: &mut EmbedderDataContext) -> Result<Self::ValueType, Error>;

    /// Evaluates code (string) from the source Luau to the foreign language
    /// 
    /// The specified modname is either a module name or the file name to load (depending on the underlying implementation)
    /// 
    /// May return one or one+ values depending on the foreign language semantics for modules (parallel luau may return multiple values while deno/v8 returns a single value)
    async fn eval_from_source(&self, modname: String) -> Result<Vec<Self::ValueType>, Error>;

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
    fn get_executor(&self) -> Arc<ConcurrentExecutor<Self::ConcurrentlyExecuteClient>>;
}

/// Extension trait for ProxyBridge's that have a direct string variant
/// 
/// It is not required for a ProxyBridge to implement this trait although most should
/// implement this trait.
pub trait ProxyBridgeWithStringExt: ProxyBridge {
    /// Creates the foreign language value type from a string
    fn from_string(s: String) -> Self::ValueType;
}

/// Extension trait for 'standard' ProxyBridge's.
/// 
/// A proxy bridge is considered 'standard' iff:
/// - It has an ObjectRegistryID that can be converted to a i64
/// - It has a ObjectRegistryType
/// - It exposes (and only exposes) a specific set of op calls: GetProperty, FunctionCall and RequestDispose
#[allow(async_fn_in_trait)]
pub trait StandardProxyBridge: ProxyBridge {
    type ObjectRegistryID: Send + Sync + Clone + Copy + Into<i64> + 'static;
    type ObjectRegistryType: Send + Sync + Clone + Copy + Into<&'static str> + 'static;

    /// GetProperty gets a property from a foreign object by its registry ID
    /// returning a T::ValueType
    async fn get_property(
        &self,
        id: Self::ObjectRegistryID,
        property: Self::ValueType,
    ) -> Result<Self::ValueType, Error>;

    /// FunctionCall calls a function on a foreign object by its registry ID
    /// with the specified arguments, returning a Vec<T::ValueType>
    async fn function_call(
        &self,
        id: Self::ObjectRegistryID,
        args: Vec<Self::ValueType>,
    ) -> Result<Vec<Self::ValueType>, Error>;

    /// Fire a request dispose synchronously without waiting for the result
    fn fire_request_disposes(
        &self,
        id: Vec<Self::ObjectRegistryID>,
    );

    /// RequestDispose requests disposal of a foreign object by its registry ID
    async fn request_dispose(
        &self,
        id: Self::ObjectRegistryID,
    ) -> Result<(), Error>;
}