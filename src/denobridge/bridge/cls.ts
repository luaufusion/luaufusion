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
     * The returned arguments are inserted back into the provided ProxiedValues object.
     * 
     * @param {ProxiedValues} args The arguments to pass
     */
    callSync: (args: ProxiedValues) => Promise<void>;

    /**
     * Calls the function asynchronously/in a new Luau thread
     * 
     * Note: this method is async and non-blocking in the JS side. The main difference of callAsync over callSync is that
     * non-async calls are executed in the main Luau thread, while async calls is executed in a new Luau thread (making
     * non-async potentially faster than async at the expense of blocking yielding).
     * 
     * Currently only supported on functions (tables with __call metamethods are not supported yet).
     * 
     * The returned arguments are inserted back into the provided ProxiedValues object.
     * 
     * @param {ProxiedValues} args The arguments to pass
     */
    callAsync: (args: ProxiedValues) => Promise<void>;

    /**
     * Index the object with the provided key
     * 
     * This method calls the __index metamethod of the object, if it exists.
     * 
     * Currently only supported on tables and userdata.
     * 
     * The returned value is inserted back into the provided ProxiedValues object.
     * 
     * @param {ProxiedValues} key The key to index the object with. This should be the only value in the ProxiedValues object.
     */
    get: (key: ProxiedValues) => Promise<void>;

    /**
     * Same as get, but specifically takes in a string key and returns a {ProxiedValues} containing the value.
     * 
     * @param {string} key The string key to index the object with
     * @returns {ProxiedValues} The value at the provided key
     */
    getPropertyString: (key: string) => Promise<ProxiedValues>;

    /**
     * Same as getPropertyString, but with a integer key.
     */
    getPropertyInteger: (key: number) => Promise<ProxiedValues>;

    /**
     * Same as getPropertyString, but with a number key.
     */
    getPropertyNumber: (key: number) => Promise<ProxiedValues>;
}

interface ProxiedValues {
    /**
     * Creates a new ProxiedValues object from the provided values
     * 
     * The passed values are converted to their Luau counterparts (see the README for luaufusion
     * for more information on the conversion semantics) which may involve storing v8 objects in
     * a 'object registry' in the Javascript side.
     * 
     * @param {any[]} values The values to store in the ProxiedValues object
     * @returns {ProxiedValues} The created ProxiedValues object
     */
    new (values: any[]): ProxiedValues;

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
     * Takes the values out of the ProxiedValues, converting them to their Javascript counterparts
     * as per the conversion semantics (see the README for luaufusion for more information on the conversion semantics).
     * 
     * Can be called after a function call or otherwise places the returned values into the ProxiedValues object
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