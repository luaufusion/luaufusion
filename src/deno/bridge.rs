use std::panic::AssertUnwindSafe;
use std::sync::Arc;

//use deno_core::error::{CoreError, CoreErrorKind};
use deno_core::v8;
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
//use deno_core::{op2, OpState};
//use deno_error::JsErrorBox;
//use mluau::serde::de;
use tokio_util::sync::CancellationToken;

use crate::luau::bridge::{LuaBridge, LuaBridgeObject, ObjectRegistryType, ProxiedLuaValue, ProxyLuaClient};

use crate::base::{ObjectRegistry, ObjectRegistryID, ProxyBridge};
use crate::deno::{CommonState, V8IsolateManagerInner};
use super::Error;

use crate::MAX_PROXY_DEPTH;

/// Minimum stack size for V8 isolates
pub const V8_MIN_STACK_SIZE: usize = 1024 * 1024 * 15; // 15MB minimum memory

fn obj_registry_type_to_i32(typ: ObjectRegistryType) -> i32 {
    match typ {
        ObjectRegistryType::Table => 1,
        ObjectRegistryType::Function => 2,
        ObjectRegistryType::Thread => 3,
        ObjectRegistryType::Buffer => 4,
        ObjectRegistryType::UserData => 5,
    }
}

fn i32_to_obj_registry_type(val: i32) -> Option<ObjectRegistryType> {
    match val {
        1 => Some(ObjectRegistryType::Table),
        2 => Some(ObjectRegistryType::Function),
        3 => Some(ObjectRegistryType::Thread),
        4 => Some(ObjectRegistryType::Buffer),
        5 => Some(ObjectRegistryType::UserData),
        _ => None,
    }
}

pub(crate) struct BridgeVals {
    type_field: v8::Global<v8::String>,
    id_field: v8::Global<v8::String>,
}

impl BridgeVals {
    pub(crate) fn new<'s>(scope: &mut v8::HandleScope<'s, ()>) -> Self {
        let id_field = v8::String::new(scope, "__luaid").unwrap();
        let type_field = v8::String::new(scope, "__luatype").unwrap();

        Self {
            id_field: v8::Global::new(scope, id_field),
            type_field: v8::Global::new(scope, type_field),
        }
    }
}

/// Marker struct for V8 objects in the object registry
#[derive(Clone, Copy)]
pub struct V8BridgeObject;

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum V8ObjRegistryType {
    Object,
    Function,
    Promise,
}

/// A V8 value that can now be easily proxied to Luau
#[derive(Serialize, Deserialize)]
pub enum ProxiedV8Value {
    Nil,
    Undefined,
    Boolean(bool),
    Integer(i32),
    Number(f64),
    String(String),
    Buffer(Vec<u8>), // Binary data
    Object(ObjectRegistryID<V8BridgeObject>), // Object ID in the map registry
    Array(ObjectRegistryID<V8BridgeObject>), // Array ID in the array registry
    Function(ObjectRegistryID<V8BridgeObject>), // Function ID in the function registry
    Promise(ObjectRegistryID<V8BridgeObject>), // Promise ID in the function registry

    // Source-owned stuff
    SrcFunction(ObjectRegistryID<LuaBridgeObject>), // Function ID in the source lua's function registry
    SrcTable(ObjectRegistryID<LuaBridgeObject>), // Table ID in the source lua's table registry
    SrcThread(ObjectRegistryID<LuaBridgeObject>), // Thread ID in the source lua's thread registry
    SrcBuffer(ObjectRegistryID<LuaBridgeObject>), // Buffer ID in the source lua's buffer registry
    SrcUserData(ObjectRegistryID<LuaBridgeObject>), // Userdata ID in the source lua's userdata registry
}

