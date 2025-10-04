import { __luabind, __luarun, __luaret } from "ext:core/ops";

// Enum for opcalls
const opCalls = {
    FunctionCallSync: 1,
    FunctionCallAsync: 2,
    Index: 3,
    Drop: 4,
}

// Enum for object registry types
const objRegistryType = {
    String: 0,
    Table: 1,
    Function: 2,
    UserData: 3,
    Buffer: 4,
    Thread: 5
}
Object.freeze(objRegistryType);
const objRegistryTypeNames = {};
for (let key in objRegistryType) {
    objRegistryTypeNames[objRegistryType[key]] = key;
}
Object.freeze(objRegistryTypeNames);

// Cache of Lua objects to avoid creating multiple JS wrappers for the same Lua object
// Map of luatype -> Map of luaid -> LuaObject
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

    get typename() {
        return objRegistryTypeNames[this.#luatype] || "Unknown"
    }

    /**
     * Perform an opcall on this object
     * 
     * Internal use only. 
     */
    async __opcall(op, args) {
        let runId = __luabind(args);
        await __luarun(runId, this.#luaid, op);
        return __luaret(runId);
    }

    /**
     * Request the disposal of this object from Luau. Useful to ensure quicker cleanup of objects
     * if you know you are done with them.
     */
    async requestDisposal() {
        // Remove from cache (if cached)
        if (__objcache[this.#luatype]) {
            __objcache[this.#luatype].delete(this.#luaid);
        }

        await this.__opcall(opCalls.Drop, [])
    }

    toString() {
        return `[LuaObject type=${this.#luatype} id=${this.#luaid}, typename=${this.typename}]`
    }
}

/**
 * A Lua object that can be called like a function etc.
 */
class LuaCallableObject extends LuaObject {
    /**
     * Calls the function synchronously/not in a new Luau thread
     * 
     * Note: this method is non-blocking in the JS side. The main benefit of callSync over callAsync is that
     * callSync is executed in the main Luau thread, while callAsync is executed in a new Luau thread (making
     * callSync potentially faster than callAsync at the expense of blocking yielding).
     * @param {any} args The arguments to pass
     * @returns {any} The returned data from the function. A single value returned by Luau is automatically
     * converted to a single value internally
     */
    async callSync(...args) {
        console.log("Calling Lua function with ID", this.luaid);
        let ret = await this.__opcall(opCalls.FunctionCallSync, args);
        if (Array.isArray(ret) && ret.length <= 1) {
            return ret[0]; // Cast to single value due to lua multivalue things
        }
        return ret;
    }

    /**
     * Calls the function asynchronously/in a new Luau thread. This is useful if the function being called may yield
     * but is slower than callSync due to the overhead of creating a new Luau thread and running the function using the
     * scheduler.
     * 
     * @param {any} args The arguments to pass
     * @returns {any} The returned data from the function. A single value returned by Luau is automatically
     * converted to a single value internally
     */
    async callAsync(...args) {
        console.log("Calling Lua function with ID", this.luaid);
        let ret = await this.__opcall(opCalls.FunctionCallAsync, args);
        if (Array.isArray(ret) && ret.length <= 1) {
            return ret[0]; // Cast to single value due to lua multivalue things
        }
        return ret;
    }
}

/**
 * A Lua object that can be indexed like a table/userdata
 * 
 * Currently only supports tables/userdata.
 */
class LuaIndexableObject extends LuaObject {
    /**
     * Index the object with the provided key
     * 
     * This method calls the __index metamethod of the object, if it exists.
     * 
     * @param {any} key The key to index the object with
     * @returns {any} The value at the provided key
     */
    async get(key) {
        console.log("Calling Lua function with ID", this.luaid);
        let ret = await this.__opcall(opCalls.Index, [key]);
        if (Array.isArray(ret) && ret.length <= 1) {
            return ret[0]; // Cast to single value due to lua multivalue things
        }
    }
}

class LuaLenghtableObject extends LuaObject {
    #length

    /**
     * 
     * @param {number} luaid The id of the type
     * @param {number} luatype The type of the type on the registry
     * @param {number} length The length of the string
     */
    constructor(luaid, luatype, length) {
        super(luaid, luatype)
        this.#length = length
    }

    /**
     * Returns the length of the string
     */
    get length() {
        return this.#length
    }
}

class LuaString extends LuaLenghtableObject {}

class LuaBuffer extends LuaObject {} 

class LuaFunction extends LuaCallableObject {}

class LuaTable extends LuaIndexableObject {}

class LuaThread extends LuaObject {}

class LuaUserData extends LuaIndexableObject {}

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

/** v8-side object registry 
 * 
 * Given a v8 object, returns a ID for it, or creates a new ID if it doesn't exist
*/
class V8ObjectRegistry {
    // Reverse mapping of object to id for quick lookup
    #objToId = new Map();
    // Objects stored as [id] = {obj=obj, refcount=refcount}
    #idToObj = new Map();
    #lastid = 1;

    add(obj) {
        if(this.#objToId.has(obj)) {
            let id = this.#objToId.get(obj);
            let entry = this.#idToObj.get(id);
            entry.refcount++;
            return id;
        }

        if(this.#lastid >= Number.MAX_SAFE_INTEGER) {
            throw new Error("Object registry full");
        }

        this.#lastid++;
        let id = this.#lastid;
        this.#objToId.set(obj, id);
        this.#idToObj.set(id, {obj: obj, refcount: 1});
        return id;
    }

    get(id) {
        let entry = this.#idToObj.get(id);
        if(entry) {
            return entry.obj;
        }
        return null;
    }

    remove(id) {
        let entry = this.#idToObj.get(id);
        if(entry) {
            entry.refcount--;
            if(entry.refcount <= 0) {
                this.#idToObj.delete(id);
                this.#objToId.delete(entry.obj);
            }
        }
    }
}

const v8objreg = new V8ObjectRegistry();

const __stringref = Symbol("luaufusion.stringref");
const createStringRef = (string) => {
    if(typeof string !== "string") throw new Error("Invalid string provided to createStringRef");
    return Object.freeze({
        [__stringref]: string
    })
}

globalThis.lua = {
    LuaObject,
    LuaString,
    LuaBuffer,
    LuaFunction,
    LuaTable,
    LuaThread,
    LuaUserData,
    V8ObjectRegistry,
    objRegistryType,
    objRegistryTypeNames,
    opCalls,
    createLuaObjectFromData,
    createStringRef,
    __stringref,
    v8objreg,
    addV8Object: v8objreg.add.bind(v8objreg),
    getV8Object: v8objreg.get.bind(v8objreg),
    removeV8Object: v8objreg.remove.bind(v8objreg),
}
Object.freeze(globalThis.lua);