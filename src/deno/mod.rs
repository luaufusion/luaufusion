pub(crate) mod extension;
pub(crate) mod base64_ops;

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

//use deno_core::error::{CoreError, CoreErrorKind};
use deno_core::v8::CreateParams;
use deno_core::v8;
use mluau::prelude::*;
//use deno_core::{op2, OpState};
//use deno_error::JsErrorBox;
//use mluau::serde::de;
use tokio_util::sync::CancellationToken;

pub type Error = Box<dyn std::error::Error>;

const MAX_REFS: usize = 500;

/// Stores a pointer to a Rust struct and a V8 weak reference (with finalizer) to ensure the Rust struct is dropped when V8 garbage collects the associated object.
struct Finalizer<T> {
    ptr: *mut T,
    weak: v8::Weak<v8::External>,
    finalized: Rc<RefCell<bool>>,
}

impl<T: 'static> Finalizer<T> {
    // Create a new Finalizer given scope and Rust struct
    //
    // Returns the V8 local `v8::External`
    // and the Finalizer struct itself which contains a weak reference to the `v8::External` with finalizer
    fn new<'s>(
        scope: &mut v8::HandleScope<'s, ()>,
        rust_obj: T,
    ) -> (*mut T, v8::Local<'s, v8::External>, Self) {
        let ptr = Box::into_raw(Box::new(rust_obj));
        let external = v8::External::new(scope, ptr as *mut std::ffi::c_void);
        let global_external = v8::Global::new(scope, external);
        
        let finalized = Rc::new(RefCell::new(false));

        let finalized_ref = finalized.clone();
        let weak = v8::Weak::with_guaranteed_finalizer(scope, global_external, Box::new(move || {
            // Finalizer callback when V8 garbage collects the object
            if !*finalized_ref.borrow() {
                *finalized_ref.borrow_mut() = true; // Mark as finalized before droping to avoid double free in Drop
                unsafe {
                    drop(Box::from_raw(ptr));
                }
            }
        }));

        let finalizer = Self {
            ptr,
            weak,
            finalized,
        };

        (ptr, external, finalizer)
    }
}

impl<T> Drop for Finalizer<T> {
    fn drop(&mut self) {
        if !*self.finalized.borrow() {
            unsafe {
                drop(Box::from_raw(self.ptr));
            }
            *self.finalized.borrow_mut() = true;
        }
    }
}

pub struct FinalizerList<T> {
    list: Rc<RefCell<Vec<Finalizer<T>>>>,
    ptrs: Rc<RefCell<HashSet<usize>>>, // SAFETY: We store them as usizes to avoid improper use of T
}

impl<T: 'static> Default for FinalizerList<T> {
    fn default() -> Self {
        Self {
            list: Rc::default(),
            ptrs: Rc::default(),
        }
    }
}

impl<T: 'static> Clone for FinalizerList<T> {
    fn clone(&self) -> Self {
        Self {
            list: self.list.clone(),
            ptrs: self.ptrs.clone(),
        }
    }
}

impl<T: 'static> FinalizerList<T> {
    pub fn add<'s>(&self, scope: &mut v8::HandleScope<'s, ()>, rust_obj: T) -> Option<v8::Local<'s, v8::External>> {
        let (ptr, external, finalizer) = Finalizer::new(scope, rust_obj);
        let mut list = self.list.borrow_mut();
        if list.len() >= MAX_REFS {
            return None;
        }

        list.push(finalizer);

        self.ptrs.borrow_mut().insert(ptr as usize);

        Some(external)
    }

    pub fn contains(&self, ptr: *mut T) -> bool {
        let list = self.ptrs.borrow();
        list.contains(&(ptr as usize))
    }
}

pub struct V8IsolateManagerInner {
    deno: deno_core::JsRuntime,
    cancellation_token: CancellationToken,
    userdata_templates: RefCell<v8::Global<v8::ObjectTemplate>>,

    uds: FinalizerList<UserData>,
    funcs: FinalizerList<Function>,
}

#[derive(Clone)]
pub struct FinalizerAttachments {
    ud: FinalizerList<UserData>,
    func: FinalizerList<Function>,
}

