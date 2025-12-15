use crate::{base::{ProxyBridge, ProxyBridgeWithMultiprocessExt}, luau::{embedder_api::{EmbedderData, EmbedderDataContext}, objreg::{LuauObjectRegistryID, ObjRegistryLuau}}};
use concurrentlyexec::{ClientContext, MultiReceiver, MultiSender, OneshotSender};
use mluau::{LuaSerdeExt, ObjectLike};
use serde::{Deserialize, Serialize};
use futures_util::stream::{FuturesUnordered, StreamExt};
use mluau::WeakLua;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
/// The client side state for proxying Lua values
/// 
/// This struct is not thread safe and must be kept on the Lua side
pub struct ProxyLuaClient {
    pub(super) weak_lua: WeakLua,
    pub array_mt: mluau::Table,
    pub obj_registry: ObjRegistryLuau,
    pub ed: EmbedderData,
}

impl ProxyLuaClient {
    /// Creates a new proxy Lua client
    pub fn new(lua: &mluau::Lua, ed: EmbedderData) -> Result<Self, mluau::Error> {
        Ok(Self {
            weak_lua: lua.weak(),
            array_mt: lua.array_metatable(),
            obj_registry: ObjRegistryLuau::new(lua)?,
            ed
        })
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum ObjectRegistryType {
    Table,
    Function,
    UserData,
    Buffer,
    Thread,
}

impl Into<&'static str> for ObjectRegistryType {
    fn into(self) -> &'static str {
        self.type_name()
    }  
} 

impl ObjectRegistryType {
    pub fn type_name(&self) -> &'static str {
        match self {
            ObjectRegistryType::Table => "table",
            ObjectRegistryType::Function => "function",
            ObjectRegistryType::UserData => "userdata",
            ObjectRegistryType::Buffer => "buffer",
            ObjectRegistryType::Thread => "thread",
        }
    }  

    pub fn from_value(val: &mluau::Value) -> Option<Self> {
        match val {
            mluau::Value::Table(_) => Some(ObjectRegistryType::Table),
            mluau::Value::Function(_) => Some(ObjectRegistryType::Function),
            mluau::Value::UserData(_) => Some(ObjectRegistryType::UserData),
            mluau::Value::Buffer(_) => Some(ObjectRegistryType::Buffer),
            mluau::Value::Thread(_) => Some(ObjectRegistryType::Thread),
            _ => None,
        }
    }
}

/// Messages sent to the Lua proxy bridge
#[derive(Serialize, Deserialize)]
pub enum LuaBridgeMessage<T: ProxyBridge> {
    FunctionCallSync {
        obj_id: LuauObjectRegistryID,
        args: Vec<T::ValueType>,
        resp: OneshotSender<Result<Vec<T::ValueType>, String>>,
    },
    FunctionCallAsync {
        obj_id: LuauObjectRegistryID,
        args: Vec<T::ValueType>,
        resp: OneshotSender<Result<Vec<T::ValueType>, String>>,
    },
    Index {
        obj_id: LuauObjectRegistryID,
        key: T::ValueType,
        resp: OneshotSender<Result<T::ValueType, String>>,
    },
    RequestDispose {
        ids: Vec<LuauObjectRegistryID>,
        resp: Option<OneshotSender<Result<(), String>>>,
    },
    Shutdown,
}

/// Proxy bridge service to expose data from Luau and another language's proxy bridge
pub struct LuaBridgeService<T: ProxyBridge> {
    bridge: T,
    rx: MultiReceiver<LuaBridgeMessage<T>>,
    ed: EmbedderData,
}

impl<T: ProxyBridge + ProxyBridgeWithMultiprocessExt> LuaBridgeService<T> {
    pub fn new(bridge: T, rx: MultiReceiver<LuaBridgeMessage<T>>, ed: EmbedderData) -> Self {
        Self { bridge, rx, ed }
    }

