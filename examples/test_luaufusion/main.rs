use std::collections::HashMap;

use concurrentlyexec::{ConcurrentExecutorState, ProcessOpts};
use mlua_scheduler::{taskmgr::NoopHooks, LuaSchedulerAsync, XRc};
use mluau::IntoLua;
use luaufusion::base::{ProxyBridge, ShutdownTimeouts};
use luaufusion::denobridge::{V8IsolateManagerServer, run_v8_process_client};
use luaufusion::luau::embedder_api::EmbedderData;
use luaufusion::luau::langbridge::LangBridge;
//use luaufusion::parallelluau::{ParallelLuaProxyBridge, run_luau_process_client};
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
                    //run_luau_process_client().await;
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
const eventHandler = async () => {
    let eb = globalThis.lua.eventBridge;
    console.log("[v8] eb", eb);
    console.log("[v8] Sending INIT message to Luau");
    try {
        eb.sendText("INIT")
    } catch(e) {
        console.error("[v8] Failed to send INIT message to Luau", e);
    }
    console.log("[v8] Sent INIT message to Luau");
    while(true) {
        let msg = await eb.receiveText();
        console.log("[v8] Received message from Luau", msg);
        if(msg == "SHUTDOWN") {
            console.log("[v8] Received SHUTDOWN, exiting event handler");
            eb.sendText("DOWN");
            break;
        } else if (msg == "GETBUFFER") {
            console.log("[v8] Receiving buffer from Luau");
            let buf = await eb.receiveBinary();
            console.log("[v8] Received buffer from Luau", buf);
            const decoder = new TextDecoder('utf-8');
            console.log("[v8] Buffer from Luau as string:", decoder.decode(buf));
            eb.sendText("BUFFERRECEIVED");
        }
    }
};

console.log(URL, URLPattern, new URL("https://google.com"))
console.log(`In foo.js ${structuredClone({})}`);
try {
    await eventHandler();
} catch(e) {
    console.error(`Error in foo.js ${e}`);
}
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
"#.to_string()),
        ]);

        let bridge_v8 = LangBridge::<V8IsolateManagerServer>::new_from_bridge(
            &lua, 
            EmbedderData {
                heap_limit: HEAP_LIMIT,
                max_payload_size: None,
                //object_disposal_enabled: true,
                //automatic_object_disposal_enabled: true,
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

        /*let vfs_luau = HashMap::from([
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
        ).await.expect("Failed to create Lua-V8 bridge");*/

        // Call the v8 function now as a async script
        let lua_code = r#"
local v8 = ...
task.spawn(function() 
    while true do
        local msg = v8:receivetext()
        if not msg then
            print("[Luau] Failed to receive message from v8")
            break
        end
        print("[Luau] Received from v8:", msg)
        if msg == "INIT" then
            -- Send a buffer to v8
            print("[Luau] Sending buffer to v8")
            local buf = buffer.fromstring("Hello from Luau!")
            v8:sendtext("GETBUFFER")
            v8:sendbinary(buf)
            continue
        elseif msg == "BUFFERRECEIVED" then
            print("[Luau] Sending SHUTDOWN to v8")
            v8:sendtext("SHUTDOWN")
        elseif msg == "DOWN" then
            print("[Luau] Received DOWN message, exiting loop")
            break
        end
    end
end)
local result = v8:run("foo.js") -- This should keep running until we are fully done
v8:shutdown()

return v8
"#;

        let func = lua.load(lua_code)
        .set_name("main_chunk")
        .into_function().expect("Failed to load Lua code");
        let th = lua.create_thread(func).expect("Failed to create Lua thread");
        
        let mut args = mluau::MultiValue::new();
        args.push_back(bridge_v8.into_lua(&lua).expect("Failed to push v8 runtime to Lua"));

        let output = task_mgr
            .spawn_thread_and_wait(th, args)
            .await
            .expect("Failed to run Lua thread")
            .expect("Lua thread returned no value")
            .expect("Lua thread returned an error");
        
        println!("Output: {:?}", output);
        assert!(output.len() == 1, "Expected single return value from Lua main chunk");
        let v8 = match output.into_iter().next().unwrap() {
            mluau::Value::UserData(ud) => {
                let v8bridge = ud.take::<LangBridge<V8IsolateManagerServer>>().expect("Failed to get V8 bridge from userdata");
                v8bridge
            },
            _ => panic!("Expected userdata return value from Lua main chunk"),
        };

        println!("Shutting down v8 bridge");
        v8.bridge().shutdown(SHUTDOWN_TIMEOUTS).await.expect("Failed to shutdown bridge");
        println!("Shutdown complete");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await; // Wait a bit for the child process to exit
    });
}