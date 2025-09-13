pub mod concurrency;

use std::{cell::RefCell, collections::{HashMap, HashSet}, rc::Rc, sync::{Arc, Mutex}};

use crate::luau::bridge::{ProxiedLuaValue, ProxyLuaClient};

pub const MAX_INTERN_SIZE: usize = 1024 * 512; // 512 KB
pub const MAX_OBJECT_REGISTRY_SIZE: usize = 1024; // 1024 objects

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
/// A list of string atoms to avoid duplicating strings in memory
pub struct StringAtomList {
    atom_list: Arc<Mutex<(usize, HashSet<Arc<[u8]>>)>>,
}

/// A string atom that references a string in the atom list
///
/// Cheap to clone
#[derive(Clone)]
pub struct StringAtom {
    s: Arc<[u8]>,
}

impl StringAtom {
    pub fn to_bytes(self) -> Arc<[u8]> {
        self.s
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.s
    }
}

impl StringAtomList {
    pub fn new() -> Self {
        Self {
            atom_list: Arc::new(Mutex::default()),
        }
    }

    /// Gets a string atom for the given mluau string
    pub fn get(&self, s: &[u8]) -> StringAtom {
        let mut atom_list = self.atom_list.lock().unwrap();

        if !atom_list.1.contains(s) {
            if atom_list.0 + s.len() > MAX_INTERN_SIZE {
                return StringAtom { s: s.to_vec().into() };
            }

            atom_list.1.insert(s.to_vec().into());
            atom_list.0 += s.len();
        }
        let s = atom_list.1.get(s).unwrap().clone();
        StringAtom { s }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
/// An ID for an object in the object registry
pub struct ObjectRegistryID(i64);

impl ObjectRegistryID {
    pub fn get(&self) -> i64 {
        self.0
    }

    pub fn from_i64(id: i64) -> Self {
        Self(id)
    }
}

impl std::fmt::Display for ObjectRegistryID {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::ops::Add<i32> for ObjectRegistryID {
    type Output = Self;

    fn add(self, rhs: i32) -> Self::Output {
        ObjectRegistryID(self.0 + rhs as i64)
    }
}

impl std::ops::AddAssign<i32> for ObjectRegistryID {
    fn add_assign(&mut self, rhs: i32) {
        self.0 += rhs as i64;
    }
}

#[derive(Clone)]
/// A list of objects that can be called from another language
/// 
/// This struct maps objects to integer IDs for easy referencing
/// 
/// Use ObjectRegistrySend for thread safe version
pub struct ObjectRegistry<T: Clone + PartialEq> {
    objs: Rc<RefCell<HashMap<ObjectRegistryID, (T, usize)>>>,  

    // If a obj ID is created, then removed, then the length may no longer
    // be reliable for ID generation / may lead to objects being overwritten
    // or no longer existing
    current_id: Rc<RefCell<ObjectRegistryID>>, 
}

impl<T: Clone + PartialEq> ObjectRegistry<T> {
    /// Creates a new object registry
    pub fn new() -> Self {
        Self {
            objs: Rc::new(RefCell::new(HashMap::new())),
            current_id: Rc::new(RefCell::new(ObjectRegistryID(0))),
        }
    }

    /// Registers a new object and returns its ID
    /// 
    /// This will try to reuse existing objects and return their ID if found
    pub fn add(&self, obj: T) -> Option<ObjectRegistryID> {
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
    pub fn get(&self, obj_id: ObjectRegistryID) -> Option<T> {
        let objs = self.objs.borrow();
        objs.get(&obj_id).cloned().map(|(obj, _)| obj)
    }

    /// Remove a object by its ID
    pub fn remove(&self, obj_id: ObjectRegistryID) {
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
pub trait ProxyBridge: Send + Sync + Clone {
    type ValueType: Send + Sync;

    /// Convert a value from the foreign language to a proxied value
    fn to_source_lua_value(&self, lua: &mluau::Lua, value: Self::ValueType, plc: &ProxyLuaClient, depth: usize) -> Result<mluau::Value, Error>;

    /// Evaluates code (string) from the source Luau to the foreign language
    async fn eval_from_source(&self, code: &str, args: Vec<ProxiedLuaValue>) -> Result<Self::ValueType, Error>;
}

mod asserter {
    use super::StringAtomList;
    use super::StringAtom;

    const fn assert_send_const<T: Send>() {}
    const _: () = assert_send_const::<StringAtomList>(); 
    const _: () = assert_send_const::<StringAtom>();
}
