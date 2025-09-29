fn main() {
    /*use mlua_scheduler::{taskmgr::NoopHooks, LuaSchedulerAsync, XRc};
    use mluau::IntoLua;
    use tokio_util::sync::CancellationToken;

    use mluau_quickjs_proxy::denobridge::V8IsolateManagerServer;

    println!("Starting V8 isolate manager test");
    let lua = mluau::Lua::new();
    let compiler = mluau::Compiler::new().set_optimization_level(2);
    lua.set_compiler(compiler);

    let manager_i = super::V8IsolateManagerInner::new(&lua, super::MIN_HEAP_LIMIT);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        println!("Creating Lua scheduler and task manager");
        let returns_tracker = mlua_scheduler::taskmgr::ReturnTracker::new();

        let mut wildcard_sender = returns_tracker.track_wildcard_thread();

        tokio::task::spawn_local(async move {
            while let Some((thread, result)) = wildcard_sender.recv().await {
                if let Err(e) = result {
                    eprintln!("Error in thread {e:?}: {:?}", thread.to_pointer());
                }
            }
        });

        let task_mgr = mlua_scheduler::taskmgr::TaskManager::new(&lua, returns_tracker, XRc::new(NoopHooks {})).await.expect("Failed to create task manager");

        lua.globals()
            .set(
                "_PANIC",
                lua.create_scheduler_async_function(|_lua, n: i32| async move {
                    panic!("Panic test: {n}");
                    #[allow(unreachable_code)]
                    Ok(())
                })
                .expect("Failed to create async function"),
            )
            .expect("Failed to set _OS global");

        lua.globals()
            .set(
                "task",
                mlua_scheduler::userdata::task_lib(&lua).expect("Failed to create table"),
            )
            .expect("Failed to set task global");

        lua.sandbox(true).expect("Sandboxed VM"); // Sandbox VM

        let (manager, rx) = V8IsolateManager::new();
        tokio::task::spawn_local(async move {
            let _inner = V8IsolateManager::run(manager_i, rx, CancellationToken::new()).await;
        });

        // Call the v8 function now as a async script
        let lua_code = r#"
local v8 = ...
local result = v8:eval([[
(function() {
async function f(waiter, v8ud, buf) {
    // Allocate a large array to test heap limits
    //let arr = new Array(1e6).fill(0).map((_, i) => i);
    //console.log('Array allocated with length:', arr.length);
    let v = await v8ud.isrunning(v8ud);
    console.log('V8 userdata:', v8ud, v, typeof v);
    console.log('Buffer:', buf, typeof buf.buffer, buf.buffer.byteLength);
    let waited = await waiter();
    return [waited, buf, v8ud]
}

return f;
})()
]], function() print('am here'); return task.wait(1) end, v8, buffer.create(10))
assert(result[3] == v8)
return result[1], result[2], result[3]
"#;

        let func = lua.load(lua_code).into_function().expect("Failed to load Lua code");
        let th = lua.create_thread(func).expect("Failed to create Lua thread");
        
        let mut args = mluau::MultiValue::new();
        args.push_back(manager.into_lua(&lua).expect("Failed to push QuickJS runtime to Lua"));

        let output = task_mgr
            .spawn_thread_and_wait(th, args)
            .await
            .expect("Failed to run Lua thread")
            .expect("Lua thread returned no value")
            .expect("Lua thread returned an error");
        
        println!("Output: {:?}", output);
    });*/
}