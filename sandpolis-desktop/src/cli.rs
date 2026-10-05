use crate::DesktopManager;
use crate::screenshot::{
    DesktopScreenshotRequest, DesktopScreenshotRequester, DesktopScreenshotResult,
};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use sandpolis_client::cli::TargetArgs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Subcommand, Debug, Clone)]
pub enum DesktopCommand {
    /// Capture a single screenshot from the target agent
    Screenshot {
        /// Display identifier on the agent (default: first display)
        #[clap(long, default_value = "")]
        desktop: String,

        /// Output PNG path (default: raw PNG to stdout)
        #[clap(long)]
        output: Option<PathBuf>,
    },
}

pub async fn dispatch(
    action: Option<DesktopCommand>,
    target: TargetArgs,
    _layer: &DesktopManager,
) -> Result<ExitCode> {
    match action {
        Some(DesktopCommand::Screenshot { desktop, output }) => {
            screenshot(target, desktop, output).await
        }
        // The viewer is interactive by nature, so there's nothing to show a
        // scripted caller. Say that, rather than letting the TUI refuse to open
        // and advise passing the `--json` that's already here.
        None if target.json => {
            println!("{{\"status\":\"unimplemented\",\"command\":\"desktop\"}}");
            Ok(ExitCode::FAILURE)
        }
        None => {
            // With --instance, open the live terminal viewer pointed at that
            // agent; without one, show a placeholder (the agent picker isn't
            // built yet).
            match target.instance {
                Some(instance) => {
                    let widget = crate::client::tui::DesktopViewerWidget::new(instance);
                    sandpolis_client::tui::run_tui(widget).await?;
                }
                None => {
                    sandpolis_client::tui::run_tui(sandpolis_client::tui::PlaceholderPanel::new(
                        "desktop viewer (pass --instance <id>)",
                    ))
                    .await?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn screenshot(
    target: TargetArgs,
    desktop: String,
    output: Option<PathBuf>,
) -> Result<ExitCode> {
    let Some(instance) = target.instance else {
        bail!("--instance is required for screenshot");
    };

    let conn = sandpolis_client::sync::wait_for_connection(Duration::from_secs(10))
        .await
        .context("no server connection")?;

    let (requester, mut rx) = DesktopScreenshotRequester::channel();
    let (id, _tx) = conn
        .open_stream_to(
            instance,
            requester,
            DesktopScreenshotRequest {
                desktop_uuid: desktop,
            },
        )
        .await?;

    let result = tokio::time::timeout(Duration::from_secs(20), rx.recv()).await;
    conn.close_stream(id);

    let bytes = match result {
        Ok(Some(DesktopScreenshotResult::Ok(png))) => png,
        Ok(Some(DesktopScreenshotResult::Failed)) => {
            if target.json {
                println!("{{\"status\":\"failed\"}}");
            } else {
                eprintln!("Screenshot capture failed");
            }
            return Ok(ExitCode::FAILURE);
        }
        _ => {
            if target.json {
                println!("{{\"status\":\"timeout\"}}");
            } else {
                eprintln!("Timed out waiting for screenshot");
            }
            return Ok(ExitCode::FAILURE);
        }
    };

    if let Some(path) = &output {
        std::fs::write(path, &bytes)?;
    } else if !target.json {
        use std::io::Write;
        std::io::stdout().write_all(&bytes)?;
    }

    if target.json {
        match &output {
            Some(p) => println!(
                "{{\"status\":\"ok\",\"bytes\":{},\"output\":{:?}}}",
                bytes.len(),
                p.display().to_string()
            ),
            None => println!("{{\"status\":\"ok\",\"bytes\":{}}}", bytes.len()),
        }
    }

    Ok(ExitCode::SUCCESS)
}
