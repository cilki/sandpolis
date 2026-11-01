use sandpolis_macros::Stream;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Request message for shell session streams.
#[derive(Serialize, Deserialize)]
pub enum ShellSessionStreamRequest {
    /// Requester wants to start the stream
    Start {
        /// Path to the shell executable
        path: PathBuf,

        // TODO request permissions
        // Permission permission = 3;
        /// Additional environment variables
        environment: HashMap<String, String>,

        /// Number of rows to request
        rows: u32,

        /// Number of columns to request
        cols: u32,
    },
    /// Requester has stdin data
    Stdin { data: Vec<u8> },
    /// Requester changed the size of the terminal
    Resize {
        /// Update the number of rows
        rows: u32,

        /// Update the number of columns
        cols: u32,
    },
}

/// Event containing shell output, or notice that the shell exited.
#[derive(Serialize, Deserialize)]
pub enum ShellSessionStreamResponse {
    /// A chunk of shell output.
    Output {
        stdout: Vec<u8>,
        /// Always empty on unix agents: the shell runs on a PTY, which merges
        /// stderr into stdout by design.
        stderr: Vec<u8>,
    },
    /// The shell exited and no more output will arrive. Without this the
    /// requester has no way to notice the session ended: stream closure is
    /// local to each registry and never crosses the relay.
    Exited {
        /// The shell's exit code, if it exited normally.
        code: Option<i32>,
    },
}

#[cfg(feature = "agent")]
mod agent {
    use super::*;
    use anyhow::Result;
    use sandpolis_instance::network::StreamResponder;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::RwLock;
    use tokio::sync::mpsc::Sender;
    use tokio_util::sync::CancellationToken;

    /// Stream that runs a bidirectional shell session.
    ///
    /// On unix the shell is attached to a real PTY, so line editing, echo, and
    /// full-screen programs behave like a local terminal. Other platforms fall
    /// back to plain pipes.
    ///
    /// The child is owned by a background task that streams its output; the
    /// [`Start`](ShellSessionStreamRequest::Start) handler spawns that task and
    /// returns immediately. Blocking here would stall the whole connection's
    /// dispatch loop, since responder handlers run inline on the socket's
    /// receive path — no output would flush and no further input (stdin, resize)
    /// would be read.
    #[derive(Stream, Default)]
    pub struct ShellSessionStreamResponder {
        /// Cancels the background reader task (and kills the child) on drop.
        cancel: CancellationToken,
        /// Write half of the PTY the shell is attached to.
        #[cfg(unix)]
        pub pty: RwLock<Option<pty_process::OwnedWritePty>>,
        /// Pipe to the shell's stdin (no PTY on this platform).
        #[cfg(not(unix))]
        pub stdin: RwLock<Option<tokio::process::ChildStdin>>,
    }

    impl StreamResponder for ShellSessionStreamResponder {
        type In = ShellSessionStreamRequest;
        type Out = ShellSessionStreamResponse;

