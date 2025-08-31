import * as webidl from 'ext:deno_webidl/00_webidl.js';

const nonEnumerable = (value) => {
    return {
        'value': value,
        writable: true,
        enumerable: false,
        configurable: true
    }
}
const applyToGlobal = (properties) => Object.defineProperties(globalThis, properties);

applyToGlobal({
    [webidl.brand]: nonEnumerable(webidl.brand),
});