pub const OBJ_REGISTRY_LUAU : &'static str = include_str!("objreg.luau");

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
/// An ID for an object in the object registry
pub struct LuauObjectRegistryID {
    objid: i64, // Unique ID representing the object
}

impl LuauObjectRegistryID {
    pub fn objid(&self) -> i64 {
        self.objid
    }

    pub fn from_i64(id: i64) -> Self {
        Self {
            objid: id,
        }
    }
}

impl std::fmt::Display for LuauObjectRegistryID {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.objid)
    }
}

#[derive(Debug, Clone)]
pub struct ObjRegistryLuau {
    objreg: mluau::Table,
    add: mluau::Function,
    get: mluau::Function,
    drop: mluau::Function,
}

impl ObjRegistryLuau {
    /// Creates a new ObjRegistryLuau from a Lua state
    pub fn new(lua: &mluau::Lua) -> Result<Self, mluau::Error> {
        let objreg_tab: mluau::Table = lua.load(OBJ_REGISTRY_LUAU).eval()?;
        let objreg: mluau::Table = objreg_tab.get("objreg")?;
        let add: mluau::Function = objreg_tab.get("add")?;
        let get: mluau::Function = objreg_tab.get("get")?;
        let drop: mluau::Function = objreg_tab.get("drop")?;
        Ok(Self {
            objreg,
            add,
            get,
            drop,
        })
    }

    /// Adds the obj ``obj`` to the registry and returns its ID
    /// 
    /// May reuse IDs corresponding to the same object if it was already added
    /// 
    /// Increments refcount of the object
    pub fn add(&self, obj: mluau::Value) -> Result<LuauObjectRegistryID, mluau::Error> {
        let id: i64 = self.add.call(obj)?;
        Ok(LuauObjectRegistryID::from_i64(id))
    }

    /// Calls get on the registry
    /// 
    /// Does not modify refcount
    pub fn get(&self, id: LuauObjectRegistryID) -> Result<mluau::Value, mluau::Error> {
        let val: mluau::Value = self.get.call(id.objid())?;
        Ok(val)
    }

    /// Calls drop on the registry
    /// 
    /// Decrements refcount of the object, removing it from the registry if refcount reaches 0
    pub fn drop(&self, id: LuauObjectRegistryID) -> Result<(), mluau::Error> {
        self.drop.call::<()>(id.objid())?;
        Ok(())
    }

    /// Returns the underlying table representing the object registry
    pub fn underlying_table(&self) -> &mluau::Table {
        &self.objreg
    }
}