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
use std::rc::Rc;
use std::cell::RefCell;

/// The core struct encapsulating a V8 object being proxied *to* luau
pub struct V8ObjectInner {
    pub id: V8ObjectRegistryID,
    pub typ: V8ObjectRegistryType,
    pub plc: ProxyLuaClient,
    pub bridge: V8IsolateManagerServer,
}

impl V8ObjectInner {
    fn new(id: V8ObjectRegistryID, typ: V8ObjectRegistryType, plc: ProxyLuaClient, bridge: V8IsolateManagerServer) -> Self {
        Self {
            id,
            typ,
            plc,
            bridge,
        }
    }

    async fn op_call(&self, obj_id: V8ObjectRegistryID, op: V8ObjectOp, args: Vec<ProxiedV8Value>) -> Result<Vec<ProxiedV8Value>, Error> {
        match self.bridge.send(super::bridge::OpCallMessage {
            obj_id,
            op,
            args,
        })
        .await? {
            Ok(v) => Ok(v),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Clone)]
pub struct V8Object {
    pub inner: Rc<RefCell<Option<V8ObjectInner>>>,
}

impl V8Object {
    fn new(id: V8ObjectRegistryID, typ: V8ObjectRegistryType, plc: ProxyLuaClient, bridge: V8IsolateManagerServer) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Some(V8ObjectInner::new(id, typ, plc, bridge)))),
        }
    }
}

impl mluau::UserData for V8Object {
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_, this, ()| {
            match this.inner.borrow().as_ref() {
                Some(inner) => Ok(inner.id.objid()),
                None => Err(mluau::Error::external("V8Object has already been dropped")),
            }
        });

        methods.add_method("typestr", |_, this, ()| {
            let typ = match this.inner.borrow().as_ref() {
                Some(inner) => inner.typ,
                None => return Err(mluau::Error::external("V8Object has already been dropped")),
            };
            let typ_str = match typ {
                V8ObjectRegistryType::ArrayBuffer => "ArrayBuffer",
                V8ObjectRegistryType::String => "String",
                V8ObjectRegistryType::Object => "Object",
                V8ObjectRegistryType::Array => "Array",
                V8ObjectRegistryType::Function => "Function",
                V8ObjectRegistryType::Promise => "Promise",
            };
            Ok(typ_str.to_string())
        });

        methods.add_method("type", |_, this, ()| {
            let typ = match this.inner.borrow().as_ref() {
                Some(inner) => inner.typ,
                None => return Err(mluau::Error::external("V8Object has already been dropped")),
            };
            Ok(v8_obj_registry_type_to_i32(typ))
        });

        methods.add_scheduler_async_method("requestdispose", async move |_, this, ()| {
            if let Some(v) = this.inner.borrow_mut().take() {
                let id = v.id;
                v.op_call(id, V8ObjectOp::RequestDispose, vec![]).await
                .map_err(|e| mluau::Error::external(format!("Failed to request dispose: {}", e)))?;
            }
            Ok(())
        }); 
    }
}


/*impl mluau::UserData for V8String {
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mluau::MetaMethod::Len, |_, this, ()| {
            Ok(this.len)
        });

        methods.add_method("object", |_, this, ()| {
            Ok(this.obj.clone())
        });
    }
}*/

macro_rules! impl_v8_obj {
    ($name:ident, $typ:expr, $ext_methods:expr) => {
        pub struct $name {
            pub obj: V8Object,
        }

        impl $name {
            pub fn new(id: V8ObjectRegistryID, plc: ProxyLuaClient, bridge: V8IsolateManagerServer) -> Self {
                Self {
                    obj: V8Object::new(id, $typ, plc, bridge),
                }
            }

            fn method_adder<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
                $ext_methods(methods);
            }
        }

        impl mluau::UserData for $name {
            fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
                methods.add_method("object", |_, this, ()| {
                    Ok(this.obj.clone())
                });

                Self::method_adder(methods);
            }
        }
    }
}