impl ProxiedV8Value {
    pub(crate) fn proxy_to_src_lua(self, lua: &mluau::Lua, _bridge: &V8IsolateManager, plc: &ProxyLuaClient, depth: usize) -> Result<mluau::Value, mluau::Error> {
        if depth > MAX_PROXY_DEPTH {
            return Err(mluau::Error::external("Maximum proxy depth exceeded"));
        }
        
        match self {
            ProxiedV8Value::Nil | ProxiedV8Value::Undefined => Ok(mluau::Value::Nil),
            ProxiedV8Value::Boolean(b) => Ok(mluau::Value::Boolean(b)),
            ProxiedV8Value::Integer(i) => Ok(mluau::Value::Integer(i as i64)),
            ProxiedV8Value::Number(n) => Ok(mluau::Value::Number(n)),
            ProxiedV8Value::String(sid) => {
                lua.create_string(sid.as_bytes()).map(mluau::Value::String)
            }
            ProxiedV8Value::Buffer(buf) => {
                lua.create_buffer(buf).map(mluau::Value::Buffer)
            },
            /*ProxiedV8Value::Array(elems) => {
                
                Ok(mluau::Value::Table(tbl))
            },*/
            /*ProxiedV8Value::Object(obj_id) => {
                struct V8ProxiedObject {
                    obj_id: i32,
                    bridge: V8IsolateManager,
                }

                impl Drop for V8ProxiedObject {
                    fn drop(&mut self) {
                        //self.bridge.(self.obj_id);
                    }
                }
            }*/
            ProxiedV8Value::SrcFunction(func_id) => {
                let func = plc.func_registry.get(func_id)
                    .ok_or_else(|| mluau::Error::external(format!("Function ID {} not found in registry", func_id)))?;
                Ok(mluau::Value::Function(func))
            }
            ProxiedV8Value::SrcTable(table_id) => {
                let table = plc.table_registry.get(table_id)
                    .ok_or_else(|| mluau::Error::external(format!("Table ID {} not found in registry", table_id)))?;
                Ok(mluau::Value::Table(table))
            }
            ProxiedV8Value::SrcThread(thread_id) => {
                let thread = plc.thread_registry.get(thread_id)
                    .ok_or_else(|| mluau::Error::external(format!("Thread ID {} not found in registry", thread_id)))?;
                Ok(mluau::Value::Thread(thread))
            }
            ProxiedV8Value::SrcBuffer(buf_id) => {
                let buffer = plc.buffer_registry.get(buf_id)
                    .ok_or_else(|| mluau::Error::external(format!("Buffer ID {} not found in registry", buf_id)))?;
                Ok(mluau::Value::Buffer(buffer))
            }
            ProxiedV8Value::SrcUserData(ud_id) => {
                let userdata = plc.userdata_registry.get(ud_id)
                    .ok_or_else(|| mluau::Error::external(format!("Userdata ID {} not found in registry", ud_id)))?;
                Ok(mluau::Value::UserData(userdata))
            }
            _ => Err(mluau::Error::external("Unsupported V8 value type for proxying to Lua")),
        }
    }