impl V8IsolateManagerInner {
    // Internal, use proxy_to_v8_safe to ensure finalizers are also set
    fn proxy_to_v8_impl(&mut self, lua: &Lua, value: LuaValue) -> Result<v8::Global<v8::Value>, Error> {
        let userdata_template = self.userdata_templates.try_borrow()?.clone();

        let finalizers = FinalizerAttachments {
            ud: self.uds.clone(),
            func: self.funcs.clone(),
        };

        let v8_ctx = self.deno.main_context();
        let isolate = self.deno.v8_isolate();
        let scope = &mut v8::HandleScope::new(isolate);
        let v8_ctx = v8::Global::new(scope, v8_ctx);
        let v8_ctx = v8::Local::new(scope, v8_ctx);
        let scope = &mut v8::ContextScope::new(scope, v8_ctx);
        let v8_value = Self::proxy_to_v8(scope, &userdata_template, finalizers, lua, value)?;
        Ok(v8::Global::new(scope, v8_value))
    }
    
    fn proxy_to_v8<'s>(
        scope: &mut v8::HandleScope<'s>, 
        userdata_template: &v8::Global<v8::ObjectTemplate>,
        finalizers: FinalizerAttachments,
        lua: &Lua,
        value: LuaValue
    ) -> Result<v8::Local<'s, v8::Value>, Error> {
        let v8_value: v8::Local<v8::Value> = match value {
            LuaValue::Nil => v8::null(scope).into(),
            LuaValue::Boolean(b) => v8::Boolean::new(scope, b).into(),
            LuaValue::Integer(i) => {
                if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                    v8::Integer::new(scope, i as i32).into()
                } else {
                    v8::Number::new(scope, i as f64).into()
                }
            },
            LuaValue::Number(n) => v8::Number::new(scope, n).into(),
            mluau::Value::Vector(v) => {
                let arr = v8::Array::new(scope, 3);
                let x = v8::Number::new(scope, v.x() as f64);
                let y = v8::Number::new(scope, v.y() as f64);
                let z = v8::Number::new(scope, v.z() as f64);
                arr.set_index(scope, 0, x.into());
                arr.set_index(scope, 1, y.into());
                arr.set_index(scope, 2, z.into());
                arr.into()
            },
            LuaValue::String(s) => {
                let s = s.to_string_lossy();
                v8::String::new(scope, &s).ok_or("Failed to create V8 string")?.into()
            },
            LuaValue::Table(t) => {
                if let Some(lua) = t.weak_lua().try_upgrade() {
                    if t.metatable() == Some(lua.array_metatable()) {
                        // Convert to array
                        let len = t.raw_len();
                        if len <= i32::MAX as usize {
                            let arr = v8::Array::new(scope, len as i32);
                            for i in 1..=len {
                                let v = t.raw_get(i)
                                    .map_err(|e| mluau::Error::external(format!("Failed to get array element {}: {}", i, e)))?;
                                let v = Self::proxy_to_v8(scope, userdata_template, finalizers.clone(), &lua, v)?;
                                arr.set_index(scope, (i - 1) as u32, v);
                            }
                            return Ok(arr.into());
                        }
                    }
                }

                let obj = v8::Object::new(scope);
                t.for_each(|k, v| {
                    let v8_key = Self::proxy_to_v8(scope, userdata_template, finalizers.clone(), lua, k).map_err(|e| LuaError::external(e.to_string()))?;
                    let v8_val = Self::proxy_to_v8(scope, userdata_template, finalizers.clone(), lua, v).map_err(|e| LuaError::external(e.to_string()))?;
                    obj.set(scope, v8_key, v8_val);
                    Ok(())
                })?;
                obj.into()
            },
            // TODO: Function, Thread, Buffer
            mluau::Value::LightUserData(ptr) => {
                let num = ptr.0 as usize as u64;
                v8::BigInt::new_from_u64(scope, num).into()
            },
            mluau::Value::UserData(ud) => {
                let obj_template = userdata_template.clone();
                
                let local_template = v8::Local::new(scope, obj_template);
                
                let external = finalizers.ud.add(scope, UserData::new(ud))
                    .ok_or("Maximum number of userdata references reached")?;
                let obj = local_template.new_instance(scope).ok_or("Failed to create V8 object")?;
                obj.set_internal_field(0, external.into());
                obj.into()
            },
            mluau::Value::Function(func) => {
                let external = finalizers.func.add(scope, Function { func, weak_lua: lua.weak(), finalizers: finalizers.clone() })
                    .ok_or("Maximum number of function references reached")?;

                let func= v8::Function::builder(|scope: &mut v8::HandleScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue| {
                    let data = args.data();
                    if !data.is_external() {
                        let message = v8::String::new(scope, "Internal function data is not valid").unwrap();
                        let exception = v8::Exception::type_error(scope, message);
                        scope.throw_exception(exception);
                        return;
                    }
                    let external = match v8::Local::<v8::External>::try_from(data) {
                        Ok(ext) => ext,
                        Err(e) => {
                            let message = v8::String::new(scope, &format!("{e:?}")).unwrap();
                            let exception = v8::Exception::type_error(scope, message);
                            scope.throw_exception(exception);
                            return;
                        }
                    };
                    if external.value().is_null() {
                        let message = v8::String::new(scope, "Cannot access function data").unwrap();
                        let exception = v8::Exception::type_error(scope, message);
                        scope.throw_exception(exception);
                        return;
                    }
                    let ptr = external.value() as *mut Function;
                    // SAFETY: We already checked that the pointer is not null
                    let func = unsafe { &*(ptr) };

                    let Some(lua) = func.weak_lua.try_upgrade() else {
                        let message = v8::String::new(scope, "Lua state has been dropped").unwrap();
                        let exception = v8::Exception::type_error(scope, message);
                        scope.throw_exception(exception);
                        return;
                    };
                    
                    let promise_resolver = match v8::PromiseResolver::new(scope) {
                        Some(pr) => pr,
                        None => {
                            let message = v8::String::new(scope, "Failed to create PromiseResolver").unwrap();
                            let exception = v8::Exception::type_error(scope, message);
                            scope.throw_exception(exception);
                            return;
                        }
                    };
                    let promise = promise_resolver.get_promise(scope);
                    rv.set(promise.into());
                })
                .constructor_behavior(v8::ConstructorBehavior::Throw)
                .data(external.into())
                .build(scope)
                .ok_or("Failed to create V8 function")?;

                /*let func = v8::Function::new(scope, move |scope: &mut v8::HandleScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue| {
                    let func = unsafe { &*(ptr) };
                    println!("V8 function called with {:?} args", func.func);
                    let Some(promise_resolver) = v8::PromiseResolver::new(scope) else {
                        let message = v8::String::new(scope, "Failed to create PromiseResolver").unwrap();
                        let exception = v8::Exception::type_error(scope, message);
                        scope.throw_exception(exception);
                        return;
                    };
                    let promise = promise_resolver.get_promise(scope);
                    rv.set(promise.into());

                    let global_p = v8::Global::new(scope, promise_resolver);
                    tokio::task::spawn_local(async move {
                        //let th = func.weak_lua();
                    });
                }).ok_or("Failed to create V8 function")?;*/
                
                func.into()
            },
            mluau::Value::Buffer(buf) => {
                let bytes = buf.to_vec();
                let bs = v8::ArrayBuffer::new_backing_store_from_boxed_slice(bytes.into_boxed_slice()).make_shared();
                let array = v8::ArrayBuffer::with_backing_store(scope, &bs);
                array.into()
            },
            mluau::Value::Error(e) => return Err(format!("Cannot proxy Lua error: {}", e).into()),
            mluau::Value::Other(_) => return Err("Cannot proxy unknown/other Lua value".into()),
            _ => v8::undefined(scope).into(),
        };

        Ok(v8_value)
    }

    fn proxy_from_v8<'s>(
        scope: &mut v8::HandleScope<'s>, 
        lua: &mluau::Lua,
        value: v8::Local<'s, v8::Value>,
        finalizers: FinalizerAttachments,
    ) -> Result<LuaValue, Error> {
        if value.is_null() || value.is_undefined() {
            return Ok(LuaValue::Nil);
        } else if value.is_boolean() {
            let b = value.to_boolean(scope).is_true();
            return Ok(LuaValue::Boolean(b));
        } else if value.is_int32() {
            let i = value.to_int32(scope).ok_or("Failed to convert to int32")?.value();
            return Ok(LuaValue::Integer(i as i64));
        } else if value.is_number() {
            let n = value.to_number(scope).ok_or("Failed to convert to number")?.value();
            return Ok(LuaValue::Number(n));
        } else if value.is_string() {
            let s = value.to_string(scope).ok_or("Failed to convert to string")?;
            let s = s.to_rust_string_lossy(scope);
            return lua.create_string(s).map(LuaValue::String).map_err(|e| e.into());
        } else if value.is_array() {
            let arr = value.to_object(scope).ok_or("Failed to convert to object")?;
            let length_str = v8::String::new(scope, "length").ok_or("Failed to create length string")?;
            let length_val = arr.get(scope, length_str.into()).ok_or("Failed to get length property")?;
            let length = length_val.to_uint32(scope).ok_or("Failed to convert length to uint32")?.value() as usize;
            let table = lua.create_table_with_capacity(length, 0)?;
            for i in 0..length {
                let elem = arr.get_index(scope, i as u32).ok_or(format!("Failed to get array element {}", i))?;
                let lua_elem = Self::proxy_from_v8(scope, lua, elem, finalizers.clone())?;
                table.raw_set(i + 1, lua_elem)?; // Lua arrays are 1-based
            }
            return Ok(LuaValue::Table(table));
        } else if value.is_array_buffer() {
            let ab = v8::Local::<v8::ArrayBuffer>::try_from(value).map_err(|_| "Failed to convert to ArrayBuffer")?;
            let bs = ab.get_backing_store();
            let Some(data) = bs.data() else {
                return lua.create_buffer_with_capacity(0).map(LuaValue::Buffer).map_err(|e| e.into());
            };
            let slice = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, bs.byte_length()) };
            let buf = lua.create_buffer(slice)?;
            return Ok(LuaValue::Buffer(buf));
        } else if value.is_object() {
            let obj = value.to_object(scope).ok_or("Failed to convert to object")?;
            let num_internal_fields = obj.internal_field_count();
            if num_internal_fields == 1 {
                let maybe_external = obj.get_internal_field(scope, 0);
                if let Some(internal) = maybe_external {
                    if let Ok(external) = v8::Local::<v8::External>::try_from(internal) {
                        
                        // Case 1: UserData proxy
                        if finalizers.ud.contains(external.value() as *mut UserData) {
                            let ptr = external.value() as *mut UserData;
                            if !ptr.is_null() {
                                let ud = unsafe { &*ptr };
                                return Ok(LuaValue::UserData(ud.ud.clone()));
                            }
                        }
                    }
                }
            }

            // Generic object to table
            let table = lua.create_table()?;
            let props = obj.get_own_property_names(scope, Default::default()).ok_or("Failed to get object properties")?;
            let length = props.length();
            for i in 0..length {
                let key = props.get_index(scope, i).ok_or(format!("Failed to get property at index {}", i))?;
                let val = obj.get(scope, key).ok_or("Failed to get property value")?;
                let lua_key = Self::proxy_from_v8(scope, lua, key, finalizers.clone())?;
                let lua_val = Self::proxy_from_v8(scope, lua, val, finalizers.clone())?;
                table.raw_set(lua_key, lua_val)?;
            }
            return Ok(LuaValue::Table(table));
        }

        Ok(LuaValue::Nil) // TODO: Implement
    }
}

