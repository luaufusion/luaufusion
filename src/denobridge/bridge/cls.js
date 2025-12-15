import { LuaObject } from "ext:core/ops";
import { V8ObjectRegistry } from "./objreg.js";

// v8 object registry instance
const v8objreg = new V8ObjectRegistry();

// TODO: Remove this once we've proven the cppgc handles are in fact being collected properly
function gc() {
    for (let i = 0; i < 10; i++) {
        new ArrayBuffer(1024 * 1024 * 10);
    }
}

globalThis.lua = {
    V8ObjectRegistry,
    v8objreg,
    addV8Object: v8objreg.add.bind(v8objreg),
    getV8Object: v8objreg.get.bind(v8objreg),
    dropV8Object: v8objreg.drop.bind(v8objreg),
    LuaObject,
}
Object.freeze(globalThis.lua);