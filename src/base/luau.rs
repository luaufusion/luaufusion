use futures_util::{stream::FuturesUnordered, StreamExt};
use mlua_scheduler::LuaSchedulerAsync;
use mlua_scheduler::LuaSchedulerAsyncUserData;
use mluau::{LuaSerdeExt, WeakLua};
use tokio::sync::{mpsc::{UnboundedReceiver, UnboundedSender}, oneshot::Sender};
use crate::base::OtherProxyBridges;

use super::{StringAtom, StringAtomList, MAX_PROXY_DEPTH, ObjectRegistry, ValueArgs, ObjectRegistryID};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
/// The client side state for proxying Lua values
/// 
/// This struct is not thread safe and must be kept on the Lua side
pub(crate) struct ProxyLuaClient {
    pub weak_lua: WeakLua,
    pub atom_list: StringAtomList,
    pub func_registry: ObjectRegistry<mluau::Function>,
    pub thread_registry: ObjectRegistry<mluau::Thread>,
    pub userdata_registry: ObjectRegistry<mluau::AnyUserData>,
    pub buffer_registry: ObjectRegistry<mluau::Buffer>,
}

/// A Lua value that can now be easily proxied to another language
pub(crate) enum ProxiedLuaValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(StringAtom), // To avoid large amounts of copying, we store strings in a separate atom list
    Table(Vec<(ProxiedLuaValue, ProxiedLuaValue)>),
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
    pub(crate) fn from_lua_value(value: mluau::Value, plc: &ProxyLuaClient, depth: usize) -> Result<Self, Error> {
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

                let mut entries = Vec::new();
                t.for_each(|k, v| {
                    let pk = ProxiedLuaValue::from_lua_value(k, plc, depth + 1)
                        .map_err(|e| mluau::Error::external(format!("Failed to convert table key: {}", e)))?;
                    let pv = ProxiedLuaValue::from_lua_value(v, plc, depth + 1)
                        .map_err(|e| mluau::Error::external(format!("Failed to convert table value: {}", e)))?;
                    entries.push((pk, pv));
                    Ok(())
                })
                .map_err(|e| format!("Failed to iterate table: {}", e))?;

                ProxiedLuaValue::Table(entries)
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
    /// Use `to_parallel_lua_value` to convert to a Luau value in another Luau VM (parallel Luau)
    pub(crate) fn to_lua_value(&self, lua: &mluau::Lua, plc: &ProxyLuaClient, depth: usize) -> mluau::Result<mluau::Value> {
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
                let table = lua.create_table()?;
                for (k, v) in entries {
                    let lua_k = k.to_lua_value(lua, plc, depth + 1)?;
                    let lua_v = v.to_lua_value(lua, plc, depth + 1)?;
                    table.raw_set(lua_k, lua_v)?;
                }
                Ok(mluau::Value::Table(table))
            }
            ProxiedLuaValue::Array(elements) => {
                let table = lua.create_table_with_capacity(elements.len(), 0)?;
                for (i, v) in elements.iter().enumerate() {
                    let lua_v = v.to_lua_value(lua, plc, depth + 1)?;
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

    /// Convert a proxied Lua value to a parallel Lua value
    /// 
    /// This may fail if the Lua state is no longer valid
    pub(crate) fn to_parallel_lua_value(
        &self, 
        lua: &mluau::Lua, 
        bridge: &LuaProxyBridge, 
        src_plc: &ProxyLuaClient,
        depth: usize
    ) -> mluau::Result<mluau::Value> {
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
                let table = lua.create_table()?;
                for (k, v) in entries {
                    let lua_k = k.to_parallel_lua_value(lua, bridge, src_plc, depth + 1)?;
                    let lua_v = v.to_parallel_lua_value(lua, bridge, src_plc, depth + 1)?;
                    table.raw_set(lua_k, lua_v)?;
                }
                Ok(mluau::Value::Table(table))
            }
            ProxiedLuaValue::Array(elements) => {
                let table = lua.create_table_with_capacity(elements.len(), 0)?;
                for (i, v) in elements.iter().enumerate() {
                    let lua_v = v.to_parallel_lua_value(lua, bridge, src_plc, depth + 1)?;
                    table.raw_set(i + 1, lua_v)?; // Lua arrays are 1-based
                }
                table.set_metatable(Some(lua.array_metatable()))?;
                Ok(mluau::Value::Table(table))
            }
            ProxiedLuaValue::Function(func_id) => {
                let bridge_ref = bridge.clone();
                let src_plc_ref = src_plc.clone();
                let func_id = *func_id;
                struct FunctionIdDtorProxy {
                    bridge: LuaProxyBridge,
                    obj_id: ObjectRegistryID,
                }

                impl mluau::UserData for FunctionIdDtorProxy {}
                impl Drop for FunctionIdDtorProxy {
                    fn drop(&mut self) {
                        self.bridge.request_drop_object(ObjectRegistryType::Function, self.obj_id);
                    }
                }
                let dtor = lua.create_userdata(FunctionIdDtorProxy { bridge: bridge_ref.clone(), obj_id: func_id })?;
                let func = lua.create_scheduler_async_function(move |lua, args: mluau::MultiValue| {
                    let bridge = bridge_ref.clone();
                    let src_plc = src_plc_ref.clone();
                    let _dtor = dtor.clone(); // dtor gets moved into the async block to keep it alive
                    async move {
                        let mut proxied_args = Vec::with_capacity(args.len());
                        for v in args {
                            let pv = ProxiedLuaValue::from_lua_value(v, &src_plc, 0)
                                .map_err(|e| mluau::Error::external(format!("Failed to convert argument to proxied Lua value: {}", e)))?;
                            proxied_args.push(pv);
                        }

                        let ret = bridge.call_function(func_id,ValueArgs::Lua(proxied_args)).await
                            .map_err(|e| mluau::Error::external(format!("Failed to call proxied Lua function: {}", e)))?;

                        let mut lua_rets = mluau::MultiValue::with_capacity(ret.len());
                        for v in ret {
                            let lv = v.to_parallel_lua_value(&lua, &bridge, &src_plc, 0)
                                .map_err(|e| mluau::Error::external(format!("Failed to convert return value to Lua value: {}", e)))?;
                            lua_rets.push_back(lv);
                        }

                        Ok(lua_rets)
                    }
                })?;

                Ok(mluau::Value::Function(func))
            }
            ProxiedLuaValue::Buffer(buf_id) => {
                pub struct ProxiedBuffer {
                    bridge: LuaProxyBridge,
                    src_plc: ProxyLuaClient,
                    buffer_id: ObjectRegistryID,
                }

                impl mluau::UserData for ProxiedBuffer {
                    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
                        methods.add_scheduler_async_method("read", |lua, this, ()| {
                            let bridge = this.bridge.clone();
                            let buffer_id = this.buffer_id;
                            async move {
                                let data = bridge.read_buffer(buffer_id).await
                                    .map_err(|e| mluau::Error::external(format!("Failed to read proxied buffer: {}", e)))?;
                                let buf = lua.create_buffer(data)?;
                                Ok(buf)
                            }
                        });
                    }
                }

                impl Drop for ProxiedBuffer {
                    fn drop(&mut self) {
                        self.bridge.request_drop_object(ObjectRegistryType::Buffer, self.buffer_id);
                    }
                }

                let b = lua.create_userdata(ProxiedBuffer { bridge: bridge.clone(), src_plc: src_plc.clone(), buffer_id: *buf_id })?;
                Ok(mluau::Value::UserData(b))
            },
            ProxiedLuaValue::Vector((x, y, z)) => {
                Ok(mluau::Value::Vector(mluau::Vector::new(*x, *y, *z)))
            }
            _ => Err(mluau::Error::external("Cannot convert this ProxiedLuaValue to a parallel Lua value")),
            /*ProxiedLuaValue::UserData(ud_id) => {
                let ud = plc.userdata_registry.get(*ud_id)
                    .ok_or_else(|| mluau::Error::external(format!("UserData ID {} not found in registry", ud_id)))?;
                Ok(mluau::Value::UserData(ud))
            }
            ProxiedLuaValue::Thread(th_id) => {
                let th = plc.thread_registry.get(*th_id)
                    .ok_or_else(|| mluau::Error::external(format!("Thread ID {} not found in registry", th_id)))?;
                Ok(mluau::Value::Thread(th))
            }*/
        }
    }
}

