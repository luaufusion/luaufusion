use std::collections::HashMap;

use deno_core::{ModuleCodeString, ModuleLoadResponse, ModuleLoader, ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType, RequestedModuleType, ResolutionKind, error::ModuleLoaderError, resolve_import};
use deno_error::JsErrorBox;

/// A module loader that you can pre-load a number of modules into and resolve from. Useful for testing and
/// embedding situations where the filesystem and snapshot systems are not usable or a good fit.
#[derive(Default)]
pub struct FusionModuleLoader {
  map: HashMap<String, ModuleCodeString>,
}

impl FusionModuleLoader {
  /// Create a new [`FusionModuleLoader`] from an `Iterator` of specifiers and code.
  pub fn new(
    from: impl IntoIterator<Item = (String, ModuleCodeString)>,
  ) -> Self {
    Self {
      map: HashMap::from_iter(
        from.into_iter().map(|(url, code)| {
          (url, code.into_cheap_copy().0)
        }),
      ),
    }
  }
}

impl ModuleLoader for FusionModuleLoader {
  fn resolve(
    &self,
    specifier: &str,
    referrer: &str,
    _kind: ResolutionKind,
  ) -> Result<ModuleSpecifier, ModuleLoaderError> {
    resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)
  }

  fn load(
    &self,
    module_specifier: &ModuleSpecifier,
    _maybe_referrer: Option<&ModuleSpecifier>,
    _is_dyn_import: bool,
    requested_module_type: RequestedModuleType,
  ) -> ModuleLoadResponse {
    // TODO: Handle this better?
    let module_type = if requested_module_type == RequestedModuleType::Json {
      ModuleType::Json
    } else {
      ModuleType::JavaScript
    };

    let path = module_specifier.path_segments().unwrap().collect::<Vec<_>>().join("/");
    println!("Loading module: {:?}", path);
    let res = if let Some(code) = self.map.get(path.as_str()) {
      Ok(ModuleSource::new(
        module_type,
        ModuleSourceCode::String(code.try_clone().unwrap()),
        module_specifier,
        None,
      ))
    } else {
      Err(JsErrorBox::generic("Module not found"))
    };
    ModuleLoadResponse::Sync(res)
  }
}

