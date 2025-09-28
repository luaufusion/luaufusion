use crate::base::{ObjectRegistryID, ProxyBridge};
use concurrentlyexec::{ClientContext, MultiReceiver, MultiSender, OneshotSender};
use mluau::ObjectLike;
use serde::{Deserialize, Serialize};
use futures_util::stream::{FuturesUnordered, StreamExt};
use crate::base::ObjectRegistry;
use mluau::WeakLua;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub fn obj_registry_type_to_i32(typ: ObjectRegistryType) -> i32 {
    match typ {
        ObjectRegistryType::String => 0,
        ObjectRegistryType::Table => 1,
        ObjectRegistryType::Function => 2,
        ObjectRegistryType::UserData => 3,
        ObjectRegistryType::Buffer => 4,
        ObjectRegistryType::Thread => 5,
    }
}

pub fn i32_to_obj_registry_type(val: i32) -> Option<ObjectRegistryType> {
    match val {
        0 => Some(ObjectRegistryType::String),
        1 => Some(ObjectRegistryType::Table),
        2 => Some(ObjectRegistryType::Function),
        3 => Some(ObjectRegistryType::UserData),
        4 => Some(ObjectRegistryType::Buffer),
        5 => Some(ObjectRegistryType::Thread),
        _ => None,
    }
}

/// Marker struct for Lua objects in the object registry
#[derive(Clone, Copy)]
pub struct LuaBridgeObject;

#[derive(Clone)]
/// The client side state for proxying Lua values
/// 
/// This struct is not thread safe and must be kept on the Lua side
pub struct ProxyLuaClient {
    pub weak_lua: WeakLua,
    pub string_registry: ObjectRegistry<mluau::String, LuaBridgeObject>,
    pub table_registry: ObjectRegistry<mluau::Table, LuaBridgeObject>,
    pub func_registry: ObjectRegistry<mluau::Function, LuaBridgeObject>,
    pub thread_registry: ObjectRegistry<mluau::Thread, LuaBridgeObject>,
    pub userdata_registry: ObjectRegistry<mluau::AnyUserData, LuaBridgeObject>,
    pub buffer_registry: ObjectRegistry<mluau::Buffer, LuaBridgeObject>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum ObjectRegistryType {
    String,
    Table,
    Function,
    UserData,
    Buffer,
    Thread,
}

/// Messages sent to the Lua proxy bridge
#[derive(Serialize, Deserialize)]
pub enum LuaBridgeMessage<T: ProxyBridge> {
    CallFunction {
        func_id: ObjectRegistryID<LuaBridgeObject>,
        args: Vec<T::ValueType>,
        resp: OneshotSender<Result<Vec<T::ValueType>, String>>,
    },
    ReadBuffer {
        buffer_id: ObjectRegistryID<LuaBridgeObject>,
        resp: OneshotSender<Result<Vec<u8>, String>>,
    },
    DropObject {
        obj_type: ObjectRegistryType,
        obj_id: ObjectRegistryID<LuaBridgeObject>,
    },
    IndexUserData {
        obj_id: ObjectRegistryID<LuaBridgeObject>,
        key: T::ValueType,
        resp: OneshotSender<Result<T::ValueType, String>>,
    },
    Shutdown,
}

/// Proxy bridge service to expose data from Luau and another language's proxy bridge
pub struct LuaBridgeService<T: ProxyBridge> {
    bridge: T,
    rx: MultiReceiver<LuaBridgeMessage<T>>,
}

impl<T: ProxyBridge> LuaBridgeService<T> {
    pub fn new(bridge: T, rx: MultiReceiver<LuaBridgeMessage<T>>) -> Self {
        Self { bridge, rx }
    }

