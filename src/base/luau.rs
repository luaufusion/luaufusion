use mluau::{Lua, LuaSerdeExt, WeakLua};
use tokio::sync::{mpsc::{UnboundedReceiver, UnboundedSender}, oneshot::Sender};

use super::{StringAtom, StringAtomList, MAX_PROXY_DEPTH, ObjectRegistry};

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
    Function(i32), // Function ID in the function registry
    UserData(i32), // UserData ID in the userdata registry
    Vector((f32, f32, f32)),
    Buffer(i32), // Buffer ID in the buffer registry
    Thread(i32), // Thread ID in the thread registry
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
            mluau::Value::String(s) => ProxiedLuaValue::String(plc.atom_list.get(s)),
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
                let func_id = plc.func_registry.add(f);
                ProxiedLuaValue::Function(func_id)
            }
            mluau::Value::UserData(ud) => {
                let userdata_id = plc.userdata_registry.add(ud);
                ProxiedLuaValue::UserData(userdata_id)
            }
            mluau::Value::Vector(v) => ProxiedLuaValue::Vector((v.x(), v.y(), v.z())),
            mluau::Value::Buffer(b) => {
                let buffer_id = plc.buffer_registry.add(b);
                ProxiedLuaValue::Buffer(buffer_id)
            }
            mluau::Value::Thread(th) => {
                let thread_id = plc.thread_registry.add(th);
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
                ProxiedLuaValue::String(plc.atom_list.get(s))
            },
        };

        Ok(v)
    }

    /// Convert a proxied Lua value back to a Lua value
    /// 
    /// This may fail if the Lua state is no longer valid
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
}

pub(crate) enum ObjectRegistryType {
    Function,
    UserData,
    Buffer,
    Thread,
}

pub(crate) enum LuaProxyBridgeMessage {
    CallFunction {
        func_id: i32,
        args: Vec<ProxiedLuaValue>,
        resp: Sender<Result<ProxiedLuaValue, Error>>,
    },
    ReadBuffer {
        buffer_id: i32,
        resp: Sender<Result<Vec<u8>, Error>>,
    },
    DropObject {
        obj_type: ObjectRegistryType,
        obj_id: i32,
    },
}

/// Thread safe proxy bridge between Luau and another language
pub struct LuaProxyBridge {
    x: UnboundedSender<LuaProxyBridgeMessage>
}

impl LuaProxyBridge {
    // TODO: Implement actual 'server' impl. For now, this is client only
    
    /// Creates a new Lua proxy bridge
    pub(crate) async fn run(plc: ProxyLuaClient, mut rx: UnboundedReceiver<LuaProxyBridgeMessage>) {

    }

    /// Calls a Lua function by its ID with the given arguments
    pub(crate) async fn call_function(&self, func_id: i32, args: Vec<ProxiedLuaValue>) -> Result<ProxiedLuaValue, Error> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.x.send(LuaProxyBridgeMessage::CallFunction { func_id, args, resp: resp_tx })
            .map_err(|e| format!("Failed to send CallFunction message: {}", e))?;
        resp_rx.await.map_err(|e| format!("Failed to receive CallFunction response: {}", e))?
    }

    /// Reads the contents of a Lua buffer by its ID
    pub(crate) async fn read_buffer(&self, buffer_id: i32) -> Result<Vec<u8>, Error> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.x.send(LuaProxyBridgeMessage::ReadBuffer { buffer_id, resp: resp_tx })
            .map_err(|e| format!("Failed to send ReadBuffer message: {}", e))?;
        resp_rx.await.map_err(|e| format!("Failed to receive ReadBuffer response: {}", e))?
    }

    /// Requests that an object be dropped from the registry
    pub(crate) fn request_drop_object(&self, obj_type: ObjectRegistryType, obj_id: i32) {
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
