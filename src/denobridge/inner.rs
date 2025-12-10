use super::bridge::{BridgeVals, V8IsolateManagerServer};

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

//use deno_core::error::{CoreError, CoreErrorKind};
use deno_core::v8::CreateParams;
use deno_core::v8;
use tokio_util::sync::CancellationToken;

use super::{
    modloader::FusionModuleLoader,
    value::ProxiedV8Value
};
use crate::luau::bridge::LuaBridgeServiceClient;
use crate::luau::embedder_api::EmbedderData;

use super::bridge::{ProxyV8Client, MIN_HEAP_LIMIT};
use super::denoexts;

/// Stores a lua function state
/// 
/// This is used internally to track async function call states
pub(super) enum FunctionRunState {
    Created {
        args: Vec<ProxiedV8Value>,
    },
    Executed {
        resp: Vec<ProxiedV8Value>,
    },
}

#[derive(Clone)]
pub(crate) struct CommonState {
    pub(super) list: Rc<RefCell<HashMap<i32, FunctionRunState>>>,
    pub(super) bridge: LuaBridgeServiceClient<V8IsolateManagerServer>,
    pub(super) obj_template: Rc<v8::Global<v8::ObjectTemplate>>,
    pub(super) proxy_client: ProxyV8Client,
    pub(super) bridge_vals: Rc<BridgeVals>,
    pub(super) ed: EmbedderData,
}

/// Internal manager for a single V8 isolate with a minimal Deno runtime.
/// 
/// This should not be used directly, use V8IsolateManager instead
/// which uses a tokio task w/ channel to communicate with the isolate manager.
pub(super) struct V8IsolateManagerInner {
    pub(super) deno: deno_core::JsRuntime,
    pub(super) cancellation_token: CancellationToken,
    pub(super) common_state: CommonState
}

impl V8IsolateManagerInner {    
    pub fn new(bridge: LuaBridgeServiceClient<V8IsolateManagerServer>, ed: EmbedderData, loader: FusionModuleLoader) -> Self {
        let heap_limit = ed.heap_limit.max(MIN_HEAP_LIMIT);

        // TODO: Support snapshots maybe
        let extensions = denoexts::extension::all_extensions(false);

        deno_core::v8::V8::set_flags_from_string("--harmony-import-assertions --harmony-import-attributes --jitless");

        let mut deno = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
            create_params: Some(
                CreateParams::default()
                .heap_limits(0, heap_limit)
            ),
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
            println!("V8 heap limit approached: {} bytes used", current_value);
            isolate_handle.terminate_execution();

            // Signal the outer runtime to cancel block_on future (avoid hanging) and return friendly error
            heap_exhausted_token_ref.cancel();

            // Spike the heap limit while terminating to avoid segfaulting
            // Callback may fire multiple times if memory usage increases quicker then termination finalizes
            5 * current_value
        });

        let obj_template = Rc::new({
            let isolate = deno.v8_isolate();
            let scope = std::pin::pin!(v8::HandleScope::new(isolate));
            let scope = &mut scope.init();
            let template = Self::create_obj_template(scope);
            v8::Global::new(scope, template)
        });

        let bridge_vals = {
            let main_ctx = deno.main_context();
            let isolate = deno.v8_isolate();
            let scope = std::pin::pin!(v8::HandleScope::new(isolate));
            let scope = &mut scope.init();
            let main_ctx = v8::Local::new(scope, main_ctx);
            let scope = &mut v8::ContextScope::new(scope, main_ctx);
            BridgeVals::new(scope)
        };

        let common_state = CommonState {
            list: Rc::new(RefCell::new(HashMap::new())),
            obj_template,
            bridge,
            proxy_client: ProxyV8Client {
                obj_registry: bridge_vals.obj_registry.clone(),
            },
            bridge_vals: Rc::new(bridge_vals),
            ed: ed.clone()
        };

        deno.op_state().borrow_mut().put(common_state.clone());

        Self {
            deno,
            cancellation_token: heap_exhausted_token,
            common_state
        }
    }

    fn create_obj_template<'s>(scope: &mut v8::PinScope<'s, '_, ()>) -> v8::Local<'s, v8::ObjectTemplate> {
        let template = v8::ObjectTemplate::new(scope);
        
        template
    }
}
