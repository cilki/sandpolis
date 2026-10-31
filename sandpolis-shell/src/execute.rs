use super::ShellCommand;
use anyhow::{Context, Result};
use sandpolis_instance::network::{StreamRequester, StreamResponder};
use sandpolis_macros::Stream;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::sync::mpsc::Sender;
use tokio::time::timeout;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

/// File descriptors that [`ShellExecuteStreamResponse::Progress`] reports on.
const STDOUT: i32 = 1;
const STDERR: i32 = 2;

/// Request message for shell execute streams.
#[derive(Serialize, Deserialize)]
pub struct ShellExecuteStreamRequest {
    /// Shell executable to use for request
    pub shell: PathBuf,

    /// Command to execute in a new shell
    pub command: ShellCommand,

    /// Execution timeout in seconds
    pub timeout: u64,

    /// Whether process output will be returned
    pub capture_output: bool,
}

/// Response message for shell execute streams.
#[derive(Serialize, Deserialize)]
pub enum ShellExecuteStreamResponse {
    Done {
        /// Process exit code
        exit_code: i32,

        /// Execution duration in seconds
        duration: f64,
        // TODO cgroup-y info like max memory, cpu time, etc
    },
    Progress {
        /// Process output on all descriptors
        output: HashMap<i32, Vec<u8>>,
    },
    Failed,
    NotFound,
    Timeout,
}

/// Why a command produced no exit code. Each corresponds to one of the
/// responder's terminal responses.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellExecuteFailure {
    /// The command ran but the responder couldn't collect its result.
    Failed,
    /// The requested shell doesn't exist on the agent.
    NotFound,
    /// The command outlived the request's timeout and was killed.
    Timeout,
}

#[derive(Stream)]
pub struct ShellExecuteStreamRequester {
    exit_code: RwLock<Option<i32>>,
    duration: RwLock<Option<f64>>,
    output: RwLock<HashMap<i32, Vec<u8>>>,
    /// Set when the command never ran to completion, which is the only way
    /// `exit_code` stays empty once the stream ends.
    failure: RwLock<Option<ShellExecuteFailure>>,
}

impl StreamRequester for ShellExecuteStreamRequester {
    type In = ShellExecuteStreamResponse;
    type Out = ShellExecuteStreamRequest;

    async fn on_message(&self, response: Self::In, _: Sender<Self::Out>) -> Result<()> {
        match response {
            ShellExecuteStreamResponse::Done {
                exit_code,
                duration,
            } => {
                *self.exit_code.write().await = Some(exit_code);
                *self.duration.write().await = Some(duration);
            }
            // Output arrives in chunks per descriptor, so append rather than
            // replace: a later chunk continues the same stream.
            ShellExecuteStreamResponse::Progress { output } => {
                let mut collected = self.output.write().await;
                for (fd, chunk) in output {
                    collected.entry(fd).or_default().extend(chunk);
                }
            }
            ShellExecuteStreamResponse::Failed => {
                *self.failure.write().await = Some(ShellExecuteFailure::Failed);
            }
            ShellExecuteStreamResponse::NotFound => {
                *self.failure.write().await = Some(ShellExecuteFailure::NotFound);
            }
            ShellExecuteStreamResponse::Timeout => {
                *self.failure.write().await = Some(ShellExecuteFailure::Timeout);
            }
        }
        Ok(())
    }

    async fn new(initial: Self::Out, tx: Sender<Self::Out>) -> Result<Self> {
        tx.send(initial).await?;
        Ok(Self {
            exit_code: RwLock::new(None),
            duration: RwLock::new(None),
            output: RwLock::new(HashMap::new()),
            failure: RwLock::new(None),
        })
    }
}

/// Stream that executes a single command and then terminates.
#[derive(Stream)]
pub struct ShellExecuteStreamResponder;

impl StreamResponder for ShellExecuteStreamResponder {
    type In = ShellExecuteStreamRequest;
    type Out = ShellExecuteStreamResponse;

    async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
        let ShellCommand(lines) = request.command;

        let mut command = Command::new(&request.shell);
        command.stdin(Stdio::piped());
        if request.capture_output {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        }

        let started = Instant::now();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                sender.send(ShellExecuteStreamResponse::NotFound).await?;
                return Ok(());
            }
            Err(_) => {
                sender.send(ShellExecuteStreamResponse::Failed).await?;
                return Ok(());
            }
        };

        // Feeding the command over stdin keeps this shell-agnostic, and closing
        // the pipe afterwards is what tells the shell to exit.
        {
            let mut stdin = child.stdin.take().context("shell has no stdin")?;
            for line in lines {
                stdin.write_all(line.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
            }
        }

        let run = async {
            let mut output = HashMap::new();
            let status = if request.capture_output {
                let mut stdout = child.stdout.take();
                let mut stderr = child.stderr.take();
                let mut out_buf = Vec::new();
                let mut err_buf = Vec::new();

                // Drain both pipes while waiting; a full pipe would otherwise
                // wedge the child until the timeout fires.
                let (status, out, err) = tokio::join!(
                    child.wait(),
                    read_pipe(stdout.as_mut(), &mut out_buf),
                    read_pipe(stderr.as_mut(), &mut err_buf),
                );
                out?;
                err?;

                output.insert(STDOUT, out_buf);
                output.insert(STDERR, err_buf);
                status?
            } else {
                child.wait().await?
            };

            Ok::<_, std::io::Error>((status, output))
        };

        let result = timeout(Duration::from_secs(request.timeout), run).await;
        match result {
            Ok(Ok((status, output))) => {
                if output.values().any(|chunk| !chunk.is_empty()) {
                    sender
                        .send(ShellExecuteStreamResponse::Progress { output })
                        .await?;
                }
                sender
                    .send(ShellExecuteStreamResponse::Done {
                        exit_code: status.code().unwrap_or(-1),
                        duration: started.elapsed().as_secs_f64(),
                    })
                    .await?;
            }
            Ok(Err(_)) => sender.send(ShellExecuteStreamResponse::Failed).await?,
            Err(_) => {
                // Nothing is waiting on the child anymore, so leaving it
                // running would leak a process for the agent's lifetime.
                let _ = child.kill().await;
                sender.send(ShellExecuteStreamResponse::Timeout).await?;
            }
        }

        Ok(())
    }
}

/// Read a child's pipe to end, tolerating the pipe not being captured at all.
async fn read_pipe<R: AsyncRead + Unpin>(
    pipe: Option<&mut R>,
    buf: &mut Vec<u8>,
) -> std::io::Result<()> {
    match pipe {
        Some(pipe) => pipe.read_to_end(buf).await.map(|_| ()),
        None => Ok(()),
    }
}
