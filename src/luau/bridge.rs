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

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum LuauObjectOp {
    FunctionCallSync = 1,
    FunctionCallAsync = 2,
    Index = 3,
}

enum ValueOrMultiValue {
    Value(mluau::Value),
    MultiValue(mluau::MultiValue),
}

impl LuauObjectOp {
    fn create_value_from_args<T: ProxyBridge>(lua: &mluau::Lua, arg: T::ValueType, bridge: &T, plc: &ProxyLuaClient) -> Result<mluau::Value, String> {
        bridge.to_source_lua_value(lua, arg, &plc)
            .map_err(|e| format!("Failed to convert argument to Lua value: {}", e))
    }
    fn create_multivalue_from_args<T: ProxyBridge>(lua: &mluau::Lua, args: Vec<T::ValueType>, bridge: &T, plc: &ProxyLuaClient) -> Result<mluau::MultiValue, String> {
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

    async fn run<T: ProxyBridge>(self, lua: &mluau::Lua, obj_id: LuauObjectRegistryID, args: Vec<T::ValueType>, bridge: T, plc: &ProxyLuaClient) -> Result<ValueOrMultiValue, Error> {
        match self {
            Self::FunctionCallSync => {
                let obj = plc.obj_registry.get(obj_id)
                    .map_err(|e| format!("Failed to get object from registry: {}", e))?;
                let args = Self::create_multivalue_from_args(lua, args, &bridge, &plc)?;
                let func = match obj {
                    mluau::Value::Function(f) => f,
                    _ => return Err("Object is not a function".into()),
                };
                func.call(args)
                .map_err(|e| format!("Failed to call function: {}", e).into())
                .map(ValueOrMultiValue::MultiValue)
            }
            Self::FunctionCallAsync => {
                let obj = plc.obj_registry.get(obj_id)
                    .map_err(|e| format!("Failed to get object from registry: {}", e))?;
                let args = Self::create_multivalue_from_args(lua, args, &bridge, &plc)?;
                let func = match obj {
                    mluau::Value::Function(f) => f,
                    _ => return Err("Object is not a function".into()),
                };

                let th = match lua.create_thread(func) {
                    Ok(t) => t,
                    Err(e) => return Err(format!("Failed to create Lua thread: {}", e).into()),
                };

                let taskmgr = mlua_scheduler::taskmgr::get(&lua);
                
                let Some(res) = taskmgr.spawn_thread_and_wait(th, args).await
                    .map_err(|e| format!("Failed to call async function: {}", e))? else {
                    return Err("Async function did not return due to an unknown error".into());
                    };
                
                Ok(ValueOrMultiValue::MultiValue(res.map_err(|e| e.to_string())?))
                
            }
            Self::Index => {
                if args.len() != 1 {
                    return Err("Index operation requires exactly one argument".into());
                }
                let arg = args.into_iter().next().ok_or("Failed to get argument for index operation".to_string())?;
                let key = Self::create_value_from_args(lua, arg, &bridge, &plc)?;

                let obj = plc.obj_registry.get(obj_id)
                    .map_err(|e| format!("Failed to get object from registry: {}", e))?;
                let v = match obj {
                    mluau::Value::Table(t) => {
                        t.get::<mluau::Value>(key).map_err(|e| format!("Failed to index table: {}", e))
                    }
                    mluau::Value::UserData(u) => {
                        u.get::<mluau::Value>(key).map_err(|e| format!("Failed to index userdata: {}", e))
                    }
                    _ => Err("Object is not indexable (not a table or userdata)".into()),
                }?;
                Ok(ValueOrMultiValue::Value(v))
            }
        }
    }
}

/// Messages sent to the Lua proxy bridge
#[derive(Serialize, Deserialize)]
pub enum LuaBridgeMessage<T: ProxyBridge> {
    OpCall {
        obj_id: LuauObjectRegistryID,
        op: LuauObjectOp,
        args: Vec<T::ValueType>,
        resp: OneshotSender<Result<Vec<T::ValueType>, String>>,
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

    /// Creates a new Lua proxy bridge
    pub async fn run(mut self, plc: ProxyLuaClient) {
        let mut op_call_queue = FuturesUnordered::new();
        let executor = self.bridge.get_executor();
        let server_ctx = executor.server_context();
        let state = executor.get_state();
        loop {
            tokio::select! {
                Ok(msg) = self.rx.recv() => {
                    match msg {
                        LuaBridgeMessage::OpCall { obj_id, op, args, resp } => {
                            let bridge = self.bridge.clone();
                            let plc = plc.clone();
                            let Some(lua) = plc.weak_lua.try_upgrade() else {
                                let _ = resp.server(server_ctx).send(Err("Lua state has been dropped".into()));
                                continue;
                            };

                            op_call_queue.push(async move {
                                (op.run(&lua, obj_id, args, bridge, &plc).await, resp)
                            });
                        }
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
                    match result {
                        Ok(ret) => {
                            let Some(lua) = plc.weak_lua.try_upgrade() else {
                                let _ = resp.server(server_ctx).send(Err("Lua state has been dropped".into()));
                                continue;
                            };

                            match ret {
                                ValueOrMultiValue::Value(v) => {
                                    let mut ed = EmbedderDataContext::new(&plc.ed);
                                    match self.bridge.from_source_lua_value(&lua, &plc, v, &mut ed) {
                                        Ok(pv) => {
                                            let _ = resp.server(server_ctx).send(Ok(vec![pv]));
                                        }
                                        Err(e) => {
                                            let _ = resp.server(server_ctx).send(Err(format!("Failed to convert return value to foreign language value: {}", e).into()));
                                        }
                                    }
                                }
                                ValueOrMultiValue::MultiValue(v) => {
                                    let mut args = Vec::with_capacity(v.len());
                                    let mut err = None;
                                    let mut ed = EmbedderDataContext::new(&plc.ed);
                                    for v in v {
                                        match self.bridge.from_source_lua_value(&lua, &plc, v, &mut ed) {
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
                            }
                        }
                        Err(e) => {
                            let _ = resp.server(server_ctx).send(Err(format!("Lua opcall failed: {}", e).into()));
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

    /// Calls a Lua function by its ID with the given arguments, returning the results
    pub async fn opcall(&self, obj_id: LuauObjectRegistryID, op: LuauObjectOp, args: Vec<T::ValueType>) -> Result<Vec<T::ValueType>, String> {
        let (resp_tx, resp_rx) = self.client_context.oneshot();
        let msg = LuaBridgeMessage::OpCall {
            obj_id,
            op,
            args,
            resp: resp_tx,
        };
        self.tx.client(&self.client_context).send(msg).map_err(|e| format!("Failed to send opcall message: {}", e))?;
        resp_rx.recv().await.map_err(|e| format!("Failed to receive opcall response: {}", e))?
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