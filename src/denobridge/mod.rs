// TODO: Switch to using LuaBridge to enable luau and v8 to be on different threads

pub(crate) mod bridge; // bridge client/server
pub(crate) mod objreg; // object registry for v8 non-primitive objects
pub mod primitives; // primitive types that can be proxied
pub mod psuedoprimitive; // psuedoprimitive types that are not fully primitive (not immutable in both v8 and luau) but are copied between v8 and luau
pub mod value; // proxied values
pub mod modloader; // module loader for luaufusion based on deno staticmoduleloader for vfs support
pub mod luauobjs; // luau objects that proxy to v8
pub(super) mod inner; // internal state management
pub(super) mod bridgeops; // deno ops for bridge
pub(super) mod denoexts;
pub mod embedder_api; // embedder api specific code

pub use bridge::V8IsolateManagerServer;
