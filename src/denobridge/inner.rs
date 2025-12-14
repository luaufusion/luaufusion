use super::bridge::V8IsolateManagerServer;

use std::rc::Rc;

//use deno_core::error::{CoreError, CoreErrorKind};
use deno_core::v8::CreateParams;
use deno_core::v8;
use tokio_util::sync::CancellationToken;

use super::{
    modloader::FusionModuleLoader,
};
use crate::denobridge::objreg::V8ObjectRegistry;
use crate::luau::bridge::LuaBridgeServiceClient;
use crate::luau::embedder_api::EmbedderData;

use super::bridge::{ProxyV8Client, MIN_HEAP_LIMIT};
use super::denoexts;

#[cfg(feature = "deno_include_snapshot")]
const V8_SNAPSHOT: &[u8] = include_bytes!("snapshot.bin");
#[cfg(all(feature = "deno_include_snapshot", any(not(target_os = "linux"), not(target_arch = "x86_64"))))]
const _: () = {
    compile_error!("Including V8 snapshot in binary is only supported on Linux x86_64 targets at this time.");
};

#[derive(Clone)]
pub struct CommonState {
    pub(super) bridge: LuaBridgeServiceClient<V8IsolateManagerServer>,
    pub(super) proxy_client: ProxyV8Client,
    pub(super) ed: EmbedderData,
}

/// Internal manager for a single V8 isolate with a minimal Deno runtime.
/// 
/// This should not be used directly, use V8IsolateManager instead
/// which uses a tokio task w/ channel to communicate with the isolate manager.
pub struct V8IsolateManagerInner {
    pub deno: deno_core::JsRuntime,
    pub cancellation_token: CancellationToken,
    pub common_state: CommonState
}

pub struct SetupRuntime {
    pub deno: deno_core::JsRuntime,
    pub obj_registry: V8ObjectRegistry,
    pub heap_exhausted_token: CancellationToken,
}

pub struct SetupRuntimeForSnapshot {
    pub deno: deno_core::JsRuntimeForSnapshot,
    pub heap_exhausted_token: CancellationToken,
}

impl V8IsolateManagerInner {  
    /// Sets up a new Deno runtime with the specified embedder data and module loader  
    pub fn setup_runtime_for_snapshot(loader: FusionModuleLoader) -> SetupRuntimeForSnapshot {
        let extensions = denoexts::extension::all_extensions(false);

        deno_core::v8::V8::set_flags_from_string("--harmony-import-assertions --harmony-import-attributes --jitless");

        let mut deno = deno_core::JsRuntimeForSnapshot::new(deno_core::RuntimeOptions {
            extensions,
            module_loader: Some(Rc::new(loader)),
            inspector: false,
            import_assertions_support: deno_core::ImportAssertionsSupport::Yes,
            ..Default::default()
        });

        let isolate_handle = deno.v8_isolate().thread_safe_handle();
        let heap_exhausted_token = CancellationToken::new();

        // Add a callback to terminate the runtime if the max_heap_size limit is approached
        let heap_exhausted_token_ref = heap_exhausted_token.clone();
        deno.add_near_heap_limit_callback(move |current_value, _| {
            eprintln!("V8 heap limit approached: {} bytes used", current_value);
            isolate_handle.terminate_execution();

            // Signal the outer runtime to cancel block_on future (avoid hanging) and return friendly error
            heap_exhausted_token_ref.cancel();

            // Spike the heap limit while terminating to avoid segfaulting
            // Callback may fire multiple times if memory usage increases quicker then termination finalizes
            5 * current_value
        });

        SetupRuntimeForSnapshot {
            deno,
            heap_exhausted_token,
        }
    }

    /// Sets up a new Deno runtime with the specified embedder data and module loader  
    pub fn setup_runtime(ed: &EmbedderData, loader: FusionModuleLoader) -> SetupRuntime {
        let heap_limit = ed.heap_limit.max(MIN_HEAP_LIMIT);

        #[cfg(feature = "deno_include_snapshot")]
        let extensions = denoexts::extension::all_extensions(true);
        #[cfg(not(feature = "deno_include_snapshot"))]
        let extensions = denoexts::extension::all_extensions(false);

        deno_core::v8::V8::set_flags_from_string("--harmony-import-assertions --harmony-import-attributes --jitless");

        #[cfg(feature = "deno_include_snapshot")]
        {
            assert!(V8_SNAPSHOT.len() > 0, "V8 snapshot is empty but deno_include_snapshot feature is enabled");
        }

        let mut deno = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
            create_params: Some(
                CreateParams::default()
                .heap_limits(0, heap_limit)
            ),
            #[cfg(feature = "deno_include_snapshot")]
            startup_snapshot: Some(V8_SNAPSHOT),
            extensions,
            module_loader: Some(Rc::new(loader)),
            inspector: false,
            import_assertions_support: deno_core::ImportAssertionsSupport::Yes,
            ..Default::default()
        });

        let isolate_handle = deno.v8_isolate().thread_safe_handle();
        let heap_exhausted_token = CancellationToken::new();

        // Add a callback to terminate the runtime if the max_heap_size limit is approached
        let heap_exhausted_token_ref = heap_exhausted_token.clone();
        deno.add_near_heap_limit_callback(move |current_value, _| {
            eprintln!("V8 heap limit approached: {} bytes used", current_value);
            isolate_handle.terminate_execution();

            // Signal the outer runtime to cancel block_on future (avoid hanging) and return friendly error
            heap_exhausted_token_ref.cancel();

            // Spike the heap limit while terminating to avoid segfaulting
            // Callback may fire multiple times if memory usage increases quicker then termination finalizes
            5 * current_value
        });

        let obj_registry = {
            let main_ctx = deno.main_context();
            let isolate = deno.v8_isolate();
            let scope = std::pin::pin!(v8::HandleScope::new(isolate));
            let scope = &mut scope.init();
            let main_ctx = v8::Local::new(scope, main_ctx);
            let scope = &mut v8::ContextScope::new(scope, main_ctx);
            V8ObjectRegistry::new(scope)
        };

        SetupRuntime {
            deno,
            obj_registry,
            heap_exhausted_token,
        }
    }

    pub fn new(bridge: LuaBridgeServiceClient<V8IsolateManagerServer>, ed: EmbedderData, loader: FusionModuleLoader) -> Self {
        let runtime = Self::setup_runtime(&ed, loader);

        let common_state = CommonState {
            bridge,
            proxy_client: ProxyV8Client {
                obj_registry: runtime.obj_registry,
            },
            ed
        };

        runtime.deno.op_state().borrow_mut().put(common_state.clone());

        Self {
            deno: runtime.deno,
            cancellation_token: runtime.heap_exhausted_token,
            common_state
        }
    }
}
