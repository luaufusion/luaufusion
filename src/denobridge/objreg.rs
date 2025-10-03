use deno_core::v8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
/// An ID for an object in the object registry
pub struct V8ObjectRegistryID {
    objid: i64, // Unique ID representing the object
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
    pub remove_v8_object: v8::Global<v8::Function>,
}

impl V8ObjectRegistry {
    /// Adds a value to the V8 object registry and returns its ID
    pub fn add(&self, scope: &mut v8::HandleScope, obj: v8::Local<v8::Value>) -> Result<V8ObjectRegistryID, String> {
        let try_catch = &mut v8::TryCatch::new(scope);
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

    /// Gets a value from the V8 object registry by ID
    /// 
    /// May be undefined if the ID does not exist
    pub fn get<'a, R>(
        &self, 
        scope: &mut v8::HandleScope<'a>, 
        id: V8ObjectRegistryID, 
        funct: impl FnOnce(&mut v8::HandleScope<'a>, v8::Local<'a, v8::Value>) -> Result<R, String>
    ) -> Result<R, String> {
        let try_catch = &mut v8::TryCatch::new(scope);
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
    pub fn remove(&self, scope: &mut v8::HandleScope, id: V8ObjectRegistryID) -> Result<(), String> {
        let try_catch = &mut v8::TryCatch::new(scope);
        let undefined = v8::undefined(try_catch);
        let remove_v8_object = v8::Local::new(try_catch, &self.remove_v8_object);
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