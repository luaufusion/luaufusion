use futures_util::stream::FuturesUnordered;
use mlua_scheduler::taskmgr::get;
use mlua_scheduler::LuaSchedulerAsync;
use mlua_scheduler::LuaSchedulerAsyncUserData;

use mluau::UserDataMethods;
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
    RunQuickjsModule {
        name: String,
        code: String,
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
    GetQuickjsObject {
        obj_idx: PersistentStoreHandler<rquickjs::Object<'static>>,
        key: mluau::Value,
        result: MSender<Result<mluau::Value, mluau::Error>>,
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
    proxy_funcs: PersistentStore<rquickjs::Function<'static>>,
    proxy_promises: PersistentStore<rquickjs::Promise<'static>>,
    proxy_objects: PersistentStore<rquickjs::Object<'static>>,
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

pub struct LoaderAndResolver {
    pub loader: rquickjs::loader::BuiltinLoader,
    pub resolver: rquickjs::loader::BuiltinResolver,
}

impl LoaderAndResolver {
    pub fn new() -> Self {
        Self {
            loader: rquickjs::loader::BuiltinLoader::default(),
            resolver: rquickjs::loader::BuiltinResolver::default(),
        }
    }

    pub fn add_module(&mut self, name: &str, code: &str) {
        self.loader.add_module(name, code);
        self.resolver.add_module(name);
    }
}

impl rquickjs::loader::Loader for LoaderAndResolver {
    fn load<'js>(&mut self, ctx: &RQCtx<'js>, name: &str) -> rquickjs::Result<rquickjs::Module<'js, rquickjs::module::Declared>> {
        self.loader.load(ctx, name)
    }
}

impl rquickjs::loader::Resolver for LoaderAndResolver {
    fn resolve<'js>(&mut self, ctx: &RQCtx<'js>, base: &str, name: &str) -> rquickjs::Result<String> {
        self.resolver.resolve(ctx, base, name)
    }
}

impl QuickjsRuntime {
    pub async fn new(lua: WeakLua, builtin_loader: LoaderAndResolver) -> Result<Self, Error> {
        let (tx, rx) = unbounded_channel();

        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let runtime = Self { 
            inner: Rc::new(QuickjsRuntimeInner {
                lua: lua.clone(), 
                tx: tx.clone(), 
                is_running: Rc::new(Cell::new(false)), 
                proxy_funcs: PersistentStore::new(),
                proxy_promises: PersistentStore::new(),
                proxy_objects: PersistentStore::new(),
            })
        };

        tokio::task::spawn_local({
            let runtime = runtime.clone();
            async move {
                runtime.run(start_tx, rx, builtin_loader).await;
            }
        });

        // Wait for the runtime to signal that it has started
        start_rx.await?;

        Ok(runtime)
    }

