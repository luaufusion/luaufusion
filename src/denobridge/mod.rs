// TODO: Switch to using LuaBridge to enable luau and v8 to be on different threads

pub(crate) mod bridge; // bridge client/server
pub mod modloader; // module loader for luaufusion based on deno staticmoduleloader for vfs support
pub mod snapshot; // snapshot generation for luaufusion+deno_core v8 isolates (mostly useful for generating prebuilt luaufusion snapshot)
pub mod inner; // internal state management
pub mod event; 
pub(super) mod denoexts;

pub use bridge::{V8IsolateManagerServer, run_v8_process_client};