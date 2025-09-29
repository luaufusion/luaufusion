import { op_structured_clone } from "ext:core/ops";
import { core, primordials } from "ext:core/mod.js";
const {
  TypeError,
  indirectEval,
  ReflectApply,
} = primordials;
const {
  getAsyncContext,
  setAsyncContext,
} = core;
import * as webidl from "ext:deno_webidl/00_webidl.js";

/**
 * Structured Clone
 */
globalThis.structuredClone = (value) => op_structured_clone(value)
globalThis.structuredClone.toString = () => "function() { [native code] }"
Object.freeze(globalThis.structuredClone);

// Timers as well
function checkThis(thisArg) {
  if (thisArg !== null && thisArg !== undefined && thisArg !== globalThis) {
    throw new TypeError("Illegal invocation");
  }
}

/**
 * Call a callback function after a delay.
 */
globalThis.setTimeout = (callback, timeout = 0, ...args) => {
  checkThis(this);
  // If callback is a string, replace it with a function that evals the string on every timeout
  if (typeof callback !== "function") {
    const unboundCallback = webidl.converters.DOMString(callback);
    callback = () => indirectEval(unboundCallback);
  }
  const unboundCallback = callback;
  const asyncContext = getAsyncContext();
  callback = () => {
    const oldContext = getAsyncContext();
    try {
      setAsyncContext(asyncContext);
      ReflectApply(unboundCallback, globalThis, args);
    } finally {
      setAsyncContext(oldContext);
    }
  };
  timeout = webidl.converters.long(timeout);
  return core.queueUserTimer(
    core.getTimerDepth() + 1,
    false,
    timeout,
    callback,
  );
}
globalThis.setTimeout.toString = () => "function() { [native code] }";
Object.freeze(globalThis.setTimeout);

/**
 * Call a callback function after a delay.
 */
globalThis.setInterval = (callback, timeout = 0, ...args) => {
  checkThis(this);
  if (typeof callback !== "function") {
    const unboundCallback = webidl.converters.DOMString(callback);
    callback = () => indirectEval(unboundCallback);
  }
  const unboundCallback = callback;
  const asyncContext = getAsyncContext();
  callback = () => {
    const oldContext = getAsyncContext(asyncContext);
    try {
      setAsyncContext(asyncContext);
      ReflectApply(unboundCallback, globalThis, args);
    } finally {
      setAsyncContext(oldContext);
    }
  };
  timeout = webidl.converters.long(timeout);
  return core.queueUserTimer(
    core.getTimerDepth() + 1,
    true,
    timeout,
    callback,
  );
}
globalThis.setInterval.toString = () => "function() { [native code] }";
Object.freeze(globalThis.setInterval);

/**
 * Clear a timeout or interval.
 */
globalThis.clearTimeout = (id = 0) => {
  checkThis(this);
  id = webidl.converters.long(id);
  core.cancelTimer(id);
}
globalThis.clearTimeout.toString = () => "function() { [native code] }";
Object.freeze(globalThis.clearTimeout);

/**
 * Clear a timeout or interval.
 */
globalThis.clearInterval = (id = 0) => {
  checkThis(this);
  id = webidl.converters.long(id);
  core.cancelTimer(id);
}
globalThis.clearInterval.toString = () => "function() { [native code] }";
Object.freeze(globalThis.clearInterval);