    async fn run(&self, start_tx: OneShotSender<()>, mut rx: MReceiver<RuntimeEvent>, builtin_loader: LoaderAndResolver) {
        if self.is_running.get() {
            panic!("QuickJS proxy runtime is already running");
        }

        let runtime = RQRuntime::new().expect("Failed to create QuickJS runtime");
        runtime.set_loader(builtin_loader.resolver, builtin_loader.loader).await;

        let context = RQContext::full(&runtime).await.expect("Failed to create QuickJS context");

        self.is_running.set(true);
        let _ = start_tx.send(());

        let mut async_queue = FuturesUnordered::new();

        let self_ref = self.clone();
        context.async_with(
            |ctx| Box::pin(async move {
                let mut async_queue_quickjs_proxy = FuturesUnordered::new();
                let mut async_queue_quickjs_promise = FuturesUnordered::new();
                let mut async_queue_quickjs_module = FuturesUnordered::new();
                let mut async_queue_quickjs_get_obj = FuturesUnordered::new();

                loop {
                    tokio::select! {
                        //biased;
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
                                RuntimeEvent::RunQuickjsProxy { func_idx, args, result } => {
                                    let self_ref_cloned = self_ref.clone();
                                    let ctx_ref = ctx.clone();
                                    async_queue_quickjs_proxy.push(
                                        Self::run_quick_js_proxy_impl(
                                            self_ref_cloned,
                                            func_idx,
                                            result,
                                            ctx_ref,
                                            args,
                                        )
                                    );
                                }
                                RuntimeEvent::RunQuickjsPromiseProxy { promise_idx, result } => {
                                    let self_ref_cloned = self_ref.clone();
                                    let ctx_ref = ctx.clone();
                                    async_queue_quickjs_promise.push(
                                        Self::run_quick_js_promise_proxy_impl(
                                            self_ref_cloned,
                                            promise_idx,
                                            result,
                                            ctx_ref,
                                        )
                                    );
                                }
                                RuntimeEvent::RunQuickjsModule { name, code, result } => {
                                    let self_ref_cloned = self_ref.clone();
                                    let ctx_ref = ctx.clone();
                                    async_queue_quickjs_module.push(
                                        Self::run_quickjs_module_impl(
                                            self_ref_cloned,
                                            name,
                                            code,
                                            ctx_ref,
                                            result,
                                        )
                                    );
                                }
                                RuntimeEvent::GetQuickjsObject { obj_idx, key, result } => {
                                    let self_ref_cloned = self_ref.clone();
                                    let ctx_ref = ctx.clone();
                                    async_queue_quickjs_get_obj.push(
                                        Self::get_quickjs_obj_impl(
                                            self_ref_cloned,
                                            obj_idx,
                                            key,
                                            ctx_ref,
                                            result,
                                        )
                                    );
                                }
                            }
                        }
                        Some(_) = async_queue_quickjs_get_obj.next() => {}
                        Some(_) = async_queue.next() => {}
                        Some(_) = async_queue_quickjs_proxy.next() => {}
                        Some(_) = async_queue_quickjs_promise.next() => {}
                        Some(_) = async_queue_quickjs_module.next() => {}
                    }
                }
            })
        )
        .await;

        self.is_running.set(false);

        // Make sure to clear the persistent proxy stores
        self.proxy_funcs.clear();
        self.proxy_promises.clear();
        self.proxy_objects.clear();
    }

    async fn run_quick_js_proxy_impl(
        self_ref: QuickjsRuntime,
        func_idx: PersistentStoreHandler<rquickjs::Function<'static>>,
        result: MSender<Result<mluau::MultiValue, mluau::Error>>,
        ctx: RQCtx<'_>,
        args: mluau::MultiValue,
    ) {
        let func = match self_ref.proxy_funcs.get(func_idx) {
            Some(f) => f,
            None => {
                let _ = result.send(Err(mluau::Error::external("Invalid QuickJS function index")));
                return;
            }
        };

        let mut func_args = rquickjs::function::Args::new(ctx.clone(), args.len());      
        for arg in args {
            let qjs_value = match proxy_object(ctx.clone(), arg, &self_ref) {
                Ok(v) => v,
                Err(e) => {
                    let _ = result.send(Err(mluau::Error::external(format!("Failed to proxy Lua argument to QuickJS: {e}"))));
                    return;
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
                return;
            }
        };

        let res = match func_lifetime.call_arg::<Value>(func_args) {
            Ok(v) => v,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to call QuickJS function: {e}"))));
                return;
            }
        };

        let Some(lua) = self_ref.lua.try_upgrade() else {
            let _ = result.send(Err(mluau::Error::external("Lua state has been dropped")));
            return;
        };

        let lua_res = match proxy_from_quickjs(ctx.clone(), &lua, &self_ref, res) {
            Ok(v) => v,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to proxy QuickJS return value to Lua: {e}"))));
                return;
            }
        };

        let mut mv = mluau::MultiValue::with_capacity(1);
        mv.push_back(lua_res);
        let _ = result.send(Ok(mv));
        drop(lua);
    }

    async fn run_quick_js_promise_proxy_impl(
        self_ref: QuickjsRuntime,
        promise_idx: PersistentStoreHandler<rquickjs::Promise<'static>>,
        result: MSender<Result<mluau::MultiValue, mluau::Error>>,
        ctx: RQCtx<'_>,
    ) {
        let promise = match self_ref.proxy_promises.get(promise_idx) {
            Some(p) => p,
            None => {
                let _ = result.send(Err(mluau::Error::external("Invalid QuickJS promise index")));
                return;
            }
        };

        let promise_lifetime = match promise.restore(&ctx) {
            Ok(p) => p,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to restore QuickJS promise: {e}"))));
                return;
            }
        };

        let res = match promise_lifetime.into_future().await {
            Ok(v) => v,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to await QuickJS promise: {e}"))));
                return;
            }
        };

        let Some(lua) = self_ref.lua.try_upgrade() else {
            let _ = result.send(Err(mluau::Error::external("Lua state has been dropped")));
            return;
        };

        let lua_res = match proxy_from_quickjs(ctx.clone(), &lua, &self_ref, res) {
            Ok(v) => v,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to proxy QuickJS return value to Lua: {e}"))));
                return;
            }
        };

        let mut mv = mluau::MultiValue::with_capacity(1);
        mv.push_back(lua_res);
        let _ = result.send(Ok(mv));
        drop(lua);
    }

    async fn run_quickjs_module_impl(
        self_ref: QuickjsRuntime,
        name: String,
        code: String,
        ctx: RQCtx<'_>,
        result: MSender<Result<mluau::MultiValue, mluau::Error>>,
    ) {
        let module = match rquickjs::Module::declare(ctx.clone(), name, code) {
            Ok(m) => m,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to declare QuickJS module: {e}"))));
                return;
            }
        };

        let (module, promise) = match module.eval() {
            Ok(m) => m,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to evaluate QuickJS module: {e}"))));
                return;
            }
        };

        match promise.into_future::<()>().await {
            Ok(_) => {},
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to await QuickJS module evaluation: {e}"))));
                return;
            }
        }

        let namespace = match module.namespace() {
            Ok(n) => n,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to get QuickJS module namespace: {e}"))));
                return;
            }
        }.into_value();

        let Some(lua) = self_ref.lua.try_upgrade() else {
            let _ = result.send(Err(mluau::Error::external("Lua state has been dropped")));
            return;
        };

        let lua_res = match proxy_from_quickjs(ctx.clone(), &lua, &self_ref, namespace) {
            Ok(v) => v,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to proxy QuickJS return value to Lua: {e}"))));
                return;
            }
        };

        let mut mv = mluau::MultiValue::with_capacity(1);
        mv.push_back(lua_res);
        let _ = result.send(Ok(mv));
        drop(lua);
    }

    async fn get_quickjs_obj_impl(
        self_ref: QuickjsRuntime,
        obj_idx: PersistentStoreHandler<rquickjs::Object<'static>>,
        key: mluau::Value,
        ctx: RQCtx<'_>,
        result: MSender<Result<mluau::Value, mluau::Error>>,
    ) {
        let Some(lua) = self_ref.lua.try_upgrade() else {
            let _ = result.send(Err(mluau::Error::external("Lua state has been dropped")));
            return;
        };

        let obj = match self_ref.proxy_objects.get(obj_idx) {
            Some(o) => o,
            None => {
                let _ = result.send(Err(mluau::Error::external("Invalid QuickJS object index")));
                return;
            }
        };

        let key = match proxy_object(ctx.clone(), key, &self_ref) {
            Ok(k) => k,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to proxy Lua key to QuickJS: {e}"))));
                return;
            }
        };

        let key_atom = match rquickjs::Atom::from_value(ctx.clone(), &key) {
            Ok(a) => a,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to convert QuickJS key to atom: {e}"))));
                return;
            }
        };

        let obj_lifetime = match obj.restore(&ctx) {
            Ok(o) => o,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to restore QuickJS object: {e}"))));
                return;
            }
        };

        let value = match obj_lifetime.get(key_atom) {
            Ok(v) => v,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to get property from QuickJS object: {e}"))));
                return;
            }
        };

        let lua_res = match proxy_from_quickjs(ctx.clone(), &lua, &self_ref, value) {
            Ok(v) => v,
            Err(e) => {
                let _ = result.send(Err(mluau::Error::external(format!("Failed to proxy QuickJS return value to Lua: {e}"))));
                return;
            }
        };

        let _ = result.send(Ok(lua_res));
        drop(lua);
    }

    pub fn weak(&self) -> WeakQuickjsRuntime {
        WeakQuickjsRuntime {
            inner: Rc::downgrade(&self.inner)
        }
    }
}

