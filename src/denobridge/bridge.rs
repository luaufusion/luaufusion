use super::primitives::ProxiedV8Primitive;
use super::value::ProxiedV8Value;

use std::collections::HashMap;
use std::sync::Arc;

use concurrentlyexec::{ConcurrentExecutor, ConcurrentExecutorState, ConcurrentlyExecute, MultiSender, OneshotSender, ProcessOpts};
use deno_core::v8::GetPropertyNamesArgs;
//use deno_core::error::{CoreError, CoreErrorKind};
use deno_core::{PollEventLoopOptions, v8};
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
//use deno_core::{op2, OpState};
//use deno_error::JsErrorBox;
//use mluau::serde::de;

use crate::denobridge::modloader::FusionModuleLoader;
use crate::luau::bridge::{
    LuaBridgeMessage, LuaBridgeService, LuaBridgeServiceClient, ProxyLuaClient,
};

use crate::base::{ObjectRegistry, ObjectRegistryID, ProxyBridge, Error};
use super::inner::{CommonState, V8IsolateManagerInner};

/// Max size for a owned v8 string
pub const MAX_OWNED_V8_STRING_SIZE: usize = 1024 * 16; // 16KB
/// Minimum stack size for V8 isolates
pub const MIN_STACK_SIZE: usize = 1024 * 1024 * 15; // 15MB minimum memory
/// Minimum heap size for V8 isolates
pub const MIN_HEAP_LIMIT: usize = 10 * 1024 * 1024; // 10MB

pub(crate) struct BridgeVals {
    pub type_field: v8::Global<v8::String>,
    pub id_field: v8::Global<v8::String>,
    pub length_field: v8::Global<v8::String>,
    pub create_lua_object_from_data: v8::Global<v8::Function>,
    pub string_ref_field: v8::Global<v8::Symbol>,
}

impl BridgeVals {
    pub(crate) fn new<'s>(scope: &mut v8::HandleScope<'s>) -> Self {
        let id_field = v8::String::new(scope, "luaid").unwrap();
        let type_field = v8::String::new(scope, "luatype").unwrap();
        let length_field = v8::String::new(scope, "length").unwrap();

        // The createLuaObjectFromData function is stored in globalThis.lua.createLuaObjectFromData
        let (string_ref_field, create_lua_object_from_data) = {
            let global = scope.get_current_context().global(scope);
            let lua_str = v8::String::new(scope, "lua").unwrap();
            let lua_obj = global.get(scope, lua_str.into()).unwrap();
            //println!("lua_obj: {:?}", lua_obj.to_rust_string_lossy(scope));
            assert!(lua_obj.is_object());
            let lua_obj = lua_obj.to_object(scope).unwrap();
            let clofd = v8::String::new(scope, "createLuaObjectFromData").unwrap();
            let create_lua_object_from_data = lua_obj.get(scope, clofd.into()).unwrap();
            assert!(create_lua_object_from_data.is_function());
            let create_lua_object_from_data = v8::Local::<v8::Function>::try_from(create_lua_object_from_data).unwrap();
            let srfk = v8::String::new(scope, "__stringref").unwrap();
            let string_ref_field = lua_obj.get(scope, srfk.into()).unwrap();
            assert!(string_ref_field.is_symbol());
            let string_ref_field = v8::Local::<v8::Symbol>::try_from(string_ref_field).unwrap();
            (string_ref_field, create_lua_object_from_data)
        };

        Self {
            id_field: v8::Global::new(scope, id_field),
            type_field: v8::Global::new(scope, type_field),
            length_field: v8::Global::new(scope, length_field),
            string_ref_field: v8::Global::new(scope, string_ref_field),
            create_lua_object_from_data: v8::Global::new(scope, create_lua_object_from_data),
        }
    }
}

/// Marker struct for V8 objects in the object registry
#[derive(Clone, Copy)]
pub struct V8BridgeObject;

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum V8ObjectRegistryType {
    ArrayBuffer,
    String,
    Object,
    Array,
    Function,
    Promise,
}

pub fn v8_obj_registry_type_to_i32(typ: V8ObjectRegistryType) -> i32 {
    match typ {
        V8ObjectRegistryType::ArrayBuffer => 0,
        V8ObjectRegistryType::String => 1,
        V8ObjectRegistryType::Object => 2,
        V8ObjectRegistryType::Array => 3,
        V8ObjectRegistryType::Function => 4,
        V8ObjectRegistryType::Promise => 5,
    }
}

