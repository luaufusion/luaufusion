use crate::base::{ObjectRegistryID, ProxyBridge};
use mluau::{LuaSerdeExt, ObjectLike};
use tokio::sync::{mpsc::UnboundedSender, mpsc::UnboundedReceiver,  oneshot::Sender};
use futures_util::stream::{FuturesUnordered, StreamExt};
use crate::base::{StringAtom, StringAtomList, ObjectRegistry};
use mluau::WeakLua;
use crate::MAX_PROXY_DEPTH;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
/// The client side state for proxying Lua values
/// 
/// This struct is not thread safe and must be kept on the Lua side
pub struct ProxyLuaClient {
    pub weak_lua: WeakLua,
    pub atom_list: StringAtomList,
    pub table_registry: ObjectRegistry<mluau::Table>,
    pub func_registry: ObjectRegistry<mluau::Function>,
    pub thread_registry: ObjectRegistry<mluau::Thread>,
    pub userdata_registry: ObjectRegistry<mluau::AnyUserData>,
    pub buffer_registry: ObjectRegistry<mluau::Buffer>,
}

/// A Lua value that can now be easily proxied to another language
pub enum ProxiedLuaValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(StringAtom), // To avoid large amounts of copying, we store strings in a separate atom list
    Table(ObjectRegistryID), // Table ID in the table registry
    Array(Vec<ProxiedLuaValue>),
    Function(ObjectRegistryID), // Function ID in the function registry
    UserData(ObjectRegistryID), // UserData ID in the userdata registry
    Vector((f32, f32, f32)),
    Buffer(ObjectRegistryID), // Buffer ID in the buffer registry
    Thread(ObjectRegistryID), // Thread ID in the thread registry
}

impl ProxiedLuaValue {
    /// Convert a Lua value to a proxied Lua value
    /// 
    /// This may fail if the Lua state is no longer valid or if the maximum proxy depth is exceeded
    pub fn from_lua_value(value: mluau::Value, plc: &ProxyLuaClient, depth: usize) -> Result<Self, Error> {
        if depth > MAX_PROXY_DEPTH {
            return Err(format!("Maximum proxy depth of {} exceeded", MAX_PROXY_DEPTH).into());
        }
        let v = match value {
            mluau::Value::Nil => ProxiedLuaValue::Nil,
            mluau::Value::LightUserData(s) => ProxiedLuaValue::Integer(s.0 as i64),
            mluau::Value::Boolean(b) => ProxiedLuaValue::Boolean(b),
            mluau::Value::Integer(i) => ProxiedLuaValue::Integer(i),
            mluau::Value::Number(n) => ProxiedLuaValue::Number(n),
            mluau::Value::String(s) => ProxiedLuaValue::String(plc.atom_list.get(s.as_bytes().as_ref())),
            mluau::Value::Table(t) => {
                let Some(lua) = t.weak_lua().try_upgrade() else {
                    return Err("Table's Lua state has been dropped".into());
                };

                if t.metatable() == Some(lua.array_metatable()) {
                    let length = t.raw_len();
                    let mut elements = Vec::with_capacity(length);
                    for i in 0..length {
                        let v = t.raw_get(i + 1)
                            .map_err(|e| format!("Failed to get array element {}: {}", i, e))?;
                        let pv = ProxiedLuaValue::from_lua_value(v, plc, depth + 1)
                            .map_err(|e| format!("Failed to convert array element {}: {}", i, e))?;
                        elements.push(pv);
                    }
                    return Ok(ProxiedLuaValue::Array(elements));
                }

                let table_id = plc.table_registry.add(t)
                    .ok_or_else(|| "Table registry is full".to_string())?;

                ProxiedLuaValue::Table(table_id)
            }
            mluau::Value::Function(f) => {
                let func_id = plc.func_registry.add(f)
                    .ok_or_else(|| "Function registry is full".to_string())?;
                ProxiedLuaValue::Function(func_id)
            }
            mluau::Value::UserData(ud) => {
                let userdata_id = plc.userdata_registry.add(ud)
                    .ok_or_else(|| "UserData registry is full".to_string())?;
                ProxiedLuaValue::UserData(userdata_id)
            }
            mluau::Value::Vector(v) => ProxiedLuaValue::Vector((v.x(), v.y(), v.z())),
            mluau::Value::Buffer(b) => {
                let buffer_id = plc.buffer_registry.add(b)
                    .ok_or_else(|| "Buffer registry is full".to_string())?;
                ProxiedLuaValue::Buffer(buffer_id)
            }
            mluau::Value::Thread(th) => {
                let thread_id = plc.thread_registry.add(th)
                    .ok_or_else(|| "Thread registry is full".to_string())?;
                ProxiedLuaValue::Thread(thread_id)
            }
            mluau::Value::Error(e) => return Err(format!("Cannot proxy Lua error value: {}", e).into()),
            mluau::Value::Other(r) => {
                let Some(lua) = plc.weak_lua.try_upgrade() else {
                    return Err("Lua state has been dropped".into());
                };
                let s = format!("unknown({r:?})");
                let s = lua.create_string(&s)
                    .map_err(|e| format!("Failed to create string for unknown value: {}", e))?;
                ProxiedLuaValue::String(plc.atom_list.get(s.as_bytes().as_ref()))
            },
        };

        Ok(v)
    }

