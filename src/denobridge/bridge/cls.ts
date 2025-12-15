/**
 * Represents a Lua object passed through the bridge
 */
interface LuaObject {
    /** The unique identifier for this Lua object */
    id: bigint;

    /** The type of the Lua object */
    type: 'table' | 'function' | 'userdata' | 'buffer' | 'thread';

    /**
     * Calls the function synchronously/in main Luau thread
     * 
     * Note: this method is async and non-blocking in the JS side. The main difference of callAsync over callSync is that
     * non-async calls are executed in the main Luau thread, while async calls is executed in a new Luau thread (making
     * non-async potentially faster than async at the expense of blocking yielding).
     * 
     * Currently only supported on functions (tables with __call metamethods are not supported yet).
     * 
     * @param {any[]} args The arguments to pass. If no args are provided, an empty argument list is used.
     * @returns {Promise<any[]>} The returned values from the function call.
     */
    callSync: (args: any[] | undefined) => Promise<any[]>;

    /**
     * Calls the function asynchronously/in a new Luau thread
     * 
     * Note: this method is async and non-blocking in the JS side. The main difference of callAsync over callSync is that
     * non-async calls are executed in the main Luau thread, while async calls is executed in a new Luau thread (making
     * non-async potentially faster than async at the expense of blocking yielding).
     * 
     * Currently only supported on functions (tables with __call metamethods are not supported yet).
     * 
     * @param {any[]} args The arguments to pass. If no args are provided, an empty argument list is used.
     * @returns {Promise<any[]>} The returned values from the function call.
     */
    callAsync: (args: any[] | undefined) => Promise<any[]>;

    /**
     * Index the object with the provided key
     * 
     * This method calls the __index metamethod of the object, if it exists.
     * 
     * Currently only supported on tables and userdata.
     * 
     * @param {any} key The key to index the object with.
     * @returns {Promise<any>} The value at the provided key.
     */
    get: (key: any) => Promise<any>;

    /**
     * Requests disposal of the Lua object on the Luau side
     * 
     * Note: this method should be used with care. Usually, luaufusion will automatically
     * handle this when the JS object is garbage collected.
     */
    requestDispose: () => Promise<void>;
}
