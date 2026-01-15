use deno_core::PollEventLoopOptions;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use tokio::sync::mpsc::unbounded_channel;

use crate::denobridge::inner::EventBridgeSetupData;
use crate::denobridge::modloader::FusionModuleLoader;
use crate::base::{ClientTransport, InitialData, ServerMessage};
use super::inner::V8IsolateManagerInner;

/// Minimum heap size for V8 isolates
pub const MIN_HEAP_LIMIT: usize = 10 * 1024 * 1024; // 10MB

pub async fn run_v8_client<T>(transport: T, data: InitialData) 
where
    T: ClientTransport + 'static,
{
    let (send_text_tx, send_text_rx) = unbounded_channel();
    let (send_binary_tx, send_binary_rx) = unbounded_channel();
    let (msg_tx, mut msg_rx) = unbounded_channel();

    //let (tx, mut rx) = client_ctx.multi();
    //data.messenger_tx.client(&client_ctx).send(tx).unwrap();

    let mut inner = V8IsolateManagerInner::new(
        msg_tx,
        data.ed,
        FusionModuleLoader::new(data.vfs.into_iter().map(|(x, y)| (x, y.into()))),
        EventBridgeSetupData {
            text_rx: send_text_rx,
            binary_rx: send_binary_rx,
        }
    );

    let url = match deno_core::url::Url::parse(&format!("file:///{}", data.module_name)) {
        Ok(u) => u,
        Err(e) => {
            panic!("Failed to parse module name as URL: {}", e);
        }
    };

    let id = tokio::select! {
        id = inner.deno.load_side_es_module(&url) => {
            match id {
                Ok(id) => id,
                Err(e) => {
                    panic!("Failed to load module: {}", e);
                }
            }
        }
        _ = inner.cancellation_token.cancelled() => {
            return;
        }
    };

    let fut = inner.deno.mod_evaluate(id);
    let mut fut_queue = FuturesUnordered::new();
    fut_queue.push(fut);

    loop {
        tokio::select! {
            msg = transport.receive_from_host() => {
                match msg {
                    Ok(ServerMessage::SendText { msg }) => {
                        let _ = send_text_tx.send(msg);
                    },
                    Ok(ServerMessage::SendBinary { msg }) => {
                        let _ = send_binary_tx.send(msg);
                    },
                    Err(e) => {
                        eprintln!("Error receiving message in V8 isolate manager: {}", e);
                        //break;
                    }
                }
            }
            Some(msg) = msg_rx.recv() => {
                let _ = transport.send_to_host(msg);
            }
            _ = inner.cancellation_token.cancelled() => {
                if cfg!(feature = "debug_message_print_enabled") {
                    println!("V8 isolate manager received shutdown message");
                }
                break;
            }
            _ = inner.deno.run_event_loop(PollEventLoopOptions {
                pump_v8_message_loop: true,
            }) => {
                tokio::task::yield_now().await;
            },
            Some(resp) = fut_queue.next() => {
                if let Err(e) = resp {
                    eprintln!("Error evaluating module in V8 isolate manager: {}", e);
                }
                break;
            }
        }
    }
}