        async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
            match request {
                ShellSessionStreamRequest::Start {
                    path,
                    mut environment,
                    rows,
                    cols,
                } => {
                    // Add a default for TERM
                    if !environment.contains_key("TERM") {
                        environment.insert("TERM".to_string(), "xterm-256color".to_string());
                    }

                    // Add a default for rows/cols
                    let rows = if rows == 0 { 24 } else { rows };
                    let cols = if cols == 0 { 80 } else { cols };

                    #[cfg(unix)]
                    {
                        let (pty, pts) = pty_process::open()?;
                        pty.resize(pty_process::Size::new(rows as u16, cols as u16))?;

                        let child = pty_process::Command::new(&path)
                            .envs(environment)
                            .spawn(pts)?;
                        let (mut reader, writer) = pty.into_split();

                        *self.pty.write().await = Some(writer);

                        let cancel = self.cancel.clone();
                        tokio::spawn(async move {
                            let mut child = child;
                            loop {
                                let mut stdout = Vec::new();
                                tokio::select! {
                                    _ = cancel.cancelled() => break,
                                    read = reader.read_buf(&mut stdout) => match read {
                                        Ok(0) => break, // EOF
                                        Ok(_) => {
                                            let response = ShellSessionStreamResponse::Output {
                                                stdout,
                                                stderr: Vec::new(),
                                            };
                                            if sender.send(response).await.is_err() {
                                                break;
                                            }
                                        }
                                        // PTY reads fail with EIO once the child exits
                                        Err(_) => break,
                                    }
                                }
                            }
                            let _ = child.start_kill();
                            let code = child.wait().await.ok().and_then(|status| status.code());
                            // Best-effort: the requester may already be gone.
                            let _ = sender
                                .send(ShellSessionStreamResponse::Exited { code })
                                .await;
                        });
                    }

                    #[cfg(not(unix))]
                    {
                        // Spawn the process and extract stdin/stdout
                        let mut child = tokio::process::Command::new(&path)
                            .envs(environment)
                            .stdin(std::process::Stdio::piped())
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::piped())
                            .spawn()?;
                        let mut stdout = child.stdout.take().unwrap();
                        let stdin = child.stdin.take().unwrap();

                        *self.stdin.write().await = Some(stdin);

                        let cancel = self.cancel.clone();
                        tokio::spawn(async move {
                            let mut child = child;
                            loop {
                                let mut data = Vec::new();
                                tokio::select! {
                                    _ = cancel.cancelled() => break,
                                    read = stdout.read_buf(&mut data) => match read {
                                        Ok(0) => break, // EOF
                                        Ok(_) => {
                                            let response = ShellSessionStreamResponse::Output {
                                                stdout: data,
                                                stderr: Vec::new(),
                                            };
                                            if sender.send(response).await.is_err() {
                                                break;
                                            }
                                        }
                                        Err(_) => break,
                                    }
                                }
                            }
                            let _ = child.start_kill();
                            let code = child.wait().await.ok().and_then(|status| status.code());
                            // Best-effort: the requester may already be gone.
                            let _ = sender
                                .send(ShellSessionStreamResponse::Exited { code })
                                .await;
                        });
                    }
                }
                ShellSessionStreamRequest::Stdin { data } => {
                    #[cfg(unix)]
                    if let Some(ref mut pty) = *self.pty.write().await {
                        let _ = pty.write_all(&data).await;
                    }
                    #[cfg(not(unix))]
                    if let Some(ref mut stdin) = *self.stdin.write().await {
                        let _ = stdin.write_all(&data).await;
                    }
                }
                ShellSessionStreamRequest::Resize { rows, cols } => {
                    #[cfg(unix)]
                    if let Some(ref pty) = *self.pty.read().await {
                        pty.resize(pty_process::Size::new(rows as u16, cols as u16))?;
                    }
                    #[cfg(not(unix))]
                    let _ = (rows, cols);
                }
            }
            Ok(())
        }
    }

    impl Drop for ShellSessionStreamResponder {
        fn drop(&mut self) {
            // Signals the background reader task to stop and kill the child.
            self.cancel.cancel();
        }
    }
}

#[cfg(feature = "agent")]
pub use agent::ShellSessionStreamResponder;

