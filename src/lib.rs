use futures_util::stream::FuturesUnordered;
use mlua_scheduler::taskmgr::get;
use mlua_scheduler::LuaSchedulerAsync;
use mlua_scheduler::LuaSchedulerAsyncUserData;

use rquickjs::class::Trace;
use tokio::sync::mpsc::{UnboundedSender as MSender, UnboundedReceiver as MReceiver, unbounded_channel};
use tokio::sync::oneshot::{Sender as OneShotSender};
use mluau::WeakLua;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use rquickjs::{AsyncRuntime as RQRuntime, AsyncContext as RQContext, Ctx as RQCtx, IntoJs, JsLifetime, Persistent, Value};
use futures_util::StreamExt;
use futures_util::FutureExt;
use futures_util::TryFutureExt;

enum RuntimeEvent {
    Shutdown,
    AddAsync {
        thread: mluau::Thread,
        args: mluau::MultiValue,
        result: MSender<Result<mluau::MultiValue, mluau::Error>>,
    },
    RunQuickjsProxy {
        func_idx: PersistentStoreHandler<rquickjs::Function<'static>>,
        args: mluau::MultiValue,
        result: MSender<Result<mluau::MultiValue, mluau::Error>>,
    },
    RunQuickjsPromiseProxy {
        promise_idx: PersistentStoreHandler<rquickjs::Promise<'static>>,
        result: MSender<Result<mluau::MultiValue, mluau::Error>>,
    },
}

#[derive(Clone)]
struct PersistentStore<T: Clone> {
    store: Rc<RefCell<Vec<Option<Persistent<T>>>>>,
    free: Rc<RefCell<VecDeque<usize>>>,
}

/// Whenever a PersistentStore value gets fully dropped by dropping this handle, the value will be removed from the store.
struct PersistentStoreHandler<T: Clone> {
    store: PersistentStore<T>,
    index: usize,
    drop: bool
}

impl<T: Clone> Clone for PersistentStoreHandler<T> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            index: self.index,
            drop: false
        }
    }
}

impl<T: Clone> Drop for PersistentStoreHandler<T> {
    fn drop(&mut self) {
        if self.drop {
            self.store.remove(self.index);
        }
    }
}

impl<T: Clone> PersistentStore<T> {
    pub fn new() -> Self {
        Self {
            store: Rc::default(),
            free: Rc::default(),
        }
    }

    pub fn add(&self, value: Persistent<T>) -> PersistentStoreHandler<T>
    {
        {
            let mut free = self.free.borrow_mut();
            if let Some(index) = free.pop_front() {
                let mut store = self.store.borrow_mut();
                store[index] = Some(value);
                return PersistentStoreHandler {
                    store: self.clone(),
                    index,
                    drop: true
                };
            }
        }

        let mut store = self.store.borrow_mut();
        store.push(Some(value));

        PersistentStoreHandler {
            store: self.clone(),
            index: store.len() - 1,
            drop: true
        }
    }

    pub fn get(&self, handle: PersistentStoreHandler<T>) -> Option<Persistent<T>>
    {
        let store = self.store.borrow();
        if handle.index >= store.len() {
            return None;
        }
        let persistent = &store[handle.index];
        persistent.clone()
    }

    fn remove(&self, index: usize) {
        let mut store = self.store.borrow_mut();
        if index >= store.len() {
            return;
        }
        store[index] = None;

        let mut free = self.free.borrow_mut();
        free.push_back(index);
    }

    pub fn clear(&self) {
        let mut store = self.store.borrow_mut();
        store.clear();
        let mut free = self.free.borrow_mut();
        free.clear();
    }
}

pub struct QuickjsRuntimeInner {
    lua: WeakLua,
    tx: MSender<RuntimeEvent>,
    is_running: Rc<Cell<bool>>,
    pub(crate) proxy_funcs: PersistentStore<rquickjs::Function<'static>>,
    pub(crate) proxy_promises: PersistentStore<rquickjs::Promise<'static>>,
}

#[derive(Clone)]
pub struct QuickjsRuntime {
    inner: Rc<QuickjsRuntimeInner>,
}