impl mluau::UserData for QuickjsRuntime {
    fn add_methods<M: mluau::UserDataMethods<Self>>(methods: &mut M) {
        // Returns if the runtime is currently running
        //
        // TODO: Remove later
        methods.add_method("isrunning", |_, this, _: ()| {
            Ok(this.is_running.get())
        });

        methods.add_scheduler_async_method("load", async move |_, this, (name, code): (String, String)| {
            // Push to the runtime event queue
            let (res_tx, mut res_rx) = tokio::sync::mpsc::unbounded_channel();
            this.tx.send(RuntimeEvent::RunQuickjsModule {
                name,
                code,
                result: res_tx,
            }).map_err(|e| mluau::Error::external(format!("Failed to send event to runtime: {}", e)))?;

            let res = res_rx.recv().await
                .ok_or(mluau::Error::external("Failed to receive result from runtime"))?
                .map_err(|e| mluau::Error::external(e.to_string()))?;

            Ok(res)
        });
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
impl Drop for QuickjsRuntimeInner {
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

/// A proxy from a QuickJS object to a Lua object
pub struct JSObjectProxy {
    obj: PersistentStoreHandler<rquickjs::Object<'static>>,
    weak_rt: WeakQuickjsRuntime,
}

impl mluau::UserData for JSObjectProxy {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_scheduler_async_method("get", async move |_lua, this, key: mluau::Value| {
            let Some(runtime) = this.weak_rt.upgrade() else {
                return Err(mluau::Error::external("Lua state has been dropped"));
            };

            let (res_tx, mut res_rx) = tokio::sync::mpsc::unbounded_channel();
            runtime.tx.send(RuntimeEvent::GetQuickjsObject {
                obj_idx: this.obj.clone(),
                key,
                result: res_tx,
            }).map_err(|e| mluau::Error::external(format!("Failed to request QuickJS for object: {e:?}")))?;

            let lua_value = res_rx.recv().await
                .ok_or(mluau::Error::external("Failed to receive result from QuickJS runtime"))??;


            Ok(lua_value)
        });
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

        table.set("proxy", JSObjectProxy {
            obj: runtime.proxy_objects.add(Persistent::save(&ctx, obj.clone())),
            weak_rt: runtime.weak(),
        })
            .map_err(|e| format!("Failed to set Lua table proxy field: {}", e))?;

        Ok(mluau::Value::Table(table))
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

#[cfg(test)]
mod test {
    use mluau::prelude::*;
    use mlua_scheduler::{LuaSchedulerAsync, XRc};
    use mlua_scheduler::taskmgr::NoopHooks;

    #[test]
    fn test_proxy() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let lua = mluau::Lua::new_with(mluau::StdLib::ALL_SAFE, mluau::LuaOptions::default())
                .expect("Failed to create Lua");

            let compiler = mluau::Compiler::new().set_optimization_level(2);

            lua.set_compiler(compiler);

            let returns_tracker = mlua_scheduler::taskmgr::ReturnTracker::new();

            let mut wildcard_sender = returns_tracker.track_wildcard_thread();

            tokio::task::spawn_local(async move {
                while let Some((thread, result)) = wildcard_sender.recv().await {
                    if let Err(e) = result {
                        eprintln!("Error in thread {e:?}: {:?}", thread.to_pointer());
                    }
                }
            });

            let task_mgr = mlua_scheduler::taskmgr::TaskManager::new(&lua, returns_tracker, XRc::new(NoopHooks {})).await.expect("Failed to create task manager");

            lua.globals()
                .set(
                    "_PANIC",
                    lua.create_scheduler_async_function(|_lua, n: i32| async move {
                        panic!("Panic test: {n}");
                        #[allow(unreachable_code)]
                        Ok(())
                    })
                    .expect("Failed to create async function"),
                )
                .expect("Failed to set _OS global");

            lua.globals()
                .set(
                    "task",
                    mlua_scheduler::userdata::task_lib(&lua).expect("Failed to create table"),
                )
                .expect("Failed to set task global");

            lua.sandbox(true).expect("Sandboxed VM"); // Sandbox VM

            let mut loader_and_resolver = super::LoaderAndResolver::new();
            loader_and_resolver.add_module("mymodule", r#"
export function add(a, b) {
    return a + b;
}
            "#);

            let qjs_proxy = super::QuickjsRuntime::new(
                lua.weak(),
                loader_and_resolver,
            )
            .await
            .expect("Failed to create QuickJS runtime");

            let lua_code = r#"
local qjs = ...
local result = qjs:load("testmodule", [[
    import { add } from './mymodule';
    export function test(a, b) {
        return add(a, b);
    }
]])

return result, result.proxy:get("test")(5, 10)
"#;

            let func = lua.load(lua_code).into_function().expect("Failed to load Lua code");
            let th = lua.create_thread(func).expect("Failed to create Lua thread");
            
            let mut args = mluau::MultiValue::new();
            args.push_back(qjs_proxy.into_lua(&lua).expect("Failed to push QuickJS runtime to Lua"));

            let output = task_mgr
                .spawn_thread_and_wait(th, args)
                .await
                .expect("Failed to run Lua thread")
                .expect("Lua thread returned no value")
                .expect("Lua thread returned an error");
            
            println!("Output: {:?}", output);

            let table = output.into_iter().next().unwrap().as_table().expect("Output is not a table").clone();
            println!("Table: {:#?}", table);
        });
    }
}