pub(crate) enum ObjectRegistryType {
    Function,
    UserData,
    Buffer,
    Thread,
}

pub(crate) enum LuaProxyBridgeMessage {
    CallFunction {
        func_id: ObjectRegistryID,
        args: ValueArgs,
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
    Shutdown,
}

/// Thread safe proxy bridge between Luau and another language
pub struct LuaProxyBridge {
    x: UnboundedSender<LuaProxyBridgeMessage>,
    drop: bool,
}

impl Drop for LuaProxyBridge {
    fn drop(&mut self) {
        if self.drop {
            let _ = self.x.send(LuaProxyBridgeMessage::Shutdown);
        }
    }
}

impl Clone for LuaProxyBridge {
    fn clone(&self) -> Self {
        Self {
            x: self.x.clone(),
            drop: false, // Cloned instances do not send Shutdown on drop
        }
    }
}

impl LuaProxyBridge {
    // TODO: Implement actual 'server' impl. For now, this is client only
    
    /// Creates a new Lua proxy bridge
    pub(crate) async fn run(plc: ProxyLuaClient, ol: OtherProxyBridges, mut rx: UnboundedReceiver<LuaProxyBridgeMessage>) {
        let mut func_call_queue = FuturesUnordered::new();
        loop {
            tokio::select! {
                Some(msg) = rx.recv() => {
                    match msg {
                        LuaProxyBridgeMessage::CallFunction { func_id, args, resp } => {
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

                            let mv = match args.to_lua_value(&lua, &plc, &ol) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = resp.send(Err(format!("Failed to convert arguments to Lua values: {}", e).into()));
                                    continue;
                                }
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
                        LuaProxyBridgeMessage::ReadBuffer { buffer_id, resp } => {
                            let result = (|| {
                                let buffer = plc.buffer_registry.get(buffer_id)
                                    .ok_or_else(|| format!("Buffer ID {} not found in registry", buffer_id))?;
                                Ok(buffer.to_vec())
                            })();
                            let _ = resp.send(result);
                        }
                        LuaProxyBridgeMessage::DropObject { obj_type, obj_id } => {
                            match obj_type {
                                ObjectRegistryType::Function => { plc.func_registry.remove(obj_id); /* Just drop reference */ }
                                ObjectRegistryType::UserData => { plc.userdata_registry.remove(obj_id); /* Just drop reference */ }
                                ObjectRegistryType::Buffer => { plc.buffer_registry.remove(obj_id); /* Just drop reference */ }
                                ObjectRegistryType::Thread => { plc.thread_registry.remove(obj_id); /* Just drop reference */ }
                            }
                        }
                        LuaProxyBridgeMessage::Shutdown => {
                            break;
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

    pub(crate) async fn call_function(&self, func_id: ObjectRegistryID, args: ValueArgs) -> Result<Vec<ProxiedLuaValue>, Error> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.x.send(LuaProxyBridgeMessage::CallFunction { func_id, args, resp: resp_tx })
            .map_err(|e| format!("Failed to send CallFunction message: {}", e))?;
        resp_rx.await.map_err(|e| format!("Failed to receive CallFunction response: {}", e))?
    }

    /// Reads the contents of a Lua buffer by its ID
    pub(crate) async fn read_buffer(&self, buffer_id: ObjectRegistryID) -> Result<Vec<u8>, Error> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.x.send(LuaProxyBridgeMessage::ReadBuffer { buffer_id, resp: resp_tx })
            .map_err(|e| format!("Failed to send ReadBuffer message: {}", e))?;
        resp_rx.await.map_err(|e| format!("Failed to receive ReadBuffer response: {}", e))?
    }

    /// Requests that an object be dropped from the registry
    pub(crate) fn request_drop_object(&self, obj_type: ObjectRegistryType, obj_id: ObjectRegistryID) {
        let _ = self.x.send(LuaProxyBridgeMessage::DropObject { obj_type, obj_id });
    }
}

mod asserter {
    use super::ProxiedLuaValue;
    use super::LuaProxyBridge;

    const fn assert_send_const<T: Send>() {}
    const _: () = assert_send_const::<ProxiedLuaValue>(); 
    const _: () = assert_send_const::<LuaProxyBridge>();
}
