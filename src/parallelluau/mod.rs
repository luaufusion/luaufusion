use mluau::LuaSerdeExt;

use crate::{base::{ObjectRegistryID, ProxyBridge, StringAtom}, luau::bridge::ProxyLuaClient};

pub enum ParallelLuaProxiedValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(StringAtom), // To avoid large amounts of copying, we store strings in a separate atom list
    Table(Vec<(ParallelLuaProxiedValue, ParallelLuaProxiedValue)>),
    Array(Vec<ParallelLuaProxiedValue>),
    SrcFunction(ObjectRegistryID), // Function ID in the source lua's function registry
    SrcUserData(ObjectRegistryID), // UserData ID in the source lua's userdata registry
    Vector((f32, f32, f32)),
    SrcBuffer(ObjectRegistryID), // Buffer ID in the source lua's buffer registry
    SrcThread(ObjectRegistryID), // Thread ID in the source lua's thread registry
}

impl ParallelLuaProxiedValue {
    /// Convert a proxied parallel Lua value to a Lua value
    /// on the source Lua state (not the proxied one)
    pub fn to_src_lua_value(
        &self, 
        lua: &mluau::Lua, 
        bridge: &ParallelLuaProxyBridge, 
        plc: &ProxyLuaClient,
        depth: usize
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
            ParallelLuaProxiedValue::Table(entries) => {
                let table = lua.create_table()?;
                for (k, v) in entries {
                    let lua_k = k.to_src_lua_value(lua, bridge, plc, depth + 1)?;
                    let lua_v = v.to_src_lua_value(lua, bridge, plc, depth + 1)?;
                    table.raw_set(lua_k, lua_v)?;
                }
                Ok(mluau::Value::Table(table))
            }
            ParallelLuaProxiedValue::Array(elements) => {
                let table = lua.create_table_with_capacity(elements.len(), 0)?;
                for (i, v) in elements.iter().enumerate() {
                    let lua_v = v.to_src_lua_value(lua, bridge, plc, depth + 1)?;
                    table.raw_set(i + 1, lua_v)?; // Lua arrays are 1-based
                }
                table.set_metatable(Some(lua.array_metatable()))?;
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
}