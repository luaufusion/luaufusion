use deno_core::v8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
/// An ID for an object in the object registry
pub struct V8ObjectRegistryID {
    objid: i64, // Unique ID representing the object
}

impl Into<i64> for V8ObjectRegistryID {
    fn into(self) -> i64 {
        self.objid
    }
}

impl V8ObjectRegistryID {
    pub fn objid(&self) -> i64 {
        self.objid
    }

    pub fn from_i64(id: i64) -> Self {
        Self {
            objid: id,
        }
    }
}

#[derive(Clone)]
pub struct V8ObjectRegistry {
    // obj registry fields (addV8Object, getV8Object and removeV8Object)
    pub add_v8_object: v8::Global<v8::Function>,
    pub get_v8_object: v8::Global<v8::Function>,
    pub drop_v8_object: v8::Global<v8::Function>,
}

impl V8ObjectRegistry {
    pub(crate) fn new<'s>(scope: &mut v8::PinScope<'s, '_>) -> Self {
        // The createLuaObjectFromData function is stored in globalThis.lua.createLuaObjectFromData
        let (add_v8_object, get_v8_object, remove_v8_object) = {
            // Get globalThis.lua
            let global = scope.get_current_context().global(scope);
            let lua_str = v8::String::new(scope, "lua").unwrap();
            let lua_obj = global.get(scope, lua_str.into()).unwrap();
            //println!("lua_obj: {:?}", lua_obj.to_rust_string_lossy(scope));
            assert!(lua_obj.is_object());
            let lua_obj = lua_obj.to_object(scope).unwrap();
            
            // get addV8Object, getV8Object and removeV8Object from V8ObjectRegistry
            let add_v8_object = {
                let addv8obj_str = v8::String::new(scope, "addV8Object").unwrap();
                let addv8obj = lua_obj.get(scope, addv8obj_str.into()).unwrap();
                assert!(addv8obj.is_function());
                let addv8obj = v8::Local::<v8::Function>::try_from(addv8obj).unwrap();
                addv8obj
            };
            let get_v8_object = {
                let getv8obj_str = v8::String::new(scope, "getV8Object").unwrap();
                let getv8obj = lua_obj.get(scope, getv8obj_str.into()).unwrap();
                assert!(getv8obj.is_function());
                let getv8obj = v8::Local::<v8::Function>::try_from(getv8obj).unwrap();
                getv8obj
            };
            let remove_v8_object = {
                let removev8obj_str = v8::String::new(scope, "dropV8Object").unwrap();
                let removev8obj = lua_obj.get(scope, removev8obj_str.into()).unwrap();
                assert!(removev8obj.is_function());
                let removev8obj = v8::Local::<v8::Function>::try_from(removev8obj).unwrap();
                removev8obj
            };

            (add_v8_object, get_v8_object, remove_v8_object)
        };

        Self {
            add_v8_object: v8::Global::new(scope, add_v8_object),
            get_v8_object: v8::Global::new(scope, get_v8_object), 
            drop_v8_object: v8::Global::new(scope, remove_v8_object),

        }
    }

    /// Adds a value to the V8 object registry and returns its ID
    /// 
    /// Increments the reference count if the object is already present
    pub fn add(&self, scope: &mut v8::PinScope, obj: v8::Local<v8::Value>) -> Result<V8ObjectRegistryID, String> {
        let try_catch = std::pin::pin!(v8::TryCatch::new(scope));
        let try_catch = &mut try_catch.init();
        let undefined = v8::undefined(try_catch);
        let add_v8_object = v8::Local::new(try_catch, &self.add_v8_object);
        // The function is bound, so recv is undefined
        let func = add_v8_object.call(try_catch, undefined.into(), &[obj]);
        match func {
            Some(result) => {
                if result.is_int32() {
                    let id = result.int32_value(try_catch).ok_or("Failed to convert result to i32")?;
                    Ok(V8ObjectRegistryID::from_i64(id as i64))
                } else {
                    Err("Result from addV8Object is not an integer".to_string())
                }
            },
            None => {
                if try_catch.has_caught() {
                    let exception = try_catch.exception().unwrap();
                    let exception_str = exception.to_rust_string_lossy(try_catch);
                    Err(format!("Failed to call addV8Object: {}", exception_str))
                } else {
                    Err("Failed to call addV8Object for unknown reason".to_string())
                }
            },
        }
    }

    /// Gets a value from the V8 object registry by ID. 
    /// 
    /// Does not increment the reference count in the registry.
    /// 
    /// May be undefined if the ID does not exist
    pub fn get<'a, R>(
        &self, 
        scope: &mut v8::PinScope<'a, '_>, 
        id: V8ObjectRegistryID, 
        funct: impl FnOnce(&mut v8::PinScope<'a, '_>, v8::Local<'a, v8::Value>) -> Result<R, String>
    ) -> Result<R, String> {
        let try_catch = std::pin::pin!(v8::TryCatch::new(scope));
        let try_catch = &mut try_catch.init();
        let undefined = v8::undefined(try_catch);
        let get_v8_object = v8::Local::new(try_catch, &self.get_v8_object);
        let id_value = v8::Integer::new(try_catch, id.objid() as i32);
        // The function is bound, so recv is undefined
        let func = get_v8_object.call(try_catch, undefined.into(), &[id_value.into()]);
        match func {
            Some(result) => {
                let result = (funct)(try_catch, result)?;
                Ok(result)
            },
            None => {
                if try_catch.has_caught() {
                    let exception = try_catch.exception().unwrap();
                    let exception_str = exception.to_rust_string_lossy(try_catch);
                    Err(format!("Failed to call getV8Object: {}", exception_str))
                } else {
                    Err("Failed to call getV8Object for unknown reason".to_string())
                }
            },
        }
    }

    /// Removes a value from the V8 object registry by ID
    /// 
    /// Decrements the reference count, removing the object if it reaches 0
    pub fn drop(&self, scope: &mut v8::PinScope, id: V8ObjectRegistryID) -> Result<(), String> {
        let try_catch = std::pin::pin!(v8::TryCatch::new(scope));
        let try_catch = &mut try_catch.init();
        let undefined = v8::undefined(try_catch);
        let remove_v8_object = v8::Local::new(try_catch, &self.drop_v8_object);
        let id_value = v8::Integer::new(try_catch, id.objid() as i32);
        // The function is bound, so recv is undefined
        let func = remove_v8_object.call(try_catch, undefined.into(), &[id_value.into()]);
        match func {
            Some(_) => Ok(()),
            None => {
                if try_catch.has_caught() {
                    let exception = try_catch.exception().unwrap();
                    let exception_str = exception.to_rust_string_lossy(try_catch);
                    Err(format!("Failed to call removeV8Object: {}", exception_str))
                } else {
                    Err("Failed to call removeV8Object for unknown reason".to_string()) 
                }
            }
        }
    }
}