impl std::ops::Deref for QuickjsRuntime {
    type Target = QuickjsRuntimeInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;

impl QuickjsRuntime {
    pub async fn new(lua: WeakLua) -> Result<Self, Error> {
        let (tx, rx) = unbounded_channel();

        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let runtime = Self { 
            inner: Rc::new(QuickjsRuntimeInner {
                lua: lua.clone(), 
                tx: tx.clone(), 
                is_running: Rc::new(Cell::new(false)), 
                proxy_funcs: PersistentStore::new(),
                proxy_promises: PersistentStore::new(),
            })
        };

        tokio::task::spawn_local({
            let runtime = runtime.clone();
            async move {
                runtime.run(start_tx, rx).await;
            }
        });

        // Wait for the runtime to signal that it has started
        start_rx.await?;

        Ok(runtime)
    }

    async fn run(&self, start_tx: OneShotSender<()>, mut rx: MReceiver<RuntimeEvent>) {
        if self.is_running.get() {
            panic!("QuickJS proxy runtime is already running");
        }

        let runtime = RQRuntime::new().expect("Failed to create QuickJS runtime");
        let context = RQContext::full(&runtime).await.expect("Failed to create QuickJS context");

        self.is_running.set(true);
        let _ = start_tx.send(());

        let mut async_queue = FuturesUnordered::new();

        let self_ref = self.clone();
        context.async_with(
            |ctx| Box::pin(async move {
                loop {
                    tokio::select! {
                        Some(event) = rx.recv() => {
                            match event {
                                RuntimeEvent::Shutdown => break,
                                RuntimeEvent::AddAsync { thread, args, result } => {
                                    let task_mgr = {
                                        let Some(lua) = self_ref.lua.try_upgrade() else {
                                            let _ = result.send(Err(mluau::Error::external("Lua state has been dropped")));
                                            continue;
                                        };

                                        get(&lua)
                                    };

                                    let thread_ref = thread.clone();
                                    let result_ref = result.clone();
                                    async_queue.push(
                                        std::panic::AssertUnwindSafe(
                                            async move {
                                                let res = match task_mgr.spawn_thread_and_wait(thread_ref, args).await {
                                                    Ok(v) => v,
                                                    Err(e) => {
                                                        let _ = result.send(Err(e));
                                                        return;
                                                    },
                                                };

                                                match res {
                                                    Some(v) => {
                                                        let _ = result.send(v);
                                                    },
                                                    None => {
                                                        let _ = result.send(Err(mluau::Error::external("Lua thread returned no value")));
                                                    },
                                                }
                                            }
                                        )
                                        .catch_unwind()
                                        .map_err(move |e| {
                                            let err_msg = if let Some(s) = e.downcast_ref::<&str>() {
                                                s.to_string()
                                            } else if let Some(s) = e.downcast_ref::<String>() {
                                                s.clone()
                                            } else {
                                                "Unknown panic".to_string()
                                            };
                                            let _ = result_ref.send(Err(mluau::Error::external(
                                                format!("Panic occurred while executing Lua thread: {err_msg}")
                                            )));
                                        })
                                    );
                                }

                                // TODO: Move the below to use futures_unordered as well
                                RuntimeEvent::RunQuickjsProxy { func_idx, args, result } => {
                                    let func = match self_ref.proxy_funcs.get(func_idx) {
                                        Some(f) => f,
                                        None => {
                                            let _ = result.send(Err(mluau::Error::external("Invalid QuickJS function index")));
                                            continue;
                                        }
                                    };

                                    let mut func_args = rquickjs::function::Args::new(ctx.clone(), args.len());      
                                    for arg in args {
                                        let qjs_value = match proxy_object(ctx.clone(), arg, &self_ref) {
                                            Ok(v) => v,
                                            Err(e) => {
                                                let _ = result.send(Err(mluau::Error::external(format!("Failed to proxy Lua argument to QuickJS: {e}"))));
                                                continue;
                                            }
                                        };

                                        match func_args.push_arg(qjs_value) {
                                            Ok(_) => {},
                                            Err(e) => {
                                                let _ = result.send(Err(mluau::Error::external(format!("Failed to push argument to QuickJS function: {e}"))));
                                                continue;
                                            }
                                        };
                                    }   

                                    let p = func.clone();
                                    let func_lifetime = match p.restore(&ctx) {
                                        Ok(f) => f,
                                        Err(e) => {
                                            let _ = result.send(Err(mluau::Error::external(format!("Failed to restore QuickJS function: {e}"))));
                                            continue;
                                        }
                                    };

                                    let res = match func_lifetime.call_arg::<Value>(func_args) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            let _ = result.send(Err(mluau::Error::external(format!("Failed to call QuickJS function: {e}"))));
                                            continue;
                                        }
                                    };

                                    let Some(lua) = self_ref.lua.try_upgrade() else {
                                        let _ = result.send(Err(mluau::Error::external("Lua state has been dropped")));
                                        continue;
                                    };

                                    let lua_res = match proxy_from_quickjs(ctx.clone(), &lua, &self_ref, res) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            let _ = result.send(Err(mluau::Error::external(format!("Failed to proxy QuickJS return value to Lua: {e}"))));
                                            continue;
                                        }
                                    };

                                    let mut mv = mluau::MultiValue::with_capacity(1);
                                    mv.push_back(lua_res);
                                    let _ = result.send(Ok(mv));
                                    drop(lua);
                                }
                                RuntimeEvent::RunQuickjsPromiseProxy { promise_idx, result } => {
                                    let promise = match self_ref.proxy_promises.get(promise_idx) {
                                        Some(p) => p,
                                        None => {
                                            let _ = result.send(Err(mluau::Error::external("Invalid QuickJS promise index")));
                                            continue;
                                        }
                                    };

                                    let promise_lifetime = match promise.restore(&ctx) {
                                        Ok(p) => p,
                                        Err(e) => {
                                            let _ = result.send(Err(mluau::Error::external(format!("Failed to restore QuickJS promise: {e}"))));
                                            continue;
                                        }
                                    };

                                    let res = match promise_lifetime.into_future().await {
                                        Ok(v) => v,
                                        Err(e) => {
                                            let _ = result.send(Err(mluau::Error::external(format!("Failed to await QuickJS promise: {e}"))));
                                            continue;
                                        }
                                    };

                                    let Some(lua) = self_ref.lua.try_upgrade() else {
                                        let _ = result.send(Err(mluau::Error::external("Lua state has been dropped")));
                                        continue;
                                    };

                                    let lua_res = match proxy_from_quickjs(ctx.clone(), &lua, &self_ref, res) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            let _ = result.send(Err(mluau::Error::external(format!("Failed to proxy QuickJS return value to Lua: {e}"))));
                                            continue;
                                        }
                                    };

                                    let mut mv = mluau::MultiValue::with_capacity(1);
                                    mv.push_back(lua_res);
                                    let _ = result.send(Ok(mv));
                                    drop(lua);
                                }
                            }
                        }
                        Some(_) = async_queue.next() => {

                        }
                    }
                }
            })
        )
        .await;

        self.is_running.set(false);

        // Make sure to clear the persistent proxy stores
        self.proxy_funcs.clear();
        self.proxy_promises.clear();
    }

    fn weak(&self) -> WeakQuickjsRuntime {
        WeakQuickjsRuntime {
            inner: Rc::downgrade(&self.inner)
        }
    }
}

