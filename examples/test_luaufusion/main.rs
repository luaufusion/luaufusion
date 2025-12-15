use std::collections::HashMap;

use concurrentlyexec::{ConcurrentExecutorState, ProcessOpts};
use mlua_scheduler::{taskmgr::NoopHooks, LuaSchedulerAsync, XRc};
use mluau::IntoLua;
use luaufusion::base::{ProxyBridge, ShutdownTimeouts};
use luaufusion::denobridge::{V8IsolateManagerServer, run_v8_process_client};
use luaufusion::luau::embedder_api::EmbedderData;
use luaufusion::luau::langbridge::{LangBridge, ProxiedValue};
use luaufusion::parallelluau::{ParallelLuaProxyBridge, run_luau_process_client};
use tokio::runtime::LocalOptions;

const HEAP_LIMIT: usize = 10 * 1024 * 1024; // 10 MB
const SHUTDOWN_TIMEOUTS: ShutdownTimeouts = ShutdownTimeouts {
    bridge_shutdown: std::time::Duration::from_millis(100),
    executor_shutdown: std::time::Duration::from_secs(5),
};
const FROM_LUAU_SHUTDOWN_TIMEOUTS: ShutdownTimeouts = ShutdownTimeouts {
    bridge_shutdown: std::time::Duration::from_millis(100),
    executor_shutdown: std::time::Duration::from_secs(5),
};

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(LocalOptions::default())
        .unwrap();

    rt.block_on(async move {
        let args = std::env::args().collect::<Vec<_>>();
        if args.len() > 1 {
            assert!(args[1] == "child", "Invalid argument");
            assert!(args.len() > 2, "Missing child type");
            match args[2].as_str() {
                "v8" => {
                    println!("[child] Starting child process mode [v8]");
                    run_v8_process_client().await;
                },
                "luau" => {
                    println!("[child] Starting child process mode [luau]");
                    run_luau_process_client().await;
                }
                _ => panic!("Invalid child type"),
            }
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
    console.log("foo [func class]: ", luafunc, luafunc.constructor.name);
    console.log("foo, type:", `${luafunc.type}`);
    console.log("fooCall", `${await globalThis.lua.callSync(luafunc)}`);
    console.log("hi");
    return 123 + (await globalThis.lua.callAsync(luafunc));
}

export function s(s) {
    console.log("Derefing string", s);
    return s
}

export function bar(x) {
    return x * 2;
}

await Promise.resolve(); // Force async
console.log(`In foo.js ${structuredClone({})}`);
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
console.log("In bar.js, sleeping for 5 seconds...");
await new Promise(resolve => setTimeout(resolve, 5000));
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

export function staticmaptest(m) {
    console.log("In staticmaptest", m);
    m.set("v8", "is pog");
    m.set("v8arr", [1,2,3,4,5]);
    return m;
}

export function testEmbedderJson(evj) {
    console.log("In testEmbedderJson", JSON.parse(evj));
}

export async function tablepropget(obj) {
    console.log("In tablepropget", obj);
    let key = await globalThis.lua.get(obj, "foo");
    console.log("Got foo key:", key);
    let key2 = await globalThis.lua.get(obj, {});
    console.log("Got non-existent key:", key2);
    return key;
}
"#.to_string()),
        ]);

        let bridge_v8 = LangBridge::<V8IsolateManagerServer>::new_from_bridge(
            &lua, 
            EmbedderData {
                heap_limit: HEAP_LIMIT,
                max_payload_size: None,
                object_disposal_enabled: true,
                automatic_object_disposal_enabled: true,
            },
            ProcessOpts {
                debug_print: false,
                start_timeout: std::time::Duration::from_secs(10),
                cmd_argv: vec!["-".to_string(), "child".to_string(), "v8".to_string()],
                cmd_envs: vec![],
            },
            ConcurrentExecutorState::new(1),
            vfs,
            FROM_LUAU_SHUTDOWN_TIMEOUTS
        ).await.expect("Failed to create Lua-V8 bridge");

        let vfs_luau = HashMap::from([
            ("init.luau".to_string(), r#"
print("In init.luau")
return {
    greeting = "Hello from Luau!",
    greetFromParallelLuau = function(name)
        return "Hello, " .. name .. ", from Parallel Luau!"
    end
}
"#.to_string()),
        ]);

        let bridge_luau: LangBridge<ParallelLuaProxyBridge> = LangBridge::<ParallelLuaProxyBridge>::new_from_bridge(
            &lua, 
            EmbedderData {
                heap_limit: HEAP_LIMIT,
                max_payload_size: None,
                object_disposal_enabled: true,
                automatic_object_disposal_enabled: true,
            },
            ProcessOpts {
                debug_print: false,
                start_timeout: std::time::Duration::from_secs(10),
                cmd_argv: vec!["-".to_string(), "child".to_string(), "luau".to_string()],
                cmd_envs: vec![],
            },
            ConcurrentExecutorState::new(1),
            vfs_luau,
            FROM_LUAU_SHUTDOWN_TIMEOUTS
        ).await.expect("Failed to create Lua-V8 bridge");

        let test_embedder_json = r#"{"embeddedJson":"embedded22","mynestedMap":{"a":{"b":123,"c":null,"d":{}}}}"#;
        let ev = ProxiedValue::<V8IsolateManagerServer>::from_str(
            test_embedder_json.to_string(), 
        );

        // Call the v8 function now as a async script
        let lua_code = r#"
local function myfooer()
    print("In myfooer")
    return 42
end

local v8, luau, ed = ...
local result = v8:run("foo.js")
-- args to pass to foo: function() print('am here'); return task.wait(1) end, v8, buffer.create(10)
print("Result from V8:", result)
local fooprop = result:getproperty("foo")
local res = fooprop:call(myfooer)
print("foo prop:", fooprop, res)
assert(res == 123 + 42, "Invalid result from foo prop call")

-- Test calling multiple times to ensure caching works
local result2 = v8:run("foo.js")
print("Result2 from V8:", result2)
local result3 = v8:run("bar.js")
print("Result3 from V8:", result3)
local result4 = v8:run("bar.js")
print("Result4 from V8:", result4)

local keysGetter = result4:getproperty("keysGetter")
print("keys getter", keysGetter:call(result4))

local staticmaptest = result4:getproperty("staticmaptest")
local function abcfunc() end
local smap = {
    abc = 123,
    luau = "is great",
    meow = { nested = "object", ["\xf0\x28\x8c\x28"] = "with null string", [abcfunc] = 44 },
    myarr = {'a','b','c'},
    null = v8:null(),
}

local smap = staticmaptest:call(smap)
for k, v in smap do
    print("staticmap key/value", k, v)
    if type(v) == "table" then
        for k2, v2 in v do
            print("  nested key/value", k2, v2)
        end
    end
end
assert(smap.v8 == "is pog", "Invalid static map value for v8 key")

local testEmbedderJson = result4:getproperty("testEmbedderJson")
testEmbedderJson:call(ed)

local tablepropget = result4:getproperty("tablepropget")
local tbl = setmetatable({}, {
    __index = function(t, k)
        print("Indexing table for key:", k)
        if k == "foo" then
            return "barvalue"
        end
        return nil
    end
})
local foo_key = tablepropget:call(tbl)
print("foo_key from tablepropget:", foo_key)
assert(foo_key == "barvalue", "Invalid foo_key from tablepropget")

v8:shutdown()

print("Running Luau code now")
local luau_result = luau:run("init.luau")
print("Luau result:", luau_result, luau_result.greeting)
assert(luau_result.greeting == "Hello from Luau!", "Invalid greeting from Luau")
local greetFromParallelLuau = luau_result.greetFromParallelLuau
print("greetFromParallelLuau:", greetFromParallelLuau)
assert(type(greetFromParallelLuau) == "userdata", "Invalid type for greetFromParallelLuau")
local greet_msg = greetFromParallelLuau:call("my love")
print("greet_msg:", greet_msg)
assert(greet_msg == "Hello, my love, from Parallel Luau!", "Invalid greet_msg from Parallel Luau")

luau:shutdown()
"#;

        let func = lua.load(lua_code)
        .set_name("main_chunk")
        .into_function().expect("Failed to load Lua code");
        let th = lua.create_thread(func).expect("Failed to create Lua thread");
        
        let mut args = mluau::MultiValue::new();
        let bridgy = bridge_v8.bridge().clone();
        let bridgy_luau = bridge_luau.bridge().clone();
        args.push_back(bridge_v8.into_lua(&lua).expect("Failed to push v8 runtime to Lua"));
        args.push_back(bridge_luau.into_lua(&lua).expect("Failed to push luau runtime to Lua"));
        args.push_back(ev.into_lua(&lua).expect("Failed to push embeddable json"));

        let output = task_mgr
            .spawn_thread_and_wait(th, args)
            .await
            .expect("Failed to run Lua thread")
            .expect("Lua thread returned no value")
            .expect("Lua thread returned an error");
        
        println!("Output: {:?}", output);

        println!("Shutting down v8 bridge");
        bridgy.shutdown(SHUTDOWN_TIMEOUTS).await.expect("Failed to shutdown bridge");
        println!("Shutting down luau bridge");
        bridgy_luau.shutdown(SHUTDOWN_TIMEOUTS).await.expect("Failed to shutdown luau bridge");
        println!("Shutdown complete");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await; // Wait a bit for the child process to exit
    });
}