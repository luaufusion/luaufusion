import * as url from 'ext:deno_url/00_url.js';
import * as urlPattern from 'ext:deno_url/01_urlpattern.js';

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
    URL: nonEnumerable(url.URL),
    URLPattern: nonEnumerable(urlPattern.URLPattern),
    URLSearchParams: nonEnumerable(url.URLSearchParams),
});