#[derive(Clone)]
pub struct WeakQuickjsRuntime {
    inner: std::rc::Weak<QuickjsRuntimeInner>,
}

impl WeakQuickjsRuntime {
    pub fn upgrade(&self) -> Option<QuickjsRuntime> {
        self.inner.upgrade().map(|inner| QuickjsRuntime { inner })
    }
}

// On drop, signal the runtime to shut down
impl Drop for QuickjsRuntime {
    fn drop(&mut self) {
        let _ = self.tx.send(RuntimeEvent::Shutdown);
    }
}

pub struct LuaFunction {}

impl LuaFunction {
    /// Internal async handler for calling the Lua function
    async fn handle_async<'js>(func: mluau::Function, runtime: QuickjsRuntime, ctx: &RQCtx<'js>, args: rquickjs::function::Rest<Value<'js>>) -> Result<rquickjs::Value<'js>, Error> {
        let mut lua_args = mluau::MultiValue::new();
        let Some(lua) = func.weak_lua().try_upgrade() else {
            return Err("Lua state has been dropped".into());
        };

        for arg in args.0 {
            let lua_arg = proxy_from_quickjs(ctx.clone(), &lua, &runtime, arg)?;
            lua_args.push_back(lua_arg);
        }

        let thread = lua.create_thread(func.clone())
            .map_err(|e| format!("Failed to create Lua thread: {}", e))?;
        let (res_tx, mut res_rx) = tokio::sync::mpsc::unbounded_channel();
        runtime.tx.send(RuntimeEvent::AddAsync {
            thread,
            args: lua_args,
            result: res_tx,
        }).map_err(|e| format!("Failed to send event to runtime: {}", e))?;

        let res = res_rx.recv().await
        .ok_or("Failed to receive result from runtime")?
        .map_err(|e| e.to_string())?;

        if res.len() == 0 {
            Ok(rquickjs::Value::new_undefined(ctx.clone()))
        } else if res.len() == 1 {
            proxy_object(ctx.clone(), res.into_iter().next().unwrap(), &runtime)
        } else {
            let arr = rquickjs::Array::new(ctx.clone())?;
            for (i, v) in res.into_iter().enumerate() {
                let js_v = proxy_object(ctx.clone(), v, &runtime)?;
                arr.set(i, js_v)?;
            }
            Ok(rquickjs::Value::from_array(arr))
        }
    }

