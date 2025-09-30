import { __dropluaobject, __luadispatch, __luarun, __luaret } from "ext:core/ops";

// Enum for object registry types
const objRegistryType = {
    String: 0,
    Table: 1,
    Function: 2,
    UserData: 3,
    Buffer: 4,
    Thread: 5
}

let __objcache = {}
for (let key in objRegistryType) {
    __objcache[objRegistryType[key]] = new Map();
}

// Lua object base class
class LuaObject {
    #luaid
    #luatype

    /**
     * 
     * @param {number} luaid The id of the type
     * @param {number} luatype The type of the type on the registry
     */
    constructor(luaid, luatype) {
        this.#luaid = luaid
        this.#luatype = luatype
    }

    get luaid() {
        return this.#luaid
    }

    get luatype() {
        return this.#luatype
    }

    /**
     * Request the disposal of this object from Luau. Useful to ensure quicker cleanup of objects
     * if you know you are done with them.
     */
    requestDisposal() {
        // Request drop from Luau side
        __dropluaobject(this.#luatype, this.#luaid);
        // Remove from cache (if cached)
        if (__objcache[this.#luatype]) {
            __objcache[this.#luatype].delete(this.#luaid);
        }
    }
}

// Represents a Luau string
class LuaString extends LuaObject {
    #length
    #stringCache = null // Cache the string once read

    /**
     * 
     * @param {number} luaid The id of the type
     * @param {number} luatype The type of the type on the registry
     * @param {number} length The length of the string
     * @param {string|null} stringCache Optional cache of the string if already read
     */
    constructor(luaid, luatype, length, stringCache) {
        super(luaid, luatype)
        this.#length = length
        this.#stringCache = stringCache
    }

    /**
     * Returns the length of the string
     */
    get length() {
        return this.#length
    }

    /**
     * Reads a string fully to a Javascript. May request multiple chunks from the string from Luau.
     * 
     * This method may error if there was an issue reading the string or if the Luau string isn't UTF-8.
     * 
     * String length is always known, but content needs to be loaded in
     *
     * @returns {string} The contents of the string from Luau
     */
    async read() {
        if(this.#stringCache !== null) return this.#stringCache; // Return cached value if already read
        let s = await NOT_IMPLEMENTED.__luagetstr(this.luaid);
        this.#stringCache = s;
        return s;
    }

    __withNewObj(obj) {
        return new LuaString(this.luaid, this.luatype, obj, this.#length, this.#stringCache);
    }
}

class LuaBuffer extends LuaObject {} // TODO: Implement methods

// Represents a Luau function
class LuaFunction extends LuaObject {
    /**
     * 
     * @param {any} args The arguments to pass
     * @returns {any} The returned data from the function. A single value returned by Luau is automatically
     * converted to a single value internally
     */
    async call(args) {
        console.log("Calling Lua function with ID", this.luaid);
        let runId = __luadispatch(this.luaid, args);
        await __luarun(runId);
        let ret = __luaret(runId);
        if (Array.isArray(ret) && ret.length <= 1) {
            return ret[0]; // Cast to single value due to lua multivalue things
        }
        return ret;
    }
}

class LuaTable extends LuaObject {
    async get(key) {
        let runId = NOT_IMPLEMENTED.__luaobjbind(this.luaid, key, null);
        await NOT_IMPLEMENTED.__luaobjget(runId);
        return NOT_IMPLEMENTED.__luaobjret(runId);
    }

    async set(key, value) {
        let runId = NOT_IMPLEMENTED.__luaobjbind(this.luaid, key, value);
        await NOT_IMPLEMENTED.__luaobjset(runId);
        return NOT_IMPLEMENTED.__luaobjret(runId);
    }

    async keys() {
        let runId = NOT_IMPLEMENTED.__luaobjbind(this.luaid, null, null);
        await NOT_IMPLEMENTED.__luaobjkeys(runId);
        return NOT_IMPLEMENTED.__luaobjret(runId);
    }

    async clear() {
        await NOT_IMPLEMENTED.__luaobjclear(this.luaid);
    }
}

class LuaThread extends LuaObject {} // TODO: Implement methods

class LuaUserData extends LuaObject {} // TODO: Implement methods

let luaObjectCache = new WeakMap(); // Cache of LuaObject instances by luaid to avoid duplicates

/**
 * Creates a Lua object from the provided data. Used internally by the Rust bridge code.
 * data should contain luaid, luatype, obj and length (for strings)
 */
const createLuaObjectFromData = (data) => {
    if(!data) throw new Error("No data provided to createLuaObject");
    if(typeof data.luaid !== "bigint") throw new Error("Invalid luaid provided to createLuaObject");
    if(typeof data.luatype !== "number") throw new Error("Invalid luatype provided to createLuaObject"); 

    // Check cache first

    switch (data.luatype) {
        case objRegistryType.String:
            if(typeof data.length !== "number") {
                throw new Error("Invalid length provided to createLuaObject for LuaString");
            }
            if (__objcache[data.luatype].has(data.luaid)) {
                return __objcache[data.luatype].get(data.luaid);
            }
            let s = new LuaString(data.luaid, data.luatype, data.length);
            __objcache[data.luatype].set(data.luaid, s);
            return s;
        case objRegistryType.Table:
            if (__objcache[data.luatype].has(data.luaid)) {
                return __objcache[data.luatype].get(data.luaid);
            }
            let t = new LuaTable(data.luaid, data.luatype);
            __objcache[data.luatype].set(data.luaid, t);
            return t;
        case objRegistryType.Function:
            if (__objcache[data.luatype].has(data.luaid)) {
                return __objcache[data.luatype].get(data.luaid);
            }
            let f = new LuaFunction(data.luaid, data.luatype);
            __objcache[data.luatype].set(data.luaid, f);
            return f
        case objRegistryType.UserData:
            if (__objcache[data.luatype].has(data.luaid)) {
                return __objcache[data.luatype].get(data.luaid);
            }
            let u = new LuaUserData(data.luaid, data.luatype);
            __objcache[data.luatype].set(data.luaid, u);
            return u;
        case objRegistryType.Buffer:
            if (__objcache[data.luatype].has(data.luaid)) {
                return __objcache[data.luatype].get(data.luaid);
            }
            let b = new LuaBuffer(data.luaid, data.luatype);
            __objcache[data.luatype].set(data.luaid, b);
            return b
        case objRegistryType.Thread:
            if (__objcache[data.luatype].has(data.luaid)) {
                return __objcache[data.luatype].get(data.luaid);
            }
            let th = new LuaThread(data.luaid, data.luatype);
            __objcache[data.luatype].set(data.luaid, th);
            return th
        default:
            throw new Error(`Unknown luatype provided to createLuaObject: ${data.luatype}`);
    }
}

globalThis.lua = {
    LuaObject,
    LuaString,
    LuaBuffer,
    LuaFunction,
    LuaTable,
    LuaThread,
    LuaUserData,
    objRegistryType,
    createLuaObjectFromData
}