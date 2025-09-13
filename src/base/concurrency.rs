use std::{sync::Arc, time::Duration};

use remoc::rch::mpsc::Sender;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::sync::{mpsc::{UnboundedReceiver, UnboundedSender}, OwnedSemaphorePermit, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio::process::Child;

/// Options for creating a new thread
pub struct ProcessOpts {
    /// Must contain the executable path as the first argument
    /// or `-` to use the current executable
    /// 
    /// ConcurrentExecutor will automatically set the needed env variables
    /// 
    /// The process must then call ConcurrentExecutor::run_process_client to connect
    /// to the parent process
    pub cmd_argv: Vec<String>,
    pub heap_limit: usize,
    pub id: String,
    pub start_timeout: Duration
}

pub enum OneshotSender<T> {
    Local {
        sender: tokio::sync::oneshot::Sender<T>,
    },
    Process {
        sender: remoc::rch::oneshot::Sender<T>,
    },
}

impl<T: Serialize + DeserializeOwned + Send + 'static> OneshotSender<T> {
    /// Creates a new local oneshot sender
    pub fn new_local(sender: tokio::sync::oneshot::Sender<T>) -> Self {
        Self::Local { sender }
    }

    /// Creates a new process oneshot sender
    pub fn new_process(sender: remoc::rch::oneshot::Sender<T>) -> Self {
        Self::Process { sender }
    }

    /// Sends a value through the oneshot sender
    pub fn send(self, value: T) -> Result<(), crate::base::Error> {
        match self {
            Self::Local { sender } => {
                let _ = sender.send(value);
            }
            Self::Process { sender } => {
                match sender.send(value) {
                    Ok(_) => {},
                    Err(e) => {
                        return Err(format!("Failed to send value through process oneshot sender: {}", e.to_string()).into());
                    }
                }
            }
        }
        Ok(())
    }
}

impl<T: Serialize + DeserializeOwned + Send + 'static> Serialize for OneshotSender<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Local { .. } => {
                Err(serde::ser::Error::custom("Cannot serialize local oneshot sender"))
            }
            Self::Process { sender } => {
                // Serialize as [0, sender]
                sender.serialize(serializer)
            }
        }
    }
}

impl<'de, T: Serialize + DeserializeOwned + Send + 'static> Deserialize<'de> for OneshotSender<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let sender = remoc::rch::oneshot::Sender::<T>::deserialize(deserializer)?;
        Ok(Self::Process { sender })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(bound = "")]
pub enum ProcessMessage<T: ConcurrentlyExecute> {
    Message {
        data: Message<T>,
    },
    LoadMpsc {
        recv: remoc::rch::mpsc::Receiver<ProcessMessage<T>>,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(bound = "")]
pub enum Message<T: ConcurrentlyExecute> {
    Data {
        data: T::Message,
        resp: Option<OneshotSender<T::Response>>,
    },
    Shutdown,
}

#[allow(async_fn_in_trait)]
/// Trait for types that can be executed concurrently
pub trait ConcurrentlyExecute: Send + Sync + Clone + Sized + 'static {
    type State;
    type Message: Serialize + for<'a> Deserialize<'a> + Send + Sync + 'static;
    type Response: Serialize + for<'a> Deserialize<'a> + Send + Sync + 'static;

    async fn run(
        state: Self::State, 
        rx: UnboundedReceiver<Message<Self>>, 
    );
}

/// State for a concurrent executor
#[derive(Clone)]
pub struct ConcurrentExecutorState<T: ConcurrentlyExecute> {
    pub cancel_token: CancellationToken,
    pub sema: Arc<Semaphore>,
    _marker: std::marker::PhantomData<T>,
}

impl<T: ConcurrentlyExecute> ConcurrentExecutorState<T> {
    /// Creates a new concurrent executor state
    /// with N permits
    pub fn new(
        cancel_token: CancellationToken,
        max: usize,
    ) -> Self {
        Self::new_with_sema(cancel_token, Arc::new(Semaphore::new(max)))
    }

    /// Creates a new concurrent executor state
    /// with a custom semaphore set
    pub fn new_with_sema(
        cancel_token: CancellationToken,
        sema: Arc<Semaphore>,
    ) -> Self {
        Self {
            cancel_token,
            sema,
            _marker: std::marker::PhantomData,
        }
    }
}