    pub(crate) fn proxy_from_v8_get_proxied<'s>(
        scope: &mut v8::HandleScope<'s>,
        common_state: &CommonState,
        obj: v8::Local<'s, v8::Object>,
    ) -> Result<Option<Self>, Error> {
        let typ_key = v8::Local::new(scope, &common_state.bridge_vals.type_field);
        let typ_val = obj.get(scope, typ_key.into());
        if let Some(typ_val) = typ_val {
            if typ_val.is_int32() {
                let typ_i32 = typ_val.to_int32(scope).ok_or("Failed to convert lua type to int32")?.value();
                if let Some(typ) = i32_to_obj_registry_type(typ_i32) {
                    match typ {
                        ObjectRegistryType::Function => {
                            // Look for __luaid
                            let p_obj_key = v8::Local::new(scope, &common_state.bridge_vals.id_field);
                            let p_obj_val = obj.get(scope, p_obj_key.into());
                            if let Some(p_obj_val) = p_obj_val {
                                if p_obj_val.is_big_int() {
                                    let func_id = p_obj_val.to_big_int(scope).ok_or("Failed to convert lua function id to int32")?.i64_value().0;
                                    return Ok(Some(Self::SrcFunction(ObjectRegistryID::from_i64(func_id))));
                                }
                            }
                        }
                        ObjectRegistryType::Table => {
                            // Look for __luaid
                            let p_obj_key = v8::Local::new(scope, &common_state.bridge_vals.id_field);
                            let p_obj_val = obj.get(scope, p_obj_key.into());
                            if let Some(p_obj_val) = p_obj_val {
                                if p_obj_val.is_big_int() {
                                    let func_id = p_obj_val.to_big_int(scope).ok_or("Failed to convert lua table id to int32")?.i64_value().0;
                                    return Ok(Some(Self::SrcTable(ObjectRegistryID::from_i64(func_id))));
                                }
                            }
                        }
                        ObjectRegistryType::Thread => {
                            // Look for __luaid
                            let p_obj_key = v8::Local::new(scope, &common_state.bridge_vals.id_field);
                            let p_obj_val = obj.get(scope, p_obj_key.into());
                            if let Some(p_obj_val) = p_obj_val {
                                if p_obj_val.is_big_int() {
                                    let func_id = p_obj_val.to_big_int(scope).ok_or("Failed to convert lua thread id to int32")?.i64_value().0;
                                    return Ok(Some(Self::SrcThread(ObjectRegistryID::from_i64(func_id))));
                                }
                            }
                        }
                        ObjectRegistryType::Buffer => {
                            // Look for __luaid
                            let p_obj_key = v8::Local::new(scope, &common_state.bridge_vals.id_field);
                            let p_obj_val = obj.get(scope, p_obj_key.into());
                            if let Some(p_obj_val) = p_obj_val {
                                if p_obj_val.is_big_int() {
                                    let func_id = p_obj_val.to_big_int(scope).ok_or("Failed to convert lua buffer id to int32")?.i64_value().0;
                                    return Ok(Some(Self::SrcBuffer(ObjectRegistryID::from_i64(func_id))));
                                }
                            }
                        }
                        ObjectRegistryType::UserData => {
                            // Look for __luaid
                            let p_obj_key = v8::Local::new(scope, &common_state.bridge_vals.id_field);
                            let p_obj_val = obj.get(scope, p_obj_key.into());
                            if let Some(p_obj_val) = p_obj_val {
                                if p_obj_val.is_big_int() {
                                    let func_id = p_obj_val.to_big_int(scope).ok_or("Failed to convert lua userdata id to int32")?.i64_value().0;
                                    return Ok(Some(Self::SrcUserData(ObjectRegistryID::from_i64(func_id))));
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    pub(crate) fn proxy_from_v8<'s>(
        scope: &mut v8::HandleScope<'s>, 
        value: v8::Local<'s, v8::Value>,
        common_state: &CommonState,
        depth: usize,
    ) -> Result<Self, Error> {
        if depth > MAX_PROXY_DEPTH {
            return Err("Maximum proxy depth exceeded".into());
        }

        if value.is_null() {
            return Ok(Self::Nil);
        } else if value.is_undefined() {
            return Ok(Self::Undefined)
        } else if value.is_boolean() {
            let b = value.to_boolean(scope).is_true();
            return Ok(Self::Boolean(b));
        } else if value.is_int32() {
            let i = value.to_int32(scope).ok_or("Failed to convert to int32")?.value();
            return Ok(Self::Integer(i));
        } else if value.is_number() {
            let n = value.to_number(scope).ok_or("Failed to convert to number")?.value();
            return Ok(Self::Number(n));
        } else if value.is_string() {
            let s = value.to_string(scope).ok_or("Failed to convert to string")?;
            let sid = s.to_rust_string_lossy(scope);
            return Ok(Self::String(sid));
        } else if value.is_array() {
            let arr = v8::Local::<v8::Array>::try_from(value).map_err(|_| "Failed to convert to array")?;
            let global_obj = v8::Global::new(scope, arr);
            let obj_id = common_state.proxy_client.array_registry.add(global_obj)
                .ok_or("Failed to register array: too many array references")?;
            return Ok(Self::Array(obj_id));
        } else if value.is_array_buffer() {
            let ab = v8::Local::<v8::ArrayBuffer>::try_from(value).map_err(|_| "Failed to convert to ArrayBuffer")?;
            let bs = ab.get_backing_store();
            let Some(data) = bs.data() else {
                return Ok(Self::Buffer(Vec::with_capacity(0)));
            };
            let slice = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, bs.byte_length()) };
            return Ok(Self::Buffer(slice.to_vec()));
        }
        
        if value.is_function() {
            let func = v8::Local::<v8::Function>::try_from(value).map_err(|_| "Failed to convert to function")?;

            if let Some(v) = Self::proxy_from_v8_get_proxied(scope, &common_state, func.into())? {
                return Ok(v);
            }

            let global_func = v8::Global::new(scope, func);
            let func_id = common_state.proxy_client.func_registry.add(global_func)
                .ok_or("Failed to register function: too many function references")?;
            return Ok(Self::Function(func_id));
        } else if value.is_promise() {
            let promise = v8::Local::<v8::Promise>::try_from(value).map_err(|_| "Failed to convert to promise")?;
            let global_promise = v8::Global::new(scope, promise);
            let promise_id = common_state.proxy_client.promise_registry.add(global_promise)
                .ok_or("Failed to register promise: too many promise references")?;
            return Ok(Self::Promise(promise_id));
        } else if value.is_object() {
            let obj = value.to_object(scope).ok_or("Failed to convert to object")?;

            // Look for __objId (luau object), __threadId (luau thread), __bufId (luau buffer), __udId (luau userdata)
            // __funcId is handled in function case above
            // Look for __objId
            if let Some(v) = Self::proxy_from_v8_get_proxied(scope, &common_state, obj)? {
                return Ok(v);
            }

            let global_obj = v8::Global::new(scope, obj);
            let obj_id = common_state.proxy_client.obj_registry.add(global_obj)
                .ok_or("Failed to register object: too many object references")?;
            return Ok(Self::Object(obj_id));
        } else {
            return Err("Unsupported V8 value type".into());
        }
    }
}

impl V8IsolateManagerInner {
    /// Proxy a ProxiedLuaValue to a V8 value
    pub(crate) fn proxy_to_v8_impl(&mut self, value: ProxiedLuaValue) -> Result<v8::Global<v8::Value>, Error> {
        let v8_ctx = self.deno.main_context();
        let isolate = self.deno.v8_isolate();

        let v8_value = {
            let mut scope = v8::HandleScope::new(isolate);
            let v8_ctx = v8::Local::new(&mut scope, v8_ctx);
            let scope = &mut v8::ContextScope::new(&mut scope, v8_ctx);
            match Self::proxy_to_v8(scope, &self.common_state, value, 0) {
                Ok(v) => Ok(v8::Global::new(scope, v)),
                Err(e) => Err(e),
            }
        };

        println!("Proxied Lua value to V8");
        
        v8_value
    }

    fn proxy_objreg_from_lua<'s>(
        scope: &mut v8::HandleScope<'s>,
        typ: ObjectRegistryType,
        id: ObjectRegistryID<LuaBridgeObject>,
        common_state: &CommonState
    ) -> Result<v8::Local<'s, v8::Value>, Error> {
        let code = match typ {
            ObjectRegistryType::Table => include_str!("_bridge_table.js"),
            ObjectRegistryType::Function => include_str!("_bridge_function.js"),
            ObjectRegistryType::Thread => include_str!("_bridge_thread.js"),
            ObjectRegistryType::Buffer => include_str!("_bridge_buffer.js"),
            ObjectRegistryType::UserData => include_str!("_bridge_ud.js"),
        }; 

        let oid_key = v8::Local::new(scope, &common_state.bridge_vals.id_field);
        let otype_key = v8::Local::new(scope, &common_state.bridge_vals.type_field);

        let obj_template = common_state.obj_template.clone();
        
        let local_template = v8::Local::new(scope, (*obj_template).clone());
        
        let bridge_ref = common_state.bridge.clone();
        let external = common_state.finalizer_attachments.func_ids.add(scope, id, Some(Box::new(move || {
            bridge_ref.request_drop_object(typ, id);
        })))
            .ok_or("Maximum number of proxied references reached")?;

        let obj = local_template.new_instance(scope).ok_or("Failed to create V8 proxy object")?;
        obj.set_internal_field(0, external.into());

        let id_val = v8::BigInt::new_from_i64(scope, id.get());
        obj.set(scope, oid_key.into(), id_val.into());
        let type_val = v8::Integer::new(scope, obj_registry_type_to_i32(typ));
        obj.set(scope, otype_key.into(), type_val.into());
        
        let try_catch = &mut v8::TryCatch::new(scope);

        let source = v8::String::new(try_catch, code).unwrap();
        let script = match v8::Script::compile(try_catch, source, None) {
            Some(s) => s,
            None => {
                if try_catch.has_caught() {
                    let exception = try_catch.exception().unwrap();
                    let exception_string = exception.to_rust_string_lossy(try_catch);
                    return Err(format!("Failed to compile proxy script: {}", exception_string).into());
                }
                return Err("Failed to compile proxy script".into())
            },
        };
        let result = script.run(try_catch).unwrap();
        let creator_fn: v8::Local<v8::Function> = result.try_into().unwrap();
        let global = try_catch.get_current_context().global(try_catch);
        let result = match creator_fn.call(try_catch, global.into(), &[obj.into()]) {
            Some(r) => r,
            None => {
                if try_catch.has_caught() {
                    let exception = try_catch.exception().unwrap();
                    let exception_string = exception.to_rust_string_lossy(try_catch);
                    return Err(format!("Failed to run proxy script: {}", exception_string).into());
                }
                return Err("Failed to run proxy script".into())
            },
        };
        Ok(result)
    }

    // Internal implementation to convert a ProxiedLuaValue to a V8 value
    pub(crate) fn proxy_to_v8<'s>(
        scope: &mut v8::HandleScope<'s>, 
        common_state: &CommonState,
        value: ProxiedLuaValue,
        depth: usize,
    ) -> Result<v8::Local<'s, v8::Value>, Error> {
        if depth > MAX_PROXY_DEPTH {
            return Err("Maximum proxy depth exceeded".into());
        }

        let v8_value: v8::Local<v8::Value> = match value {
            ProxiedLuaValue::Nil => v8::null(scope).into(),
            ProxiedLuaValue::Boolean(b) => v8::Boolean::new(scope, b).into(),
            ProxiedLuaValue::Integer(i) => {
                if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                    v8::Integer::new(scope, i as i32).into()
                } else {
                    v8::Number::new(scope, i as f64).into()
                }
            },
            ProxiedLuaValue::Number(n) => v8::Number::new(scope, n).into(),
            ProxiedLuaValue::Vector((x,y,z)) => {
                let arr = v8::Array::new(scope, 3);
                let x = v8::Number::new(scope, x as f64);
                let y = v8::Number::new(scope, y as f64);
                let z = v8::Number::new(scope, z as f64);
                arr.set_index(scope, 0, x.into());
                arr.set_index(scope, 1, y.into());
                arr.set_index(scope, 2, z.into());
                arr.into()
            },
            ProxiedLuaValue::String(atom) => {
                v8::String::new(scope, &atom).ok_or("Failed to create V8 string")?.into()
            }
            ProxiedLuaValue::Table(table_id) => {
                Self::proxy_objreg_from_lua(scope, ObjectRegistryType::Table, table_id, common_state)?
            }
            ProxiedLuaValue::Function(func_id) => {
                Self::proxy_objreg_from_lua(scope, ObjectRegistryType::Function, func_id, common_state)?
            }
            ProxiedLuaValue::Thread(thread_id) => {
                Self::proxy_objreg_from_lua(scope, ObjectRegistryType::Thread, thread_id, common_state)?
            }
            ProxiedLuaValue::UserData(ud_id) => {
                Self::proxy_objreg_from_lua(scope, ObjectRegistryType::UserData, ud_id, common_state)?
            }
            ProxiedLuaValue::Buffer(buf_id) => {
                Self::proxy_objreg_from_lua(scope, ObjectRegistryType::Buffer, buf_id, common_state)?
            }
        };

        Ok(v8_value)
    }
}

