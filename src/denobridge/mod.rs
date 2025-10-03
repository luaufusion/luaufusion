// TODO: Switch to using LuaBridge to enable luau and v8 to be on different threads

pub mod bridge; // bridge client/server
pub mod objreg; // object registry for v8 non-primitive objects
pub mod primitives; // primitive types that can be proxied
pub mod value; // proxied values
pub mod modloader; // module loader for luaufusion based on deno staticmoduleloader for vfs support
pub mod luauobjs; // luau objects that proxy to v8
pub mod inner; // internal state management
pub mod bridgeops; // deno ops for bridge
pub(crate) mod denoexts;

pub use bridge::V8IsolateManagerServer;
