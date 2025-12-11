use crate::denobridge::{inner::V8IsolateManagerInner, modloader::FusionModuleLoader};

/// Creates a V8 snapshot for luaufusion+deno_core isolates
pub fn create_v8_snapshot() -> Vec<u8> {
    let rt = V8IsolateManagerInner::setup_runtime_for_snapshot(FusionModuleLoader::default());
    rt.deno.snapshot().into_vec()
}