#[derive(Clone)]
/// The client side state for proxying Lua values
/// 
/// This struct is not thread safe and must be kept on the Lua side
pub struct ProxyV8Client {
    pub array_registry: ObjectRegistry<v8::Global<v8::Array>, V8BridgeObject>,
    pub obj_registry: ObjectRegistry<v8::Global<v8::Object>, V8BridgeObject>,
    pub func_registry: ObjectRegistry<v8::Global<v8::Function>, V8BridgeObject>,
    pub promise_registry: ObjectRegistry<v8::Global<v8::Promise>, V8BridgeObject>,
}


pub enum V8IsolateManagerMessage {
    Shutdown,
    ErrSubscribe {
        resp: tokio::sync::oneshot::Sender<tokio::sync::watch::Receiver<Option<deno_core::error::CoreError>>>,
    },
    CodeExec {
        code: String,
        args: Vec<ProxiedLuaValue>,
        resp: tokio::sync::oneshot::Sender<Result<ProxiedV8Value, Error>>,
    },
    GetObjectProperty {
        obj_id: ObjectRegistryID<V8BridgeObject>,
        key: ProxiedLuaValue,
        resp: tokio::sync::oneshot::Sender<Result<ProxiedV8Value, Error>>,
    },
}

/// Internal manager for a single V8 isolate with a minimal Deno runtime.
#[derive(Clone)]
pub enum V8IsolateManager {
    Threaded {
        tx: tokio::sync::mpsc::UnboundedSender<V8IsolateManagerMessage>,
        threadsafe_handle: deno_core::v8::IsolateHandle,
        thread_handle: Arc<std::thread::JoinHandle<Result<(), Box<dyn std::any::Any + Send + 'static>>>>
    },
    Local {
        tx: tokio::sync::mpsc::UnboundedSender<V8IsolateManagerMessage>,
        threadsafe_handle: deno_core::v8::IsolateHandle,
    },
    Process {
        cmd: Vec<String>,
        tx: tokio::sync::mpsc::UnboundedSender<V8IsolateManagerMessage>,
    }
}