    /// Creates a new rquickjs proxy function that calls the given Lua function asynchronously
    pub fn new<'js>(ctx: RQCtx<'js>, func: mluau::Function, runtime: QuickjsRuntime) -> Result<rquickjs::Value<'js>, Error> {
        let weak_rt = runtime.weak();
        drop(runtime);
        
        let rust_func = rquickjs::Function::new(
            ctx,
            rquickjs::prelude::Async(move |ctx: RQCtx<'js>, args: rquickjs::function::Rest<Value<'js>>| {
                let func_ref = func.clone();
                let weak_rt = weak_rt.clone();
                async move {
                    let Some(runtime_ref) = weak_rt.upgrade() else {
                        let err_val = rquickjs::String::from_str(ctx.clone(), "Lua state has been dropped")?.into_value();
                        return Err(ctx.throw(err_val));
                    };

                    match Self::handle_async(func_ref, runtime_ref, &ctx, args).await {
                        Ok(v) => return Ok(v),
                        Err(e) => {
                            let string_val = rquickjs::String::from_str(ctx.clone(), &e.to_string())?;
                            Err(ctx.throw(string_val.into_value()))
                        }
                    }
                }
            })
        )?;
        Ok(Value::from_function(rust_func))
    }

    // /// Creates a new Lua proxy function that calls the given rquickjs function asynchronously
    pub fn from_quickjs<'js>(ctx: RQCtx<'js>, func: rquickjs::Function<'js>, runtime: QuickjsRuntime, lua: &mluau::Lua) -> Result<mluau::Function, Error> {
        let persistent_func = Persistent::save(&ctx, func);
        let func_idx = runtime.proxy_funcs.add(persistent_func);
        let weak_rt = runtime.weak();
        drop(ctx);
        drop(runtime);

        let lua_func = lua.create_scheduler_async_function(move |_, args: mluau::MultiValue| {
            let weak_rt = weak_rt.clone();
            let func_idx = func_idx.clone();
            async move {
                let Some(runtime) = weak_rt.upgrade() else {
                    return Err(mluau::Error::external("Lua state has been dropped"));
                };

                let (res_tx, mut res_rx) = tokio::sync::mpsc::unbounded_channel();
                runtime.tx.send(RuntimeEvent::RunQuickjsProxy {
                    func_idx,
                    args,
                    result: res_tx,
                }).map_err(|e| mluau::Error::external(e))?;

                let res = res_rx.recv().await
                    .ok_or(mluau::Error::external("Failed to receive result from QuickJS runtime"))??;
                Ok(res)
            }
        })
            .map_err(|e| format!("Failed to create Lua function: {}", e))?;

        Ok(lua_func)
    }
}

#[rquickjs::class(frozen)]
#[derive(Trace, JsLifetime)]
pub struct LuaThread {
    #[qjs(skip_trace)]
    th: mluau::Thread
}

#[rquickjs::class(frozen)]
#[derive(Trace, JsLifetime)]
pub struct LuaUserData {
    #[qjs(skip_trace)]
    ud: mluau::AnyUserData
}

#[rquickjs::class(frozen)]
#[derive(Trace, JsLifetime)]
pub struct LuaLightUserData {
    #[qjs(skip_trace)]
    lud: mluau::LightUserData
}

