import { __luabind, __luarun, __luaret } from "ext:core/ops";

// Enum for opcalls
const opCalls = {
    FunctionCallSync: 1,
    Index: 2,
    StringRead: 3,
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
        let result = await this.__opcall(opCalls.StringRead, []); // TODO: Support reading substrings with args
        let s = result[0];
        if(typeof s !== "string") {
            throw new Error("Failed to read string from Luau: returned value is not a string");
        }
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
    async callSync(args) {
        console.log("Calling Lua function with ID", this.luaid);
        let ret = await this.__opcall(opCalls.FunctionCallSync, args);
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
    return {
        __stringref: string
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
    V8ObjectRegistry,
    objRegistryType,
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