(function(dataToCapture) {
    let obj = {
        __threadId: dataToCapture.__threadId,
        __obj: dataToCapture, // Keep reference to the original object to avoid GC
        // TODO: implement thread methods
    }
    return obj;   
})
