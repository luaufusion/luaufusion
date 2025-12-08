use std::cell::RefCell;
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::MAX_PROXY_DEPTH;
use crate::base::{Error, ProxyBridge, ProxyBridgeWithStringExt};

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Embedder specific configuration data
pub struct EmbedderData {
    /// Heap limit
    pub heap_limit: usize,
    /// Maximum number of bytes allowed for the payload
    pub max_payload_size: Option<usize>,
}

impl EmbedderData {
    /// Creates a new EmbedderData with default values
    pub fn new(heap_limit: usize, max_payload_size: Option<usize>) -> Self {
        Self {
            heap_limit,
            max_payload_size,
        }
    }
}

pub struct EmbedderDataContext {
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

/// An TransferValue is a opaque userdata object that can be used to transfer data
/// from the Luau side to the foreign language side while avoiding having its size counted within the proxy's effective size code
pub struct SourceTransferValue<T: ProxyBridge> {
    pub(crate) inner: Rc<RefCell<Option<T::ValueType>>>,
}

impl<T: ProxyBridge> SourceTransferValue<T> {
    /// Creates a new SourceTransferValue from a T::ValueType
    pub fn new(value: T::ValueType) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Some(value))),
        }
    }

    /// Creates a new SourceTransferValue from a raw string
    pub fn from_str(s: String) -> Self 
    where T: ProxyBridgeWithStringExt,
    {
        Self::new(
            T::from_string(s)
        )
    }

    pub fn take(&self) -> Option<T::ValueType> {
        self.inner.borrow_mut().take()
    }
}

impl<T: ProxyBridge> mluau::UserData for SourceTransferValue<T> {}
