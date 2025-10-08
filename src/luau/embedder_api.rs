#[cfg(feature = "embedder_json")]
use std::cell::RefCell;
#[cfg(feature = "embedder_json")]
use std::rc::Rc;

use serde::{Deserialize, Serialize};
#[cfg(feature = "embedder_json")]
use serde_json::Value as JsonValue;
#[cfg(feature = "embedder_json")]
use serde_json::value::RawValue as JsonRawValue;

use crate::MAX_PROXY_DEPTH;
use crate::base::Error;
use crate::denobridge::bridge::MIN_HEAP_LIMIT;

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Embedder specific configuration data
pub struct EmbedderData {
    /// Heap limit
    pub heap_limit: usize,
    /// Maximum number of bytes allowed for the payload
    pub max_payload_size: Option<usize>,
}

impl Default for EmbedderData {
    fn default() -> Self {
        Self {
            heap_limit: MIN_HEAP_LIMIT,
            max_payload_size: None,
        }
    }
}

pub(crate) struct EmbedderDataContext {
    ed: EmbedderData,
    size: usize, // Current size of constructed values
    limit: bool,
    depth: usize, // Current depth of constructed values
}

impl EmbedderDataContext {
    pub fn new(ed: &EmbedderData) -> Self {
        Self { ed: ed.clone(), size: 0, limit: true, depth: 0 }
    }

    // Used when a LangTransferValue is sent to disable limits during processing it
    pub fn disable_limits(&self) -> Self {
        Self {
            ed: self.ed.clone(),
            size: self.size,
            limit: false,
            depth: self.depth,
        }
    }

    pub fn nest_in_depth(&self) -> Self {
        Self {
            ed: self.ed.clone(),
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
            ed: self.ed.clone(),
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

pub enum LangTransferValueInner {
    Luau(mluau::Value),
    #[cfg(feature = "embedder_json")]
    RawJson(Box<JsonRawValue>),
    #[cfg(feature = "embedder_json")]
    Json(JsonValue),
}

/// An LangTransferValue is a opaque userdata object that can be used to quickly transfer data
/// from the Luau side to JS side (often without needing to parse it into a LuaValue first before sending
/// to JS) and convert it there. This is useful for large JSON blobs, as it avoids the overhead of
/// parsing and serializing the JSON multiple times and the memory overhead of holding multiple copies
/// of the data in memory (both in Luau and in JS).
/// 
/// The other use case of LangTransferValue is to enable embedders to transfer data from Luau to JS while 
/// avoiding having its size counted within the proxy's effective size code (TODO: this does not currently work)
pub struct LangTransferValue {
    pub(crate) inner: Rc<RefCell<Option<LangTransferValueInner>>>,
}

impl LangTransferValue {
    /// Creates a new LangTransferValue from a RawValue
    #[cfg(feature = "embedder_json")]
    pub fn new_raw(value: Box<JsonRawValue>) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Some(LangTransferValueInner::RawJson(value)))),
        }
    }

    /// Creates a new LangTransferValue from a owned JsonValue
    #[cfg(feature = "embedder_json")]
    pub fn new_owned(value: JsonValue) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Some(LangTransferValueInner::Json(value)))),
        }
    }

    /// Creates a new LangTransferValue from a Luau value
    pub fn new_luau(value: mluau::Value) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Some(LangTransferValueInner::Luau(value)))),
        }
    }

    pub fn take(&self) -> Option<LangTransferValueInner> {
        self.inner.borrow_mut().take()
    }
}

// Empty userdata impl
#[cfg(feature = "embedder_json")]
impl mluau::UserData for LangTransferValue {}