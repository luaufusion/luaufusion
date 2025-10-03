# LuauFusion

A proxy between Luau (via mluau crate) to v8 (via deno_core + rusty_v8) and other languages.

## Types

There are three main types of proxy types within LuauFusion: primitives, object references and owned values.

- **Primitives** are simple values that are copied directly between Luau and the foreign language. These include:
  - Nil / null 
  - Undefined
  - Boolean
  - Integer 
  - BigInt (a bigint not within the range of a 64-bit signed integer will be converted to a owned string)
  - Number (floating point etc.)
  - Strings (not to be confused with string references, see "Owned Strings" below, js+luau)

In the v8 bridge, these are represented as a ``ProxiedV8Primitive`` enum.

- **Object references** are references to objects that live in the other runtime. These include:
  - Objects (js) / Luau tables (luau)
  - Arrays (js)
  - Functions (js+luau)
  - ArrayBuffers / Buffers (js+luau)
  - JS Promises (js)
  - String references (normal (owned/primitive) strings do exist as well, see "Strings" below, js+luau)

### Strings

Strings are special in that they can be represented as both a primitive and a object reference.

From Luau, a string is always a owned/primitive unless marked as a 'string reference' using ``LangBridge:stringref(string)``. A owned/primitive string is cloned into the target runtime as a 'normal' string and is as such a primitive with lower overhead than a string reference which has to be stored in a object registry and converted to a object/userdata/whatever.

### Operations across the bridge

There are only 3 operations that can be performed across the bridge:

- **Get property**: Get a property from an object reference. This returns a proxied value (primitive or object reference).
- **Call function**: Call a function reference with a list of proxied values (primitives or object references) as arguments. This returns a proxied value (primitive or object reference).
- **Shutdown**: Shutdown the bridge and free all resources.

Using these 3 operations, you can do anything you want across the bridge. For example, to set a property on an Luau table from JS, you could have the Luau layer pass a function to JS that takes a table, key and value as arguments and sets the property on the table. Then you can call that function from JS with the table reference, key and value as arguments.

### Passing multiple values between runtimes

You can pass multiple values between runtimes using function arguments (via callback functions etc.). When passing function arguments to opcalls, the bridge will convert the arguments into a list of proxied values and send those all over.