    /// Creates a new Lua proxy bridge
    pub async fn run(mut self, plc: ProxyLuaClient) {
        let mut func_call_queue = FuturesUnordered::new();
        let executor = self.bridge.get_executor();
        let server_ctx = executor.server_context();
        let state = executor.get_state();
        loop {
            tokio::select! {
                Ok(msg) = self.rx.recv() => {
                    match msg {
                        LuaBridgeMessage::CallFunction { func_id, args, resp } => {
                            let func = match plc.func_registry.get(func_id) {
                                Some(f) => f,
                                None => {
                                    let _ = resp.server(server_ctx).send(Err(format!("Function ID {} not found in registry", func_id).into()));
                                    continue;
                                }
                            };

                            let Some(lua) = plc.weak_lua.try_upgrade() else {
                                let _ = resp.server(server_ctx).send(Err("Lua state has been dropped".into()));
                                continue;
                            };

                            let mv = {
                                let mut mv = mluau::MultiValue::with_capacity(args.len());
                                let mut err = None;
                                for arg in args {
                                    match self.bridge.to_source_lua_value(&lua, arg, &plc) {
                                        Ok(v) => {
                                            mv.push_back(v);
                                        },
                                        Err(e) => {
                                            err = Some(e);
                                            break;
                                        }
                                    }
                                }

                                if let Some(e) = err {
                                    let _ = resp.server(server_ctx).send(Err(format!("Failed to convert argument to Lua value: {}", e).into()));
                                    continue;
                                }

                                mv
                            };

                            let th = match lua.create_thread(func) {
                                Ok(t) => t,
                                Err(e) => {
                                    let _ = resp.server(server_ctx).send(Err(format!("Failed to create Lua thread: {}", e).into()));
                                    continue;
                                }
                            };

                            let taskmgr = mlua_scheduler::taskmgr::get(&lua);
                            
                            func_call_queue.push(async move {
                                let result = taskmgr.spawn_thread_and_wait(th, mv).await;
                                (result, resp)
                            });
                        }
                        LuaBridgeMessage::ReadBuffer { buffer_id, resp } => {
                            let result = (|| {
                                let buffer = plc.buffer_registry.get(buffer_id)
                                    .ok_or_else(|| format!("Buffer ID {} not found in registry", buffer_id))?;
                                Ok(buffer.to_vec())
                            })();
                            let _ = resp.server(server_ctx).send(result);
                        }
                        LuaBridgeMessage::DropObject { obj_type, obj_id } => {
                            match obj_type {
                                ObjectRegistryType::String => { plc.string_registry.remove(obj_id); /* Just drop reference */ }
                                ObjectRegistryType::Table => { plc.table_registry.remove(obj_id); /* Just drop reference */ }
                                ObjectRegistryType::Function => { plc.func_registry.remove(obj_id); /* Just drop reference */ }
                                ObjectRegistryType::UserData => { plc.userdata_registry.remove(obj_id); /* Just drop reference */ }
                                ObjectRegistryType::Buffer => { plc.buffer_registry.remove(obj_id); /* Just drop reference */ }
                                ObjectRegistryType::Thread => { plc.thread_registry.remove(obj_id); /* Just drop reference */ }
                            }
                        }
                        LuaBridgeMessage::Shutdown => {
                            break;
                        }
                        LuaBridgeMessage::IndexUserData { obj_id, key, resp } => {
                            let result = (|| {
                                let userdata = plc.userdata_registry.get(obj_id)
                                    .ok_or_else(|| format!("UserData ID {} not found in registry", obj_id))?;
                                let Some(lua) = plc.weak_lua.try_upgrade() else {
                                    return Err("Lua state has been dropped".to_string());
                                };
                                let key_val = self.bridge.to_source_lua_value(&lua, key, &plc)
                                    .map_err(|e| format!("Failed to convert key to Lua value: {}", e))?;
                                let val = userdata.get::<mluau::Value>(key_val)
                                    .map_err(|e| format!("Failed to index UserData: {}", e))?;
                                self.bridge.from_source_lua_value(&lua, &plc, val)
                                    .map_err(|e| format!("Failed to convert indexed value to foreign language value: {}", e))
                            })();
                            let _ = resp.server(server_ctx).send(result);
                        }
                    }
                }
                _ = state.cancel_token.cancelled() => {
                    break; // Executor is shutting down
                }
                Some((result, resp)) = func_call_queue.next() => {
                    match result {
                        Ok(ret) => {
                            match ret {
                                Some(v) => {
                                    match v {
                                        Ok(v) => {
                                            let Some(lua) = plc.weak_lua.try_upgrade() else {
                                                let _ = resp.server(server_ctx).send(Err("Lua state has been dropped".to_string().into()));
                                                continue;
                                            };

                                            let mut args = Vec::with_capacity(v.len());
                                            let mut err = None;
                                            for v in v {
                                                match self.bridge.from_source_lua_value(&lua, &plc, v) {
                                                    Ok(pv) => {
                                                        args.push(pv);
                                                    }
                                                    Err(e) => {
                                                        err = Some(e);
                                                        break;
                                                    }
                                                }
                                            }

                                            if let Some(e) = err {
                                                let _ = resp.server(server_ctx).send(Err(format!("Failed to convert return value to foreign language value: {}", e).into()));
                                                continue;
                                            }

                                            let _ = resp.server(server_ctx).send(Ok(args));
                                        }
                                        Err(e) => {
                                            let _ = resp.server(server_ctx).send(Err(format!("Lua function error: {}", e).into()));
                                        }
                                    }
                                }
                                None => {
                                    let _ = resp.server(server_ctx).send(Ok(vec![]));
                                }
                            }
                        }
                        Err(e) => {
                            let _ = resp.server(server_ctx).send(Err(format!("Failed to call Lua function: {}", e).into()));
                        }
                    }
                }
            }
        }
    }

