#[cfg(feature = "embedder_json")]
use std::cell::RefCell;
#[cfg(feature = "embedder_json")]
use std::rc::Rc;

use serde::{Deserialize, Serialize};
#[cfg(feature = "embedder_json")]
use serde_json::Value as JsonValue;
#[cfg(feature = "embedder_json")]
use serde_json::value::RawValue as JsonRawValue;

use crate::base::Error;
use crate::denobridge::bridge::MIN_HEAP_LIMIT;

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Embedder specific configuration data
pub struct EmbedderData {
    /// Heap limit
    pub heap_limit: usize,
    /// Maximum number of bytes allowed for a single value within the payload
    pub max_value_size: Option<usize>,
    /// Maximum number of bytes allowed for a whole (constructed) value
    pub max_constructed_value_size: Option<usize>,
    /// Maximum number of function arguments that can be passed to a function call
    pub max_function_args: Option<usize>,
}

impl Default for EmbedderData {
    fn default() -> Self {
        Self {
            heap_limit: MIN_HEAP_LIMIT,
            max_value_size: None,
            max_constructed_value_size: None,
            max_function_args: Some(32),
        }
    }
}

impl EmbedderData {
    pub(crate) fn check_value_len(&self, len: usize, typ: &'static str) -> Result<(), Error> {
        if let Some(max) = self.max_value_size {
            if len > max {
                return Err(format!("Bytes exceeds maximum allowed size of {} bytes (sizeof {}) [at {}]", max, len, typ).into());
            }
        }
        Ok(())
    }

    pub(crate) fn check_constructed_value_len(&self, len: usize, typ: &'static str) -> Result<(), Error> {
        if let Some(max) = self.max_constructed_value_size {
            if len > max {
                return Err(format!("Constructed value exceeds maximum allowed size of {} bytes (sizeof {}) [at {}]", max, len, typ).into());
            }
        }
        Ok(())
    }

    pub(crate) fn check_function_args_len(&self, len: usize) -> Result<(), Error> {
        if let Some(max) = self.max_function_args {
            if len > max {
                return Err(format!("Too many function arguments passed (max {})", max).into());
            }
        }
        Ok(())
    }
}

pub enum LangTransferValueInner {
    Transfer(mluau::Value),
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
            inner: Rc::new(RefCell::new(Some(LangTransferValueInner::Transfer(value)))),
        }
    }

    pub fn take(&self) -> Option<LangTransferValueInner> {
        self.inner.borrow_mut().take()
    }
}

// Empty userdata impl
#[cfg(feature = "embedder_json")]
impl mluau::UserData for LangTransferValue {}