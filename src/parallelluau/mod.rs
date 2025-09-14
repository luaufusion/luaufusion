use crate::{base::{ObjectRegistryID, ProxyBridge}, luau::bridge::{LuaBridgeObject, ProxyLuaClient}};

#[derive(Clone, Copy)]
pub struct ParallelLuaBridgeObject;

pub enum ParallelLuaProxiedValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(String), // To avoid large amounts of copying, we store strings in a separate atom list
    Vector((f32, f32, f32)),
    SrcFunction(ObjectRegistryID<LuaBridgeObject>), // Function ID in the source lua's function registry
    SrcUserData(ObjectRegistryID<LuaBridgeObject>), // UserData ID in the source lua's userdata registry
    SrcTable(ObjectRegistryID<LuaBridgeObject>), // Table ID in the source lua's table registry
    SrcBuffer(ObjectRegistryID<LuaBridgeObject>), // Buffer ID in the source lua's buffer registry
    SrcThread(ObjectRegistryID<LuaBridgeObject>), // Thread ID in the source lua's thread registry
}

impl ParallelLuaProxiedValue {
    /// Convert a proxied parallel Lua value to a Lua value
    /// on the source Lua state (not the proxied one)
    pub fn to_src_lua_value(
        &self, 
        lua: &mluau::Lua, 
        _bridge: &ParallelLuaProxyBridge, 
        plc: &ProxyLuaClient,
        _depth: usize
    ) -> mluau::Result<mluau::Value> {
        match self {
            ParallelLuaProxiedValue::Nil => Ok(mluau::Value::Nil),
            ParallelLuaProxiedValue::Boolean(b) => Ok(mluau::Value::Boolean(*b)),
            ParallelLuaProxiedValue::Integer(i) => Ok(mluau::Value::Integer(*i)),
            ParallelLuaProxiedValue::Number(n) => Ok(mluau::Value::Number(*n)),
            ParallelLuaProxiedValue::String(s) => {
                let s = lua.create_string(s.as_bytes())?;
                Ok(mluau::Value::String(s))
            }
            ParallelLuaProxiedValue::SrcTable(table_id) => {
                let table = plc.table_registry.get(*table_id)
                    .ok_or_else(|| mluau::Error::external(format!("Table ID {} not found in registry", table_id)))?;
                Ok(mluau::Value::Table(table))
            }
            ParallelLuaProxiedValue::SrcFunction(func_id) => {
                let func = plc.func_registry.get(*func_id)
                    .ok_or_else(|| mluau::Error::external(format!("Function ID {} not found in registry", func_id)))?;
                Ok(mluau::Value::Function(func))
            }
            ParallelLuaProxiedValue::SrcUserData(ud_id) => {
                let ud = plc.userdata_registry.get(*ud_id)
                    .ok_or_else(|| mluau::Error::external(format!("UserData ID {} not found in registry", ud_id)))?;
                Ok(mluau::Value::UserData(ud))
            }
            ParallelLuaProxiedValue::Vector((x, y, z)) => {
                Ok(mluau::Value::Vector(mluau::Vector::new(*x, *y, *z)))
            }
            ParallelLuaProxiedValue::SrcBuffer(buf_id) => {
                let b = plc.buffer_registry.get(*buf_id)
                    .ok_or_else(|| mluau::Error::external(format!("Buffer ID {} not found in registry", buf_id)))?;
                Ok(mluau::Value::Buffer(b))
            }
            ParallelLuaProxiedValue::SrcThread(th_id) => {
                let th = plc.thread_registry.get(*th_id)
                    .ok_or_else(|| mluau::Error::external(format!("Thread ID {} not found in registry", th_id)))?;
                Ok(mluau::Value::Thread(th))
            }
        }
    }
}

#[derive(Clone)]
pub enum ParallelLuaProxyBridge {}

impl ProxyBridge for ParallelLuaProxyBridge {
    type ValueType = ParallelLuaProxiedValue;
    
    fn to_source_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, plc: &ProxyLuaClient, depth: usize) -> Result<mluau::Value, crate::base::Error> {
        return Ok(value.to_src_lua_value(lua, self, plc, depth).map_err(|x| x.to_string())?)
    }

    async fn eval_from_source(&self, _code: &str, _args: Vec<crate::luau::bridge::ProxiedLuaValue>) -> Result<Self::ValueType, crate::base::Error> {
        Err("Not implemented".into())
    }
}