use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::luau::embedder_api::EmbedderData;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Serialize, Deserialize)]
/// Internal representation of a message between host and foreign language
pub enum ClientMessage {
    SendText {
        msg: String,
    },
    SendBinary {
        msg: serde_bytes::ByteBuf,
    },
}

#[derive(Serialize, Deserialize)]
/// Internal representation of a message between foreign language and host
pub enum ServerMessage {
    SendText {
        msg: String,
    },
    SendBinary {
        msg: serde_bytes::ByteBuf,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct InitialData {
    pub vfs: HashMap<String, String>,
    pub ed: EmbedderData,
    pub module_name: String,
}

#[allow(async_fn_in_trait)]
pub trait ServerTransport: 'static {
    /// Push a message to the foreign language
    fn send_to_foreign(&self, msg: ServerMessage) -> Result<(), Error>;

    /// Receive a message from the foreign language
    async fn receive_from_foreign(&self) -> Result<ClientMessage, Error>;

    /// Shuts down the bridge and its resources
    async fn shutdown(&self) -> Result<(), Error> {
        self.fire_shutdown();
        if !self.is_shutdown() {
            return Err("Transport did not shutdown properly".into());
        }
        Ok(())
    }

    /// Fires a shutdown request without waiting for the result
    fn fire_shutdown(&self);

    /// Returns true if the bridge has been shutdown
    fn is_shutdown(&self) -> bool;
}

#[allow(async_fn_in_trait)]
pub trait ClientTransport: 'static {
    /// Push a message to the host language
    fn send_to_host(&self, msg: ClientMessage) -> Result<(), Error>;

    /// Receive a binary message from the host language
    async fn receive_from_host(&self) -> Result<ServerMessage, Error>;
}