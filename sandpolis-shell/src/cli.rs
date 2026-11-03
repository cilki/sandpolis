use crate::ShellManager;
use crate::session::{
    ShellEvent, ShellSessionStreamRequest, ShellSessionStreamRequester, spawn_shell_stream,
};
use anyhow::{Result, bail};
use sandpolis_client::cli::TargetArgs;
use sandpolis_instance::InstanceId;
use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// Open an interactive shell on the target agent. With `--instance` this relays
/// the local terminal byte-for-byte to a PTY on the agent (like ssh); without
/// one it shows a placeholder (the agent picker isn't built yet). Noninteractive
/// (`--json`) operation is not implemented, and reports so.
pub async fn dispatch(target: TargetArgs, _shell: ShellManager) -> Result<ExitCode> {
    if target.json {
        println!("{{\"status\":\"unimplemented\",\"command\":\"shell\"}}");
        return Ok(ExitCode::FAILURE);
    }

    let Some(instance) = target.instance else {
        sandpolis_client::tui::run_tui(sandpolis_client::tui::PlaceholderPanel::new(
            "shell (pass --instance <id>)",
        ))
        .await?;
        return Ok(ExitCode::SUCCESS);
    };

    run_raw_session(instance).await
}

/// Restores the terminal's cooked mode on drop, so every exit path (including
/// errors) leaves the terminal usable.
struct RawModeGuard(bool);

impl RawModeGuard {
    /// Enter raw mode if stdin is a terminal. A non-terminal stdin (a piped
    /// script) still relays, it just has no mode to change.
    fn new() -> Self {
        let enabled =
            std::io::stdin().is_terminal() && crossterm::terminal::enable_raw_mode().is_ok();
        Self(enabled)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Relay the local terminal byte-for-byte to a shell on `instance`: raw mode
/// locally so every keystroke (arrows, Ctrl sequences) passes through to the
/// agent's PTY untouched, output written straight to stdout, and SIGWINCH
/// propagated as a resize. Ends when the remote shell exits, adopting its exit
/// code.
async fn run_raw_session(instance: InstanceId) -> Result<ExitCode> {
    let Some(conn) = sandpolis_client::sync::wait_for_connection(Duration::from_secs(10)).await
    else {
        bail!("No server connection");
    };

    // Match the remote PTY to the local terminal from the start
    let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));
    let mut environment = HashMap::new();
    if let Ok(term) = std::env::var("TERM") {
        environment.insert("TERM".to_string(), term);
    }

    let (requester, mut events) = ShellSessionStreamRequester::channel();
    let (outbound, outbound_rx) = tokio::sync::mpsc::channel(32);
    spawn_shell_stream(
        conn,
        instance,
        requester,
        ShellSessionStreamRequest::Start {
            path: PathBuf::from("/bin/sh"),
            environment,
            rows: rows as u32,
            cols: cols as u32,
        },
        outbound_rx,
    );

    let raw = RawModeGuard::new();

    // A plain thread rather than `tokio::io::stdin()`: tokio reads stdin on its
    // blocking pool, and shutting the runtime down waits for that read to
    // return. Once the remote shell exits nobody is typing, so the process
    // would hang until the next keypress. Process exit doesn't wait for this
    // thread.
    let stdin_outbound = outbound.clone();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let request = ShellSessionStreamRequest::Stdin {
                        data: buf[..n].to_vec(),
                    };
                    if stdin_outbound.blocking_send(request).is_err() {
                        break;
                    }
                }
            }
        }
    });

    #[cfg(unix)]
    {
        let resize_outbound = outbound.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let Ok(mut winch) = signal(SignalKind::window_change()) else {
                return;
            };
            while winch.recv().await.is_some() {
                let Ok((cols, rows)) = crossterm::terminal::size() else {
                    continue;
                };
                let request = ShellSessionStreamRequest::Resize {
                    rows: rows as u32,
                    cols: cols as u32,
                };
                if resize_outbound.send(request).await.is_err() {
                    break;
                }
            }
        });
    }

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let code = loop {
        match events.recv().await {
            Some(ShellEvent::Output(output)) => {
                if !output.stdout.is_empty() {
                    stdout.write_all(&output.stdout)?;
                    stdout.flush()?;
                }
                if !output.stderr.is_empty() {
                    stderr.write_all(&output.stderr)?;
                    stderr.flush()?;
                }
            }
            Some(ShellEvent::Exited(code)) => break code,
            // The stream was closed without an exit event: the server
            // couldn't reach the agent, or the connection went away.
            None => {
                drop(raw);
                eprintln!("Connection to {instance} lost (is it connected?)");
                return Ok(ExitCode::FAILURE);
            }
        }
    };

    Ok(match code {
        Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        // Killed by a signal (or the platform doesn't report codes)
        None => ExitCode::SUCCESS,
    })
}
