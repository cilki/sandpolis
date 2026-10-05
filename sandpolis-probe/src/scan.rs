//! Probe "scanning mode": periodic discovery of network devices.
//!
//! Each pass ARP-sweeps the configured (or auto-detected) local subnets to find
//! live hosts, then TCP-connects to each host's candidate protocol ports to see
//! what it speaks. Every host that answers on at least one port is registered as
//! a credential-less [`DeviceConfig`] via [`crate::management::add_discovered`],
//! so it persists and streams to clients exactly like a hand-registered device.
//! An operator fills in credentials afterward by editing the device in the realm
//! config.
//!
//! ARP needs raw sockets, so the service only works with `CAP_NET_RAW` (or
//! root), and only reaches hosts on the server's own L2 segments.

use crate::ProbeType;
use crate::config::{DeviceConfig, ScanConfig};
use anyhow::Result;
use futures::stream::StreamExt;
use libarp::client::ArpClient;
use pnet::datalink;
use pnet::ipnetwork::{IpNetwork, Ipv4Network};
use sandpolis_instance::LayerName;
use sandpolis_instance::notification::{self, Notification};
use sandpolis_instance::service::{Service, ServiceReport, ServiceSchedule};
use sandpolis_server::ServerUrl;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Refuse to sweep a subnet larger than this many addresses; a misconfigured
/// prefix (say a `/8`) would otherwise ARP millions of hosts.
const MAX_SUBNET_HOSTS: u64 = 1 << 16;

/// One subnet to sweep, and the interface to send from.
struct ScanTarget {
    network: Ipv4Network,
    /// Interface name to bind the ARP client to; `None` lets the client guess.
    iface: Option<String>,
}

/// Discovers reachable probe devices on the server's local network.
pub struct ScanService {
    config: ScanConfig,
    /// Stamped onto each discovered device so persistence routes it to the right
    /// realm. `None` when the server's own URL isn't known.
    server: Option<ServerUrl>,
}

impl ScanService {
    pub fn new(config: ScanConfig, server: Option<ServerUrl>) -> Self {
        Self { config, server }
    }

    /// The subnets to sweep this pass: the configured CIDRs, or every suitable
    /// local interface subnet when none are configured.
    fn resolve_targets(&self) -> Vec<ScanTarget> {
        if self.config.networks.is_empty() {
            auto_detect_targets()
        } else {
            configured_targets(&self.config.networks)
        }
    }
}

impl Service for ScanService {
    fn name(&self) -> &'static str {
        "scan"
    }

    fn layer(&self) -> LayerName {
        LayerName::from("Probe")
    }

    fn description(&self) -> &'static str {
        "Discovers reachable probe devices on the local network"
    }

    fn schedule(&self) -> ServiceSchedule {
        ServiceSchedule::every(Duration::from_secs(self.config.interval.max(60)))
    }

    async fn run(&self, cancel: CancellationToken) -> Result<ServiceReport> {
        let mut report = ServiceReport::default();

        let targets = self.resolve_targets();
        if targets.is_empty() {
            debug!("Probe scan: no target networks to sweep");
            return Ok(report);
        }

        let local_ips = local_ipv4_addrs();

        // ARP uses raw sockets and pnet's receive blocks, so the sweep runs off
        // the async executor.
        let sweep_cancel = cancel.clone();
        let arp_timeout = Duration::from_millis(self.config.arp_timeout_ms.max(1));
        let live =
            tokio::task::spawn_blocking(move || arp_sweep(&targets, arp_timeout, &local_ips, &sweep_cancel))
                .await??;

        report.scanned = live.len() as u64;
        if live.is_empty() || cancel.is_cancelled() {
            return Ok(report);
        }

        let ports = scan_ports();
        let connect_timeout = Duration::from_millis(self.config.connect_timeout_ms.max(1));
        let detected = port_scan(&live, &ports, connect_timeout, self.config.concurrency.max(1), &cancel).await;

        let new_devices: Vec<DeviceConfig> = detected
            .into_iter()
            .filter(|(_, protocols)| !protocols.is_empty())
            .map(|(ip, protocols)| {
                let mut device = DeviceConfig {
                    ip: IpAddr::V4(ip),
                    server: self.server.clone(),
                    ..Default::default()
                };
                for protocol in protocols {
                    device.add_detected(protocol);
                }
                device
            })
            .collect();

        let added = crate::management::add_discovered(new_devices);
        report.updated = added as u64;
        if added > 0 {
            notification::notify(
                Notification::warn("Probe", format!("Discovered {added} new device(s)"))
                    .body("Fill in credentials by editing the devices in the realm config"),
            );
        }

        Ok(report)
    }
}

/// The distinct ports to probe, each mapped to the protocol it implies. Ports
/// shared by several protocols (HTTP and ONVIF both use 80) resolve to the first
/// in [`ProbeType::all`] order, so a bare open port isn't reported as a protocol
/// that needs its own handshake to confirm.
fn scan_ports() -> Vec<(u16, ProbeType)> {
    let mut seen = HashSet::new();
    ProbeType::all()
        .iter()
        .filter_map(|t| t.default_port().map(|port| (port, *t)))
        .filter(|(port, _)| seen.insert(*port))
        .collect()
}

/// Suitable local subnets derived from the server's own interfaces.
fn auto_detect_targets() -> Vec<ScanTarget> {
    let mut targets = Vec::new();
    for iface in datalink::interfaces() {
        if iface.is_loopback() || !iface.is_up() {
            continue;
        }
        for ip in &iface.ips {
            let IpNetwork::V4(network) = ip else {
                continue;
            };
            let addr = network.ip();
            if addr.is_loopback() || addr.is_link_local() {
                continue;
            }
            if subnet_size(network) > MAX_SUBNET_HOSTS {
                debug!(iface = %iface.name, %network, "Probe scan: skipping oversized local subnet");
                continue;
            }
            targets.push(ScanTarget {
                network: *network,
                iface: Some(iface.name.clone()),
            });
        }
    }
    targets
}