    // Calls a Lua function by its ID with the given arguments, returning the results
    /**/
}

/// The client side part of the Lua bridge service
#[derive(Clone)]
pub struct LuaBridgeServiceClient<T: ProxyBridge> {
    client_context: ClientContext,
    tx: MultiSender<LuaBridgeMessage<T>>,
}

impl<T: ProxyBridge> LuaBridgeServiceClient<T> {
    /// Creates a new Lua bridge service client
    pub fn new(client_context: ClientContext, tx: MultiSender<LuaBridgeMessage<T>>) -> Self {
        Self { client_context, tx }
    }

    /// Calls a Lua function by its ID with the given arguments, returning the results
    pub async fn call_function(&self, func_id: ObjectRegistryID<LuaBridgeObject>, args: Vec<T::ValueType>) -> Result<Vec<T::ValueType>, Error> {
        let (resp_tx, resp_rx) = self.client_context.oneshot();
        self.tx.client(&self.client_context).send(LuaBridgeMessage::CallFunction { func_id, args, resp: resp_tx })
            .map_err(|e| format!("Failed to send CallFunction message: {}", e))?;
        Ok(resp_rx.recv().await.map_err(|e| format!("Failed to receive CallFunction response: {}", e))??)
    }

    /// Reads the contents of a Lua buffer by its ID
    pub async fn read_buffer(&self, buffer_id: ObjectRegistryID<LuaBridgeObject>) -> Result<Vec<u8>, Error> {
        let (resp_tx, resp_rx) = self.client_context.oneshot();
        self.tx.client(&self.client_context).send(LuaBridgeMessage::ReadBuffer { buffer_id, resp: resp_tx })
            .map_err(|e| format!("Failed to send ReadBuffer message: {}", e))?;
        Ok(resp_rx.recv().await.map_err(|e| format!("Failed to receive ReadBuffer response: {}", e))??)
    }

    /// Requests that an object be dropped from the registry
    pub fn request_drop_object(&self, obj_type: ObjectRegistryType, obj_id: ObjectRegistryID<LuaBridgeObject>) {
        let _ = self.tx.client(&self.client_context).send(LuaBridgeMessage::DropObject { obj_type, obj_id });
    }
}

mod asserter {
    //const fn assert_send_const<T: Send>() {}
    //const _: () = assert_send_const::<LuaBridge<crate::deno::bridge::V8ProxyBridge>>(); 
    //const _: () = assert_send_const::<LuaBridge<crate::base::quickjs::bridge::QuickJSProxyBridge>>();
}