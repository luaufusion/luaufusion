use super::{ObjectRegistry, StringAtom, StringAtomList};
use crate::deno::{Error, V8IsolateManager, MAX_PROXY_DEPTH};
use deno_core::v8;

/// A V8 value that can now be easily proxied to Luau
pub(crate) enum ProxiedV8Value {
    Nil,
    Undefined,
    Boolean(bool),
    Integer(i32),
    Number(f64),
    String(StringAtom), // To avoid large amounts of copying, we store strings in a separate atom list
    Buffer(Vec<u8>), // Binary data
    Object(i32), // Object ID in the map registry
    Array(Vec<ProxiedV8Value>),
    Function(i32), // Function ID in the function registry
    Promise(i32), // Promise ID in the function registry
}

impl ProxiedV8Value {
    pub(crate) fn proxy_to_lua(self, lua: &mluau::Lua, bridge: &V8IsolateManager) -> Result<mluau::Value, mluau::Error> {
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
            ProxiedV8Value::Array(elems) => {
                let tbl = lua.create_table_with_capacity(elems.len(), 0)?;
                for elem in elems {
                    tbl.raw_push(elem.proxy_to_lua(lua, bridge)?)?;
                }
                Ok(mluau::Value::Table(tbl))
            },
            _ => Err(mluau::Error::external("Unsupported V8 value type for proxying to Lua")),
        }
    }

    pub(crate) fn proxy_from_v8<'s>(
        scope: &mut v8::HandleScope<'s>, 
        value: v8::Local<'s, v8::Value>,
        plc: &ProxyV8Client,
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
            let sid = plc.atom_list.get(s.to_rust_string_lossy(scope).as_bytes());
            return Ok(Self::String(sid));
        } else if value.is_array() {
            let arr = value.to_object(scope).ok_or("Failed to convert to object")?;
            let length_str = v8::String::new(scope, "length").ok_or("Failed to create length string")?;
            let length_val = arr.get(scope, length_str.into()).ok_or("Failed to get length property")?;
            let length = length_val.to_uint32(scope).ok_or("Failed to convert length to uint32")?.value() as usize;
            let mut elems = Vec::with_capacity(length);
            for i in 0..length {
                let elem = arr.get_index(scope, i as u32).ok_or(format!("Failed to get array element {}", i))?;
                let lua_elem = Self::proxy_from_v8(scope, elem, plc, depth + 1)?;
                elems.push(lua_elem);
            }
            return Ok(Self::Array(elems));
        } else if value.is_array_buffer() {
            let ab = v8::Local::<v8::ArrayBuffer>::try_from(value).map_err(|_| "Failed to convert to ArrayBuffer")?;
            let bs = ab.get_backing_store();
            let Some(data) = bs.data() else {
                return Ok(Self::Buffer(Vec::with_capacity(0)));
            };
            let slice = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, bs.byte_length()) };
            return Ok(Self::Buffer(slice.to_vec()));
        } else if value.is_function() {
            // TODO: Support function proxy directly
            //
            // For now, we just proxy directly to an object ID
            let func = v8::Local::<v8::Function>::try_from(value).map_err(|_| "Failed to convert to function")?;
            let global_func = v8::Global::new(scope, func);
            let func_id = plc.func_registry.add(global_func)
                .ok_or("Failed to register function: too many function references")?;
            return Ok(Self::Function(func_id));
        } else if value.is_promise() {
            let promise = v8::Local::<v8::Promise>::try_from(value).map_err(|_| "Failed to convert to promise")?;
            let global_promise = v8::Global::new(scope, promise);
            let promise_id = plc.promise_registry.add(global_promise)
                .ok_or("Failed to register promise: too many promise references")?;
            return Ok(Self::Promise(promise_id));
        } else if value.is_object() {
            let obj = value.to_object(scope).ok_or("Failed to convert to object")?;
            let global_obj = v8::Global::new(scope, obj);
            let obj_id = plc.obj_registry.add(global_obj)
                .ok_or("Failed to register object: too many object references")?;
            return Ok(Self::Object(obj_id));
        } else {
            return Err("Unsupported V8 value type".into());
        }
    }
}

#[derive(Clone)]
/// The client side state for proxying Lua values
/// 
/// This struct is not thread safe and must be kept on the Lua side
pub(crate) struct ProxyV8Client {
    pub atom_list: StringAtomList,
    pub obj_registry: ObjectRegistry<v8::Global<v8::Object>>,
    pub func_registry: ObjectRegistry<v8::Global<v8::Function>>,
    pub promise_registry: ObjectRegistry<v8::Global<v8::Promise>>,
}

mod asserter {
    use super::ProxiedV8Value;
    //use super::LuaProxyBridge;

    const fn assert_send_const<T: Send>() {}
    const _: () = assert_send_const::<ProxiedV8Value>(); 
    //const _: () = assert_send_const::<LuaProxyBridge>();
}