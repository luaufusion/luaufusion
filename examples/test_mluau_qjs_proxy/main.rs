use std::collections::HashMap;

use concurrentlyexec::{ConcurrentExecutor, ConcurrentExecutorState, ProcessOpts};
use mlua_scheduler::{taskmgr::NoopHooks, LuaSchedulerAsync, XRc};
use mluau::IntoLua;
use mluau_quickjs_proxy::base::ProxyBridge;
use mluau_quickjs_proxy::denobridge::V8IsolateManagerServer;
use mluau_quickjs_proxy::luau::bridge::LangBridge;

const HEAP_LIMIT: usize = 10 * 1024 * 1024; // 10 MB

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let args = std::env::args().collect::<Vec<_>>();
        if args.len() > 1 {
            assert!(args[1] == "child", "Invalid argument");
            println!("[child] Starting child process mode");
            ConcurrentExecutor::<<V8IsolateManagerServer as ProxyBridge>::ConcurrentlyExecuteClient>::run_process_client().await;
            println!("[child] Exiting child process mode");
            return;
        }

        println!("Creating Lua scheduler and task manager");

        let lua = mluau::Lua::new();
        let compiler = mluau::Compiler::new().set_optimization_level(2);
        lua.set_compiler(compiler);

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

        let vfs = HashMap::from([
            ("foo.js".to_string(), r#"
export async function foo(luafunc) { 
    console.log("foo", `${luafunc}`);
    console.log("fooCall", `${await luafunc.callSync()}`);
    console.log("hi");
    return 123 + (await luafunc.callAsync());
}

export async function stringreftest() {
    return globalThis.lua.createStringRef("Hello from V8");
}

export function derefStringRef(ref) {
    console.log("Derefing string ref", ref);
    return ref
}

export function bar(x) {
    return x * 2;
}

await Promise.resolve(); // Force async
console.log(`In foo.js ${structuredClone}`);
"#.to_string()),
    ("foo1.json".to_string(), "{\"foo\":1929}".to_string()),
    ("dir/foo.js".to_string(), r#"
import * as bar from "./bar/bar.js";
console.log("In dir/foo.js");
"#.to_string()),
    ("dir/bar/bar.js".to_string(), r#"
import * as baz from "../baz.js";
console.log("In dir/bar/bar.js");
"#.to_string()),
    ("dir/baz.js".to_string(), r#"
console.log("In dir/baz.js");
"#.to_string()),
    ("bar.js".to_string(), r#"
import * as foobar from "./dir/foo.js";
import a from "./foo1.json" with { type: "json" };
console.log("In bar.js, imported foo1.json:", a);
export function foo2() {
    return 123 
}

export function baz(x) {
    return x * 2;
}
// Sleep for 2 seconds
console.log("In bar.js, sleeping for 10 seconds...");
console.log(typeof Deno.core.queueUserTimer)
await new Promise(resolve => setTimeout(resolve, 10000));
console.log("Awake now!");
console.log(`In foo2.js ${structuredClone} ${structuredClone([1,2,3,4,{},JSON.stringify({a:1,b:2})])}`);

let timeNow = Date.now();
console.log("Time now is: " + timeNow);
// Sleep for 0 seconds
await new Promise(resolve => setTimeout(resolve, 0));
console.log("Awake now again! Time now is: " + Date.now() + ", timeNow -  Date.now()", Date.now() - timeNow);
//console.log(WebAssembly);

export function keysGetter(obj) {
    console.log("In keysGetter", obj);
    return Object.keys(obj).join(", ");
}
"#.to_string()),
        ]);

        let bridge = LangBridge::<V8IsolateManagerServer>::new_from_bridge(
            &lua, 
            HEAP_LIMIT,
            ProcessOpts {
                debug_print: false,
                start_timeout: std::time::Duration::from_secs(10),
                cmd_argv: vec!["-".to_string(), "child".to_string()],
                cmd_envs: vec![],
            },
            ConcurrentExecutorState::new(1),
            vfs
        ).await.expect("Failed to create Lua-V8 bridge");

        // Call the v8 function now as a async script
        let lua_code = r#"
local function myfooer()
    print("In myfooer")
    return 42
end

local v8 = ...
local result = v8:run("foo.js")
-- args to pass to foo: function() print('am here'); return task.wait(1) end, v8, buffer.create(10)
print("Result from V8:", result)
local fooprop = result:getproperty("foo")
local res = fooprop:call(myfooer)
print("foo prop:", fooprop, res)
assert(res == 123 + 42, "Invalid result from foo prop call")

local stringref = result:getproperty("stringreftest"):call()
print("String ref from V8:", stringref, stringref:typename())
assert(stringref:typename() == "V8String", "Invalid type for stringref")
local derefed = result:getproperty("derefStringRef"):call(stringref)
print("Derefed string ref from V8:", derefed, typeof(derefed))
assert(derefed == "Hello from V8", "Invalid derefed string ref")

local ok, err = pcall(function() 
    stringref:call() 
end)
print("Expected error calling stringref as function:", err, ok)
assert(not ok, "Expected error calling stringref as function")

-- Test calling multiple times to ensure caching works
local result2 = v8:run("foo.js")
print("Result2 from V8:", result2)
local result3 = v8:run("bar.js")
print("Result3 from V8:", result3)
local result4 = v8:run("bar.js")
print("Result4 from V8:", result4)

local keysGetter = result4:getproperty("keysGetter")
print("keys getter", keysGetter:call(result4))
"#;

        let func = lua.load(lua_code)
        .set_name("main_chunk")
        .into_function().expect("Failed to load Lua code");
        let th = lua.create_thread(func).expect("Failed to create Lua thread");
        
        let mut args = mluau::MultiValue::new();
        args.push_back(bridge.into_lua(&lua).expect("Failed to push QuickJS runtime to Lua"));

        let output = task_mgr
            .spawn_thread_and_wait(th, args)
            .await
            .expect("Failed to run Lua thread")
            .expect("Lua thread returned no value")
            .expect("Lua thread returned an error");
        
        println!("Output: {:?}", output);
    });
}