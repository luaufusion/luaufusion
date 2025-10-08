use crate::{base::ProxyBridge, luau::{embedder_api::EmbedderData, objreg::{LuauObjectRegistryID, ObjRegistryLuau}}};
use concurrentlyexec::{ClientContext, MultiReceiver, MultiSender, OneshotSender};
use mluau::{LuaSerdeExt, ObjectLike};
use serde::{Deserialize, Serialize};
use futures_util::stream::{FuturesUnordered, StreamExt};
use mluau::WeakLua;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub fn luau_value_to_obj_registry_type(val: &mluau::Value) -> Option<ObjectRegistryType> {
    match val {
        mluau::Value::Table(_) => Some(ObjectRegistryType::Table),
        mluau::Value::Function(_) => Some(ObjectRegistryType::Function),
        mluau::Value::UserData(_) => Some(ObjectRegistryType::UserData),
        mluau::Value::Buffer(_) => Some(ObjectRegistryType::Buffer),
        mluau::Value::Thread(_) => Some(ObjectRegistryType::Thread),
        _ => None,
    }
}

pub fn obj_registry_type_to_i32(typ: ObjectRegistryType) -> i32 {
    match typ {
        ObjectRegistryType::Table => 0,
        ObjectRegistryType::Function => 1,
        ObjectRegistryType::UserData => 2,
        ObjectRegistryType::Buffer => 3,
        ObjectRegistryType::Thread => 4,
    }
}

pub fn i32_to_obj_registry_type(val: i32) -> Option<ObjectRegistryType> {
    match val {
        0 => Some(ObjectRegistryType::Table),
        1 => Some(ObjectRegistryType::Function),
        2 => Some(ObjectRegistryType::UserData),
        3 => Some(ObjectRegistryType::Buffer),
        4 => Some(ObjectRegistryType::Thread),
        _ => None,
    }
}

#[derive(Clone)]
/// The client side state for proxying Lua values
/// 
/// This struct is not thread safe and must be kept on the Lua side
pub struct ProxyLuaClient {
    pub weak_lua: WeakLua,
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

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum LuauObjectOp {
    FunctionCallSync = 1,
    FunctionCallAsync = 2,
    Index = 3,
    Drop = 4,
}

impl TryFrom<u8> for LuauObjectOp {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(LuauObjectOp::FunctionCallSync),
            2 => Ok(LuauObjectOp::FunctionCallAsync),
            3 => Ok(LuauObjectOp::Index),
            4 => Ok(LuauObjectOp::Drop),
            _ => Err(format!("Invalid ObjectOp value: {}", value)),
        }
    }
}

enum ValueOrMultiValue {
    Value(mluau::Value),
    MultiValue(mluau::MultiValue),
}

impl LuauObjectOp {
    fn create_value_from_args<T: ProxyBridge>(arg: T::ValueType, bridge: &T, plc: &ProxyLuaClient) -> Result<mluau::Value, String> {
        let Some(lua) = plc.weak_lua.try_upgrade() else {
            return Err("Lua state has been dropped".into());
        };
        bridge.to_source_lua_value(&lua, arg, &plc)
            .map_err(|e| format!("Failed to convert argument to Lua value: {}", e))
    }
    fn create_multivalue_from_args<T: ProxyBridge>(args: Vec<T::ValueType>, bridge: &T, plc: &ProxyLuaClient) -> Result<mluau::MultiValue, String> {
        let Some(lua) = plc.weak_lua.try_upgrade() else {
            return Err("Lua state has been dropped".into());
        };
        let mut mv = mluau::MultiValue::with_capacity(args.len());
        let mut err = None;
        for arg in args {
            match bridge.to_source_lua_value(&lua, arg, &plc) {
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

    async fn run<T: ProxyBridge>(self, obj_id: LuauObjectRegistryID, args: Vec<T::ValueType>, bridge: T, plc: &ProxyLuaClient) -> Result<ValueOrMultiValue, Error> {
        match self {
            Self::FunctionCallSync => {
                let obj = plc.obj_registry.get(obj_id)
                    .map_err(|e| format!("Failed to get object from registry: {}", e))?;
                let args = Self::create_multivalue_from_args(args, &bridge, &plc)?;
                let func = match obj {
                    mluau::Value::Function(f) => f,
                    _ => return Err("Object is not a function".into()),
                };
                func.call(args)
                .map_err(|e| format!("Failed to call function: {}", e).into())
                .map(ValueOrMultiValue::MultiValue)
            }
            Self::FunctionCallAsync => {
                let Some(lua) = plc.weak_lua.try_upgrade() else {
                    return Err("Lua state has been dropped".into());
                };
                let obj = plc.obj_registry.get(obj_id)
                    .map_err(|e| format!("Failed to get object from registry: {}", e))?;
                let args = Self::create_multivalue_from_args(args, &bridge, &plc)?;
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
                let key = Self::create_value_from_args(arg, &bridge, &plc)?;

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
            Self::Drop => {
                if !args.is_empty() {
                    return Err("Drop operation does not take arguments".into());
                }
                plc.obj_registry.remove(obj_id)
                    .map_err(|e| format!("Failed to remove object from registry: {}", e))?;
                // Dropping is handled by the registry; just return nil
                Ok(ValueOrMultiValue::Value(mluau::Value::Nil))
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

                            op_call_queue.push(async move {
                                (op.run(obj_id, args, bridge, &plc).await, resp)
                            });

                            /*let th = match lua.create_thread(func.value()) {
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
                            });*/
                        }
                        LuaBridgeMessage::Shutdown => {
                            break;
                        }
                    }
                }
                _ = state.cancel_token.cancelled() => {
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
                                    match self.bridge.from_source_lua_value(&lua, &plc, v) {
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
}

impl<T: ProxyBridge> LuaBridgeServiceClient<T> {
    /// Creates a new Lua bridge service client
    pub fn new(client_context: ClientContext, tx: MultiSender<LuaBridgeMessage<T>>) -> Self {
        Self { client_context, tx }
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
}