/// Internal manager for a single V8 isolate with a minimal Deno runtime.
#[derive(Clone)]
pub struct V8IsolateManager {
    inner: Rc<RefCell<V8IsolateManagerInner>>,
}

const MIN_HEAP_LIMIT: usize = 10 * 1024 * 1024; // 10MB

impl V8IsolateManager {
    pub fn new(heap_limit: usize) -> Self {
        let heap_limit = heap_limit.max(MIN_HEAP_LIMIT);

        // TODO: Support snapshots maybe
        let extensions = extension::all_extensions(false);

        let mut deno = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
            create_params: Some(
                CreateParams::default()
                .heap_limits(0, heap_limit)
            ),
            extensions,
            ..Default::default()
        });

        let isolate_handle = deno.v8_isolate().thread_safe_handle();
        let heap_exhausted_token = CancellationToken::new();

        // Add a callback to terminate the runtime if the max_heap_size limit is approached
        let heap_exhausted_token_ref = heap_exhausted_token.clone();
        deno.add_near_heap_limit_callback(move |current_value, _| {
            isolate_handle.terminate_execution();

            // Signal the outer runtime to cancel block_on future (avoid hanging) and return friendly error
            heap_exhausted_token_ref.cancel();

            // Spike the heap limit while terminating to avoid segfaulting
            // Callback may fire multiple times if memory usage increases quicker then termination finalizes
            5 * current_value
        });

        let userdata_template = {
            let isolate = deno.v8_isolate();
            let scope = &mut v8::HandleScope::new(isolate);
            let template = Self::create_ud_proxy_template(scope);
            v8::Global::new(scope, template)
        };

        Self {
            inner: Rc::new(RefCell::new(V8IsolateManagerInner {
                deno,
                cancellation_token: heap_exhausted_token,
                userdata_templates: RefCell::new(userdata_template),
                uds: FinalizerList::default(),
                funcs: FinalizerList::default(),
            }))
        }
    }

    fn create_ud_proxy_template<'s>(scope: &mut v8::HandleScope<'s, ()>) -> v8::Local<'s, v8::ObjectTemplate> {
        let template = v8::ObjectTemplate::new(scope);
        // Reserve space for the pointer to the Rust struct.
        template.set_internal_field_count(1);
        
        // 4. Configure the template to use our getter and setter callbacks.
        let named_handler = v8::NamedPropertyHandlerConfiguration::new()
            .getter(UserData::named_property_getter)
            .setter(UserData::named_property_setter);
        template.set_named_property_handler(named_handler);

        template
    }

    fn proxy_to_v8(&self, lua: &Lua, value: LuaValue) -> Result<v8::Global<v8::Value>, Error> {
        let value = {
            let mut inner = self.inner.try_borrow_mut()?;
            inner.proxy_to_v8_impl(lua, value)?
        };
        
        Ok(value)
    }

    pub fn weak(&self) -> WeakV8IsolateManager {
        WeakV8IsolateManager {
            inner: Rc::downgrade(&self.inner)
        }
    }
}

