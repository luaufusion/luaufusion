use mluau::LuaSerdeExt;

use crate::base::{ObjectRegistryID, ProxyBridge, StringAtom};

pub enum ParallelLuaProxiedValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(StringAtom), // To avoid large amounts of copying, we store strings in a separate atom list
    Table(Vec<(ParallelLuaProxiedValue, ParallelLuaProxiedValue)>),
    Array(Vec<ParallelLuaProxiedValue>),
    Function(ObjectRegistryID), // Function ID in the function registry
    UserData(ObjectRegistryID), // UserData ID in the userdata registry
    Vector((f32, f32, f32)),
    Buffer(ObjectRegistryID), // Buffer ID in the buffer registry
    Thread(ObjectRegistryID), // Thread ID in the thread registry
}

impl ParallelLuaProxiedValue {
    /// Convert a proxied Lua value to a parallel Lua value
    /// 
    /// This may fail if the Lua state is no longer valid
    pub fn to_parallel_lua_value(
        &self, 
        lua: &mluau::Lua, 
        bridge: &ProxyLua, 
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
                    let lua_k = k.to_parallel_lua_value(lua, bridge, depth + 1)?;
                    let lua_v = v.to_parallel_lua_value(lua, bridge, depth + 1)?;
                    table.raw_set(lua_k, lua_v)?;
                }
                Ok(mluau::Value::Table(table))
            }
            ParallelLuaProxiedValue::Array(elements) => {
                let table = lua.create_table_with_capacity(elements.len(), 0)?;
                for (i, v) in elements.iter().enumerate() {
                    let lua_v = v.to_parallel_lua_value(lua, bridge, depth + 1)?;
                    table.raw_set(i + 1, lua_v)?; // Lua arrays are 1-based
                }
                table.set_metatable(Some(lua.array_metatable()))?;
                Ok(mluau::Value::Table(table))
            }
            _ => return Err(mluau::Error::external("Cannot proxy unsupported type"))
            /*ParallelLuaProxiedValue::Function(func_id) => {
                let bridge_ref = bridge.clone();
                let src_plc_ref = src_plc.clone();
                let func_id = *func_id;
                struct FunctionIdDtorProxy {
                    bridge: ProxyLua,
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

                let b = lua.create_userdata(ProxiedBuffer { bridge: bridge.clone(), buffer_id: *buf_id })?;
                Ok(mluau::Value::UserData(b))
            },
            ProxiedLuaValue::Vector((x, y, z)) => {
                Ok(mluau::Value::Vector(mluau::Vector::new(*x, *y, *z)))
            }
            _ => Err(mluau::Error::external("Cannot convert this ProxiedLuaValue to a parallel Lua value")),
            ProxiedLuaValue::UserData(ud_id) => {
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

#[derive(Clone)]
pub enum ProxyLua {}

impl ProxyBridge for ProxyLua {
    type ValueType = ParallelLuaProxiedValue;
    
    fn to_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, depth: usize) -> Result<mluau::Value, crate::base::Error> {
        return Ok(value.to_parallel_lua_value(lua, self, depth).map_err(|x| x.to_string())?)
    }
}