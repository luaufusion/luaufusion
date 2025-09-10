pub mod luau;

use std::{cell::RefCell, collections::{HashMap, HashSet}, rc::Rc, sync::{Arc, Mutex, RwLock}};

use crate::deno::MAX_PROXY_DEPTH;

pub const MAX_INTERN_SIZE: usize = 1024 * 512; // 512 KB

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
/// A list of string atoms to avoid duplicating strings in memory
pub(crate) struct StringAtomList {
    atom_list: Arc<Mutex<(usize, HashSet<Arc<[u8]>>)>>,
}

/// A string atom that references a string in the atom list
///
/// Cheap to clone
#[derive(Clone)]
pub(crate) struct StringAtom {
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

#[derive(Clone)]
/// A list of objects that can be called from another language
/// 
/// This struct maps objects to integer IDs for easy referencing
/// 
/// Use ObjectRegistrySend for thread safe version
pub(crate) struct ObjectRegistry<T: Clone + PartialEq> {
    objs: Rc<RefCell<HashMap<i32, T>>>,   
}

impl<T: Clone + PartialEq> ObjectRegistry<T> {
    /// Creates a new object registry
    pub fn new() -> Self {
        Self {
            objs: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// Registers a new object and returns its ID
    pub fn add(&self, obj: T) -> i32 {
        let mut objs = self.objs.borrow_mut();
        for (id, f) in objs.iter() {
            if f == &obj {
                return *id;
            }
        }

        let obj_id = objs.len() as i32 + 1;
        objs.insert(obj_id, obj);
        obj_id
    }

    /// Gets a object by its ID
    pub fn get(&self, obj_id: i32) -> Option<T> {
        let objs = self.objs.borrow();
        objs.get(&obj_id).cloned()
    }

    /// Remove a object by its ID
    pub fn remove(&self, obj_id: i32) {
        let mut objs = self.objs.borrow_mut();
        objs.remove(&obj_id);
    }
}

#[derive(Clone)]
/// A list of objects that can be called from another language
/// 
/// This struct maps objects to integer IDs for easy referencing
/// 
/// Use ObjectRegistrySend for thread safe version
pub(crate) struct ObjectRegistrySend<T: Clone + PartialEq> {
    objs: Arc<RwLock<HashMap<i32, T>>>,   
}

impl<T: Clone + PartialEq> ObjectRegistrySend<T> {
    /// Creates a new object registry
    pub fn new() -> Self {
        Self {
            objs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Registers a new object and returns its ID
    pub fn add(&self, obj: T) -> i32 {
        let mut objs = self.objs.write().unwrap();
        for (id, f) in objs.iter() {
            if f == &obj {
                return *id;
            }
        }

        let obj_id = objs.len() as i32 + 1;
        objs.insert(obj_id, obj);
        obj_id
    }

    /// Gets a object by its ID
    pub fn get(&self, obj_id: i32) -> Option<T> {
        let objs = self.objs.read().unwrap();
        objs.get(&obj_id).cloned()
    }

    /// Remove a object by its ID
    pub fn remove(&self, obj_id: i32) {
        let mut objs = self.objs.write().unwrap();
        objs.remove(&obj_id);
    }
}

mod asserter {
    use super::StringAtomList;
    use super::StringAtom;
    use super::ObjectRegistrySend;

    const fn assert_send_const<T: Send>() {}
    const _: () = assert_send_const::<StringAtomList>(); 
    const _: () = assert_send_const::<StringAtom>();
    const _: () = assert_send_const::<ObjectRegistrySend<()>>();
}