#[derive(Clone)]
pub struct WeakV8IsolateManager {
    inner: std::rc::Weak<RefCell<V8IsolateManagerInner>>,
}

impl WeakV8IsolateManager {
    pub fn upgrade(&self) -> Option<V8IsolateManager> {
        self.inner.upgrade().map(|inner| V8IsolateManager { inner })
    }
}

struct Function {
    func: mluau::Function,
    weak_lua: WeakLua,
    finalizers: FinalizerAttachments,
}

/*
impl Function {
    async fn create() {
        let args_count = args.length();
        let mut lua_args = LuaMultiValue::with_capacity(args_count as usize);
        for i in 0..args_count {
            let arg = args.get(i);
            match Self::proxy_from_v8(scope, &lua, arg, finalizers.clone()) {
                Ok(v) => lua_args.push_back(v),
                Err(e) => {
                    let _ = promise_resolver.reject(scope, v8::String::new(scope, &format!("Failed to convert argument {}: {}", i, e)).unwrap().into());
                    return;
                }
            }
        }

        let th = match lua.create_thread(func.func.clone()) {
            Ok(th) => th,
            Err(e) => {
                let _ = promise_resolver.reject(scope, v8::String::new(scope, &format!("Failed to create Lua thread: {}", e)).unwrap().into());
                return;
            }
        };

        let scheduler = taskmgr::get(&lua);
        let result = match scheduler.spawn_thread_and_wait(th, lua_args).await {
            Ok(ret) => {
                match ret {
                    Some(v) => v,
                    None => {
                        let _ = promise_resolver.reject(scope, v8::String::new(scope, "Lua thread returned no values").unwrap().into());
                        return;
                    }
                }
            },
            Err(e) => {
                let _ = promise_resolver.reject(scope, v8::String::new(scope, &format!("Lua thread error: {}", e)).unwrap().into());
                return;
            }
        };
        


        match result {
            Ok(ret) => {
                let results = vec![];
                for ret in ret {
                    match Self::proxy_to_v8(scope, userdata_template, finalizers.clone(), &lua, ret) {
                        Ok(v8_ret) => results.push(v8_ret),
                        Err(e) => {
                            let _ = promise_resolver.reject(scope, v8::String::new(scope, &format!("Failed to convert return value: {}", e)).unwrap().into());
                            return;
                        }
                    }
                }

                let arr = v8::Array::new(scope, results.len() as i32);
                for (i, v) in results.into_iter().enumerate() {
                    arr.set_index(scope, i as u32, v);
                }

                let _ = promise_resolver.resolve(scope, arr.into());
            },
            Err(e) => {
                let _ = promise_resolver.reject(scope, v8::String::new(scope, &format!("Lua function error: {}", e)).unwrap().into());
            }
        }
    }
}*/