    // Helpers to create MultiValue from args
    fn create_multivalue_from_args(lua: &mluau::Lua, args: Vec<T::ValueType>, bridge: &T, plc: &ProxyLuaClient) -> Result<mluau::MultiValue, String> {
        let mut mv = mluau::MultiValue::with_capacity(args.len());
        let mut err = None;
        for arg in args {
            match bridge.to_source_lua_value(lua, arg, &plc) {
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
            return Err(format!("Failed to convert argument to Lua value: {}", e));
        }

        Ok(mv)
    }

    fn create_args_from_multivalue(lua: &mluau::Lua, mv: mluau::MultiValue, bridge: &T, plc: &ProxyLuaClient) -> Result<Vec<T::ValueType>, String> {
        let mut args = Vec::with_capacity(mv.len());
        let mut err = None;
        for v in mv {
            match bridge.from_source_lua_value(lua, &plc, v, &mut EmbedderDataContext::new(plc.ed)) {
                Ok(pv) => {
                    args.push(pv);
                },
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }

        if let Some(e) = err {
            return Err(format!("Failed to convert argument to foreign language value: {}", e));
        }

        Ok(args)
    }

    fn validate_func_call(
        &self,
        plc: &ProxyLuaClient,
        obj_id: LuauObjectRegistryID,
        args: Vec<T::ValueType>,
    ) -> Result<(mluau::Function, mluau::MultiValue, mluau::Lua), String> {
        // For now, we don't have any specific validation logic
        // In the future, we could check if the object ID exists in a registry, etc.
        let Some(lua) = plc.weak_lua.try_upgrade() else {
            return Err("Lua state has been dropped".into());
        };

        let obj = plc.obj_registry.get(obj_id)
            .map_err(|e| format!("Failed to get object from registry: {}", e))?;

        let func = match obj {
            mluau::Value::Function(f) => f,
            _ => {
                return Err("Object is not a function".into());
            },
        };

        let args_mv = Self::create_multivalue_from_args(&lua, args, &self.bridge, &plc)?;
        Ok((func, args_mv, lua))
    }

    /// Creates a new Lua proxy bridge
    pub async fn run(mut self, plc: ProxyLuaClient) {
        let mut op_call_queue = FuturesUnordered::new();
        let executor = &self.bridge.get_executor();
        let server_ctx = executor.server_context();
        let state = executor.get_state();
        loop {
            tokio::select! {
                Ok(msg) = self.rx.recv() => {
                    match msg {
                        LuaBridgeMessage::FunctionCallSync { obj_id, args, resp } => {
                            let (funct, args, lua) = match Self::validate_func_call(
                                &self,
                                &plc,
                                obj_id,
                                args,
                            ) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = resp.server(server_ctx).send(Err(e));
                                    continue;
                                }
                            };

                            match funct.call::<mluau::MultiValue>(args) {
                                Ok(ret) => {
                                    match Self::create_args_from_multivalue(&lua, ret, &self.bridge, &plc) {
                                        Ok(ret_vals) => {
                                            let _ = resp.server(server_ctx).send(Ok(ret_vals));
                                        }
                                        Err(e) => {
                                            let _ = resp.server(server_ctx).send(Err(format!("Failed to convert return value to foreign language value: {}", e).into()));
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = resp.server(server_ctx).send(Err(format!("Failed to call function: {}", e).into()));
                                }
                            }
                        },
                        LuaBridgeMessage::FunctionCallAsync { obj_id, args, resp } => {
                            let (funct, args, lua) = match Self::validate_func_call(
                                &self,
                                &plc,
                                obj_id,
                                args,
                            ) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = resp.server(server_ctx).send(Err(e));
                                    continue;
                                }
                            };

                            // Async calls get pushed to the op_call_queue
                            let th = match lua.create_thread(funct) {
                                Ok(t) => t,
                                Err(e) => {
                                    let _ = resp.server(server_ctx).send(Err(format!("Failed to create Lua thread: {}", e).into()));
                                    continue;
                                }
                            };

                            op_call_queue.push(async move {
                                let taskmgr = mlua_scheduler::taskmgr::get(&lua);
                                (taskmgr.spawn_thread_and_wait(th, args).await, resp)
                            });
                        }
                        LuaBridgeMessage::Index { obj_id, key, resp } => {
                            let obj = match plc.obj_registry.get(obj_id) {
                                Ok(o) => o,
                                Err(e) => {
                                    let _ = resp.server(server_ctx).send(Err(format!("Failed to get object from registry: {}", e)));
                                    continue;
                                }
                            };

                            let Some(lua) = plc.weak_lua.try_upgrade() else {
                                let _ = resp.server(server_ctx).send(Err("Lua state has been dropped".into()));
                                continue;
                            };

                            let key = match self.bridge.to_source_lua_value(&lua, key, &plc) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = resp.server(server_ctx).send(Err(format!("Failed to convert argument to Lua value: {}", e)));
                                    continue;
                                }
                            };

                            let v = match obj {
                                mluau::Value::Table(t) => {
                                    t.get::<mluau::Value>(key).map_err(|e| format!("Failed to index table: {}", e))
                                }
                                mluau::Value::UserData(u) => {
                                    u.get::<mluau::Value>(key).map_err(|e| format!("Failed to index userdata: {}", e))
                                }
                                _ => Err("Object is not indexable (not a table or userdata)".into()),
                            };

                            let Ok(v) = v else {
                                let _ = resp.server(server_ctx).send(Err(format!("Failed to get property: {}", v.err().unwrap())));
                                continue;
                            };
                            
                            let mut ed = EmbedderDataContext::new(plc.ed);
                            match self.bridge.from_source_lua_value(&lua, &plc, v, &mut ed) {
                                Ok(pv) => {
                                    let _ = resp.server(server_ctx).send(Ok(pv));
                                }
                                Err(e) => {
                                    let _ = resp.server(server_ctx).send(Err(format!("Failed to convert return value to foreign language value: {}", e).into()));
                                }
                            }
                        },
                        LuaBridgeMessage::RequestDispose { ids, resp } => {
                            if !self.ed.object_disposal_enabled {
                                continue;
                            }
                            
                            if cfg!(feature = "debug_message_print_enabled") {
                                println!("Host Luau received request to dispose object IDs {:?}", ids);
                            }

                            let mut errors = Vec::new();
                            for id in ids {
                                match plc.obj_registry.drop(id) {
                                    Ok(_) => {},
                                    Err(e) => {
                                        errors.push(e);
                                    }
                                }
                            }

                            if let Some(resp) = resp {
                                if errors.is_empty() {
                                    let _ = resp.server(server_ctx).send(Ok(()));
                                } else {
                                    let err_msg = format!("Failed to dispose some objects: {:?}", errors);
                                    let _ = resp.server(server_ctx).send(Err(err_msg));
                                }
                            }
                        }
                        LuaBridgeMessage::Shutdown => {
                            if cfg!(feature = "debug_message_print_enabled") {
                                println!("Host Luau received shutdown message");
                            }
                            break;
                        }
                    }
                }
                _ = state.cancel_token.cancelled() => {
                    if cfg!(feature = "debug_message_print_enabled") {
                        println!("Lua bridge service received shutdown cancellation");
                    }
                    break; // Executor is shutting down
                }
                Some((result, resp)) = op_call_queue.next() => {  
                    let result = match result {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = resp.server(server_ctx).send(Err(format!("Failed to call async function: {}", e).into()));
                            continue;
                        }
                    };

                    let ret_v = match result {
                        Some(ret) => ret,
                        None => {
                            let _ = resp.server(server_ctx).send(Err("Async function did not return due to an unknown error".into()));
                            continue;
                        }
                    };

                    match ret_v {
                        Ok(ret) => {
                            let Some(lua) = plc.weak_lua.try_upgrade() else {
                                let _ = resp.server(server_ctx).send(Err("Lua state has been dropped".into()));
                                continue;
                            };

                            match Self::create_args_from_multivalue(&lua, ret, &self.bridge, &plc) {
                                Ok(ret_vals) => {
                                    let _ = resp.server(server_ctx).send(Ok(ret_vals));
                                }
                                Err(e) => {
                                    let _ = resp.server(server_ctx).send(Err(format!("Failed to convert return value to foreign language value: {}", e).into()));
                                }
                            }
                        }
                        Err(e) => {
                            let _ = resp.server(server_ctx).send(Err(format!("Lua async function error: {}", e).into()));
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
    pub(crate) ed: EmbedderData,

}

impl<T: ProxyBridge> LuaBridgeServiceClient<T> {
    /// Creates a new Lua bridge service client
    pub fn new(client_context: ClientContext, tx: MultiSender<LuaBridgeMessage<T>>, ed: EmbedderData) -> Self {
        Self { client_context, tx, ed }
    }

    /// Calls a Lua function by its ID with the given arguments, returning the results (sync/mainthread mode)
    pub async fn call_function_sync(&self, obj_id: LuauObjectRegistryID, args: Vec<T::ValueType>) -> Result<Vec<T::ValueType>, String> {
        let (resp_tx, resp_rx) = self.client_context.oneshot();
        let msg = LuaBridgeMessage::FunctionCallSync {
            obj_id,
            args,
            resp: resp_tx,
        };
        self.tx.client(&self.client_context).send(msg).map_err(|e| format!("Failed to send opcall message: {}", e))?;
        resp_rx.recv().await.map_err(|e| format!("Failed to receive opcall response: {}", e))?
    }

    /// Calls a Lua function by its ID with the given arguments, returning the results (async mode)
    pub async fn call_function_async(&self, obj_id: LuauObjectRegistryID, args: Vec<T::ValueType>) -> Result<Vec<T::ValueType>, String> {
        let (resp_tx, resp_rx) = self.client_context.oneshot();
        let msg = LuaBridgeMessage::FunctionCallAsync {
            obj_id,
            args,
            resp: resp_tx,
        };
        self.tx.client(&self.client_context).send(msg).map_err(|e| format!("Failed to send opcall message: {}", e))?;
        resp_rx.recv().await.map_err(|e| format!("Failed to receive opcall response: {}", e))?
    }

    /// Indexes a Lua object by its ID with the given key, returning the result
    pub async fn index(&self, obj_id: LuauObjectRegistryID, key: T::ValueType) -> Result<T::ValueType, String> {
        let (resp_tx, resp_rx) = self.client_context.oneshot();
        let msg = LuaBridgeMessage::Index {
            obj_id,
            key,
            resp: resp_tx,
        };
        self.tx.client(&self.client_context).send(msg).map_err(|e| format!("Failed to send index message: {}", e))?;
        resp_rx.recv().await.map_err(|e| format!("Failed to receive index response: {}", e))?
    }

    /// Requests disposal of a Lua object by its ID
    pub async fn request_dispose(&self, obj_id: LuauObjectRegistryID) -> Result<(), String> {
        if !self.ed.object_disposal_enabled {
            return Ok(());
        }

        let (resp_tx, resp_rx) = self.client_context.oneshot();
        let msg = LuaBridgeMessage::RequestDispose {
            ids: vec![obj_id],
            resp: Some(resp_tx),
        };
        self.tx.client(&self.client_context).send(msg).map_err(|e| format!("Failed to send request dispose message: {}", e))?;
        resp_rx.recv().await.map_err(|e| format!("Failed to receive request dispose response: {}", e))?
    }

    /// Fires a dispose request without waiting for the result
    pub fn fire_request_disposes(&self, ids: Vec<LuauObjectRegistryID>) {
        if !self.ed.object_disposal_enabled || !self.ed.automatic_object_disposal_enabled {
            return;
        }

        let msg = LuaBridgeMessage::RequestDispose {
            ids,
            resp: None,
        };
        let _ = self.tx.client(&self.client_context).send(msg);
    }
}