use crate::ProbeId;
use crate::ProbeManager;
use crate::ProbeType;
use crate::service::{ProbeServiceOp, ProbeServiceResponse, client as probe_service};
use anyhow::{Context, Result};
use clap::{Subcommand, ValueEnum};
use sandpolis_client::cli::TargetArgs;
use std::process::ExitCode;
use std::time::Duration;

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum ServiceProtocol {
    Docker,
    Libvirt,
}

impl From<ServiceProtocol> for ProbeType {
    fn from(protocol: ServiceProtocol) -> Self {
        match protocol {
            ServiceProtocol::Docker => ProbeType::Docker,
            ServiceProtocol::Libvirt => ProbeType::Libvirt,
        }
    }
}

#[derive(Subcommand, Debug, Clone)]
pub enum ServiceAction {
    /// List the device's containers/VMs
    List,
    /// Start a container/VM
    Start { id: String },
    /// Stop a container/VM (gracefully unless --force)
    Stop {
        id: String,
        /// Power a VM off instead of requesting a graceful shutdown
        #[clap(long)]
        force: bool,
    },
    /// Restart a container/VM
    Restart { id: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ProbeCommand {
    /// List the registered probe devices and their ids
    List,
    /// Manage service instances (containers/VMs) on a probe device
    Service {
        /// The registered device id (`probe-<base32>`, as `probe list` shows)
        #[clap(long)]
        device: ProbeId,

        /// The protocol to reach the device by
        #[clap(long)]
        protocol: ServiceProtocol,

        #[command(subcommand)]
        action: ServiceAction,
    },
}

/// Probe devices. The interactive device-list TUI is not yet built, so the bare
/// command currently opens a placeholder panel (or reports unimplemented for
/// `--json`).
pub async fn dispatch(
    action: Option<ProbeCommand>,
    target: TargetArgs,
    _layer: &ProbeManager,
) -> Result<ExitCode> {
    match action {
        Some(ProbeCommand::List) => return list(target).await,
        Some(ProbeCommand::Service {
            device,
            protocol,
            action,
        }) => return service(target, device, protocol.into(), action).await,
        None => {}
    }

    if target.json {
        println!("{{\"status\":\"unimplemented\",\"command\":\"probe\"}}");
        return Ok(ExitCode::FAILURE);
    }
    sandpolis_client::tui::run_tui(sandpolis_client::tui::PlaceholderPanel::new("probe")).await?;
    Ok(ExitCode::SUCCESS)
}

async fn list(target: TargetArgs) -> Result<ExitCode> {
    let conn = sandpolis_client::sync::wait_for_connection(Duration::from_secs(10))
        .await
        .context("no server connection")?;

    let devices = match crate::management::list_once(conn, Duration::from_secs(10)).await {
        Ok(devices) => devices,
        Err(e) => {
            if target.json {
                println!(
                    "{{\"status\":\"failed\",\"error\":{}}}",
                    serde_json::to_string(&e.to_string())?
                );
            } else {
                eprintln!("Request failed: {e}");
            }
            return Ok(ExitCode::FAILURE);
        }
    };

    if target.json {
        let devices: Vec<_> = devices
            .iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id.to_string(),
                    "name": d.device.name,
                    "ip": d.device.ip,
                    "gateway": d.gateway.to_string(),
                    "protocols": d
                        .device
                        .protocols()
                        .iter()
                        .map(|p| p.display_name())
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&devices)?);
    } else if devices.is_empty() {
        println!("No probe devices registered");
    } else {
        for d in &devices {
            let protocols: Vec<_> = d
                .device
                .protocols()
                .iter()
                .map(|p| p.display_name())
                .collect();
            println!(
                "{:<20} {:<24} {:<16} {}",
                d.id.to_string(),
                d.display_name(),
                d.device.ip,
                protocols.join(",")
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn service(
    target: TargetArgs,
    device: ProbeId,
    protocol: ProbeType,
    action: ServiceAction,
) -> Result<ExitCode> {
    let op = match action {
        ServiceAction::List => ProbeServiceOp::List,
        ServiceAction::Start { id } => ProbeServiceOp::Start { id },
        ServiceAction::Stop { id, force } => ProbeServiceOp::Stop { id, force },
        ServiceAction::Restart { id } => ProbeServiceOp::Restart { id },
    };

    let conn = sandpolis_client::sync::wait_for_connection(Duration::from_secs(10))
        .await
        .context("no server connection")?;

    let response =
        probe_service::request_once(conn, device.body(), protocol, op, Duration::from_secs(60))
            .await;

    match response {
        Ok(ProbeServiceResponse::Containers(containers)) => {
            if target.json {
                println!(
                    "{{\"status\":\"ok\",\"containers\":{}}}",
                    serde_json::to_string(&containers)?
                );
            } else {
                for c in &containers {
                    println!(
                        "{:<14} {:<24} {:<8} {}",
                        c.id,
                        c.name,
                        c.state.label(),
                        c.status.as_deref().unwrap_or("")
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Ok(ProbeServiceResponse::Domains(domains)) => {
            if target.json {
                println!(
                    "{{\"status\":\"ok\",\"domains\":{}}}",
                    serde_json::to_string(&domains)?
                );
            } else {
                for d in &domains {
                    println!(
                        "{:<24} {:<8} {}",
                        d.name,
                        d.state.label(),
                        d.status.as_deref().unwrap_or("")
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Ok(ProbeServiceResponse::Failed(reason)) => {
            if target.json {
                println!(
                    "{{\"status\":\"failed\",\"error\":{}}}",
                    serde_json::to_string(&reason)?
                );
            } else {
                eprintln!("Request failed: {reason}");
            }
            Ok(ExitCode::FAILURE)
        }
        Err(e) => {
            if target.json {
                println!(
                    "{{\"status\":\"failed\",\"error\":{}}}",
                    serde_json::to_string(&e.to_string())?
                );
            } else {
                eprintln!("Request failed: {e}");
            }
            Ok(ExitCode::FAILURE)
        }
    }
}
