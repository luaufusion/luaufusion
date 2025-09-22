use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

use concurrentlyexec::{ConcurrentExecutor, ConcurrentlyExecute};
use serde::{Deserialize, Serialize};

use crate::luau::bridge::{ProxiedLuaValue, ProxyLuaClient};

pub const MAX_INTERN_SIZE: usize = 1024 * 512; // 512 KB
pub const MAX_OBJECT_REGISTRY_SIZE: usize = 2048; // 2048 objects
pub const MAX_BUFFER_SIZE: usize = 4096; // For now, 4096 bytes max buffer size

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug)]
/// An ID for an object in the object registry
pub struct ObjectRegistryID<T> {
    objid: i64, // Unique ID representing the object
    _marker: std::marker::PhantomData<T>,
}

impl<T> PartialEq for ObjectRegistryID<T> {
    fn eq(&self, other: &Self) -> bool {
        self.objid == other.objid
    }
}
impl<T> Eq for ObjectRegistryID<T> {}

impl<T> PartialOrd for ObjectRegistryID<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.objid.partial_cmp(&other.objid)
    }
}
impl<T> Ord for ObjectRegistryID<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.objid.cmp(&other.objid)
    }
}
impl<T> std::hash::Hash for ObjectRegistryID<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.objid.hash(state);
    }
}
impl<T> Clone for ObjectRegistryID<T> {
    fn clone(&self) -> Self {
        Self {
            objid: self.objid,
            _marker: std::marker::PhantomData,
        }
    }
}
impl<T> Copy for ObjectRegistryID<T> {}

impl<T> serde::Serialize for ObjectRegistryID<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i64(self.objid)
    }
}

impl<'de, T> Deserialize<'de> for ObjectRegistryID<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = i64::deserialize(deserializer)?;
        Ok(Self {
            objid: v,
            _marker: std::marker::PhantomData,
        })
    }
}

impl<T> ObjectRegistryID<T> {
    pub fn objid(&self) -> i64 {
        self.objid
    }

    pub fn from_i64(id: i64) -> Self {
        Self {
            objid: id,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T> std::fmt::Display for ObjectRegistryID<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.objid)
    }
}

impl<T> std::ops::Add<i32> for ObjectRegistryID<T> {
    type Output = Self;

    fn add(self, rhs: i32) -> Self::Output {
        ObjectRegistryID {
            objid: self.objid + rhs as i64,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T> std::ops::AddAssign<i32> for ObjectRegistryID<T> {
    fn add_assign(&mut self, rhs: i32) {
        self.objid += rhs as i64;
    }
}

#[derive(Clone)]
/// A list of objects that can be called from another language
/// 
/// This struct maps objects to integer IDs for easy referencing
/// 
/// Use ObjectRegistrySend for thread safe version
pub struct ObjectRegistry<T: Clone + PartialEq, U> {
    objs: Rc<RefCell<HashMap<ObjectRegistryID<U>, (T, usize)>>>,  

    // If a obj ID is created, then removed, then the length may no longer
    // be reliable for ID generation / may lead to objects being overwritten
    // or no longer existing
    current_id: Rc<RefCell<ObjectRegistryID<U>>>, 
}

impl<T: Clone + PartialEq, U> ObjectRegistry<T, U> {
    /// Creates a new object registry
    pub fn new() -> Self {
        Self {
            objs: Rc::new(RefCell::new(HashMap::new())),
            current_id: Rc::new(RefCell::new(ObjectRegistryID::from_i64(0))),
        }
    }

    /// Registers a new object and returns its ID
    /// 
    /// This will try to reuse existing objects and return their ID if found
    pub fn add(&self, obj: T) -> Option<ObjectRegistryID<U>> {
        let mut objs = self.objs.borrow_mut();
        for (id, (f, count)) in objs.iter_mut() {
            if f == &obj {
                *count += 1;
                return Some(*id);
            }
        }
        let obj_id = {
            let mut id = self.current_id.borrow_mut();
            *id += 1;
            *id
        };
        if objs.len() >= MAX_OBJECT_REGISTRY_SIZE {
            return None;
        }
        objs.insert(obj_id, (obj, 1));
        Some(obj_id)
    }

    /// Gets a object by its ID
    pub fn get(&self, obj_id: ObjectRegistryID<U>) -> Option<T> {
        let objs = self.objs.borrow();
        objs.get(&obj_id).cloned().map(|(obj, _)| obj)
    }

    /// Remove a object by its ID
    pub fn remove(&self, obj_id: ObjectRegistryID<U>) {
        let mut objs = self.objs.borrow_mut();
        if let Some((_, count)) = objs.get_mut(&obj_id) {
            if *count > 1 {
                // Don't delete the object if it's still referenced
                *count -= 1;
                return;
            }
        }
        objs.remove(&obj_id);
    }
}

#[allow(async_fn_in_trait)]
pub trait ProxyBridge: Clone {
    type ValueType: Send + Sync + Serialize + for<'de> Deserialize<'de> + 'static;
    type ConcurrentlyExecuteClient: ConcurrentlyExecute;

    /// Returns the executor for concurrently executing tasks on a separate process
    fn get_executor(&self) -> Arc<ConcurrentExecutor<Self::ConcurrentlyExecuteClient>>;

    /// Convert a value from the foreign language to a proxied value
    fn to_source_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, plc: &ProxyLuaClient, depth: usize) -> Result<mluau::Value, Error>;

    /// Evaluates code (string) from the source Luau to the foreign language
    async fn eval_from_source(&self, code: String, args: Vec<ProxiedLuaValue>) -> Result<Self::ValueType, Error>;
}