impl Drop for V8IsolateManager {
    fn drop(&mut self) {
        let _ = match self.terminate() {
            Ok(_) => {},
            Err(e) => {
                eprintln!("Failed to send shutdown message to V8 isolate manager: {}", e);
            }
        }; 
    }
}

pub struct V8IsolateManagerThreadData {
    pub stack_size: usize,
    pub heap_limit: usize,
    pub id: String,
}

impl V8IsolateManager {
    /// Create a new isolate manager and spawn its task
    /// 
    /// Safety note: it is unsafe to create multiple isolates on the same thread
    /// As such, V8IsolateManager will create its own thread
    pub async fn new(
        thread_opts: V8IsolateManagerThreadData,
        cancellation_token: CancellationToken,
        bridge: LuaBridge<V8IsolateManager>,
    ) -> Result<Self, Error> {
        let thread_stack_size = thread_opts.stack_size.max(V8_MIN_STACK_SIZE);
        let heap_limit = thread_opts.heap_limit;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        
        let (th_tx, th_rx) = tokio::sync::oneshot::channel();
        let tjh = std::thread::Builder::new()
        .stack_size(thread_stack_size)
        .name(thread_opts.id)
        .spawn(move || {
            let ct_ref = cancellation_token.clone();
            let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build_local(tokio::runtime::LocalOptions::default())
                    .expect("Failed to create tokio runtime");

                rt.block_on(async move {
                    let mut inner = V8IsolateManagerInner::new(bridge, heap_limit);
                    let _ = th_tx.send(inner.thread_safe_handle());
                    Self::run(inner, rx, cancellation_token).await;
                });
            }));