struct UserData {
    pub ud: mluau::AnyUserData,
}

impl UserData {
    pub fn new(ud: mluau::AnyUserData) -> Self {
        Self { ud }
    }

    fn named_property_getter(
        scope: &mut v8::HandleScope,
        key: v8::Local<v8::Name>,
        args: v8::PropertyCallbackArguments,
        mut rv: v8::ReturnValue,
    ) -> v8::Intercepted {
        let this = args.this();
        let external = v8::Local::<v8::External>::try_from(this.get_internal_field(scope, 0).unwrap()).unwrap();
        if external.value().is_null() {
            if let Some(message) = v8::String::new(scope, "Cannot access external value") {
                let exception = v8::Exception::type_error(scope, message);
                scope.throw_exception(exception);
                return v8::Intercepted::No;
            }
            return v8::Intercepted::No;
        }
        let _rust_obj = unsafe { &*(external.value() as *mut UserData) };
        println!("UD: getting property {:?}, ud: {:?}", key, _rust_obj.ud);
        return v8::Intercepted::No; // TODO: Implement
    }

    fn named_property_setter(
        scope: &mut v8::HandleScope,
        key: v8::Local<v8::Name>,
        value: v8::Local<v8::Value>,
        args: v8::PropertyCallbackArguments,
        mut rv: v8::ReturnValue<()>,
    ) -> v8::Intercepted {
        let this = args.this();
        let external = v8::Local::<v8::External>::try_from(this.get_internal_field(scope, 0).unwrap()).unwrap();
        if external.value().is_null() {
            if let Some(message) = v8::String::new(scope, "Cannot access external value") {
                let exception = v8::Exception::type_error(scope, message);
                scope.throw_exception(exception);
                return v8::Intercepted::No;
            }
            return v8::Intercepted::No;
        }
        let _rust_obj = unsafe { &*(external.value() as *mut UserData) };
        return v8::Intercepted::No; // TODO: Implement
    }
}