pub fn i32_to_v8_obj_registry_type(i: i32) -> Option<V8ObjectRegistryType> {
    match i {
        0 => Some(V8ObjectRegistryType::ArrayBuffer),
        1 => Some(V8ObjectRegistryType::String),
        2 => Some(V8ObjectRegistryType::Object),
        3 => Some(V8ObjectRegistryType::Array),
        4 => Some(V8ObjectRegistryType::Function),
        5 => Some(V8ObjectRegistryType::Promise),
        _ => None,
    }
}

#[derive(Clone)]
/// The client side state for proxying Lua values
/// 
/// This struct is not thread safe and must be kept on the Lua side
pub struct ProxyV8Client {
    pub array_buffer_registry: ObjectRegistry<v8::Global<v8::ArrayBuffer>, V8BridgeObject>,
    pub string_registry: ObjectRegistry<v8::Global<v8::String>, V8BridgeObject>,
    pub array_registry: ObjectRegistry<v8::Global<v8::Array>, V8BridgeObject>,
    pub obj_registry: ObjectRegistry<v8::Global<v8::Object>, V8BridgeObject>,
    pub func_registry: ObjectRegistry<v8::Global<v8::Function>, V8BridgeObject>,
    pub promise_registry: ObjectRegistry<v8::Global<v8::Promise>, V8BridgeObject>,
}

#[derive(Serialize, Deserialize)]
/// Internal representation of a message that can be sent to the V8 isolate manager
pub(super) enum V8IsolateManagerMessage {
    CodeExec {
        modname: String,
        resp: OneshotSender<Result<ProxiedV8Value, String>>,
    },
    ObjectProperties {
        obj_id: ObjectRegistryID<V8BridgeObject>,
        own_props: bool,
        resp: OneshotSender<Result<ProxiedV8Value, String>>,
    },
    ObjectGetProperty {
        obj_id: ObjectRegistryID<V8BridgeObject>,
        key: ProxiedV8Primitive,
        resp: OneshotSender<Result<ProxiedV8Value, String>>,
    },
    DropObject {
        obj_type: V8ObjectRegistryType,
        obj_id: ObjectRegistryID<V8BridgeObject>,
    },
    Shutdown,
}

/// A message that can be sent to the V8 isolate manager
pub(super) trait V8IsolateSendableMessage: Send + 'static {
    type Response: Send + serde::Serialize + for<'de> serde::Deserialize<'de> + 'static;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage;
}

pub(super) struct CodeExecMessage {
    pub modname: String,
}

impl V8IsolateSendableMessage for CodeExecMessage {
    type Response = Result<ProxiedV8Value, String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::CodeExec {
            modname: self.modname,
            resp,
        }
    }
}

pub(super) struct ObjectPropertiesMessage {
    pub obj_id: ObjectRegistryID<V8BridgeObject>,
    pub own_props: bool,
}

impl V8IsolateSendableMessage for ObjectPropertiesMessage {
    type Response = Result<ProxiedV8Value, String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::ObjectProperties {
            obj_id: self.obj_id,
            own_props: self.own_props,
            resp,
        }
    }
}

pub(super) struct ObjectGetPropertyMessage {
    pub obj_id: ObjectRegistryID<V8BridgeObject>,
    pub key: ProxiedV8Primitive,
}

impl V8IsolateSendableMessage for ObjectGetPropertyMessage {
    type Response = Result<ProxiedV8Value, String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::ObjectGetProperty {
            obj_id: self.obj_id,
            key: self.key,
            resp,
        }
    }
}

pub(super) struct DropObjectMessage {
    pub obj_type: V8ObjectRegistryType,
    pub obj_id: ObjectRegistryID<V8BridgeObject>,
}

impl V8IsolateSendableMessage for DropObjectMessage {
    type Response = ();
    fn to_message(self, _resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::DropObject {
            obj_type: self.obj_type,
            obj_id: self.obj_id,
        }
    }
}

pub(super) struct ShutdownMessage;

impl V8IsolateSendableMessage for ShutdownMessage {
    type Response = ();
    fn to_message(self, _resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::Shutdown
    }
}

#[derive(Clone)]
pub struct V8IsolateManagerClient {}

