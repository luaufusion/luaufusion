#[cfg(feature = "embedder_json")]
use std::cell::RefCell;
#[cfg(feature = "embedder_json")]
use std::rc::Rc;

#[cfg(feature = "embedder_json")]
use serde_json::Value as JsonValue;
#[cfg(feature = "embedder_json")]
use serde_json::value::RawValue as JsonRawValue;

#[cfg(feature = "embedder_json")]
pub enum EmbeddableJsonInner {
    Raw(Box<JsonRawValue>),
    Owned(JsonValue),
}

/// An EmbeddableJson is a opaque userdata object that can be used to quickly transfer raw JSON data
/// from the Luau side to JS side without needing to parse it into a LuaValue first and then send the LuaValue
/// to JS and convert it there. This is useful for large JSON blobs, as it avoids the overhead of
/// parsing and serializing the JSON multiple times and the memory overhead of holding multiple copies
/// of the data in memory (both in Luau and in JS).
#[cfg(feature = "embedder_json")]
pub struct EmbeddableJson {
    pub(crate) inner: Rc<RefCell<Option<EmbeddableJsonInner>>>,
    pub(crate) check_chars_limit: bool,
}

#[cfg(feature = "embedder_json")]
impl EmbeddableJson {
    /// Creates a new EmbeddableJson from a RawValue
    /// 
    /// If `check_chars_limit` is true, will check that the total number of string characters
    /// does not exceed MAX_OWNED_V8_STRING_SIZE (this is useful if the JSON is coming from
    /// an untrusted source, such as over the network).
    pub fn new_raw(value: Box<JsonRawValue>, check_chars_limit: bool) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Some(EmbeddableJsonInner::Raw(value)))),
            check_chars_limit,
        }
    }

    /// Creates a new EmbeddableJson from a owned JsonValue
    /// 
    /// If `check_chars_limit` is true, will check that the total number of string characters
    /// does not exceed MAX_OWNED_V8_STRING_SIZE (this is useful if the JSON is coming from
    /// an untrusted source, such as over the network).
    pub fn new_owned(value: JsonValue, check_chars_limit: bool) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Some(EmbeddableJsonInner::Owned(value)))),
            check_chars_limit,
        }
    }

    pub fn take(&self) -> Option<EmbeddableJsonInner> {
        self.inner.borrow_mut().take()
    }
}

// Empty userdata impl
#[cfg(feature = "embedder_json")]
impl mluau::UserData for EmbeddableJson {}