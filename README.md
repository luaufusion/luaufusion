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
  - Static Lists (which are arrays containing arbitrary values, reason: not immutable or hashable, Arrays on js side and *frozen* tables with array metatable on luau side as normal tables will be treated as references)
  - Static Maps (which are maps containing only primitive keys but with arbitrary values, reason: not immutable or hashable, Maps on js side and *frozen* tables without array metatable on luau side as normal tables will be treated as references)

These are represented as a ``ProxiedV8PsuedoPrimitive`` enum in the v8 bridge.

- **Object references** are references to objects that live in the other runtime. These include:
  - Objects (js) / Unfrozen Luau tables (luau)
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

### Pitfalls

1. StaticLists (arrays) are snapshots of the data at the time of passing. If you modify the StaticList on either side, it will not be reflected on the other side. For example, if you send the array `[1,2,3]` from JS to Luau, then modify the array in JS to `[1,2,3,4]`, the Luau side will still see `{1,2,3}`.
2. StaticMaps (maps) are also snapshots of the data at the time of passing. If you modify the StaticMap on either side, it will not be reflected on the other side. For example, if you send a map containing a=1, b=2 from JS to Luau, then modify the map in JS to a=1, b=2, c=3, the Luau side will still see `{a=1, b=2}`.

If you want a mutable array or map, you will want to wrap the object in an object to make it a object reference. As an example:

**Luau**

```lua
local myarr = table.freeze({1,2,3}) 
setmetatable(myarr, array_metatable) -- Make it a StaticList
local obj = { arr = myarr } -- Wrap in an object to make it a reference
myjsfunc:call(obj) -- Pass the object reference to JS
```

**JS**

```js
let map = new Map();
map.set("a", 1);
map.set("b", 2);
let obj = { map: map }; // Wrap in an object to make it a reference
return obj // Assuming this is a js function called from Luau, return the object reference to Luau
```