pub(crate) mod extension;
pub(crate) mod base64_ops;

use deno_core::v8::CreateParams;
use deno_core::v8 as v8;
use tokio_util::sync::CancellationToken;

/// Internal manager for a single V8 isolate with a minimal Deno runtime.
pub struct V8IsolateManager {
    deno: deno_core::JsRuntime,
    cancellation_token: CancellationToken,
}

const MIN_HEAP_LIMIT: usize = 5 * 1024 * 1024; // 5MB

impl V8IsolateManager {
    pub fn new(heap_limit: usize) -> Self {
        let heap_limit = heap_limit.max(MIN_HEAP_LIMIT); // Minimum 5MB

        // TODO: Support snapshots maybe
        let extensions = extension::all_extensions(false);

        let mut deno = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
            create_params: Some(
                CreateParams::default()
                .heap_limits(0, heap_limit)
            ),
            extensions,
            ..Default::default()
        });

        let isolate_handle = deno.v8_isolate().thread_safe_handle();
        let heap_exhausted_token = CancellationToken::new();

        // Add a callback to terminate the runtime if the max_heap_size limit is approached
        let heap_exhausted_token_ref = heap_exhausted_token.clone();
        deno.add_near_heap_limit_callback(move |current_value, _| {
            isolate_handle.terminate_execution();

            // Signal the outer runtime to cancel block_on future (avoid hanging) and return friendly error
            heap_exhausted_token_ref.cancel();

            // Spike the heap limit while terminating to avoid segfaulting
            // Callback may fire multiple times if memory usage increases quicker then termination finalizes
            5 * current_value
        });

        Self { deno, cancellation_token: heap_exhausted_token }
    }

    /// Internal helper method to copy a property from one context to another
    fn copy_global_property_from_context_to_context(
        isolate: &mut v8::Isolate,
        source_context_handle: &v8::Global<v8::Context>,
        dest_context_handle: &v8::Global<v8::Context>,
        property_name: &str,
    ) {
        let handle_scope = &mut v8::HandleScope::new(isolate);

        // Get local handles to the contexts.
        let source_context = v8::Local::new(handle_scope, source_context_handle);
        let dest_context = v8::Local::new(handle_scope, dest_context_handle);

        let key = v8::String::new(handle_scope, property_name).unwrap();

        // Read the value from the source context.
        let value_to_copy = {
            let scope = &mut v8::ContextScope::new(handle_scope, source_context);
            let global = source_context.global(scope);
            global.get(scope, key.into()).unwrap()
        }; // The source context scope ends here.

        // Write the value to the destination context.
        {
            let scope = &mut v8::ContextScope::new(handle_scope, dest_context);
            let global = dest_context.global(scope);
            global.set(scope, key.into(), value_to_copy).unwrap();
        } // The destination context scope ends here.
    }

    fn create_context(&mut self) -> v8::Global<v8::Context> {
        let new_ctx = {
            let isolate = self.deno.v8_isolate();
            let scope = &mut v8::HandleScope::new(isolate);
            let context = v8::Context::new(scope, v8::ContextOptions::default());

            v8::Global::new(scope, context)
        };

        {
            for prop in [
                "URL", 
                "URLPattern",
                "URLSearchParams",
                "atob", 
                "btoa",
                "window", 
            ] {
                let main_context = self.deno.main_context();
                Self::copy_global_property_from_context_to_context(
                    self.deno.v8_isolate(),
                    &main_context,
                    &new_ctx,
                    prop
                );
            }
        }

        new_ctx
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_v8_isolate_manager() {
        let mut manager = super::V8IsolateManager::new(super::MIN_HEAP_LIMIT);
        let ctx = manager.create_context();
    }
}