/// Concurrent tokio execution
///
/// Assumes use of local tokio runtime
pub enum ConcurrentExecutor<T: ConcurrentlyExecute> {
    Local {
        state: ConcurrentExecutorState<T>,
        message_tx: UnboundedSender<Message<T>>,
        permit: OwnedSemaphorePermit,
    },
    Process {
        state: ConcurrentExecutorState<T>,
        message_tx: Sender<ProcessMessage<T>>,
        proc_handle: Arc<RwLock<Child>>,
        permit: OwnedSemaphorePermit,
        uds_path: NamedTempFile, // On drop, this destroys the file
        is_local_tokio: bool,

        // Not used, but we need to keep them around
        tx: remoc::rch::base::Sender<ProcessMessage<T>>,
        rx: remoc::rch::base::Receiver<ProcessMessage<T>>
    },
}

impl<T: ConcurrentlyExecute> Drop for ConcurrentExecutor<T> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl<T: ConcurrentlyExecute> ConcurrentExecutor<T> {
    /// Creates a new local concurrent executor
    /// that runs in the current thread
    /// 
    /// Asserts that it is being used in a local tokio runtime or LocalSet
    pub async fn new_local(
        state: T::State,
        cs_state: ConcurrentExecutorState<T>,
    ) -> Result<Self, crate::base::Error> {
        let permit = cs_state.sema.clone().acquire_owned().await?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        
        tokio::task::spawn_local(async move {
            T::run(state, rx).await;
        });

        Ok(Self::Local {
            state: cs_state,
            message_tx: tx,
            permit,
        })
    }

    /// Creates a new process concurrent executor
    /// that runs in a separate process
    pub async fn new_process(
        cs_state: ConcurrentExecutorState<T>,
        mut opts: ProcessOpts,
        is_local_tokio: bool,
    ) -> Result<Self, crate::base::Error> {
        let permit = cs_state.sema.clone().acquire_owned().await?;
        // Create a unix socket pair for communication
        let rand_str = {
            use rand::Rng;

            fn generate_random_alphanumeric_string(length: usize) -> String {
                const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
                let mut rng = rand::rng();
                let random_string: String = (0..length)
                    .map(|_| {
                        let idx = rng.random_range(0..CHARSET.len());
                        CHARSET[idx] as char
                    })
                    .collect();
                random_string
            }

            generate_random_alphanumeric_string(16)
        };
        let uds_path = tempfile::NamedTempFile::with_suffix(format!("ce{rand_str}.sock"))?;
        let unix_listener = tokio::net::UnixListener::bind(
            std::env::temp_dir().join(format!("concurrent_executor_{rand_str}.sock"))
        )?;

        // Spawn the process here
        // Note that we do not touch stdout/stdin/stderr as the child process
        // should inherit those from the parent
        let exe = {
            if opts.cmd_argv.len() == 0 {
                return Err("cmd_argv must contain at least the executable path".into());
            }
            if opts.cmd_argv[0] == "-" {
                std::env::current_exe()?.to_string_lossy().into_owned()
            } else {
                std::mem::take(&mut opts.cmd_argv[0])
            }
        };
        let args = opts.cmd_argv[1..].to_vec();
        let env = [
            (
                "CONCURRENT_EXECUTOR_UDS_PATH",
                uds_path.path().to_str().ok_or("Failed to convert UDS path to str")?,
            ),
            (
                "CONCURRENT_EXECUTOR_ID",
                opts.id.as_str(),
            ),
            (
                "CONCURRENT_EXECUTOR_HEAP_LIMIT",
                &opts.heap_limit.to_string(),
            ),
        ];
        let mut cmd = tokio::process::Command::new(exe);
        cmd.args(args);
        cmd.envs(env);
        let mut proc_handle = cmd.spawn()?;
        
        // Wait for a connection
        let timer = tokio::time::sleep(opts.start_timeout);
        let (stream, addr) = tokio::select! {
            _ = timer => {
                return Err("Timed out waiting for process to connect".into());
            }
            res = unix_listener.accept() => res?,
        };
        let addr_pathname = match addr.as_pathname() {
            Some(p) => p.to_owned(),
            None => {
                return Err("Failed to get pathname from unix socket address".into());
            }
        };
        if addr_pathname != uds_path.path() {
            return Err("Unix socket address pathname does not match expected path".into());
        }

        // Create remoc channel
        let (reader, writer) = stream.into_split();
        let (conn, mut tx, rx) = remoc::Connect::io_buffered::<tokio::net::unix::OwnedReadHalf, tokio::net::unix::OwnedWriteHalf, ProcessMessage<T>, ProcessMessage<T>, remoc::codec::Postbag>(
            remoc::Cfg::compact(), 
            reader, 
            writer, 
            100
        )
            .await
            .map_err(|e| format!("Failed to create remoc connection: {}", e.to_string()))?;
        
        let cancel_token = cs_state.cancel_token.clone();
        let fut = async move { 
            tokio::select! {
                _ = cancel_token.cancelled() => {}
                _ = conn => {
                }
            }
        };
        if is_local_tokio {
            tokio::task::spawn_local(fut);
        } else {
            tokio::task::spawn(fut);
        }

        let (message_tx, message_rx) = remoc::rch::mpsc::channel::<ProcessMessage<T>, _>(256);
        tx.send(ProcessMessage::LoadMpsc { recv: message_rx }).await.map_err(|e| format!("Failed to send LoadMpsc message to process: {}", e.to_string()))?;

        Ok(Self::Process {
            state: cs_state,
            message_tx,
            proc_handle: Arc::new(RwLock::new(proc_handle)),
            permit,
            uds_path,
            is_local_tokio,
            tx,
            rx,
        })
    }

