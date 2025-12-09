pub const OBJ_REGISTRY_LUAU : &'static str = include_str!("objreg.luau");

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
/// An ID for an object in the object registry
pub struct PLuauObjectRegistryID {
    objid: i64, // Unique ID representing the object
}

impl PLuauObjectRegistryID {
    pub fn objid(&self) -> i64 {
        self.objid
    }

    pub fn from_i64(id: i64) -> Self {
        Self {
            objid: id,
        }
    }
}

impl std::fmt::Display for PLuauObjectRegistryID {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.objid)
    }
}

#[derive(Debug, Clone)]
pub struct PObjRegistryLuau {
    objreg: mluau::Table,
    add: mluau::Function,
    get: mluau::Function,
    remove: mluau::Function,
}

impl PObjRegistryLuau {
    /// Creates a new ObjRegistryLuau from a Lua state
    pub fn new(lua: &mluau::Lua) -> Result<Self, mluau::Error> {
        let objreg_tab: mluau::Table = lua.load(OBJ_REGISTRY_LUAU).eval()?;
        let objreg: mluau::Table = objreg_tab.get("objreg")?;
        let add: mluau::Function = objreg_tab.get("add")?;
        let get: mluau::Function = objreg_tab.get("get")?;
        let remove: mluau::Function = objreg_tab.get("remove")?;
        Ok(Self {
            objreg,
            add,
            get,
            remove,
        })
    }

    /// Adds the obj ``obj`` to the registry and returns its ID
    /// 
    /// May reuse IDs corresponding to the same object if it was already added
    pub fn add(&self, obj: mluau::Value) -> Result<PLuauObjectRegistryID, mluau::Error> {
        let id: i64 = self.add.call(obj)?;
        Ok(PLuauObjectRegistryID::from_i64(id))
    }

    /// Calls get on the registry
    pub fn get(&self, id: PLuauObjectRegistryID) -> Result<mluau::Value, mluau::Error> {
        let val: mluau::Value = self.get.call(id.objid())?;
        Ok(val)
    }

    /// Calls remove on the registry
    pub fn remove(&self, id: PLuauObjectRegistryID) -> Result<(), mluau::Error> {
        self.remove.call::<()>(id.objid())?;
        Ok(())
    }

    /// Returns the underlying table representing the object registry
    pub fn underlying_table(&self) -> &mluau::Table {
        &self.objreg
    }
}