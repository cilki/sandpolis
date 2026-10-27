//! SSH sessions against probe devices.
//!
//! A probe isn't an instance and can't run an agent, so there's no PTY responder
//! to relay to. Instead the device's owning *server* holds the SSH connection and
//! this stream carries the same bytes an agent session would: the client sends
//! keystrokes and resizes, the server sends back terminal output. Reusing
//! [`ShellSessionStreamResponse`] as the response type means the whole client-side
//! terminal — decoder, alacritty grid, renderer — is shared with agent sessions.
//!
//! Credentials never leave the server. The client sends only a device id, which
//! the server resolves against the probe subsystem's device registry.

use crate::session::ShellSessionStreamResponse;
use serde::{Deserialize, Serialize};

/// Request message for SSH probe sessions.
#[derive(Serialize, Deserialize)]
pub enum SshSessionStreamRequest {
    /// Open a shell on the probe device with this id. The server looks the
    /// device's credentials up itself.
    Start {
        device_id: u64,
        /// Number of rows to request
        rows: u32,
        /// Number of columns to request
        cols: u32,
    },
    /// Requester has stdin data
    Stdin { data: Vec<u8> },
    /// Requester changed the size of the terminal
    Resize { rows: u32, cols: u32 },
}

#[cfg(all(feature = "server", feature = "probe"))]
mod server {
    use super::*;
    use anyhow::{Result, bail};
    use russh::ChannelMsg;
    use russh::client::{self, AuthResult, Handle};
    use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
    use sandpolis_instance::network::StreamResponder;
    use sandpolis_macros::Stream;
    use sandpolis_probe::config::SshProbeConfig;
    use std::net::IpAddr;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tokio::sync::mpsc::{Sender, channel};
    use tokio_util::sync::CancellationToken;
    use tracing::{debug, warn};

    /// What the responder forwards to a running session task.
    enum SshCommand {
        Stdin(Vec<u8>),
        Resize { rows: u32, cols: u32 },
    }

    /// Stream responder that holds an SSH connection to a probe device.
    ///
    /// The connection is owned by a background task that
    /// [`Start`](SshSessionStreamRequest::Start) spawns before returning.
    /// Connecting inline would stall the whole connection's dispatch loop, since
    /// responder handlers run on the socket's receive path — and that same loop
    /// is what flushes outbound messages, so not a byte would ever reach the
    /// client.
    #[derive(Stream, Default)]
    pub struct SshSessionStreamResponder {
        /// Cancels the background session task (on drop).
        cancel: CancellationToken,
        /// Sends keystrokes and resizes to that task. `None` until `Start`.
        input: RwLock<Option<Sender<SshCommand>>>,
    }

    impl StreamResponder for SshSessionStreamResponder {
        type In = SshSessionStreamRequest;
        type Out = ShellSessionStreamResponse;