pub struct QuickJSPromise {
    promise: Option<PersistentStoreHandler<rquickjs::Promise<'static>>>,
    weak_rt: WeakQuickjsRuntime,
}

impl mluau::UserData for QuickJSPromise {
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_scheduler_async_method_mut("await", async move |_, mut this, _: ()| {
            let Some(runtime) = this.weak_rt.upgrade() else {
                return Err(mluau::Error::external("Lua state has been dropped"));
            };

            let promise_idx = match this.promise.take() {
                Some(p) => p,
                None => return Err(mluau::Error::external("QuickJS promise has already been awaited")),
            };

            let (res_tx, mut res_rx) = tokio::sync::mpsc::unbounded_channel();
            runtime.tx.send(RuntimeEvent::RunQuickjsPromiseProxy {
                promise_idx,
                result: res_tx,
            }).map_err(|e| mluau::Error::external(e))?;
            let res = res_rx.recv().await
                .ok_or(mluau::Error::external("Failed to receive result from QuickJS runtime"))??;

            Ok(res)
        })
    }
}

pub fn proxy_object<'js>(ctx: RQCtx<'js>, value: mluau::Value, runtime: &QuickjsRuntime) -> Result<rquickjs::Value<'js>, Error> {
    match value {
        mluau::Value::Nil => Ok(rquickjs::Value::new_null(ctx)),
        mluau::Value::Boolean(b) => Ok(rquickjs::Value::new_bool(ctx, b)),
        mluau::Value::LightUserData(lu) => {
            let lua_light_userdata = LuaLightUserData { lud: lu };
            Ok(lua_light_userdata.into_js(&ctx)?)
        }
        mluau::Value::Integer(i) => {
            if i > i32::MAX.into() {
                Ok(rquickjs::Value::new_big_int(ctx, i))
            } else {
                Ok(rquickjs::Value::new_int(ctx, i as i32))
            }
        },
        mluau::Value::Number(n) => Ok(rquickjs::Value::new_float(ctx, n)),
        mluau::Value::Vector(v) => {
            let arr = rquickjs::Array::new(ctx.clone())?;
            arr.set(0, rquickjs::Value::new_float(ctx.clone(), v.x() as f64))?;
            arr.set(1, rquickjs::Value::new_float(ctx.clone(), v.y() as f64))?;
            arr.set(2, rquickjs::Value::new_float(ctx.clone(), v.z() as f64))?;
            Ok(rquickjs::Value::from_array(arr))
        }
        mluau::Value::String(s) => {
            let s = s.to_string_lossy();
            let qjs_str = rquickjs::String::from_str(ctx, &s)?;
            Ok(rquickjs::Value::from_string(qjs_str))
        }
        mluau::Value::Table(t) => {
            // Make it a js object
            let obj = rquickjs::Object::new(ctx.clone())?;
            t.for_each(|k, v| {
                let k = proxy_object(ctx.clone(), k, runtime).map_err(|e| mluau::Error::external(format!("Failed to proxy key: {}", e)))?;
                let v = proxy_object(ctx.clone(), v, runtime).map_err(|e| mluau::Error::external(format!("Failed to proxy value: {}", e)))?;
                obj.set(k, v).map_err(|e| mluau::Error::external(format!("Failed to set object property: {}", e)))?;
                Ok(())
            }).map_err(|e| format!("Failed to iterate table: {}", e))?;
            Ok(rquickjs::Value::from_object(obj))
        }
        mluau::Value::Function(f) => {
            LuaFunction::new(ctx, f, runtime.clone())
        }
        mluau::Value::Thread(t) => {
            let lua_thread = LuaThread { th: t };
            Ok(lua_thread.into_js(&ctx)?)
        }
        mluau::Value::UserData(u) => {
            let lua_userdata = LuaUserData { ud: u };
            Ok(lua_userdata.into_js(&ctx)?)
        }
        mluau::Value::Buffer(buf) => {
            let bytes = buf.to_vec();
            let array = rquickjs::ArrayBuffer::new(ctx.clone(), bytes)?;
            Ok(array.into_js(&ctx)?)
        }
        mluau::Value::Error(e) => Err(format!("Cannot proxy Lua error: {}", e).into()),
        mluau::Value::Other(_) => Err("Cannot proxy unknown/other Lua value".into()),
    }
}

