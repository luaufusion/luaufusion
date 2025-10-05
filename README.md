# LuauFusion

A proxy between Luau (via mluau crate) to v8 (via deno_core + rusty_v8) and other languages.

# V8Bridge

## Types

There are three main types of proxy types within LuauFusion: primitives, psuedoprimitives and references.

- **Primitives** are simple values that are immutable, hashable and can be copied directly between Luau and the foreign language. These include:
  - Nil / null 
  - Undefined
  - Boolean
  - Integer 
  - BigInt (a bigint not within the range of a 64-bit signed integer will be converted to a string)
  - String (UTF-8 only, reason: immutable and hashable)

In the v8 bridge, these are represented as a ``ProxiedV8Primitive`` enum.

- **Psuedoprimitives** are more complex values that are still copied between Luau and the foreign language, but are either not hashable or not immutable on either side of the proxy bridge (and as such, send a snapshot of the current data). These include:
  - Number (floating point etc., reason: not hashable, numbers on luau+js side)
  - Vectors (which are vectors in luau and arrays of 3 numbers in js)
  - Byte sequences (which are strings with non-UTF8 characters in luau and Uint8Array's in js)
  - Static Maps (which are maps containing only primitive keys but with arbitrary values, reason: not immutable or hashable, Maps on js side and *frozen* tables on luau side as normal tables will be treated as references)

These are represented as a ``ProxiedV8PsuedoPrimitive`` enum in the v8 bridge.

- **Object references** are references to objects that live in the other runtime. These include:
  - Objects (js) / Luau tables (luau)
  - Arrays (js)
  - Functions (js+luau)
  - ArrayBuffers / Buffers (js+luau)
  - JS Promises (js)

These are internally represented as a ``ProxiedV8Value`` enum in the v8 bridge. In the v8 side, these are objects with 2 symbols for id and type, and in the luau side these are userdata with a metatable.

### Operations across the bridge

There are only 3 operations that can be performed across the bridge:

- **Get property**: Get a property from an object reference. This returns a proxied value (primitive or object reference).
- **Call function**: Call a function reference with a list of proxied values (primitives or object references) as arguments. This returns a proxied value (primitive or object reference).
- **Shutdown**: Shutdown the bridge and free all resources.

Using these 3 operations, you can do anything you want across the bridge. For example, to set a property on an Luau table from JS, you could have the Luau layer pass a function to JS that takes a table, key and value as arguments and sets the property on the table. Then you can call that function from JS with the table reference, key and value as arguments.

### Passing multiple values between runtimes

You can pass multiple values between runtimes using function arguments (via callback functions etc.). When passing function arguments to opcalls, the bridge will convert the arguments into a list of proxied values and send those all over.