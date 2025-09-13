(function(dataToCapture) {
    let obj = {
        __luaid: dataToCapture.__luaid,
        __luatype: dataToCapture.__luatype,
        __obj: dataToCapture, // Keep reference to the original object to avoid GC
        // TODO: implement buffer methods
    }
    return obj;   
})