struct ProxyData {
    userdata_wrapper: v8::Global<v8::Object>,
    isolate: v8::Global<v8::Isolate>,
}

/*#[derive(Clone)]
struct ProxyFn {
    func: Rc<dyn Fn()>
}

#[op2(async)]
async fn function_proxy(
   state: Rc<RefCell<OpState>>,
) -> Result<(), CoreError> {
    let state_guard = state.try_borrow()
        .map_err(|e| CoreError(
            Box::new(CoreErrorKind::JsBox(JsErrorBox::generic(e.to_string()))))
        )?;
    
    let state = state_guard.try_borrow::<ProxyFn>()
        .ok_or_else(|| CoreError(
            Box::new(CoreErrorKind::JsBox(JsErrorBox::generic("Proxy function not found".to_string())))
        ))?;

    Ok(())
}*/

#[cfg(test)]
mod tests {
    use deno_core::v8;
    #[test]
    fn test_v8_isolate_manager() {
        let manager = super::V8IsolateManager::new(super::MIN_HEAP_LIMIT);

        // Test userdata
        let lua = mluau::Lua::new();
        struct MyUD {
            value: i32
        }

        impl mluau::UserData for MyUD {
            fn add_methods<'lua, M: mluau::UserDataMethods<Self>>(methods: &mut M) {
                methods.add_method("get_value", |_, this, ()| {
                    Ok(this.value)
                });
            }
        }

        let ud = lua.create_userdata(MyUD { value: 42 }).unwrap();
        let lua_value = mluau::Value::UserData(ud);

        // Create a ProxyData
        let v8_value = manager.proxy_to_v8(&lua, lua_value).unwrap();
        let mut inner = manager.inner.borrow_mut();
        let context = inner.deno.main_context();

        let isolate = inner.deno.v8_isolate();
        let scope = &mut v8::HandleScope::new(isolate);
        let v8_value = v8::Local::new(scope, v8_value);
        println!("V8 Value: {:?} is_obj={}", v8_value, v8_value.is_object());

        let context = v8::Local::new(scope, context);
        let scope = &mut v8::ContextScope::new(scope, context);
        let Some(obj) = v8_value.to_object(scope) else {
            panic!("Expected V8 object");
        };

        let s = v8::String::new(scope, "get_value").unwrap();
        let v = obj.get(scope, s.into()).unwrap();
        println!("typeof: {:?}", v.type_of(scope).to_rust_string_lossy(scope));
    }
}