impl V8IsolateManagerClient {
    fn obj_op<'s, R>(
        inner: &'s mut V8IsolateManagerInner, 
        obj_id: ObjectRegistryID<V8BridgeObject>,
        func: impl FnOnce(&mut v8::HandleScope<'s>, &CommonState, v8::Local<'s, v8::Object>) -> Result<R, crate::base::Error>,
    ) -> Result<R, crate::base::Error> {
        let obj = match inner.common_state.proxy_client.obj_registry.get(obj_id) {
            Some(o) => o,
            None => {
                return Err(format!("Object ID {} not found", obj_id).into());
            }
        };
        let result = {
            let main_ctx = inner.deno.main_context();
            let isolate = inner.deno.v8_isolate();
            let mut scope = v8::HandleScope::new(isolate);
            let main_ctx = v8::Local::new(&mut scope, main_ctx);
            let scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
            let obj = v8::Local::new(scope, obj.clone());
            func(scope, &inner.common_state, obj)
        };
        result.map_err(|e| format!("V8 object operation failed: {}", e).into())
    }
}

#[derive(Serialize, Deserialize)]
pub struct V8BootstrapData {
    heap_limit: usize,
    messenger_tx: OneshotSender<MultiSender<V8IsolateManagerMessage>>,
    lua_bridge_tx: MultiSender<LuaBridgeMessage<V8IsolateManagerServer>>,
    vfs: HashMap<String, String>,
}