    /// Convert a proxied Lua value back to a Lua value
    /// 
    /// This may fail if the Lua state is no longer valid
    /// 
    /// Not used directly in the bridge, but useful for testing
    pub fn convert_to_lua_value(&self, lua: &mluau::Lua, plc: &ProxyLuaClient) -> mluau::Result<mluau::Value> {
        match self {
            ProxiedLuaValue::Nil => Ok(mluau::Value::Nil),
            ProxiedLuaValue::Boolean(b) => Ok(mluau::Value::Boolean(*b)),
            ProxiedLuaValue::Integer(i) => Ok(mluau::Value::Integer(*i)),
            ProxiedLuaValue::Number(n) => Ok(mluau::Value::Number(*n)),
            ProxiedLuaValue::String(s) => {
                let s = lua.create_string(s.as_bytes())?;
                Ok(mluau::Value::String(s))
            }
            ProxiedLuaValue::Table(entries) => {
                let table = plc.table_registry.get(*entries)
                    .ok_or_else(|| mluau::Error::external(format!("Table ID {} not found in registry", entries)))?;
                Ok(mluau::Value::Table(table))
            }
            ProxiedLuaValue::Array(elements) => {
                let table = lua.create_table_with_capacity(elements.len(), 0)?;
                for (i, v) in elements.iter().enumerate() {
                    let lua_v = v.convert_to_lua_value(lua, plc)?;
                    table.raw_set(i + 1, lua_v)?; // Lua arrays are 1-based
                }
                table.set_metatable(Some(lua.array_metatable()))?;
                Ok(mluau::Value::Table(table))
            }
            ProxiedLuaValue::Function(func_id) => {
                let func = plc.func_registry.get(*func_id)
                    .ok_or_else(|| mluau::Error::external(format!("Function ID {} not found in registry", func_id)))?;
                Ok(mluau::Value::Function(func))
            }
            ProxiedLuaValue::UserData(ud_id) => {
                let ud = plc.userdata_registry.get(*ud_id)
                    .ok_or_else(|| mluau::Error::external(format!("UserData ID {} not found in registry", ud_id)))?;
                Ok(mluau::Value::UserData(ud))
            }
            ProxiedLuaValue::Vector((x, y, z)) => {
                Ok(mluau::Value::Vector(mluau::Vector::new(*x, *y, *z)))
            }
            ProxiedLuaValue::Buffer(buf_id) => {
                let b = plc.buffer_registry.get(*buf_id)
                    .ok_or_else(|| mluau::Error::external(format!("Buffer ID {} not found in registry", buf_id)))?;
                Ok(mluau::Value::Buffer(b))
            }
            ProxiedLuaValue::Thread(th_id) => {
                let th = plc.thread_registry.get(*th_id)
                    .ok_or_else(|| mluau::Error::external(format!("Thread ID {} not found in registry", th_id)))?;
                Ok(mluau::Value::Thread(th))
            }
        }
    }
}

#[derive(Clone, Copy)]
pub enum ObjectRegistryType {
    Table,
    Function,
    UserData,
    Buffer,
    Thread,
}

/// Messages sent to the Lua proxy bridge
pub enum LuaBridgeMessage<T: ProxyBridge> {
    CallFunction {
        func_id: ObjectRegistryID,
        args: Vec<T::ValueType>,
        resp: Sender<Result<Vec<ProxiedLuaValue>, Error>>,
    },
    ReadBuffer {
        buffer_id: ObjectRegistryID,
        resp: Sender<Result<Vec<u8>, Error>>,
    },
    DropObject {
        obj_type: ObjectRegistryType,
        obj_id: ObjectRegistryID,
    },
    IndexUserData {
        obj_id: ObjectRegistryID,
        key: T::ValueType,
        resp: Sender<Result<ProxiedLuaValue, Error>>,
    },
    Shutdown,
}

/// Thread safe proxy bridge between Luau and another language's proxy bridge
pub struct LuaBridge<T: ProxyBridge> {
    x: UnboundedSender<LuaBridgeMessage<T>>,
    drop: bool,
}

impl<T: ProxyBridge> Drop for LuaBridge<T> {
    fn drop(&mut self) {
        if self.drop {
            let _ = self.x.send(LuaBridgeMessage::Shutdown);
        }
    }
}

impl<T: ProxyBridge> Clone for LuaBridge<T> {
    fn clone(&self) -> Self {
        Self {
            x: self.x.clone(),
            drop: false, // Cloned instances do not send Shutdown on drop
        }
    }
}

