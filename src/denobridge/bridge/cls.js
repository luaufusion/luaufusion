import { __luabind, __luarun, __luaret } from "ext:core/ops";
import { V8ObjectRegistry } from "./objreg.js";

// Constants for objects in general
const luaidSymbol = Symbol("luaufusion.luaid");
const luatypeSymbol = Symbol("luaufusion.luatype");
// v8 object registry instance
const v8objreg = new V8ObjectRegistry();

// Enum for opcalls
const opCalls = {
    FunctionCallSync: 1,
    FunctionCallAsync: 2,
    Index: 3,
    Drop: 4,
}

// Enum for object registry types
const objRegistryType = {
    Table: 0,
    Function: 1,
    UserData: 2,
    Buffer: 3,
    Thread: 4
}
Object.freeze(objRegistryType);
const objRegistryTypeNames = {};
for (let key in objRegistryType) {
    objRegistryTypeNames[objRegistryType[key]] = key;
}
Object.freeze(objRegistryTypeNames);

/**
 * Perform an opcall on this object
 * 
 * Internal use only. 
 */
const __opcall = async (luaid, op, args) => {
    let runId = __luabind(args);
    await __luarun(runId, luaid, op);
    return __luaret(runId);
}

/**
 * Request the disposal of this object from Luau. Useful to ensure quicker cleanup of objects
 * if you know you are done with them.
 */
const requestDisposal = async (obj) => {
    let luaid = obj[luaidSymbol]
    if (typeof luaid !== "bigint") {
        throw new Error("Invalid obj provided to __opcall");
    }
    await __opcall(luaid, opCalls.Drop, [])
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
    let luaid = obj[luaidSymbol]
    if (typeof luaid !== "bigint") {
        throw new Error("Invalid obj provided to __opcall");
    }
    if (typeof isAsync !== "boolean") {
        throw new Error("Invalid isAsync provided to callSync");
    }
    let ret = await __opcall(luaid, isAsync ? opCalls.FunctionCallAsync : opCalls.FunctionCallSync, args);
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
    let luaid = obj[luaidSymbol]
    if (typeof luaid !== "bigint") {
        throw new Error("Invalid obj provided to __opcall");
    }
    let ret = await __opcall(luaid, opCalls.Index, [key]);
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
    if (obj && typeof obj === "object" && luatypeSymbol in obj) {
        return objRegistryTypeNames[obj[luatypeSymbol]];
    }
    return "unknown";
}

globalThis.lua = {
    V8ObjectRegistry,
    objRegistryType,
    objRegistryTypeNames,
    opCalls,
    v8objreg,
    addV8Object: v8objreg.add.bind(v8objreg),
    getV8Object: v8objreg.get.bind(v8objreg),
    removeV8Object: v8objreg.remove.bind(v8objreg),
    luaidSymbol,
    luatypeSymbol,
    requestDisposal,
    call,
    getproperty,
    objtype
}
Object.freeze(globalThis.lua);