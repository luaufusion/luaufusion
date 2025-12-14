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
     * The returned arguments are inserted back into the provided ArgBuffer object.
     * 
     * @param {ArgBuffer} args The arguments to pass
     */
    callSync: (args: ArgBuffer) => Promise<void>;

    /**
     * Calls the function asynchronously/in a new Luau thread
     * 
     * Note: this method is async and non-blocking in the JS side. The main difference of callAsync over callSync is that
     * non-async calls are executed in the main Luau thread, while async calls is executed in a new Luau thread (making
     * non-async potentially faster than async at the expense of blocking yielding).
     * 
     * Currently only supported on functions (tables with __call metamethods are not supported yet).
     * 
     * The returned arguments are inserted back into the provided ArgBuffer object.
     * 
     * @param {ArgBuffer} args The arguments to pass
     */
    callAsync: (args: ArgBuffer) => Promise<void>;

    /**
     * Index the object with the provided key
     * 
     * This method calls the __index metamethod of the object, if it exists.
     * 
     * Currently only supported on tables and userdata.
     * 
     * The returned value is inserted back into the provided ArgBuffer object.
     * 
     * @param {ArgBuffer} key The key to index the object with. This should be the only value in the ArgBuffer object.
     */
    get: (key: ArgBuffer) => Promise<void>;

    /**
     * Requests disposal of the Lua object on the Luau side
     * 
     * Note: this method should be used with care. Usually, luaufusion will automatically
     * handle this when the JS object is garbage collected.
     */
}

/**
 * Represents a buffer of arguments passed through the bridge
 */
interface ArgBuffer {
    /**
     * Creates a new ArgBuffer object from the provided values
     * 
     * The passed values are converted to their Luau counterparts (see the README for luaufusion
     * for more information on the conversion semantics) which may involve storing v8 objects in
     * a 'object registry' in the Javascript side.
     * 
     * @param {any[]} values The values to store in the ArgBuffer object
     * @returns {ArgBuffer} The created ArgBuffer object
     */
    new (...values: any[]): ArgBuffer;

    /**
     * Pushes values into the ArgBuffer, converting them to their Luau counterparts (see the README for luaufusion
     * for more information on the conversion semantics).
     * 
     * @param {...any} values The values to push into the ArgBuffer
     */
    push: (...values: any[]) => void;

    /**
     * Pops a value from the end of the ArgBuffer, converting it to its Luau counterpart (see the README for luaufusion
     * for more information on the conversion semantics).
     * 
     * Returns undefined if there are no values to pop (similar to Array.prototype.pop).
     * 
     * @returns {any} The popped value
     */
    pop: () => any;

    /**
     * Returns the length of the proxied values
     * 
     * Errors if the length is greater than 2^32 - 1. Use sizeBigInt() to get the length as a bigint instead in that case.
     * 
     * @returns {number} The length of the proxied values
     */
    size: () => number;
    
    /**
     * Returns the length of the proxied values as a bigint. This will not error for large lengths
     * but may be slower to work with (due to it being a bigint and not a number).
     * 
     * @returns {bigint} The length of the proxied values
     */
    sizeBigInt: () => bigint;

    /**
     * Takes the values out of the ArgBuffer, converting them to their Javascript counterparts
     * as per the conversion semantics (see the README for luaufusion for more information on the conversion semantics).
     * 
     * Can be called after a function call or otherwise places the returned values into the ArgBuffer object
     * to get out the values.
     */
    take: () => any[];
    
    /**
     * Similar to take, but flattens single-valued results into just that value
     * and returns undefined for zero-length results. Still returns an array for
     * multi-valued results (lua multivalues for example).
     */
    extract: () => any | any[] | undefined;
}