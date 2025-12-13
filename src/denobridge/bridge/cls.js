import { LuaObject, ProxiedValues } from "ext:core/ops";
import { V8ObjectRegistry } from "./objreg.js";

// v8 object registry instance
const v8objreg = new V8ObjectRegistry();

// TODO: Remove this once we've proven the cppgc handles are in fact being collected properly
function gc() {
    for (let i = 0; i < 10; i++) {
        new ArrayBuffer(1024 * 1024 * 10);
    }
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
    let boundArgs = new ProxiedValues(...args);
    if (isAsync) {
        await obj.callAsync(boundArgs);
    } else {
        await obj.callSync(boundArgs);
    }
    gc()
    return boundArgs.extract();
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
    // Try fast-paths for common key types
    if (typeof key === "number") {
        if (Number.isInteger(key)) {
            let ret = await obj.getPropertyInteger(key);
            return ret.extract()
        } else {
            let ret = await obj.getPropertyNumber(key);
            return ret.extract();
        }
    } else if (typeof key === "string") {
        let ret = await obj.getPropertyString(key);
        return ret.extract();
    }

    let keyPV = new ProxiedValues(key);
    await obj.get(keyPV);
    return keyPV.extract();
}

globalThis.lua = {
    V8ObjectRegistry,
    v8objreg,
    addV8Object: v8objreg.add.bind(v8objreg),
    getV8Object: v8objreg.get.bind(v8objreg),
    removeV8Object: v8objreg.remove.bind(v8objreg),
    call,
    getproperty,
    LuaObject,
    ProxiedValues,
}
Object.freeze(globalThis.lua);