#[cfg(feature = "client")]
mod client {
    use super::*;
    use anyhow::Result;
    use sandpolis_instance::InstanceId;
    use sandpolis_instance::network::{StreamRequester, stream::StreamMessage};
    use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};
    use tracing::warn;

    /// An output chunk surfaced to the GUI terminal as a shell session runs.
    pub struct ShellOutput {
        pub stdout: Vec<u8>,
        pub stderr: Vec<u8>,
    }

    /// What a running session surfaces to its consumer (GUI terminal or the
    /// CLI's raw relay).
    pub enum ShellEvent {
        /// A chunk of output to display.
        Output(ShellOutput),
        /// The remote shell exited with this code, if known. Nothing follows.
        Exited(Option<i32>),
    }

    /// Client side of a shell session: forwards output and exit events to the
    /// consumer through an unbounded channel. The tag matches
    /// [`ShellSessionStreamResponder`] so the agent terminates the relayed
    /// stream with a real PTY.
    #[derive(Stream)]
    pub struct ShellSessionStreamRequester {
        output: UnboundedSender<ShellEvent>,
    }

    impl ShellSessionStreamRequester {
        /// Construct a requester paired with the receiver the consumer drains.
        pub fn channel() -> (Self, UnboundedReceiver<ShellEvent>) {
            let (output, rx) = unbounded_channel();
            (Self { output }, rx)
        }
    }

    impl StreamRequester for ShellSessionStreamRequester {
        type In = ShellSessionStreamResponse;
        type Out = ShellSessionStreamRequest;

        async fn on_message(&self, response: Self::In, _tx: Sender<Self::Out>) -> Result<()> {
            let event = match response {
                ShellSessionStreamResponse::Output { stdout, stderr } => {
                    ShellEvent::Output(ShellOutput { stdout, stderr })
                }
                ShellSessionStreamResponse::Exited { code } => ShellEvent::Exited(code),
            };
            // Consumer's receiver may be gone (session closed); dropping is fine.
            let _ = self.output.send(event);
            Ok(())
        }
    }

    /// Open a relayed shell session to `instance` over `conn` and forward
    /// outbound requests (stdin, resize) over it until the channel closes.
    pub fn spawn_shell_stream(
        conn: std::sync::Arc<sandpolis_instance::network::InstanceConnection>,
        instance: InstanceId,
        requester: ShellSessionStreamRequester,
        initial: ShellSessionStreamRequest,
        mut outbound_rx: Receiver<ShellSessionStreamRequest>,
    ) {
        sandpolis_client::sync::spawn(async move {
            let (id, msg_tx) = match conn.open_stream_to(instance, requester, initial).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "Failed to open shell session");
                    return;
                }
            };
            while let Some(req) = outbound_rx.recv().await {
                let payload = match serde_cbor::to_vec(&req) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if msg_tx
                    .send(StreamMessage::to(id, payload, instance))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            conn.close_stream(id);
        });
    }
}

#[cfg(feature = "client")]
pub use client::{ShellEvent, ShellOutput, ShellSessionStreamRequester, spawn_shell_stream};

#[cfg(all(test, feature = "agent", unix))]
mod test_shell_session {
    use super::*;
    use sandpolis_instance::network::StreamResponder;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// Start a `/bin/sh` session and return its output stream + join handle.
    fn start_session(
        responder: &Arc<ShellSessionStreamResponder>,
        environment: HashMap<String, String>,
        rows: u32,
        cols: u32,
    ) -> (
        mpsc::Receiver<ShellSessionStreamResponse>,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let (tx, rx) = mpsc::channel::<ShellSessionStreamResponse>(32);
        let request = ShellSessionStreamRequest::Start {
            path: PathBuf::from("/bin/sh"),
            environment,
            rows,
            cols,
        };

        // Run on_message in a separate task since it blocks reading output
        let responder = responder.clone();
        let handle = tokio::spawn(async move { responder.on_message(request, tx).await });
        (rx, handle)
    }

