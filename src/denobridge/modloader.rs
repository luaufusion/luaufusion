use std::collections::HashMap;

use deno_core::{ModuleCodeString, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader, ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType, RequestedModuleType, ResolutionKind, error::ModuleLoaderError, resolve_import};
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
    let res = resolve_import(specifier, referrer).map_err(|e| {
      JsErrorBox::from_err(e)
    });

    //println!("Resolving module: {:?} from {:?} -> {:?}", specifier, referrer, res);

    res
  }

  fn load(
    &self,
    module_specifier: &ModuleSpecifier,
    _maybe_referrer: Option<&ModuleLoadReferrer>,
    options: ModuleLoadOptions,
  ) -> ModuleLoadResponse {
    // TODO: Handle this better?

    let path = module_specifier.path_segments().unwrap().collect::<Vec<_>>().join("/");

    let module_type = if options.requested_module_type == RequestedModuleType::Json {
      ModuleType::Json
    } else {
      let ext = path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_lowercase();
      if ext == "json" {
        ModuleType::Json
      } else {
        ModuleType::JavaScript
      }
    };

    //println!("Loading module: {:?} {module_type} {requested_module_type}", path);
    let res = if let Some(code) = self.map.get(path.as_str()) {
      Ok(ModuleSource::new(
        module_type.clone(),
        ModuleSourceCode::String(code.try_clone().unwrap()), // SAFETY: This should never panic due to the into_cheap_copy above?
        module_specifier,
        None,
      ))
    } else {
      Err(JsErrorBox::generic("Module not found"))
    };
    //println!("Loaded module: {:?} {res:?}", path);
    ModuleLoadResponse::Sync(res)
  }
}