    /// Gets the state of the executor
    pub fn get_state(&self) -> &ConcurrentExecutorState<T> {
        match self {
            Self::Local { state, .. } => &state,
            Self::Process { state, .. } => &state,
        }
    }

    /// Fires a message to the executor
    pub async fn fire(&self, msg: T::Message) -> Result<(), crate::base::Error> {
        match self {
            Self::Local { message_tx, .. } => {
                message_tx.send(Message::Data { data: msg, resp: None })?;
            }
            Self::Process { message_tx, .. } => {
                match message_tx.send(ProcessMessage::Message { data: Message::Data { data: msg, resp: None }}).await {
                    Ok(_) => {},
                    Err(e) => {
                        return Err(format!("Failed to send message to process executor: {}", e.to_string()).into());
                    }
                };
            }
        }
        Ok(())
    }

    /// Sends a message to the executor
    pub async fn send(&self, msg: T::Message) -> Result<T::Response, crate::base::Error> {
        match self {
            Self::Local { message_tx, .. } => {
                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                let oneshot = OneshotSender::new_local(resp_tx);
                message_tx.send(Message::Data { data: msg, resp: Some(oneshot) })?;
                let resp = resp_rx.await.map_err(|e| format!("Failed to receive response from local executor: {}", e.to_string()))?;
                Ok(resp)
            }
            Self::Process { message_tx, .. } => {
                let (resp_tx, resp_rx) = remoc::rch::oneshot::channel();
                let oneshot = OneshotSender::new_process(resp_tx);
                match message_tx.send(ProcessMessage::Message { data: Message::Data { data: msg, resp: Some(oneshot) }}).await {
                    Ok(_) => {
                        let resp = resp_rx.await.map_err(|e| format!("Failed to receive response from process executor: {}", e.to_string()))?;
                        Ok(resp)
                    },
                    Err(e) => {
                        return Err(format!("Failed to send message to process executor: {}", e.to_string()).into());
                    }
                }
            }
        }
    }

    /// Shuts down the executor
    pub fn shutdown(&self) -> Result<(), crate::base::Error> {
        match self {
            Self::Local { message_tx, .. } => {
                message_tx.send(Message::Shutdown)?;
                self.get_state().cancel_token.cancel();
            }
            Self::Process { message_tx, proc_handle, is_local_tokio, .. } => {
                let _ = message_tx.send(ProcessMessage::Message { data: Message::Shutdown });
                // Give some time for the process to exit gracefully
                let proc_handle = proc_handle.clone();
                let cancel_token = self.get_state().cancel_token.clone();
                let fut = async move {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    // If the process is still running, kill it
                    match proc_handle.write().await.kill().await {
                        Ok(_) => {},
                        Err(e) => {
                            eprintln!("Failed to kill process: {}", e);
                        }
                    };    
                    // Set cancel token
                    cancel_token.cancel();            
                };
                if *is_local_tokio {
                    tokio::task::spawn_local(fut);
                } else {
                    tokio::task::spawn(fut);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tokio::runtime::LocalOptions;

    #[test]
    fn test_spawn_in_local() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(LocalOptions::default())
            .unwrap();

        runtime.block_on(async {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            tokio::task::spawn(async move {
                println!("Hello from task!");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                tokio::task::yield_now().await;
                let _ = tx.send(());
            });
            let _ = rx.recv().await;
        });
    }
}