macro_rules! impl_v8_obj_with_len {
    ($name:ident, $typ:expr, $ext_methods:expr) => {
        pub struct $name {
            pub obj: V8Object,
            pub len: usize,
        }

        impl $name {
            pub fn new(id: V8ObjectRegistryID, plc: ProxyLuaClient, bridge: V8IsolateManagerServer, len: usize) -> Self {
                Self {
                    obj: V8Object::new(id, $typ, plc, bridge),
                    len,
                }
            }

            fn method_adder<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
                $ext_methods(methods);
            }
        }

        impl mluau::UserData for $name {
            fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
                methods.add_meta_method(mluau::MetaMethod::Len, |_, this, ()| {
                    Ok(this.len)
                });

                methods.add_method("object", |_, this, ()| {
                    Ok(this.obj.clone())
                });

                Self::method_adder(methods);
            }
        }
    }
}

impl_v8_obj_with_len!(V8String, V8ObjectRegistryType::String, |_m| {});

impl_v8_obj!(V8ArrayBuffer, V8ObjectRegistryType::ArrayBuffer, |_m| {});

impl_v8_obj!(V8ObjectObj, V8ObjectRegistryType::Object, __objmethods);
fn __objmethods<M: mluau::UserDataMethods<V8ObjectObj>>(methods: &mut M) {
    methods.add_scheduler_async_method("getproperties", async move |lua, this, _: ()| {
        let _g = this.obj.inner.try_borrow()
        .map_err(|e| mluau::Error::external(format!("Failed to borrow V8Object inner: {}", e)))?;
        let inner = match _g.as_ref() {
            Some(inner) => inner,
            None => return Err(mluau::Error::external("V8Object has already been dropped")),
        };
        let resp = inner.op_call(inner.id, V8ObjectOp::ObjectProperties, vec![])
        .await
        .map_err(|e| mluau::Error::external(format!("Failed to get properties: {}", e)))?;
        
        if resp.len() != 1 {
            return Err(mluau::Error::external(format!("Expected 1 return value from ObjectProperties, got {}", resp.len())));
        }

        let resp = resp.into_iter().next().unwrap();
        let resp = resp.proxy_to_src_lua(&lua, &inner.plc, &inner.bridge)
        .map_err(|e| mluau::Error::external(format!("Failed to convert properties to Lua: {}", e)))?;

        Ok(resp)
    });

    methods.add_scheduler_async_method("getproperty", async move |lua, this, key: mluau::Value| {
        let _g = this.obj.inner.try_borrow()
        .map_err(|e| mluau::Error::external(format!("Failed to borrow V8Object inner: {}", e)))?;
        let inner = match _g.as_ref() {
            Some(inner) => inner,
            None => return Err(mluau::Error::external("V8Object has already been dropped")),
        };
        let key = ProxiedV8Primitive::luau_to_primitive(&key)
        .map_err(|e| mluau::Error::external(format!("Failed to proxy key to ProxiedV8Value: {}", e)))?
        .ok_or(mluau::Error::external("Key is not a primitive value"))?;
        
        let resp = inner.op_call(inner.id, V8ObjectOp::ObjectGetProperty, vec![ProxiedV8Value::Primitive(key)])
        .await
        .map_err(|e| mluau::Error::external(format!("Failed to get property: {}", e)))?;

        if resp.len() != 1 {
            return Err(mluau::Error::external(format!("Expected 1 return value from GetProperty, got {}", resp.len())));
        }

        let resp = resp.into_iter().next().unwrap();
        let resp = resp.proxy_to_src_lua(&lua, &inner.plc, &inner.bridge)
        .map_err(|e| mluau::Error::external(format!("Failed to convert property to Lua: {}", e)))?;

        Ok(resp)
    });
}

//impl_v8_obj_stub!(V8ObjectObj, V8ObjectRegistryType::Object);
impl_v8_obj!(V8Array, V8ObjectRegistryType::Array, |_m| {});
impl_v8_obj!(V8Function, V8ObjectRegistryType::Function, |_m| {});
impl_v8_obj!(V8Promise, V8ObjectRegistryType::Promise, |_m| {});