        async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
            match request {
                SshSessionStreamRequest::Start {
                    device_id,
                    rows,
                    cols,
                } => {
                    let Some(device) = sandpolis_probe::REGISTERED_DEVICES
                        .read()
                        .ok()
                        .and_then(|devices| {
                            devices.iter().find(|d| d.id.body() == device_id).cloned()
                        })
                    else {
                        report(&sender, format!("device {device_id} is not registered")).await;
                        return Ok(());
                    };
                    let Some(config) = device.device.ssh.clone() else {
                        report(&sender, "device has no SSH configuration".into()).await;
                        return Ok(());
                    };

                    let (input, rx) = channel(64);
                    *self.input.write().await = Some(input);

                    let ip = device.device.ip;
                    let cancel = self.cancel.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            run_session(config, ip, rows, cols, rx, sender.clone(), cancel).await
                        {
                            report(&sender, e.to_string()).await;
                        }
                    });
                }
                SshSessionStreamRequest::Stdin { data } => {
                    // Dropped if the session hasn't opened yet, same as the
                    // agent responder does with stdin before `Start`.
                    if let Some(input) = self.input.read().await.as_ref() {
                        let _ = input.send(SshCommand::Stdin(data)).await;
                    }
                }
                SshSessionStreamRequest::Resize { rows, cols } => {
                    if let Some(input) = self.input.read().await.as_ref() {
                        let _ = input.send(SshCommand::Resize { rows, cols }).await;
                    }
                }
            }
            Ok(())
        }
    }

    impl Drop for SshSessionStreamResponder {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    /// Surface a failure as terminal output.
    ///
    /// [`ShellSessionStreamResponse`] has no error variant, and inventing one
    /// would fork the response type away from agent sessions. Writing the message
    /// into the terminal instead means a bad password or a fingerprint mismatch
    /// is visible in the panel rather than looking like a hung session.
    async fn report(sender: &Sender<ShellSessionStreamResponse>, message: String) {
        warn!(message = %message, "SSH probe session failed");
        let _ = sender
            .send(ShellSessionStreamResponse::Output {
                stdout: format!("\r\n[sandpolis] ssh: {message}\r\n").into_bytes(),
                stderr: Vec::new(),
            })
            .await;
    }

    /// Verifies the server's host key against the configured fingerprint.
    struct SshHandler {
        /// Expected SHA256 fingerprint, with or without the `SHA256:` prefix.
        fingerprint: Option<String>,
    }

    impl client::Handler for SshHandler {
        type Error = russh::Error;

        async fn check_server_key(
            &mut self,
            key: &PublicKeyOrCertificate,
        ) -> Result<bool, Self::Error> {
            // A host certificate carries the host key; the fingerprint is of
            // that key either way, so an operator configures the same value
            // regardless.
            let actual = key.public_key().fingerprint(HashAlg::Sha256).to_string();
            let Some(expected) = self.fingerprint.as_deref() else {
                // Trust on first use. Loud, because it's the one case where a
                // man in the middle goes unnoticed.
                warn!(
                    fingerprint = %actual,
                    "Probe has no configured SSH fingerprint; accepting host key unverified"
                );
                return Ok(true);
            };
            let matches = expected == actual
                || actual
                    .strip_prefix("SHA256:")
                    .is_some_and(|stripped| expected == stripped);
            if !matches {
                warn!(expected = %expected, actual = %actual, "SSH host key mismatch");
            }
            Ok(matches)
        }
    }

    /// Connect, authenticate, open a shell, and pump bytes until cancelled.
    async fn run_session(
        config: SshProbeConfig,
        ip: IpAddr,
        rows: u32,
        cols: u32,
        mut commands: tokio::sync::mpsc::Receiver<SshCommand>,
        sender: Sender<ShellSessionStreamResponse>,
        cancel: CancellationToken,
    ) -> Result<()> {
        // The config's host wins, but a device registered with only an IP is
        // perfectly usable.
        let host = if config.host.is_empty() {
            ip.to_string()
        } else {
            config.host.clone()
        };
        let port = config.port.unwrap_or(22);
        let username = config.username.clone().unwrap_or_else(|| "root".into());

        let mut handle: Handle<SshHandler> = client::connect(
            Arc::new(client::Config::default()),
            (host.as_str(), port),
            SshHandler {
                fingerprint: config.fingerprint.clone(),
            },
        )
        .await?;

        let auth = if let Some(path) = config.private_key_path.as_deref() {
            let key = load_secret_key(path, config.password.as_deref())?;
            let hash_alg = handle.best_supported_rsa_hash().await?.flatten();
            handle
                .authenticate_publickey(
                    &username,
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
                )
                .await?
        } else {
            handle
                .authenticate_password(&username, config.password.clone().unwrap_or_default())
                .await?
        };
        if !matches!(auth, AuthResult::Success) {
            bail!("authentication failed for {username}@{host}");
        }

        let channel = handle.channel_open_session().await?;
        channel
            .request_pty(
                true,
                "xterm-256color",
                cols.max(1),
                rows.max(1),
                0,
                0,
                &[],
            )
            .await?;
        channel.request_shell(true).await?;
        debug!(host = %host, "SSH probe session opened");

        let (mut read, write) = channel.split();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                message = read.wait() => match message {
                    Some(ChannelMsg::Data { data }) => {
                        if sender
                            .send(ShellSessionStreamResponse::Output {
                                stdout: data.to_vec(),
                                stderr: Vec::new(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if sender
                            .send(ShellSessionStreamResponse::Output {
                                stdout: Vec::new(),
                                stderr: data.to_vec(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                    Some(_) => {}
                },
                command = commands.recv() => match command {
                    Some(SshCommand::Stdin(data)) => write.data_bytes(data).await?,
                    Some(SshCommand::Resize { rows, cols }) => {
                        write.window_change(cols.max(1), rows.max(1), 0, 0).await?
                    }
                    None => break,
                },
            }
        }

        debug!(host = %host, "SSH probe session closed");
        // SSH doesn't report the shell's exit code through this channel; the
        // event still lets the client notice the session ended.
        let _ = sender
            .send(ShellSessionStreamResponse::Exited { code: None })
            .await;
        Ok(())
    }
}

#[cfg(all(feature = "server", feature = "probe"))]
pub use server::SshSessionStreamResponder;

#[cfg(all(feature = "client", feature = "probe"))]
mod client {
    use super::*;
    use crate::session::{ShellEvent, ShellOutput};
    use anyhow::Result;
    use sandpolis_instance::network::StreamRequester;
    use sandpolis_macros::Stream;
    use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};

    /// Client side of an SSH probe session. Identical in shape to
    /// [`ShellSessionStreamRequester`](crate::session::ShellSessionStreamRequester)
    /// so both feed the same terminal.
    #[derive(Stream)]
    pub struct SshSessionStreamRequester {
        output: UnboundedSender<ShellEvent>,
    }

    impl SshSessionStreamRequester {
        /// Construct a requester paired with the receiver the GUI drains.
        pub fn channel() -> (Self, UnboundedReceiver<ShellEvent>) {
            let (output, rx) = unbounded_channel();
            (Self { output }, rx)
        }
    }

    impl StreamRequester for SshSessionStreamRequester {
        type In = ShellSessionStreamResponse;
        type Out = SshSessionStreamRequest;

        async fn new(initial: Self::Out, tx: Sender<Self::Out>) -> Result<Self> {
            tx.send(initial).await?;
            // The GUI-facing constructor is `channel()`; this trait path has no
            // receiver attached, so decoded output is discarded.
            let (output, _rx) = unbounded_channel();
            Ok(Self { output })
        }

        async fn on_message(&self, response: Self::In, _tx: Sender<Self::Out>) -> Result<()> {
            let event = match response {
                ShellSessionStreamResponse::Output { stdout, stderr } => {
                    ShellEvent::Output(ShellOutput { stdout, stderr })
                }
                ShellSessionStreamResponse::Exited { code } => ShellEvent::Exited(code),
            };
            // GUI receiver may be gone (controller closed); dropping is fine.
            let _ = self.output.send(event);
            Ok(())
        }
    }
}

#[cfg(all(feature = "client", feature = "probe"))]
pub use client::SshSessionStreamRequester;
