use std::{collections::HashSet, sync::{Arc, Mutex}};

use mluau::LuaSerdeExt;
use tokio::sync::mpsc::{UnboundedSender, UnboundedReceiver};

pub const MAX_INTERN_SIZE: usize = 1024 * 512; // 512 KB

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
struct StringAtomList {
    atom_list: Arc<Mutex<(usize, HashSet<Arc<[u8]>>)>>,
}

struct StringAtom {
    s: Arc<[u8]>,
}

impl StringAtom {
    fn to_bytes(self) -> Arc<[u8]> {
        self.s
    }

    fn as_bytes(&self) -> &[u8] {
        &self.s
    }
}

impl StringAtomList {
    fn new() -> Self {
        Self {
            atom_list: Arc::new(Mutex::default()),
        }
    }

    fn get(&self, s: mluau::String) -> StringAtom {
        let mut atom_list = self.atom_list.lock().unwrap();

        if !atom_list.1.contains(s.as_bytes().as_ref()) {
            if atom_list.0 + s.as_bytes().len() > MAX_INTERN_SIZE {
                return StringAtom { s: s.as_bytes().to_vec().into() };
            }

            atom_list.1.insert(s.as_bytes().to_vec().into());
            atom_list.0 += s.as_bytes().len();
        }
        let s = atom_list.1.get(s.as_bytes().as_ref()).unwrap().clone();
        StringAtom { s }
    }
}


enum ProxiedLuaValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(StringAtom), // To avoid large amounts of copying, we store strings in a separate atom list
    Table(Vec<(ProxiedLuaValue, ProxiedLuaValue)>),
    Array(Vec<ProxiedLuaValue>),
}

impl ProxiedLuaValue {
    fn from_lua_value(value: mluau::Value, atom_list: &StringAtomList, depth: usize) -> Result<Self, Error> {
        let v = match value {
            mluau::Value::Nil => ProxiedLuaValue::Nil,
            mluau::Value::LightUserData(s) => ProxiedLuaValue::Integer(s.0 as i64),
            mluau::Value::Boolean(b) => ProxiedLuaValue::Boolean(b),
            mluau::Value::Integer(i) => ProxiedLuaValue::Integer(i),
            mluau::Value::Number(n) => ProxiedLuaValue::Number(n),
            mluau::Value::String(s) => ProxiedLuaValue::String(atom_list.get(s)),
            mluau::Value::Table(t) => {
                let Some(lua) = t.weak_lua().try_upgrade() else {
                    return Err("Table's Lua state has been dropped".into());
                };

                if t.metatable() == Some(lua.array_metatable()) {
                    let length = t.raw_len();
                    let mut elements = Vec::with_capacity(length);
                    for i in 0..length {
                        let v = t.raw_get(i + 1)
                            .map_err(|e| format!("Failed to get array element {}: {}", i, e))?;
                        let pv = ProxiedLuaValue::from_lua_value(v, atom_list, depth + 1)
                            .map_err(|e| format!("Failed to convert array element {}: {}", i, e))?;
                        elements.push(pv);
                    }
                    return Ok(ProxiedLuaValue::Array(elements));
                }

                let mut entries = Vec::new();
                t.for_each(|k, v| {
                    let pk = ProxiedLuaValue::from_lua_value(k, atom_list, depth + 1)
                        .map_err(|e| mluau::Error::external(format!("Failed to convert table key: {}", e)))?;
                    let pv = ProxiedLuaValue::from_lua_value(v, atom_list, depth + 1)
                        .map_err(|e| mluau::Error::external(format!("Failed to convert table value: {}", e)))?;
                    entries.push((pk, pv));
                    Ok(())
                })
                .map_err(|e| format!("Failed to iterate table: {}", e))?;

                ProxiedLuaValue::Table(entries)
            }
            _ => ProxiedLuaValue::Nil, // Unsupported types are converted to nil
        };

        Ok(v)
    }

    fn to_lua_value(&self, lua: &mluau::Lua, depth: usize) -> mluau::Result<mluau::Value> {
        match self {
            ProxiedLuaValue::Nil => Ok(mluau::Value::Nil),
            ProxiedLuaValue::Boolean(b) => Ok(mluau::Value::Boolean(*b)),
            ProxiedLuaValue::Integer(i) => Ok(mluau::Value::Integer(*i)),
            ProxiedLuaValue::Number(n) => Ok(mluau::Value::Number(*n)),
            ProxiedLuaValue::String(s) => {
                let s = lua.create_string(s.as_bytes())?;
                Ok(mluau::Value::String(s))
            }
            ProxiedLuaValue::Table(entries) => {
                let table = lua.create_table()?;
                for (k, v) in entries {
                    let lua_k = k.to_lua_value(lua, depth + 1)?;
                    let lua_v = v.to_lua_value(lua, depth + 1)?;
                    table.raw_set(lua_k, lua_v)?;
                }
                Ok(mluau::Value::Table(table))
            }
            ProxiedLuaValue::Array(elements) => {
                let table = lua.create_table_with_capacity(elements.len(), 0)?;
                for (i, v) in elements.iter().enumerate() {
                    let lua_v = v.to_lua_value(lua, depth + 1)?;
                    table.raw_set(i + 1, lua_v)?; // Lua arrays are 1-based
                }
                table.set_metatable(Some(lua.array_metatable()))?;
                Ok(mluau::Value::Table(table))
            }
        }
    }
}

enum ParallelLuauMessage {
    RunScript {
        code: String,
        resp: tokio::sync::oneshot::Sender<Result<ProxiedLuaValue, Error>>,
    }
}

pub struct ParallelLuauThread {
    tx: UnboundedSender<ParallelLuauMessage>,
    th: std::thread::JoinHandle<()>,
}

impl ParallelLuauThread {
    async fn run(
        rx: tokio::sync::mpsc::UnboundedReceiver<Box<dyn FnOnce(&mluau::Lua) + Send + 'static>>,
    ) {}
}

mod asserter {
    use crate::parallelluau::{ParallelLuauThread, ProxiedLuaValue};

    const fn assert_send_const<T: Send>() {}
    const _: () = assert_send_const::<ProxiedLuaValue>(); 
    const _: () = assert_send_const::<ParallelLuauThread>();
}