use super::value::ProxiedV8Value;

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use concurrentlyexec::{ConcurrentExecutor, ConcurrentExecutorState, ConcurrentlyExecute, MultiSender, OneshotSender, ProcessOpts};
//use deno_core::error::{CoreError, CoreErrorKind};
use deno_core::{PollEventLoopOptions, v8};
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
//use deno_core::{op2, OpState};
//use deno_error::JsErrorBox;
//use mluau::serde::de;

use crate::denobridge::modloader::FusionModuleLoader;
use crate::denobridge::objreg::{V8ObjectRegistry, V8ObjectRegistryID};
use crate::denobridge::primitives::ProxiedV8Primitive;
use crate::luau::bridge::{
    LuaBridgeMessage, LuaBridgeService, LuaBridgeServiceClient, ProxyLuaClient,
};

use crate::base::{Error, ProxyBridge, ProxyBridgeWithStringExt};
use crate::luau::embedder_api::{EmbedderData, EmbedderDataContext};
use super::inner::V8IsolateManagerInner;

/// Minimum heap size for V8 isolates
pub const MIN_HEAP_LIMIT: usize = 10 * 1024 * 1024; // 10MB

pub(crate) struct BridgeVals {
    // obj registry fields (addV8Object, getV8Object and removeV8Object)
    pub obj_registry: V8ObjectRegistry,

    pub lua_id_symbol: v8::Global<v8::Symbol>,
    pub lua_type_symbol: v8::Global<v8::Symbol>,
}

impl BridgeVals {
    pub(crate) fn new<'s>(scope: &mut v8::PinScope<'s, '_>) -> Self {
        // The createLuaObjectFromData function is stored in globalThis.lua.createLuaObjectFromData
        let (add_v8_object, get_v8_object, remove_v8_object, luaid_symbol, luatype_symbol) = {
            // Get globalThis.lua
            let global = scope.get_current_context().global(scope);
            let lua_str = v8::String::new(scope, "lua").unwrap();
            let lua_obj = global.get(scope, lua_str.into()).unwrap();
            //println!("lua_obj: {:?}", lua_obj.to_rust_string_lossy(scope));
            assert!(lua_obj.is_object());
            let lua_obj = lua_obj.to_object(scope).unwrap();
            
            // get addV8Object, getV8Object and removeV8Object from V8ObjectRegistry
            let add_v8_object = {
                let addv8obj_str = v8::String::new(scope, "addV8Object").unwrap();
                let addv8obj = lua_obj.get(scope, addv8obj_str.into()).unwrap();
                assert!(addv8obj.is_function());
                let addv8obj = v8::Local::<v8::Function>::try_from(addv8obj).unwrap();
                addv8obj
            };
            let get_v8_object = {
                let getv8obj_str = v8::String::new(scope, "getV8Object").unwrap();
                let getv8obj = lua_obj.get(scope, getv8obj_str.into()).unwrap();
                assert!(getv8obj.is_function());
                let getv8obj = v8::Local::<v8::Function>::try_from(getv8obj).unwrap();
                getv8obj
            };
            let remove_v8_object = {
                let removev8obj_str = v8::String::new(scope, "removeV8Object").unwrap();
                let removev8obj = lua_obj.get(scope, removev8obj_str.into()).unwrap();
                assert!(removev8obj.is_function());
                let removev8obj = v8::Local::<v8::Function>::try_from(removev8obj).unwrap();
                removev8obj
            };
            let (luaid_symbol, luatype_symbol) = {
                let luaid_str = v8::String::new(scope, "luaidSymbol").unwrap();
                let luaidobj = lua_obj.get(scope, luaid_str.into()).unwrap();
                assert!(luaidobj.is_symbol());
                let luaid_sym = v8::Local::<v8::Symbol>::try_from(luaidobj).unwrap();

                let luatype_str = v8::String::new(scope, "luatypeSymbol").unwrap();
                let luatypeobj = lua_obj.get(scope, luatype_str.into()).unwrap();   
                assert!(luatypeobj.is_symbol());
                let luatype_sym = v8::Local::<v8::Symbol>::try_from(luatypeobj).unwrap();
                (luaid_sym, luatype_sym)
            };

            (add_v8_object, get_v8_object, remove_v8_object, luaid_symbol, luatype_symbol)
        };

