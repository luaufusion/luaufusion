import { EventBridge } from "ext:core/ops";

// TODO: Remove this once we've proven the cppgc handles are in fact being collected properly
function gc() {
    for (let i = 0; i < 10; i++) {
        new ArrayBuffer(1024 * 1024 * 10);
    }
}

globalThis.lua = {
    EventBridge,
}
