use crate::{luau::embedder_api::EmbedderData};
use concurrentlyexec::{ClientContext, MultiReceiver, MultiSender};
use mluau::LuaSerdeExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
/// The client side state for proxying Lua values
/// 
/// This struct is not thread safe and must be kept on the Lua side
pub struct ProxyLuaClient {
    pub array_mt: mluau::Table,
    pub ed: EmbedderData,
}

impl ProxyLuaClient {
    /// Creates a new proxy Lua client
    pub fn new(lua: &mluau::Lua, ed: EmbedderData) -> Result<Self, mluau::Error> {
        Ok(Self {
            array_mt: lua.array_metatable(),
            ed
        })
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum ObjectRegistryType {
    Table,
    Function,
    UserData,
    Buffer,
    Thread,
}

impl Into<&'static str> for ObjectRegistryType {
    fn into(self) -> &'static str {
        self.type_name()
    }  
} 

impl ObjectRegistryType {
    pub fn type_name(&self) -> &'static str {
        match self {
            ObjectRegistryType::Table => "table",
            ObjectRegistryType::Function => "function",
            ObjectRegistryType::UserData => "userdata",
            ObjectRegistryType::Buffer => "buffer",
            ObjectRegistryType::Thread => "thread",
        }
    }  

    pub fn from_value(val: &mluau::Value) -> Option<Self> {
        match val {
            mluau::Value::Table(_) => Some(ObjectRegistryType::Table),
            mluau::Value::Function(_) => Some(ObjectRegistryType::Function),
            mluau::Value::UserData(_) => Some(ObjectRegistryType::UserData),
            mluau::Value::Buffer(_) => Some(ObjectRegistryType::Buffer),
            mluau::Value::Thread(_) => Some(ObjectRegistryType::Thread),
            _ => None,
        }
    }
}

/// Messages sent to the Lua proxy bridge
#[derive(Serialize, Deserialize)]
pub enum LuaBridgeMessage {
    SendText { msg: String }, // todo: consider adding response channel
    SendBinary { msg: serde_bytes::ByteBuf },
    Shutdown,
}

/// Proxy bridge service to expose data from Luau and another language's proxy bridge
pub struct LuaBridgeService {
    text_tx: UnboundedSender<String>,
    binary_tx: UnboundedSender<serde_bytes::ByteBuf>,
    rx: MultiReceiver<LuaBridgeMessage>,
}

pub struct LuaBridgeServiceHandle {
    pub text_rx: UnboundedReceiver<String>,
    pub binary_rx: UnboundedReceiver<serde_bytes::ByteBuf>,
}

impl LuaBridgeService {
    pub fn new(rx: MultiReceiver<LuaBridgeMessage>) -> (Self, LuaBridgeServiceHandle) {
        let (text_tx, text_rx) = unbounded_channel();
        let (binary_tx, binary_rx) = unbounded_channel();
        (Self { rx, text_tx, binary_tx  }, LuaBridgeServiceHandle { text_rx, binary_rx })
    }

    /// Creates a new Lua proxy bridge
    pub async fn run(mut self, cancel_token: CancellationToken) {
        loop {
            tokio::select! {
                Ok(msg) = self.rx.recv() => {
                    match msg {
                        LuaBridgeMessage::SendText { msg } => {
                            let _ = self.text_tx.send(msg);
                        },
                        LuaBridgeMessage::SendBinary { msg } => {
                            let _ = self.binary_tx.send(msg);
                        },
                        LuaBridgeMessage::Shutdown => {
                            if cfg!(feature = "debug_message_print_enabled") {
                                println!("Host Luau received shutdown message");
                            }
                            break;
                        }
                    }
                }
                _ = cancel_token.cancelled() => {
                    if cfg!(feature = "debug_message_print_enabled") {
                        println!("Lua bridge service received shutdown cancellation");
                    }
                    break; // Executor is shutting down
                }
            }
        }
    }
}


/// The client side part of the Lua bridge service
#[derive(Clone)]
pub struct LuaBridgeServiceClient {
    client_context: ClientContext,
    tx: MultiSender<LuaBridgeMessage>,

}

impl LuaBridgeServiceClient {
    /// Creates a new Lua bridge service client
    pub fn new(client_context: ClientContext, tx: MultiSender<LuaBridgeMessage>) -> Self {
        Self { client_context, tx }
    }

    /// Sends a text message to the Lua side
    pub fn send_text(&self, msg: String) -> Result<(), String> {
        let msg = LuaBridgeMessage::SendText {
            msg,
        };
        self.tx.client(&self.client_context).send(msg).map_err(|e| format!("Failed to send text message: {}", e))?;
        Ok(())
    }

    /// Sends a binary message to the Lua side
    pub fn send_binary(&self, msg: serde_bytes::ByteBuf) -> Result<(), String> {
        let msg = LuaBridgeMessage::SendBinary {
            msg,
        };
        self.tx.client(&self.client_context).send(msg).map_err(|e| format!("Failed to send binary message: {}", e))?;
        Ok(())
    }
}