pub fn proxy_from_quickjs<'js>(ctx: RQCtx<'js>, lua: &mluau::Lua, runtime: &QuickjsRuntime, value: rquickjs::Value<'js>) -> Result<mluau::Value, Error> {
    if value.is_null() || value.is_undefined() {
        Ok(mluau::Value::Nil)
    } else if let Some(b) = value.as_bool() {
        Ok(mluau::Value::Boolean(b))
    } else if let Some(i) = value.as_int() {
        Ok(mluau::Value::Integer(i as i64))
    } else if let Some(n) = value.as_float() {
        Ok(mluau::Value::Number(n))
    } else if let Some(s) = value.as_string() {
        let s = s.to_string()?;
        Ok(lua.create_string(&s).map(mluau::Value::String).map_err(|e| format!("Failed to create Lua string: {}", e))?)
    } else if let Some(arr) = value.as_array() {
        let len = arr.len();
        if len == 3 {
            let x = arr.get::<f32>(0)?;
            let y = arr.get::<f32>(1)?;
            let z = arr.get::<f32>(2)?;
            Ok(mluau::Value::Vector(mluau::Vector::new(x, y, z)))
        } else {
            // Convert to table
            let table = lua.create_table_with_capacity(len, 0)
            .map_err(|e| format!("Failed to create Lua table: {}", e))?;

            for value in arr.iter() {
                let v = proxy_from_quickjs(ctx.clone(), lua, runtime, value?)?;
                table.raw_push(v).map_err(|e| format!("Failed to push value to Lua table: {}", e))?;
            }

            Ok(mluau::Value::Table(table))
        }
    } else if let Some(obj) = value.as_object() {
        let table = lua.create_table_with_capacity(0, obj.len())
            .map_err(|e| format!("Failed to create Lua table: {}", e))?;

        for prop in obj.own_props(rquickjs::Filter::new().enum_only()) {
            let (k, v) = prop?;
            let k = proxy_from_quickjs(ctx.clone(), lua, runtime, k)?;
            let v = proxy_from_quickjs(ctx.clone(), lua, runtime, v)?;
            table.set(k, v)
                .map_err(|e| format!("Failed to set Lua table key/value: {}", e))?;
        }
        Ok(mluau::Value::Table(table))
    } else if let Some(func) = value.as_function() {
        Ok(LuaFunction::from_quickjs(ctx, func.clone(), runtime.clone(), lua)
            .map(mluau::Value::Function)
            .map_err(|e| format!("Failed to create Lua function from QuickJS function: {}", e))?)
    } else if let Ok(lua_thread) = <rquickjs::Class<LuaThread>>::from_value(&value) {
        let v = lua_thread.try_borrow()?;
        Ok(mluau::Value::Thread(v.th.clone()))
    } else if let Ok(lua_userdata) = <rquickjs::Class<LuaUserData>>::from_value(&value) {
        let v = lua_userdata.try_borrow()?;
        Ok(mluau::Value::UserData(v.ud.clone()))
    } else if let Ok(lua_light_userdata) = <rquickjs::Class<LuaLightUserData>>::from_value(&value) {
        let v = lua_light_userdata.try_borrow()?;
        Ok(mluau::Value::LightUserData(v.lud.clone()))
    } else if let Some(promise) = value.as_promise() {
        let persistent_promise = Persistent::save(&ctx, promise.clone());
        let promise_idx = runtime.proxy_promises.add(persistent_promise);
        drop(ctx);

        let quickjs_promise = QuickJSPromise { promise: Some(promise_idx), weak_rt: runtime.weak() };
        Ok(mluau::Value::UserData(lua.create_userdata(quickjs_promise)
            .map_err(|e| format!("Failed to create Lua QuickJS promise userdata: {}", e))?))
    } else if let Some(array_buf) = rquickjs::ArrayBuffer::from_value(value) {
        let Some(bytes) = array_buf.as_bytes() else {
            return Err("Failed to get bytes from ArrayBuffer".into());
        };
        Ok(lua.create_buffer(bytes)
            .map(mluau::Value::Buffer)
            .map_err(|e| format!("Failed to create Lua buffer: {}", e))?
        )
    } else {
        Err("Cannot proxy unknown JS value to Lua".into())
    }
}
