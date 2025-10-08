use mlua_scheduler::LuaSchedulerAsyncUserData;
use super::primitives::ProxiedV8Primitive;
use super::bridge::{
    V8ObjectRegistryType, V8IsolateManagerServer, v8_obj_registry_type_to_i32,
};
use crate::base::Error;
use crate::denobridge::bridge::V8ObjectOp;
use crate::denobridge::objreg::V8ObjectRegistryID;
use crate::denobridge::value::ProxiedV8Value;
use crate::luau::bridge::ProxyLuaClient;
use crate::luau::embedder_api::EmbedderDataContext;

/// The core struct encapsulating a V8 object being proxied *to* luau
pub struct V8Value {
    pub id: V8ObjectRegistryID,
    pub typ: V8ObjectRegistryType,
    pub plc: ProxyLuaClient,
    pub bridge: V8IsolateManagerServer,
}

impl V8Value {
    /// Create a new V8Value
    pub fn new(id: V8ObjectRegistryID, plc: ProxyLuaClient, bridge: V8IsolateManagerServer, typ: V8ObjectRegistryType) -> Self {
        Self {
            id,
            typ,
            plc,
            bridge,
        }
    }

    fn type_name(&self) -> &'static str {
        match self.typ {
            V8ObjectRegistryType::Function => "V8Function",
            V8ObjectRegistryType::Object => "V8Object",
            V8ObjectRegistryType::ArrayBuffer => "V8ArrayBuffer",
            V8ObjectRegistryType::Promise => "V8Promise",
        }
    }

    /// Do a op call with a single primitive argument (optimized)
    async fn op_call_primitive(&self, lua: &mluau::Lua, obj_id: V8ObjectRegistryID, op: V8ObjectOp, args: mluau::Value) -> Result<mluau::MultiValue, Error> {
        let mut ed = EmbedderDataContext::new(&self.plc.ed);
        let arg = ProxiedV8Primitive::from_luau(&args, &mut ed)
        .map_err(|e| format!("Failed to proxy argument to ProxiedV8Value: {}", e))?
        .ok_or(format!("Argument is not a primitive value"))?;
        
        match self.bridge.send(super::bridge::OpCallMessage {
            obj_id,
            op,
            args: vec![ProxiedV8Value::Primitive(arg)],
        })
        .await? {
            Ok(v) => {
                let mut proxied = mluau::MultiValue::with_capacity(v.len());
                let mut ed = EmbedderDataContext::new(&self.plc.ed);
                for ret in v {
                    let ret = ret.to_luau(&lua, &self.plc, &self.bridge, &mut ed)
                    .map_err(|e| format!("Failed to convert return value to Lua: {}", e))?;
                    proxied.push_back(ret);
                }
                Ok(proxied)
            },
            Err(e) => Err(e.into()),
        }
    
    }

    /// Do a op call with multiple arguments
    async fn op_call(&self, lua: &mluau::Lua, obj_id: V8ObjectRegistryID, op: V8ObjectOp, args: mluau::MultiValue) -> Result<mluau::MultiValue, Error> {
        let mut ed = EmbedderDataContext::new(&self.plc.ed);
        
        let mut args_proxied = Vec::with_capacity(args.len());
        for arg in args {
            let proxied = ProxiedV8Value::from_luau(&self.plc, arg, &mut ed)
            .map_err(|e| format!("Failed to proxy argument to ProxiedV8Value: {}", e))?;
            args_proxied.push(proxied);
        }
        match self.bridge.send(super::bridge::OpCallMessage {
            obj_id,
            op,
            args: args_proxied,
        })
        .await? {
            Ok(v) => {
                let mut proxied = mluau::MultiValue::with_capacity(v.len());
                let mut ed = EmbedderDataContext::new(&self.plc.ed);
                for ret in v {
                    let ret = ret.to_luau(lua, &self.plc, &self.bridge, &mut ed)
                    .map_err(|e| format!("Failed to convert return value to Lua: {}", e))?;
                    proxied.push_back(ret);
                }
                Ok(proxied)
            },
            Err(e) => Err(e.into()),
        }
    }
}

impl mluau::UserData for V8Value {
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_scheduler_async_method("requestdispose", async move |lua, this, ()| {
            let id = this.id;
            this.op_call(&lua, id, V8ObjectOp::RequestDispose, mluau::MultiValue::with_capacity(0)).await
            .map_err(|e| mluau::Error::external(format!("Failed to request dispose: {}", e)))?;
            Ok(())
        }); 

        methods.add_method("id", |_, this, ()| {
            Ok(this.id.objid())
        });

        methods.add_method("type", |_, this, ()| {
            Ok(v8_obj_registry_type_to_i32(this.typ))
        });

        // Underlying JS type name
        methods.add_method("typename", |_, this, ()| {
            Ok(this.type_name())
        });

        methods.add_scheduler_async_method("call", async move |lua, this, args: mluau::MultiValue| {
            let resp = this.op_call(&lua, this.id, V8ObjectOp::FunctionCall, args)
            .await
            .map_err(|e| mluau::Error::external(format!("Failed to perform function call: {}", e)))?;

            if resp.len() != 1 {
                return Err(mluau::Error::external(format!("Expected 1 return value from GetProperty, got {}", resp.len())));
            }

            let resp = resp.into_iter().next().unwrap();

            Ok(resp)
        });

        methods.add_scheduler_async_method("getproperty", async move |lua, this, key: mluau::Value| {        
            let resp = this.op_call_primitive(&lua, this.id, V8ObjectOp::ObjectGetProperty, key)
            .await
            .map_err(|e| mluau::Error::external(format!("Failed to get property: {}", e)))?;

            if resp.len() != 1 {
                return Err(mluau::Error::external(format!("Expected 1 return value from GetProperty, got {}", resp.len())));
            }

            let resp = resp.into_iter().next().unwrap();

            Ok(resp)
        });
    }
}
