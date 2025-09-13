(function(dataToCapture) {
    let func = async function(...args) {
        let funcId = dataToCapture.__luaid;
        console.log("Calling Lua function with ID", funcId);
        let runId = Deno.core.ops.__luadispatch(funcId, args);
        await Deno.core.ops.__luarun(runId);
        let ret = Deno.core.ops.__luaresult(runId);
        if (Array.isArray(ret) && ret.length <= 1) {
            return ret[0]; // Cast to single value due to lua multivalue things
        }
        return ret;
    };
    func.__luaid = dataToCapture.__luaid;
    func.__luatype = dataToCapture.__luatype;
    func.__obj = dataToCapture; // Keep reference to the original object to avoid GC
    return func;   
})