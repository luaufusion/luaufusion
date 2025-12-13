import { LuaObject, ProxiedValues } from "ext:core/ops";
import { V8ObjectRegistry } from "./objreg.js";

// v8 object registry instance
const v8objreg = new V8ObjectRegistry();

// Enum for opcalls
const opCalls = {
    FunctionCallSync: 1,
    FunctionCallAsync: 2,
    Index: 3,
    Drop: 4,
}

// TODO: Remove this once we've proven the cppgc handles are in fact being collected properly
function gc() {
    for (let i = 0; i < 10; i++) {
        new ArrayBuffer(1024 * 1024 * 10);
    }
}

/**
 * Perform an opcall on this object. For backward compatibility only.
 * 
 * Internal use only. 
 */
const __opcall = async (obj, op, args) => {
    let boundArgs = new ProxiedValues(...args);
    await obj.opcall(boundArgs, op);
    gc()
    return boundArgs.take();
}

/**
 * Request the disposal of this object from Luau. Useful to ensure quicker cleanup of objects
 * if you know you are done with them.
 * 
 * Note 1: values on the registry and *not* reference counted. As such, calling this method will
 * most likely free all references to this object. Any further use of this object will result
 * in errors.
 * Note 2: It is guaranteed that the object's ID will never be reused for another object within the 
 * proxy bridge.
 * 
 * @param {any} obj The object to request disposal of. Must be a Lua object [e.g. has the luaid/luatype symbols]
 */
const requestDisposal = async (obj) => {
    return // no-op
}

/**
 * Calls the function synchronously/not in a new Luau thread
 * 
 * Note: this method is non-blocking in the JS side. The main difference of async over non async is that
 * non-async calls are executed in the main Luau thread, while async calls is executed in a new Luau thread (making
 * non-async potentially faster than async at the expense of blocking yielding).
 * 
 * Currently only supported on functions (tables with __call metamethods are not supported yet).
 * 
 * @param {any} obj The object of the function. Must be a Lua function object [e.g. has the luaid/luatype symbols]
 * @param {boolean} isAsync Whether to call the function asynchronously (in a new Luau thread) or not
 * @param {any} args The arguments to pass
 * @returns {any} The returned data from the function. A single value returned by Luau is automatically
 * converted to a single value internally
 */
const call = async (obj, isAsync, ...args) => {
    let ret = await __opcall(obj, isAsync ? opCalls.FunctionCallAsync : opCalls.FunctionCallSync, args);
    if (Array.isArray(ret) && ret.length <= 1) {
        return ret[0]; // Cast to single value due to lua multivalue things
    }
    return ret;
}

/**
 * Index the object with the provided key
 * 
 * This method calls the __index metamethod of the object, if it exists.
 * 
 * Currently only supported on tables and userdata.
 * 
 * @param {any} key The key to index the object with
 * @returns {any} The value at the provided key
 */
const getproperty = async (obj, key) => {
    let ret = await __opcall(obj, opCalls.Index, [key]);
    if (Array.isArray(ret) && ret.length <= 1) {
        return ret[0]; // Cast to single value due to lua multivalue things
    }
    return ret;
}

/**
 * Helper method to get the type of a lua object
 * 
 * Returns one of "String", "Table", "Function", "UserData", "Buffer", "Thread" or "unknown"
 */
const objtype = (obj) => {
    return obj.type
}

globalThis.lua = {
    V8ObjectRegistry,
    opCalls,
    v8objreg,
    addV8Object: v8objreg.add.bind(v8objreg),
    getV8Object: v8objreg.get.bind(v8objreg),
    removeV8Object: v8objreg.remove.bind(v8objreg),
    requestDisposal,
    call,
    getproperty,
    objtype,
    LuaObject,
    ProxiedValues,
}
Object.freeze(globalThis.lua);