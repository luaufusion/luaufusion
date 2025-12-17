// Copyright 2018-2025 the Deno authors. MIT license.

import { core } from "ext:core/mod.js";
import * as globalInterfaces from "ext:deno_web/03_global_interfaces.js";

function memoizeLazy(f) {
  let v_ = null;
  return () => {
    if (v_ === null) {
      v_ = f();
    }
    return v_;
  };
}

const mainRuntimeGlobalProperties = {
  Window: globalInterfaces.windowConstructorDescriptor,
  self: core.propGetterOnly(() => globalThis),
};

export { mainRuntimeGlobalProperties, memoizeLazy };