impl ConcurrentlyExecute for V8IsolateManagerClient {
    type BootstrapData = V8BootstrapData;
    async fn run(
        data: Self::BootstrapData,
        client_ctx: concurrentlyexec::ClientContext
    ) {
        let (tx, mut rx) = client_ctx.multi();
        data.messenger_tx.client(&client_ctx).send(tx).unwrap();

        let mut inner = V8IsolateManagerInner::new(
            LuaBridgeServiceClient::new(client_ctx.clone(), data.lua_bridge_tx),
            data.heap_limit,
            FusionModuleLoader::new(data.vfs.into_iter().map(|(x, y)| (x, y.into())))
        );

        let mut evaluated_modules = HashMap::new();
        let mut module_evaluate_queue = FuturesUnordered::new();

        loop {
            tokio::select! {
                Ok(msg) = rx.recv() => {
                    match msg {
                        V8IsolateManagerMessage::CodeExec { modname, resp } => {
                            /*let nargs = {
                                let mut nargs = Vec::with_capacity(args.len());

                                let main_ctx = inner.deno.main_context();
                                let isolate = inner.deno.v8_isolate();
                                let mut scope = v8::HandleScope::new(isolate);
                                let main_ctx = v8::Global::new(&mut scope, main_ctx);
                                let main_ctx = v8::Local::new(&mut scope, main_ctx);
                                let context_scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
                                let mut err = None;
                                for arg in args {
                                    match arg.proxy_to_v8(context_scope, &inner.common_state) {
                                        Ok(arg) => {
                                            let global_arg = v8::Global::new(context_scope, arg);
                                            nargs.push(global_arg);
                                        },
                                        Err(e) => {
                                            err = Some(e);
                                            break;
                                        }
                                    };
                                }

                                if let Some(err) = err {
                                    let _ = resp.client(&client_ctx).send(Err(err.to_string()));
                                    continue;
                                }

                                nargs
                            };*/

                            if let Some(module_id) = evaluated_modules.get(&modname) {
                                // Module already evaluated, just return the namespace object
                                let namespace_obj = match inner.deno.get_module_namespace(*module_id) {
                                    Ok(obj) => obj,
                                    Err(e) => {
                                        let _ = resp.client(&client_ctx).send(Err(format!("Failed to get module namespace: {}", e).into()));
                                        continue;
                                    }
                                };
                                // Proxy the namespace object to a ProxiedV8Value
                                let proxied = {
                                    let main_ctx = inner.deno.main_context();
                                    let isolate = inner.deno.v8_isolate();
                                    let mut scope = v8::HandleScope::new(isolate);
                                    let main_ctx = v8::Local::new(&mut scope, main_ctx);
                                    let context_scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
                                    let namespace_obj = v8::Local::new(context_scope, namespace_obj);
                                    match ProxiedV8Value::proxy_from_v8(context_scope, namespace_obj.into(), &inner.common_state, 0) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            let _ = resp.client(&client_ctx).send(Err(format!("Failed to proxy module namespace: {}", e).into()));
                                            continue;
                                        }
                                    }
                                };

                                let _ = resp.client(&client_ctx).send(Ok(proxied));
                                continue;
                            }

                            let url = match deno_core::url::Url::parse(&format!("file:///{modname}")) {
                                Ok(u) => u,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to parse module name as URL: {}", e).into()));
                                    continue;
                                }
                            };

                            let id = match inner.deno.load_side_es_module(&url).await {
                                Ok(id) => id,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to load module: {}", e).into()));
                                    continue;
                                }
                            };
                            let fut = inner.deno.mod_evaluate(id);
                            module_evaluate_queue.push(async move {
                                let module_id = fut.await.map(|_| id);
                                (modname, module_id, resp)
                            });
                        },
                        V8IsolateManagerMessage::ObjectProperties { obj_id, own_props, resp } => {
                            match Self::obj_op(&mut inner, obj_id, |scope, common_state, obj| {
                                // TODO: Allow customizing GetPropertyNamesArgs 
                                let props = if own_props {
                                    obj.get_own_property_names(scope, GetPropertyNamesArgs::default())
                                    .ok_or("Failed to get object property names")?
                                } else {
                                    obj.get_property_names(scope, GetPropertyNamesArgs::default())
                                    .ok_or("Failed to get object property names")?
                                };
                                ProxiedV8Value::proxy_from_v8(scope, props.into(), common_state, 0)
                            }) {
                                Ok(v) => {
                                    let _ = resp.client(&client_ctx).send(Ok(v));
                                }
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(e.to_string()));
                                }
                            }
                        },
                        V8IsolateManagerMessage::ObjectGetProperty { obj_id, key, resp } => {
                            match Self::obj_op(&mut inner, obj_id, |scope, common_state, obj| {
                                let key = key.to_v8(scope)?;
                                let key = v8::Local::new(scope, key);
                                match obj.get(scope, key) {
                                    Some(v) => ProxiedV8Value::proxy_from_v8(scope, v, common_state, 0),
                                    None => Err("Failed to get object property".into()),
                                }
                            }) {
                                Ok(v) => {
                                    let _ = resp.client(&client_ctx).send(Ok(v));
                                }
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(e.to_string()));
                                }
                            }
                        },
                        V8IsolateManagerMessage::Shutdown => {
                            break;
                        }
                        V8IsolateManagerMessage::DropObject { obj_type, obj_id } => {
                            match obj_type {
                                V8ObjectRegistryType::ArrayBuffer => {
                                    inner.common_state.proxy_client.array_buffer_registry.remove(obj_id);
                                }
                                V8ObjectRegistryType::String => {
                                    inner.common_state.proxy_client.string_registry.remove(obj_id);
                                }
                                V8ObjectRegistryType::Array => {
                                    inner.common_state.proxy_client.array_registry.remove(obj_id);
                                }
                                V8ObjectRegistryType::Object => {
                                    inner.common_state.proxy_client.obj_registry.remove(obj_id);
                                }
                                V8ObjectRegistryType::Function => {
                                    inner.common_state.proxy_client.func_registry.remove(obj_id);
                                }
                                V8ObjectRegistryType::Promise => {
                                    inner.common_state.proxy_client.promise_registry.remove(obj_id);
                                }
                            }
                        }
                    }
                }
                _ = inner.cancellation_token.cancelled() => {
                    break;
                }
                _ = inner.deno.run_event_loop(PollEventLoopOptions {
                    wait_for_inspector: false,
                    pump_v8_message_loop: true,
                }) => {
                    tokio::task::yield_now().await;
                },
                Some((modname, result, resp)) = module_evaluate_queue.next() => {
                    let Ok(result) = result else {
                        let _ = resp.client(&client_ctx).send(Err("Failed to evaluate module".to_string().into()));
                        continue;
                    };
                    evaluated_modules.insert(modname, result);
                    let namespace_obj = match inner.deno.get_module_namespace(result) {
                        Ok(obj) => obj,
                        Err(e) => {
                            let _ = resp.client(&client_ctx).send(Err(format!("Failed to get module namespace: {}", e).into()));
                            continue;
                        }
                    };
                    // Proxy the namespace object to a ProxiedV8Value
                    let proxied = {
                        let main_ctx = inner.deno.main_context();
                        let isolate = inner.deno.v8_isolate();
                        let mut scope = v8::HandleScope::new(isolate);
                        let main_ctx = v8::Local::new(&mut scope, main_ctx);
                        let context_scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
                        let namespace_obj = v8::Local::new(context_scope, namespace_obj);
                        {
                            let props = namespace_obj.get_own_property_names(context_scope, GetPropertyNamesArgs::default()).unwrap();
                            println!("Got namespace object: {:?}", props.to_rust_string_lossy(context_scope));
                        }
                        match ProxiedV8Value::proxy_from_v8(context_scope, namespace_obj.into(), &inner.common_state, 0) {
                            Ok(v) => v,
                            Err(e) => {
                                let _ = resp.client(&client_ctx).send(Err(format!("Failed to proxy module namespace: {}", e).into()));
                                continue;
                            }
                        }
                    };

                    let _ = resp.client(&client_ctx).send(Ok(proxied));
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct V8IsolateManagerServer {
    executor: Arc<ConcurrentExecutor<V8IsolateManagerClient>>,
    messenger: Arc<MultiSender<V8IsolateManagerMessage>>,
}

impl Drop for V8IsolateManagerServer {
    fn drop(&mut self) {
        let _ = self.messenger.server(self.executor.server_context()).send(V8IsolateManagerMessage::Shutdown);
        let _ = self.executor.shutdown();
    }
}

impl V8IsolateManagerServer {
    /// Create a new V8 isolate manager server
    async fn new(
        cs_state: ConcurrentExecutorState<V8IsolateManagerClient>, 
        heap_limit: usize, 
        process_opts: ProcessOpts,
        plc: ProxyLuaClient,
        vfs: HashMap<String, String>,
    ) -> Result<Self, crate::base::Error> {
        let (executor, (lua_bridge_rx, ms_rx)) = ConcurrentExecutor::new(
            cs_state,
            process_opts,
            move |cei| {
                let (tx, rx) = cei.create_multi();
                let (msg_tx, msg_rx) = cei.create_oneshot();
                (V8BootstrapData {
                    heap_limit,
                    messenger_tx: msg_tx,
                    lua_bridge_tx: tx,
                    vfs
                }, (rx, msg_rx))
            }
        ).await.map_err(|e| format!("Failed to create V8 isolate manager executor: {}", e))?;
        let messenger = ms_rx.recv().await.map_err(|e| format!("Failed to receive messenger: {}", e))?;

        let self_ret = Self { executor: Arc::new(executor), messenger: Arc::new(messenger) };
        let self_ref = self_ret.clone();
        tokio::task::spawn_local(async move {
            let lua_bridge_service = LuaBridgeService::new(
                self_ref,
                lua_bridge_rx,
            );
            lua_bridge_service.run(plc).await;
        });

        Ok(self_ret)
    }

    /// Send a message to the V8 isolate process and wait for a response
    pub(super) async fn send<T: V8IsolateSendableMessage>(&self, msg: T) -> Result<T::Response, crate::base::Error> {
        let (resp_tx, resp_rx) = self.executor.create_oneshot();
        self.messenger.server(self.executor.server_context()).send(msg.to_message(resp_tx))
            .map_err(|e| format!("Failed to send message to V8 isolate: {}", e))?;
        resp_rx.recv().await.map_err(|e| format!("Failed to receive response from V8 isolate: {}", e).into())
    }
}


impl ProxyBridge for V8IsolateManagerServer {
    type ValueType = ProxiedV8Value;
    type ConcurrentlyExecuteClient = V8IsolateManagerClient;

    fn name() -> &'static str {
        "v8"
    }

    async fn new(
        cs_state: ConcurrentExecutorState<Self::ConcurrentlyExecuteClient>, 
        heap_limit: usize, 
        process_opts: ProcessOpts,
        plc: ProxyLuaClient,
        vfs: HashMap<String, String>,
    ) -> Result<Self, crate::base::Error> {
        Self::new(cs_state, heap_limit, process_opts, plc, vfs).await        
    }

    fn get_executor(&self) -> Arc<ConcurrentExecutor<Self::ConcurrentlyExecuteClient>> {
        self.executor.clone()
    }

    fn to_source_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, plc: &ProxyLuaClient) -> Result<mluau::Value, Error> {
        Ok(value.proxy_to_src_lua(lua, plc, self).map_err(|e| e.to_string())?)
    }

    fn from_source_lua_value(&self, _lua: &mluau::Lua, plc: &ProxyLuaClient, value: mluau::Value) -> Result<Self::ValueType, crate::base::Error> {
        Ok(ProxiedV8Value::proxy_from_src_lua(plc, value).map_err(|e| e.to_string())?)
    }

    async fn eval_from_source(&self, modname: String) -> Result<Self::ValueType, crate::base::Error> {
        Ok(self.send(CodeExecMessage { modname }).await??)
    }

    async fn shutdown(&self) -> Result<(), crate::base::Error> {
        self.send(ShutdownMessage).await
    }
}

