use serde::{Deserialize, Serialize};

use crate::MAX_PROXY_DEPTH;
use crate::base::Error;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
/// Embedder specific configuration data
pub struct EmbedderData {
    /// Heap limit
    pub heap_limit: usize,
    /// Maximum number of bytes allowed for the payload
    pub max_payload_size: Option<usize>,
    /// Whether or not object disposal is enabled at all
    pub object_disposal_enabled: bool,
    /// Whether or not automatic disposal of objects is enabled
    /// (via fire_request_dispose messages)
    pub automatic_object_disposal_enabled: bool,
}

pub struct EmbedderDataContext {
    ed: EmbedderData,
    size: usize, // Current size of constructed values
    limit: bool,
    depth: usize, // Current depth of constructed values
}

impl EmbedderDataContext {
    pub fn new(ed: EmbedderData) -> Self {
        Self { ed, size: 0, limit: true, depth: 0 }
    }

    // Used when a LangTransferValue is sent to disable limits during processing it
    pub fn disable_limits(&self) -> Self {
        Self {
            ed: self.ed,
            size: self.size,
            limit: false,
            depth: self.depth,
        }
    }

    pub fn nest_in_depth(&self) -> Self {
        Self {
            ed: self.ed,
            size: self.size,
            limit: self.limit,
            depth: self.depth,
        }
    }

    pub fn nest(&self) -> Result<Self, Error> {
        if self.depth >= MAX_PROXY_DEPTH {
            return Err("Maximum nesting depth exceeded".into());
        }
        Ok(Self {
            ed: self.ed,
            size: self.size,
            limit: self.limit,
            depth: self.depth + 1,
        })
    }

    pub fn add(&mut self, len: usize, typ: &'static str) -> Result<(), Error> {
        //println!("add called: {len} {typ} limit={}", self.limit);
        if !self.limit {
            return Ok(()); // Limits disabled
        }
        self.size += len;
        if let Some(max) = self.ed.max_payload_size {
            if self.size > max {
                return Err(format!("Total payload size exceeds maximum allowed size of {} bytes (got {}) [at {}]", max, self.size, typ).into());
            }
        }
        Ok(())
    }

    pub fn merge(&mut self, other: EmbedderDataContext) -> Result<(), Error> {
        if !self.limit {
            return Ok(()); // Limits disabled
        }
        self.size = other.size;
        if let Some(max) = self.ed.max_payload_size {
            if self.size > max {
                return Err(format!("Total payload size exceeds maximum allowed size of {} bytes (got {})", max, self.size).into());
            }
        }

        Ok(())
    }
}
