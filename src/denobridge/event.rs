use deno_core::{GarbageCollected, op2, v8};
use tokio::sync::{Mutex, mpsc::{UnboundedReceiver, UnboundedSender}};

use crate::{base::ClientMessage, luau::embedder_api::EmbedderData};

/// Bridge object for sending and receiving events between Luau and V8
pub struct EventBridge {
    pub(super) ed: EmbedderData,
    pub(super) msg_tx: UnboundedSender<ClientMessage>,
    pub(super) text_rx: Mutex<UnboundedReceiver<String>>,
    pub(super) binary_rx: Mutex<UnboundedReceiver<serde_bytes::ByteBuf>>,
}

impl EventBridge {
    /// Creates a new EventBridge object with channels for text and binary messages
    pub fn new(
        ed: EmbedderData,
        msg_tx: UnboundedSender<ClientMessage>,
        text_rx: UnboundedReceiver<String>,
        binary_rx: UnboundedReceiver<serde_bytes::ByteBuf>,
    ) -> Self {
        Self {
            ed,
            msg_tx,
            text_rx: Mutex::new(text_rx),
            binary_rx: Mutex::new(binary_rx),
        }
    }
}

unsafe impl GarbageCollected for EventBridge {
    fn get_name(&self) -> &'static std::ffi::CStr {
        c"Bridge"
    }

    fn trace(&self, _visitor: &mut v8::cppgc::Visitor) {
        // No fields to trace
    }
}

#[op2]
impl EventBridge {
    #[fast]
    #[method]
    #[rename("sendText")]
    fn send_text(&self, #[string] msg: String) -> Result<(), deno_error::JsErrorBox> {
        if let Some(max_payload_size) = self.ed.max_payload_size && msg.len() > max_payload_size {
            return Err(deno_error::JsErrorBox::generic(format!(
                "Payload size {} exceeds maximum allowed size of {} bytes",
                msg.len(),
                max_payload_size
            )));
        }
        self.msg_tx.send(ClientMessage::SendText { msg })
            .map_err(|e| deno_error::JsErrorBox::generic(format!("Failed to send text message: {}", e)))?;
        Ok(())
    }

    #[fast]
    #[method]
    #[rename("sendBinary")]
    fn send_binary(&self, #[buffer] msg: &[u8]) -> Result<(), deno_error::JsErrorBox> {
        if let Some(max_payload_size) = self.ed.max_payload_size && msg.len() > max_payload_size {
            return Err(deno_error::JsErrorBox::generic(format!(
                "Payload size {} exceeds maximum allowed size of {} bytes",
                msg.len(),
                max_payload_size
            )));
        }
        self.msg_tx.send(ClientMessage::SendBinary { msg: serde_bytes::ByteBuf::from(msg) })
            .map_err(|e| deno_error::JsErrorBox::generic(format!("Failed to send text message: {}", e)))?;
        Ok(())
    }

    #[async_method]
    #[rename("receiveText")]
    #[string]
    async fn receive_text(&self) -> Result<Option<String>, deno_error::JsErrorBox> {
        if self.text_rx.try_lock().is_err() {
            return Err(deno_error::JsErrorBox::generic("Text message channel is already locked".to_string()));
        }

        let mut text_rx = self.text_rx.lock().await;
        match text_rx.recv().await {
            Some(msg) => Ok(Some(msg)),
            None => Ok(None),
        }
    }

    #[async_method]
    #[rename("receiveBinary")]
    #[buffer]
    async fn receive_binary(&self) -> Result<Option<Vec<u8>>, deno_error::JsErrorBox> {
        if self.binary_rx.try_lock().is_err() {
            return Err(deno_error::JsErrorBox::generic("Binary message channel is already locked".to_string()));
        }

        let mut binary_rx = self.binary_rx.lock().await;
        match binary_rx.recv().await {
            Some(msg) => Ok(Some(msg.into_vec())),
            None => Ok(None),
        }   
    }
}