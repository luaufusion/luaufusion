import { __luadispatch, __luarun, __luaret } from "ext:core/ops";

// Lua object base class
class LuaObject {
    #luaid
    #luatype
    #obj // Keep reference to the original object to avoid GC if not a weak reference

    /**
     * 
     * @param {number} luaid The id of the type
     * @param {number} luatype The type of the type on the registry
     * @param {any} obj The object with finalizer attached to it. Using null here creates a weak reference
     */
    constructor(luaid, luatype, obj) {
        this.#luaid = luaid
        this.#luatype = luatype
        this.#obj = obj
    }

    get luaid() {
        return this.#luaid
    }

    get luatype() {
        return this.#luatype
    }

    get isweak() {
        return !!this.#obj
    }
}

// Represents a Luau string
class LuaString extends LuaObject {
    #length

    /**
     * 
     * @param {number} luaid The id of the type
     * @param {number} luatype The type of the type on the registry
     * @param {any} obj The object with finalizer attached to it. Using null here creates a weak reference
     * @param {number} length The length of the string
     */
    constructor(luaid, luatype, obj, length) {
        super(luaid, luatype, obj)
        this.#length = length
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
        return await NOT_IMPLEMENTED.__luagetstr(this.luaid);
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

// Enum for object registry types
const objRegistryType = {
    String: 0,
    Table: 1,
    Function: 2,
    UserData: 3,
    Buffer: 4,
    Thread: 5
}

/**
 * Creates a Lua object from the provided data. Used internally by the Rust bridge code.
 * data should contain luaid, luatype, obj and length (for strings)
 */
const createLuaObjectFromData = (data) => {
    if(!data) throw new Error("No data provided to createLuaObject");
    if(typeof data.luaid !== "number") throw new Error("Invalid luaid provided to createLuaObject");
    if(typeof data.luatype !== "number") throw new Error("Invalid luatype provided to createLuaObject"); 
    if(data.obj === null || data.obj === undefined) throw new Error("Invalid obj provided to createLuaObject");

    switch (data.luatype) {
        case objRegistryType.String:
            if(typeof data.length !== "number") {
                throw new Error("Invalid length provided to createLuaObject for LuaString");
            }
            return new LuaString(data.luaid, data.luatype, data.obj, data.length);
        case objRegistryType.Table:
            return new LuaTable(data.luaid, data.luatype, data.obj);
        case objRegistryType.Function:
            return new LuaFunction(data.luaid, data.luatype, data.obj);
        case objRegistryType.UserData:
            return new LuaUserData(data.luaid, data.luatype, data.obj);
        case objRegistryType.Buffer:
            return new LuaBuffer(data.luaid, data.luatype, data.obj);
        case objRegistryType.Thread:
            return new LuaThread(data.luaid, data.luatype, data.obj);
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