/// Parse operator-configured CIDR ranges. IPv6 and unparseable entries are
/// skipped with a warning; ARP is IPv4-only.
fn configured_targets(networks: &[String]) -> Vec<ScanTarget> {
    let mut targets = Vec::new();
    for cidr in networks {
        match cidr.parse::<Ipv4Network>() {
            Ok(network) if subnet_size(&network) <= MAX_SUBNET_HOSTS => {
                targets.push(ScanTarget {
                    network,
                    iface: None,
                });
            }
            Ok(network) => {
                warn!(%network, "Probe scan: configured subnet is too large to sweep");
            }
            Err(e) => {
                warn!(cidr = %cidr, error = %e, "Probe scan: ignoring invalid scan network");
            }
        }
    }
    targets
}

/// Number of addresses in a subnet, from its prefix length.
fn subnet_size(network: &Ipv4Network) -> u64 {
    1u64 << (32 - network.prefix() as u32)
}

/// Every IPv4 address bound to a local interface, so the sweep never registers
/// the server itself as a device.
fn local_ipv4_addrs() -> HashSet<Ipv4Addr> {
    datalink::interfaces()
        .iter()
        .flat_map(|iface| iface.ips.iter())
        .filter_map(|ip| match ip {
            IpNetwork::V4(net) => Some(net.ip()),
            IpNetwork::V6(_) => None,
        })
        .collect()
}

/// ARP-sweep every target subnet and return the live hosts. Blocking: opens raw
/// sockets and waits on ARP replies. Errors only when no interface could open an
/// ARP socket at all (usually missing `CAP_NET_RAW`).
fn arp_sweep(
    targets: &[ScanTarget],
    timeout: Duration,
    skip: &HashSet<Ipv4Addr>,
    cancel: &CancellationToken,
) -> Result<Vec<Ipv4Addr>> {
    let mut live = Vec::new();
    let mut any_client_ok = false;

    for target in targets {
        if cancel.is_cancelled() {
            break;
        }

        let client = match &target.iface {
            Some(name) => ArpClient::new_with_iface_name(name),
            None => ArpClient::new(),
        };
        let mut client = match client {
            Ok(client) => {
                any_client_ok = true;
                client
            }
            Err(e) => {
                warn!(iface = ?target.iface, error = %e, "Probe scan: cannot open ARP socket");
                continue;
            }
        };

        let network = target.network.network();
        let broadcast = target.network.broadcast();
        for host in target.network.iter() {
            if cancel.is_cancelled() {
                break;
            }
            if host == network || host == broadcast || skip.contains(&host) {
                continue;
            }
            // `ip_to_mac` is only nominally async: it waits on pnet's blocking
            // receive, so there is nothing for an executor to interleave.
            if futures::executor::block_on(client.ip_to_mac(host, Some(timeout))).is_ok() {
                live.push(host);
            }
        }
    }

    if !any_client_ok {
        anyhow::bail!("could not open any ARP socket (discovery needs CAP_NET_RAW or root)");
    }

    live.sort_unstable();
    live.dedup();
    Ok(live)
}

/// TCP-connect to each candidate port on each live host, returning the protocols
/// each host answered on. Bounded to `concurrency` connections in flight.
async fn port_scan(
    live: &[Ipv4Addr],
    ports: &[(u16, ProbeType)],
    timeout: Duration,
    concurrency: usize,
    cancel: &CancellationToken,
) -> HashMap<Ipv4Addr, Vec<ProbeType>> {
    // Collected rather than left lazy: a borrowing iterator held across the
    // awaits below makes the whole future fail the `Send` bound `Service::run`
    // requires.
    let probes: Vec<(Ipv4Addr, u16, ProbeType)> = live
        .iter()
        .flat_map(|ip| ports.iter().map(move |(port, protocol)| (*ip, *port, *protocol)))
        .collect();

    let hits = futures::stream::iter(probes)
        .map(|(ip, port, protocol)| async move {
            if cancel.is_cancelled() {
                return None;
            }
            let addr = SocketAddr::new(IpAddr::V4(ip), port);
            match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
                Ok(Ok(_stream)) => Some((ip, protocol)),
                _ => None,
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;

    let mut detected: HashMap<Ipv4Addr, Vec<ProbeType>> = HashMap::new();
    for (ip, protocol) in hits.into_iter().flatten() {
        detected.entry(ip).or_default().push(protocol);
    }
    detected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_ports_are_unique_and_drop_shared_port() {
        let ports = scan_ports();
        let unique: HashSet<u16> = ports.iter().map(|(p, _)| *p).collect();
        assert_eq!(unique.len(), ports.len(), "each port probed once");

        // Port 80 is shared by HTTP and ONVIF; HTTP wins, ONVIF is dropped.
        assert!(ports.contains(&(80, ProbeType::Http)));
        assert!(!ports.iter().any(|(_, t)| *t == ProbeType::Onvif));
        assert!(ports.contains(&(22, ProbeType::Ssh)));
    }

    #[test]
    fn subnet_size_from_prefix() {
        assert_eq!(subnet_size(&"10.0.0.0/24".parse().unwrap()), 256);
        assert_eq!(subnet_size(&"10.0.0.0/30".parse().unwrap()), 4);
        assert_eq!(subnet_size(&"10.0.0.0/16".parse().unwrap()), 65536);
    }
}