impl<T: ProxyBridge> LuaBridge<T> {
    /// Creates a new Lua proxy bridge
    pub async fn run(plc: ProxyLuaClient, bridge: T, mut rx: UnboundedReceiver<LuaBridgeMessage<T>>) {
        let mut func_call_queue = FuturesUnordered::new();
        loop {
            tokio::select! {
                Some(msg) = rx.recv() => {
                    match msg {
                        LuaBridgeMessage::CallFunction { func_id, args, resp } => {
                            let func = match plc.func_registry.get(func_id) {
                                Some(f) => f,
                                None => {
                                    let _ = resp.send(Err(format!("Function ID {} not found in registry", func_id).into()));
                                    continue;
                                }
                            };

                            let Some(lua) = plc.weak_lua.try_upgrade() else {
                                let _ = resp.send(Err("Lua state has been dropped".into()));
                                continue;
                            };

                            let mv = {
                                let mut mv = mluau::MultiValue::with_capacity(args.len());
                                let mut err = None;
                                for arg in args {
                                    match bridge.to_source_lua_value(&lua, arg, &plc, 0) {
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
                                    let _ = resp.send(Err(format!("Failed to convert argument to Lua value: {}", e).into()));
                                    continue;
                                }

                                mv
                            };

                            let th = match lua.create_thread(func) {
                                Ok(t) => t,
                                Err(e) => {
                                    let _ = resp.send(Err(format!("Failed to create Lua thread: {}", e).into()));
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
                            let _ = resp.send(result);
                        }
                        LuaBridgeMessage::DropObject { obj_type, obj_id } => {
                            match obj_type {
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
                                    return Err("Lua state has been dropped".into());
                                };
                                let key_val = bridge.to_source_lua_value(&lua, key, &plc, 0)
                                    .map_err(|e| format!("Failed to convert key to Lua value: {}", e))?;
                                let val = userdata.get::<mluau::Value>(key_val)
                                    .map_err(|e| format!("Failed to index UserData: {}", e))?;
                                ProxiedLuaValue::from_lua_value(val, &plc, 0)
                            })();
                            let _ = resp.send(result);
                        }
                    }
                }
                Some((result, resp)) = func_call_queue.next() => {
                    match result {
                        Ok(ret) => {
                            match ret {
                                Some(v) => {
                                    match v {
                                        Ok(v) => {
                                            let mut args = Vec::with_capacity(v.len());
                                            for v in v {
                                                match ProxiedLuaValue::from_lua_value(v, &plc, 0) {
                                                    Ok(pv) => {
                                                        args.push(pv);
                                                    }
                                                    Err(e) => {
                                                        let _ = resp.send(Err(format!("Failed to convert return value to proxied Lua value: {}", e).into()));
                                                        return;
                                                    }
                                                }
                                            }

                                            let _ = resp.send(Ok(args));
                                        }
                                        Err(e) => {
                                            let _ = resp.send(Err(format!("Lua function error: {}", e).into()));
                                        }
                                    }
                                }
                                None => {
                                    let _ = resp.send(Ok(vec![]));
                                }
                            }
                        }
                        Err(e) => {
                            let _ = resp.send(Err(format!("Failed to call Lua function: {}", e).into()));
                        }
                    }
                }
            }
        }
    }

    /// Calls a Lua function by its ID with the given arguments, returning the results
    pub async fn call_function(&self, func_id: ObjectRegistryID, args: Vec<T::ValueType>) -> Result<Vec<ProxiedLuaValue>, Error> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.x.send(LuaBridgeMessage::CallFunction { func_id, args, resp: resp_tx })
            .map_err(|e| format!("Failed to send CallFunction message: {}", e))?;
        resp_rx.await.map_err(|e| format!("Failed to receive CallFunction response: {}", e))?
    }

    /// Reads the contents of a Lua buffer by its ID
    pub async fn read_buffer(&self, buffer_id: ObjectRegistryID) -> Result<Vec<u8>, Error> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.x.send(LuaBridgeMessage::ReadBuffer { buffer_id, resp: resp_tx })
            .map_err(|e| format!("Failed to send ReadBuffer message: {}", e))?;
        resp_rx.await.map_err(|e| format!("Failed to receive ReadBuffer response: {}", e))?
    }

    /// Requests that an object be dropped from the registry
    pub fn request_drop_object(&self, obj_type: ObjectRegistryType, obj_id: ObjectRegistryID) {
        let _ = self.x.send(LuaBridgeMessage::DropObject { obj_type, obj_id });
    }
}

mod asserter {
    //const fn assert_send_const<T: Send>() {}
    //const _: () = assert_send_const::<LuaBridge<crate::deno::bridge::V8ProxyBridge>>(); 
    //const _: () = assert_send_const::<LuaBridge<crate::base::quickjs::bridge::QuickJSProxyBridge>>();
}