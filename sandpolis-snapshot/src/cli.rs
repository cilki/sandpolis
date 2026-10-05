use crate::client;
use crate::streams::{SnapshotMgmtRequest, SnapshotMgmtResponse};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use sandpolis_client::cli::TargetArgs;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Subcommand, Debug, Clone)]
pub enum SnapshotCommand {
    /// List stored snapshots
    List {
        /// Only snapshots of this partition UUID
        #[clap(long)]
        partition: Option<String>,
    },
    /// Capture a new snapshot of a partition
    Create {
        /// The partition UUID to capture
        #[clap(long)]
        partition: String,

        /// Human-readable label for the snapshot
        #[clap(long)]
        label: Option<String>,
    },
    /// Restore a snapshot onto its partition
    Apply {
        /// The partition UUID to restore
        #[clap(long)]
        partition: String,

        /// The snapshot UUID to restore
        #[clap(long)]
        snapshot: String,
    },
    /// Delete a snapshot nothing else is backed by
    Delete {
        /// The partition UUID the snapshot belongs to
        #[clap(long)]
        partition: String,

        /// The snapshot UUID to delete
        #[clap(long)]
        snapshot: String,
    },
}

/// How long a noninteractive create/apply may run. Generous: a base capture
/// moves the whole partition.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(3600);

pub async fn dispatch(action: Option<SnapshotCommand>, target: TargetArgs) -> Result<ExitCode> {
    match action {
        Some(SnapshotCommand::List { partition }) => list(target, partition).await,
        Some(SnapshotCommand::Create { partition, label }) => {
            operate(
                target,
                |agent| SnapshotMgmtRequest::Create {
                    agent,
                    partition_uuid: partition.clone(),
                    label: label.clone(),
                },
                OPERATION_TIMEOUT,
            )
            .await
        }
        Some(SnapshotCommand::Apply {
            partition,
            snapshot,
        }) => {
            operate(
                target,
                |agent| SnapshotMgmtRequest::Apply {
                    agent,
                    partition_uuid: partition.clone(),
                    snapshot_uuid: snapshot.clone(),
                },
                OPERATION_TIMEOUT,
            )
            .await
        }
        Some(SnapshotCommand::Delete {
            partition,
            snapshot,
        }) => {
            operate(
                target,
                |agent| SnapshotMgmtRequest::Delete {
                    agent,
                    partition_uuid: partition.clone(),
                    snapshot_uuid: snapshot.clone(),
                },
                Duration::from_secs(30),
            )
            .await
        }
        // No subcommand: list noninteractively under `--json`, otherwise open a
        // placeholder browser. Without the `--json` arm the placeholder's TUI
        // would refuse to open and tell a scripted caller to pass the flag it
        // just passed.
        None if target.json => list(target, None).await,
        None => {
            sandpolis_client::tui::run_tui(sandpolis_client::tui::PlaceholderPanel::new(
                "snapshot browser (pass a subcommand, e.g. `snapshot list`)",
            ))
            .await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn list(target: TargetArgs, partition: Option<String>) -> Result<ExitCode> {
    sandpolis_client::sync::wait_for_connection(Duration::from_secs(30))
        .await
        .context("No server connection")?;

    match target.instance {
        Some(instance) => client::subscribe(instance),
        None => client::subscribe_everything(),
    }
    // Give the opening sync snapshot a moment to replicate down.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let snapshots: Vec<crate::SnapshotData> =
        sandpolis_client::sync::scan_latest::<crate::SnapshotData>()?
            .into_iter()
            .filter(|s| target.instance.is_none_or(|id| s._instance_id == id))
            .filter(|s| {
                partition
                    .as_deref()
                    .is_none_or(|uuid| s.partition_uuid == uuid)
            })
            .collect();

    if target.json {
        println!("{}", serde_json::to_string(&snapshots)?);
    } else if snapshots.is_empty() {
        println!("No snapshots");
    } else {
        for s in &snapshots {
            println!(
                "{} {} partition={} size={} stored={}{}",
                s._creation.timestamp().format("%Y-%m-%d %H:%M:%S"),
                s.uuid,
                s.partition_uuid,
                s.size,
                s.stored_size,
                s.label
                    .as_deref()
                    .map(|l| format!(" label={l}"))
                    .unwrap_or_default(),
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Run one management request against the target agent and report its outcome.
async fn operate(
    target: TargetArgs,
    request: impl Fn(sandpolis_instance::InstanceId) -> SnapshotMgmtRequest,
    timeout: Duration,
) -> Result<ExitCode> {
    let Some(instance) = target.instance else {
        bail!("--instance is required");
    };

    sandpolis_client::sync::wait_for_connection(Duration::from_secs(30))
        .await
        .context("No server connection")?;

    let mut rx = client::request(request(instance));
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let response = tokio::time::timeout_at(deadline, rx.recv()).await;
        let (status, error) = match response {
            Ok(Some(SnapshotMgmtResponse::Started)) => continue,
            Ok(Some(SnapshotMgmtResponse::Finished { snapshot_uuid, .. })) => {
                if target.json {
                    println!(
                        "{{\"status\":\"ok\",\"snapshot\":{}}}",
                        serde_json::to_string(&snapshot_uuid)?
                    );
                } else if let Some(uuid) = snapshot_uuid {
                    println!("{uuid}");
                }
                return Ok(ExitCode::SUCCESS);
            }
            Ok(Some(SnapshotMgmtResponse::Deleted)) => {
                if target.json {
                    println!("{{\"status\":\"ok\"}}");
                }
                return Ok(ExitCode::SUCCESS);
            }
            Ok(Some(SnapshotMgmtResponse::Failed(e))) => ("failed", e),
            Ok(None) => ("failed", "The server closed the stream".to_string()),
            Err(_) => ("timeout", "The operation timed out".to_string()),
        };

        if target.json {
            println!(
                "{{\"status\":\"{status}\",\"error\":{}}}",
                serde_json::to_string(&error)?
            );
        } else {
            eprintln!("{error}");
        }
        return Ok(ExitCode::FAILURE);
    }
}
