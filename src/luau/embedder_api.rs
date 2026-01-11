use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
/// Embedder specific configuration data
pub struct EmbedderData {
    /// Heap limit
    pub heap_limit: usize,
    /// Maximum number of bytes allowed for the payload
    pub max_payload_size: Option<usize>,
}