    /// Collect session output until the stream ends or times out, returning the
    /// output and the exit code reported by the final `Exited` event (if seen).
    async fn collect_session(
        mut rx: mpsc::Receiver<ShellSessionStreamResponse>,
    ) -> (String, Option<Option<i32>>) {
        let mut output = Vec::new();
        let mut exited = None;
        while let Ok(response) =
            tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv()).await
        {
            match response {
                Some(ShellSessionStreamResponse::Output { stdout, .. }) => output.extend(stdout),
                Some(ShellSessionStreamResponse::Exited { code }) => {
                    exited = Some(code);
                    break;
                }
                None => break,
            }
        }
        (String::from_utf8_lossy(&output).into_owned(), exited)
    }

    /// Collect session output until the stream ends or times out.
    async fn collect_output(rx: mpsc::Receiver<ShellSessionStreamResponse>) -> String {
        collect_session(rx).await.0
    }

    async fn send_stdin(responder: &ShellSessionStreamResponder, data: &[u8]) {
        responder
            .on_message(
                ShellSessionStreamRequest::Stdin {
                    data: data.to_vec(),
                },
                mpsc::channel(1).0,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_start_and_receive_output() {
        let responder = Arc::new(ShellSessionStreamResponder::default());
        let (rx, handle) = start_session(&responder, HashMap::new(), 24, 80);

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        send_stdin(&responder, b"echo hello\nexit 3\n").await;

        let (output, exited) = collect_session(rx).await;
        let _ = handle.await;

        assert!(
            output.contains("hello"),
            "Expected 'hello' in output, got: {}",
            output
        );
        assert_eq!(
            exited,
            Some(Some(3)),
            "Expected an Exited event carrying the shell's exit code"
        );
    }

    #[tokio::test]
    async fn test_stdin_before_start_is_noop() {
        let responder = ShellSessionStreamResponder::default();
        let (tx, _rx) = mpsc::channel::<ShellSessionStreamResponse>(32);

        // Sending stdin before starting should not panic
        let request = ShellSessionStreamRequest::Stdin {
            data: b"test".to_vec(),
        };
        let result = responder.on_message(request, tx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_environment_defaults() {
        let responder = Arc::new(ShellSessionStreamResponder::default());

        // Start with empty environment and zero rows/cols to test defaults
        let (rx, handle) = start_session(&responder, HashMap::new(), 0, 0);

        // Wait for shell to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // The PTY echoes input back, so assert on expanded values that cannot
        // appear in the echoed command line itself
        send_stdin(&responder, b"echo RESULT=$TERM; stty size\nexit\n").await;

        let output = collect_output(rx).await;
        let _ = handle.await;

        assert!(
            output.contains("RESULT=xterm-256color"),
            "Expected default TERM, got: {}",
            output
        );
        assert!(
            output.contains("24 80"),
            "Expected default winsize 24x80, got: {}",
            output
        );
    }

    #[tokio::test]
    async fn test_environment_custom_values() {
        let responder = Arc::new(ShellSessionStreamResponder::default());

        // Start with custom environment
        let mut env = HashMap::new();
        env.insert("TERM".to_string(), "vt100".to_string());
        env.insert("MY_VAR".to_string(), "custom_value".to_string());

        let (rx, handle) = start_session(&responder, env, 50, 100);

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        send_stdin(&responder, b"echo RESULT=$TERM,$MY_VAR; stty size\nexit\n").await;

        let output = collect_output(rx).await;
        let _ = handle.await;

        // Custom TERM should be preserved (not overwritten)
        assert!(
            output.contains("RESULT=vt100,custom_value"),
            "Expected custom TERM and MY_VAR, got: {}",
            output
        );
        // Custom rows/cols should be applied to the PTY
        assert!(
            output.contains("50 100"),
            "Expected winsize 50x100, got: {}",
            output
        );
    }

    #[tokio::test]
    async fn test_resize() {
        let responder = Arc::new(ShellSessionStreamResponder::default());
        let (rx, handle) = start_session(&responder, HashMap::new(), 24, 80);

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        responder
            .on_message(
                ShellSessionStreamRequest::Resize { rows: 31, cols: 99 },
                mpsc::channel(1).0,
            )
            .await
            .unwrap();

        send_stdin(&responder, b"stty size\nexit\n").await;

        let output = collect_output(rx).await;
        let _ = handle.await;

        assert!(
            output.contains("31 99"),
            "Expected winsize 31x99 after resize, got: {}",
            output
        );
    }
}