            ct_ref.cancel();
            res 
        })?;
        
        let handle = th_rx.await?;

        Ok(Self::Threaded {
            tx,
            threadsafe_handle: handle,
            thread_handle: Arc::new(tjh)
        })
    }

    /// Create a new isolate manager and spawn its task
    /// 
    /// Safety note: it is unsafe to create multiple isolates on the same thread
    pub fn new_local(
        mut inner: V8IsolateManagerInner,
        cancellation_token: CancellationToken,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                
        let handle = inner.thread_safe_handle();

        tokio::task::spawn_local(async move {
            let _ = Self::run(inner, rx, cancellation_token).await;
        });

        Self::Local {
            tx,
            threadsafe_handle: handle,
        }
    }

    /// Run the isolate manager task
    /// 
    /// This must be called for anything to work
    /// 
    /// Returns the inner isolate manager for any pool cleanup etc. after shutdown
    async fn run(
        mut inner: V8IsolateManagerInner, 
        mut rx: tokio::sync::mpsc::UnboundedReceiver<V8IsolateManagerMessage>, 
        shutdown_token: CancellationToken,
    ) {
        let (err_tx, err_rx) = tokio::sync::watch::channel(None);

        // Async futures unordered queue for code execution
        let mut code_exec_queue = FuturesUnordered::new();

        loop {
            tokio::select! {
                Some(msg) = rx.recv() => {
                    // Handle message
                    match msg {
                        V8IsolateManagerMessage::Shutdown => {
                            // Terminate isolate
                            break;
                        }
                        V8IsolateManagerMessage::ErrSubscribe { resp } => {
                            let _ = resp.send(err_rx.clone());
                        }
                        V8IsolateManagerMessage::CodeExec { code, args, resp } => {
                            let func = {
                                let main_ctx = inner.deno.main_context();
                                let isolate = inner.deno.v8_isolate();
                                let mut scope = v8::HandleScope::new(isolate);
                                let main_ctx = v8::Global::new(&mut scope, main_ctx);
                                let main_ctx = v8::Local::new(&mut scope, main_ctx);
                                let context_scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
                                let try_catch = &mut v8::TryCatch::new(context_scope);
                                let script = v8::String::new(try_catch, &code)
                                    .and_then(|s| v8::Script::compile(try_catch, s, None))
                                    .ok_or_else(|| {
                                        if try_catch.has_caught() {
                                            let exception = try_catch.exception().unwrap();
                                            let exception_string = exception.to_rust_string_lossy(try_catch);
                                            format!("Failed to compile script: {}", exception_string)
                                        } else {
                                            "Failed to compile script".to_string()
                                        }
                                    });

                                let script = match script {
                                    Ok(s) => s,
                                    Err(e) => {
                                        let _ = resp.send(Err(e.into()));
                                        continue;
                                    }
                                };

                                // Convert and proxy
                                let local_func = match script.run(try_catch) {
                                    Some(result) => {
                                        if result.is_function() {
                                            let v = v8::Local::<v8::Function>::try_from(result);
                                            match v {
                                                Ok(f) => f,
                                                Err(_) => {
                                                    let _ = resp.send(Err("Script did not return a function".to_string().into()));
                                                    continue;
                                                },
                                            }
                                        } else {
                                            let _ = resp.send(Err("Script did not return a function".to_string().into()));
                                            continue;
                                        }
                                    },
                                    None => {
                                        if try_catch.has_caught() {
                                            let exception = try_catch.exception().unwrap();
                                            let exception_string = exception.to_rust_string_lossy(try_catch);
                                            let _ = resp.send(Err(format!("Failed to run script: {}", exception_string).into()));
                                            continue;
                                        } else {
                                            let _ = resp.send(Err("Failed to run script".to_string().into()));
                                            continue;
                                        }
                                    }
                                };

                                v8::Global::new(try_catch, local_func)
                            };

                            // Call the function with args
                            // using denos' call_with_args
                            let args = args.into_iter().map(|v| inner.proxy_to_v8_impl(v)).collect::<Result<Vec<_>, _>>();
                            let args = match args {
                                Ok(a) => a,
                                Err(e) => {
                                    let _ = resp.send(Err(e));
                                    continue;
                                }
                            };
                            let fut = inner.deno.call_with_args(&func, &args);
                            code_exec_queue.push(async {
                                let result = fut.await;
                                (result, resp)
                            });
                        },
                        V8IsolateManagerMessage::GetObjectProperty { obj_id, key, resp } => {
                            let obj = match inner.common_state.proxy_client.obj_registry.get(obj_id) {
                                Some(v) => v,
                                None => {
                                    let _ = resp.send(Err("Object ID not found".into()));
                                    continue;
                                }
                            };

                            let key = match inner.proxy_to_v8_impl(key) {
                                Ok(k) => k,
                                Err(e) => {
                                    let _ = resp.send(Err(e));
                                    continue;
                                }
                            };

                            {
                                let main_ctx = inner.deno.main_context();
                                let isolate = inner.deno.v8_isolate();
                                let mut scope = v8::HandleScope::new(isolate);
                                let main_ctx = v8::Local::new(&mut scope, main_ctx);
                                let context_scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
                                let try_catch = &mut v8::TryCatch::new(context_scope);

                                let local_obj = v8::Local::new(try_catch, obj);
                                let local_key = v8::Local::new(try_catch, key);

                                let result = local_obj.get(try_catch, local_key);
                                let result = match result {
                                    Some(v) => v,
                                    None => {
                                        if try_catch.has_caught() {
                                            let exception = try_catch.exception().unwrap();
                                            let exception_string = exception.to_rust_string_lossy(try_catch);
                                            let _ = resp.send(Err(format!("Failed to get object property: {}", exception_string).into()));
                                            continue;
                                        } else {
                                            let _ = resp.send(Err("Failed to get object property".into()));
                                            continue;
                                        }
                                    }
                                };

                                // Convert to ProxiedV8Value
                                match ProxiedV8Value::proxy_from_v8(try_catch, result, &inner.common_state, 0) {
                                    Ok(v) => {
                                        let _ = resp.send(Ok(v));
                                    }
                                    Err(e) => {
                                        if try_catch.has_caught() {
                                            let exception = try_catch.exception().unwrap();
                                            let exception_string = exception.to_rust_string_lossy(try_catch);
                                            let _ = resp.send(Err(format!("Failed to convert object property to ProxiedV8Value: {} ({})", exception_string, e).into()));
                                            continue;
                                        }
                                        let _ = resp.send(Err(e.into()));
                                    }
                                };
                            }
                        }
                    }
                },
                _ = inner.cancellation_token.cancelled() => {
                    // Terminate isolate
                    break;
                }
                Some((result, resp)) = code_exec_queue.next() => {
                    // A code execution future has completed
                    let result = match result {
                        Ok(v) => {
                            // Convert v8::Global<v8::Value> to ProxiedV8Value
                            let main_ctx = inner.deno.main_context();
                            let isolate = inner.deno.v8_isolate();
                            let mut scope = v8::HandleScope::new(isolate);
                            let main_ctx = v8::Local::new(&mut scope, main_ctx);
                            let mut scope = &mut v8::ContextScope::new(&mut scope, main_ctx);
                            let v = v8::Local::new(&mut scope, v);
                            match ProxiedV8Value::proxy_from_v8(
                                &mut scope, 
                                v,
                                &inner.common_state, 
                                0
                            ) {
                                Ok(pv) => Ok(pv),
                                Err(e) => Err(e),
                            }
                        }
                        Err(e) => Err(format!("JavaScript execution error: {}", e).into()),
                    };
                    let _ = resp.send(result.map_err(|e| e.into()));
                }
                res = inner.deno.run_event_loop(deno_core::PollEventLoopOptions::default()), if !code_exec_queue.is_empty() => {
                    // Continue running
                    match res {
                        Ok(_) => {},
                        Err(e) => {
                            err_tx.send_replace(Some(e));
                        }
                    };

                    tokio::task::yield_now().await; // Yield to allow other tasks to run
                }
            }
        }

        if !inner.thread_safe_handle().is_execution_terminating() {
            inner.thread_safe_handle().terminate_execution();
        }

        shutdown_token.cancel();
    }

    /// Sends a message to the isolate manager to subscribe to error events
    pub(crate) fn send(&self, msg: V8IsolateManagerMessage) -> Result<(), Error> {
        match self {
            V8IsolateManager::Threaded { tx, .. } => {
                tx.send(msg).map_err(|_| "Failed to send message to isolate manager".to_string().into())
            }
            V8IsolateManager::Local { tx, .. } => {
                tx.send(msg).map_err(|_| "Failed to send message to isolate manager".to_string().into())
            }
            V8IsolateManager::Process { tx, .. } => {
                tx.send(msg).map_err(|_| "Failed to send message to isolate process manager".to_string().into())
            }
        }
    }

    /// Returns a watch receiver for errors from the isolates event loop
    pub async fn err_subscribe(&self) -> Result<tokio::sync::watch::Receiver<Option<deno_core::error::CoreError>>, Error> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let msg = V8IsolateManagerMessage::ErrSubscribe { resp: tx };
        self.send(msg)?;
        let rx = rx.await.map_err(|_| "Failed to receive ErrSubscribe response".to_string())?;
        Ok(rx)
    }

    /// Evaluates code in the isolate and returns the result as a ProxiedV8Value
    pub async fn eval(&self, code: &str, args: Vec<ProxiedLuaValue>) -> Result<ProxiedV8Value, Error> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let msg = V8IsolateManagerMessage::CodeExec { code: code.to_string(), args, resp: tx };
        self.send(msg)?;
        let value = rx.await.map_err(|_| "Failed to receive CodeExec response".to_string())??;
        Ok(value)   
    }

    pub fn terminate(&self) -> Result<(), Error> {
        self.send(V8IsolateManagerMessage::Shutdown)
    }
}

impl ProxyBridge for V8IsolateManager {
    type ValueType = ProxiedV8Value;

    fn to_source_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, plc: &ProxyLuaClient, depth: usize) -> Result<mluau::Value, Error> {
        Ok(value.proxy_to_src_lua(lua, self, plc, depth).map_err(|e| e.to_string())?)
    }

    async fn eval_from_source(&self, code: &str, args: Vec<ProxiedLuaValue>) -> Result<Self::ValueType, crate::base::Error> {
        self.eval(code, args).await
    }
}