        Self {
            lua_id_symbol: v8::Global::new(scope, luaid_symbol),
            lua_type_symbol: v8::Global::new(scope, luatype_symbol),
            obj_registry: V8ObjectRegistry { 
                add_v8_object: v8::Global::new(scope, add_v8_object),
                get_v8_object: v8::Global::new(scope, get_v8_object), 
                remove_v8_object: v8::Global::new(scope, remove_v8_object),
            }
        }
    }

    // Adds a value to the V8 object registry and returns its ID

}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum V8ObjectRegistryType {
    ArrayBuffer,
    Object,
    Function,
    Promise,
}

impl V8ObjectRegistryType {
    pub fn type_name(&self) -> &'static str {
        match self {
            V8ObjectRegistryType::Function => "Function",
            V8ObjectRegistryType::Object => "Object",
            V8ObjectRegistryType::ArrayBuffer => "ArrayBuffer",
            V8ObjectRegistryType::Promise => "Promise",
        }
    }  
}

#[derive(Clone)]
/// The client side state for proxying Lua values
/// 
/// This struct is not thread safe and must be kept on the Lua side
pub struct ProxyV8Client {
    pub obj_registry: V8ObjectRegistry
}

#[derive(Serialize, Deserialize)]
/// Internal representation of a message that can be sent to the V8 isolate manager
pub(super) enum V8IsolateManagerMessage {
    CodeExec {
        modname: String,
        resp: OneshotSender<Result<ProxiedV8Value, String>>,
    },
    OpCall {
        obj_id: V8ObjectRegistryID,
        op: V8ObjectOp,
        args: Vec<ProxiedV8Value>,
        resp: OneshotSender<Result<Vec<ProxiedV8Value>, String>>,
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

pub(super) struct OpCallMessage {
    pub obj_id: V8ObjectRegistryID,
    pub op: V8ObjectOp,
    pub args: Vec<ProxiedV8Value>,
}

impl V8IsolateSendableMessage for OpCallMessage {
    type Response = Result<Vec<ProxiedV8Value>, String>;
    fn to_message(self, resp: OneshotSender<Self::Response>) -> V8IsolateManagerMessage {
        V8IsolateManagerMessage::OpCall {
            obj_id: self.obj_id,
            op: self.op,
            args: self.args,
            resp,
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

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum V8ObjectOp {
    ObjectGetProperty,
    FunctionCall,
    RequestDispose,
}

enum OpCallRet {
    ProxiedMulti(Vec<ProxiedV8Value>),
    FunctAsync((v8::Global<v8::Function>, Vec<v8::Global<v8::Value>>))
}

impl V8ObjectOp {
    fn run<'s>(self, inner: &mut V8IsolateManagerInner, obj_id: V8ObjectRegistryID, args: Vec<ProxiedV8Value>) -> Result<OpCallRet, Error> {        
        match self {
            Self::FunctionCall => {
                    let main_ctx = inner.deno.main_context();
                    let isolate = inner.deno.v8_isolate();
                    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
                    let mut scope = &mut scope.init();
                    let main_ctx = v8::Local::new(&mut scope, main_ctx);
                    let mut context_scope = &mut v8::ContextScope::new(scope, main_ctx);

                let func = match inner.common_state.proxy_client.obj_registry.get(context_scope, obj_id, |tc, v| {
                    if !v.is_function() {
                        return Err("Object is not a function".into());
                    }
                    let func = v8::Local::<v8::Function>::try_from(v)
                        .map_err(|e| format!("Failed to convert V8 value to function: {}", e))?;
                    Ok(v8::Global::new(tc, func))
                }) {
                    Ok(o) => o,
                    Err(e) => return Err(format!("Failed to get V8 object from registry: {}", e).into()),
                };

                let mut v8_args = Vec::with_capacity(args.len());
                let mut ed = EmbedderDataContext::new(&inner.common_state.ed);
                for arg in args {
                    let v8_arg = match arg.to_v8(&mut context_scope, &inner.common_state, &mut ed) {
                        Ok(v) => v,
                        Err(e) => return Err(format!("Failed to convert argument to V8: {}", e).into()),
                    };
                    v8_args.push(v8::Global::new(&mut context_scope, v8_arg));
                }

                Ok(OpCallRet::FunctAsync((func, v8_args)))
            }
            Self::ObjectGetProperty => {
                let main_ctx = inner.deno.main_context();
                let isolate = inner.deno.v8_isolate();
                let scope = std::pin::pin!(v8::HandleScope::new(isolate));
                let mut scope = &mut scope.init();
                let main_ctx = v8::Local::new(&mut scope, main_ctx);
                let mut context_scope = &mut v8::ContextScope::new(scope, main_ctx);

                if args.len() != 1 {
                    return Err("ObjectGetProperty requires exactly one argument".into());
                }
                let key = match args.into_iter().next().unwrap() {
                    ProxiedV8Value::Primitive(p) => p,
                    _ => return Err("ObjectGetProperty key must be a primitive".into()),
                };
                let mut ed_a = EmbedderDataContext::new(&inner.common_state.ed);
                let key = match key.to_v8(&mut context_scope, &mut ed_a) {
                    Ok(v) => v,
                    Err(e) => return Err(format!("Failed to convert key to V8: {}", e).into()),
                };

                let obj = match inner.common_state.proxy_client.obj_registry.get(context_scope, obj_id, |tc, v| {
                    if !v.is_object() {
                        return Err("Object is not an object".into());
                    }
                    let obj = v8::Local::<v8::Object>::try_from(v)
                        .map_err(|e| format!("Failed to convert V8 value to object: {}", e))?;
                    Ok(v8::Global::new(tc, obj))
                }) {
                    Ok(o) => o,
                    Err(e) => return Err(format!("Failed to get V8 object from registry: {}", e).into()),
                };

                let obj = v8::Local::new(&mut context_scope, &obj);

                let prop_names = obj.get(&mut context_scope, key)
                    .ok_or("Failed to get property names")?;
                
                let mut ed_b = EmbedderDataContext::new(&inner.common_state.ed);

                let prop_names = ProxiedV8Value::from_v8(&mut context_scope, prop_names.into(), &inner.common_state, &mut ed_b)
                    .map_err(|e| format!("Failed to proxy property names: {}", e))?;

                Ok(OpCallRet::ProxiedMulti(vec![prop_names]))
            }
            V8ObjectOp::RequestDispose => {
                let main_ctx = inner.deno.main_context();
                let isolate = inner.deno.v8_isolate();
                let scope = std::pin::pin!(v8::HandleScope::new(isolate));
                let mut scope = &mut scope.init();
                let main_ctx = v8::Local::new(&mut scope, main_ctx);
                let mut context_scope = &mut v8::ContextScope::new(scope, main_ctx);

                match inner.common_state.proxy_client.obj_registry.remove(&mut context_scope, obj_id) {
                    Ok(_) => Ok(OpCallRet::ProxiedMulti(vec![])),
                    Err(e) => Err(format!("Failed to remove V8 object from registry: {}", e).into()),
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct V8IsolateManagerClient {}

#[derive(Serialize, Deserialize)]
pub struct V8BootstrapData {
    ed: EmbedderData,
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
            data.ed,
            FusionModuleLoader::new(data.vfs.into_iter().map(|(x, y)| (x, y.into())))
        );

        let mut evaluated_modules = HashMap::new();
        let mut module_evaluate_queue = FuturesUnordered::new();
        let mut op_call_queue = FuturesUnordered::new();

        loop {
            tokio::select! {
                Ok(msg) = rx.recv() => {
                    match msg {
                        V8IsolateManagerMessage::CodeExec { modname, resp } => {
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
                                    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
                                    let mut scope = &mut scope.init();
                                    let main_ctx = v8::Local::new(&mut scope, main_ctx);
                                    let context_scope = &mut v8::ContextScope::new(scope, main_ctx);
                                    let namespace_obj = v8::Local::new(context_scope, namespace_obj);
                                    let mut ed = EmbedderDataContext::new(&inner.common_state.ed);
                                    match ProxiedV8Value::from_v8(context_scope, namespace_obj.into(), &inner.common_state, &mut ed) {
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

                            let id = tokio::select! {
                                id = inner.deno.load_side_es_module(&url) => {
                                    match id {
                                        Ok(id) => id,
                                        Err(e) => {
                                            let _ = resp.client(&client_ctx).send(Err(format!("Failed to load module: {}", e).into()));
                                            continue;
                                        }
                                    }
                                }
                                _ = inner.cancellation_token.cancelled() => {
                                    return;
                                }
                            };

                            let fut = inner.deno.mod_evaluate(id);
                            module_evaluate_queue.push(async move {
                                let module_id = fut.await.map(|_| id);
                                (modname, module_id, resp)
                            });
                        },
                        V8IsolateManagerMessage::OpCall { obj_id, op, args, resp } => {
                            let fut = match op.run(&mut inner, obj_id, args) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to run op: {}", e).into()));
                                    continue;
                                }
                            };
                            match fut {
                                OpCallRet::ProxiedMulti(v) => {
                                    let _ = resp.client(&client_ctx).send(Ok(v));
                                }
                                OpCallRet::FunctAsync((func, args)) => {
                                    let fut = inner.deno.call_with_args(&func, &args);
                                    op_call_queue.push(async move {
                                        let result = fut.await;
                                        (result, resp)
                                    });
                                }
                            }
                        },
                        V8IsolateManagerMessage::Shutdown => {
                            println!("V8 isolate manager received shutdown message");
                            break;
                        }
                    }
                }
                _ = inner.cancellation_token.cancelled() => {
                    println!("V8 isolate manager received shutdown message");
                    break;
                }
                _ = inner.deno.run_event_loop(PollEventLoopOptions {
                    wait_for_inspector: false,
                    pump_v8_message_loop: true,
                }) => {
                    tokio::task::yield_now().await;
                },
                Some((result, resp)) = op_call_queue.next() => {
                    match result {
                        Ok(res) => {
                            let main_ctx = inner.deno.main_context();
                            let isolate = inner.deno.v8_isolate();
                            let scope = std::pin::pin!(v8::HandleScope::new(isolate));
                            let mut scope = &mut scope.init();
                            let main_ctx = v8::Local::new(&mut scope, main_ctx);
                            let mut context_scope = &mut v8::ContextScope::new(scope, main_ctx);
                            let res = v8::Local::new(&mut context_scope, res);
                            let mut ed = EmbedderDataContext::new(&inner.common_state.ed);
                            let res = ProxiedV8Value::from_v8(context_scope, res, &inner.common_state, &mut ed);
                            match res {
                                Ok(v) => {
                                    let _ = resp.client(&client_ctx).send(Ok(vec![v]));
                                }
                                Err(e) => {
                                    let _ = resp.client(&client_ctx).send(Err(format!("Failed to proxy function result: {}", e).into()));
                                }
                            }
                        },
                        Err(e) => {
                            let _ = resp.client(&client_ctx).send(Err(e.to_string()));
                        }
                    }
                }
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
                        let scope = std::pin::pin!(v8::HandleScope::new(isolate));
                        let mut scope = &mut scope.init();
                        let main_ctx = v8::Local::new(&mut scope, main_ctx);
                        let context_scope = &mut v8::ContextScope::new(scope, main_ctx);
                        let namespace_obj = v8::Local::new(context_scope, namespace_obj);
                        let mut ed = EmbedderDataContext::new(&inner.common_state.ed);
                        match ProxiedV8Value::from_v8(context_scope, namespace_obj.into(), &inner.common_state, &mut ed) {
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

pub struct V8IsolateManagerServerInner {
    executor: Arc<ConcurrentExecutor<V8IsolateManagerClient>>,
    messenger: Arc<MultiSender<V8IsolateManagerMessage>>,
}

#[derive(Clone)]
pub struct V8IsolateManagerServer {
    inner: Rc<V8IsolateManagerServerInner>,
}

impl std::ops::Deref for V8IsolateManagerServer {
    type Target = V8IsolateManagerServerInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl V8IsolateManagerServer {
    /// Create a new V8 isolate manager server
    async fn new(
        cs_state: ConcurrentExecutorState<V8IsolateManagerClient>, 
        ed: EmbedderData, 
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
                    ed,
                    messenger_tx: msg_tx,
                    lua_bridge_tx: tx,
                    vfs
                }, (rx, msg_rx))
            }
        ).await.map_err(|e| format!("Failed to create V8 isolate manager executor: {}", e))?;
        let messenger = ms_rx.recv().await.map_err(|e| format!("Failed to receive messenger: {}", e))?;

        let self_ret = Self { 
            inner: Rc::new(V8IsolateManagerServerInner {
                executor: Arc::new(executor), 
                messenger: Arc::new(messenger)
            })
         };
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
        
        let resp = resp_rx.recv().await;

        if self.is_shutdown() {
            return Err("V8 isolate manager is shut down (likely due to timeout)".into());
        }
        
        resp.map_err(|e| format!("Failed to receive response from V8 isolate: {}", e).into())
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
        ed: EmbedderData,
        process_opts: ProcessOpts,
        plc: ProxyLuaClient,
        vfs: HashMap<String, String>,
    ) -> Result<Self, crate::base::Error> {
        Self::new(cs_state, ed, process_opts, plc, vfs).await        
    }

    fn get_executor(&self) -> Arc<ConcurrentExecutor<Self::ConcurrentlyExecuteClient>> {
        self.executor.clone()
    }

    fn to_source_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, plc: &ProxyLuaClient) -> Result<mluau::Value, Error> {
        let mut ed = EmbedderDataContext::new(&plc.ed);
        Ok(value.to_luau(lua, plc, self, &mut ed).map_err(|e| e.to_string())?)
    }

    fn from_source_lua_value(&self, _lua: &mluau::Lua, plc: &ProxyLuaClient, value: mluau::Value, ed: &mut EmbedderDataContext) -> Result<Self::ValueType, crate::base::Error> {
        Ok(ProxiedV8Value::from_luau(plc, value, ed).map_err(|e| e.to_string())?)
    }

    async fn eval_from_source(&self, modname: String) -> Result<Self::ValueType, crate::base::Error> {
        Ok(self.send(CodeExecMessage { modname }).await??)
    }

    async fn shutdown(&self) -> Result<(), crate::base::Error> {
        let _ = self.send(ShutdownMessage).await;
        self.executor.shutdown().await?;
        self.executor.wait().await?;
        Ok(())
    }

    fn is_shutdown(&self) -> bool {
        self.executor.get_state().cancel_token.is_cancelled()
    }
}

impl ProxyBridgeWithStringExt for V8IsolateManagerServer {
    /// Creates the foreign language value type from a string
    fn from_string(s: String) -> Self::ValueType {
        ProxiedV8Value::Primitive(ProxiedV8Primitive::String(s))
    }
}

/// Helper method to run the process client
pub async fn run_v8_process_client() {
    ConcurrentExecutor::<<V8IsolateManagerServer as ProxyBridge>::ConcurrentlyExecuteClient>::run_process_client().await;
}