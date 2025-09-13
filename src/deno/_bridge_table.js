(function(dataToCapture) {
    let obj = {
        __objId: dataToCapture.__objId,
        __obj: dataToCapture, // Keep reference to the original object to avoid GC
        get: async function(key) {
            let objId = dataToCapture.__objId;
            let runId = Deno.core.ops.__luaobjbind(objId, key, null);
            await Deno.core.ops.__luaobjget(runId);
            return Deno.core.ops.__luaobjret(runId);
        },
        set: async function(key, value) {
            let objId = dataToCapture.__objId;
            let runId = Deno.core.ops.__luaobjbind(objId, key, value);
            await Deno.core.ops.__luaobjset(runId);
            return Deno.core.ops.__luaobjret(runId);
        },
        keys: async function() {
            let objId = dataToCapture.__objId;
            let runId = Deno.core.ops.__luaobjbind(objId, null, null);
            await Deno.core.ops.__luaobjkeys(runId);
            return Deno.core.ops.__luaobjret(runId);
        }
    }
    return obj;   
})
