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
use pnet::datalink::{self, Channel, DataLinkReceiver, DataLinkSender, NetworkInterface};
use pnet::ipnetwork::{IpNetwork, Ipv4Network};
use pnet::packet::{MutablePacket, Packet};
use pnet::packet::arp::{ArpHardwareTypes, ArpOperations, ArpPacket, MutableArpPacket};
use pnet::packet::ethernet::{EtherTypes, EthernetPacket, MutableEthernetPacket};
use pnet::util::MacAddr;
use sandpolis_instance::LayerName;
use sandpolis_instance::notification::{self, Notification};
use sandpolis_instance::service::{Service, ServiceReport, ServiceSchedule};
use sandpolis_server::ServerUrl;
use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Refuse to sweep a subnet larger than this many addresses; a misconfigured
/// prefix (say a `/8`) would otherwise ARP millions of hosts.
const MAX_SUBNET_HOSTS: u64 = 1 << 16;

/// ARP requests are sent in bursts of this many frames...
const ARP_BURST: usize = 64;
/// ...with this pause between bursts, so sweeping a `/16` doesn't flood the
/// segment (or the NIC's transmit queue) with 65k broadcasts at line rate.
const ARP_BURST_PAUSE: Duration = Duration::from_millis(1);
/// How long the receiver blocks per read before rechecking whether to stop.
const ARP_READ_TIMEOUT: Duration = Duration::from_millis(100);
/// Ethernet header (14) + ARP payload for IPv4 over Ethernet (28).
const ARP_FRAME_LEN: usize = 42;

/// One subnet to sweep, and the interface to send from.
struct ScanTarget {
    network: Ipv4Network,
    /// Interface to send from. `None` picks the interface whose address is in
    /// `network`, falling back to the first usable one.
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

        // ARP uses raw sockets and pnet's send/receive block, so the sweep runs
        // off the async executor.
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
    let mut any_socket_ok = false;

    for target in targets {
        if cancel.is_cancelled() {
            break;
        }

        let Some((iface, mac, source)) = sweep_interface(target) else {
            warn!(
                iface = ?target.iface,
                network = %target.network,
                "Probe scan: no interface with a MAC and IPv4 address to sweep from"
            );
            continue;
        };

        let config = datalink::Config {
            read_timeout: Some(ARP_READ_TIMEOUT),
            // Replies are unicast to our own MAC; there's no need to see the
            // rest of the segment's traffic.
            promiscuous: false,
            ..Default::default()
        };
        let (tx, rx) = match datalink::channel(&iface, config) {
            Ok(Channel::Ethernet(tx, rx)) => {
                any_socket_ok = true;
                (tx, rx)
            }
            Ok(_) => {
                warn!(iface = %iface.name, "Probe scan: unsupported datalink channel type");
                continue;
            }
            Err(e) => {
                warn!(iface = %iface.name, error = %e, "Probe scan: cannot open ARP socket");
                continue;
            }
        };

        let network = target.network.network();
        let broadcast = target.network.broadcast();
        let hosts: Vec<Ipv4Addr> = target
            .network
            .iter()
            .filter(|host| *host != network && *host != broadcast && !skip.contains(host))
            .collect();

        debug!(
            iface = %iface.name,
            network = %target.network,
            hosts = hosts.len(),
            "Probe scan: ARP-sweeping subnet"
        );
        live.extend(sweep_subnet(tx, rx, mac, source, &hosts, timeout, cancel));
    }

    if !any_socket_ok {
        anyhow::bail!("could not open any ARP socket (discovery needs CAP_NET_RAW or root)");
    }

    live.sort_unstable();
    live.dedup();
    Ok(live)
}

/// The interface to sweep `target` from, with its MAC and the IPv4 address to
/// put in the requests' sender field.
///
/// The sender address is the interface's address *in the target subnet* when it
/// has one: some hosts drop ARP requests whose sender isn't on their segment.
fn sweep_interface(target: &ScanTarget) -> Option<(NetworkInterface, MacAddr, Ipv4Addr)> {
    let usable = |iface: &NetworkInterface| {
        iface.is_up()
            && !iface.is_loopback()
            && iface.mac.is_some_and(|mac| mac != MacAddr::zero())
            && iface.ips.iter().any(IpNetwork::is_ipv4)
    };
    let in_target = |iface: &NetworkInterface| {
        iface.ips.iter().any(|ip| match ip {
            IpNetwork::V4(net) => target.network.contains(net.ip()),
            IpNetwork::V6(_) => false,
        })
    };

    let interfaces = datalink::interfaces();
    let iface = match &target.iface {
        Some(name) => interfaces.into_iter().find(|i| &i.name == name),
        None => {
            let candidates: Vec<_> = interfaces.into_iter().filter(usable).collect();
            candidates
                .iter()
                .find(|i| in_target(i))
                .or(candidates.first())
                .cloned()
        }
    }
    .filter(usable)?;

    let mac = iface.mac?;
    let v4 = || {
        iface.ips.iter().filter_map(|ip| match ip {
            IpNetwork::V4(net) => Some(net.ip()),
            IpNetwork::V6(_) => None,
        })
    };
    let source = v4()
        .find(|ip| target.network.contains(*ip))
        .or_else(|| v4().next())?;
    Some((iface, mac, source))
}

/// Broadcast a who-has for every host, then collect replies until `timeout`
/// after the last request goes out.
///
/// Sending and receiving run concurrently (the receiver on its own thread), so
/// the whole subnet costs roughly one `timeout` rather than one per silent host,
/// and replies can't pile up in the socket's receive buffer while we're still
/// transmitting.
fn sweep_subnet(
    mut tx: Box<dyn DataLinkSender>,
    mut rx: Box<dyn DataLinkReceiver>,
    mac: MacAddr,
    source: Ipv4Addr,
    hosts: &[Ipv4Addr],
    timeout: Duration,
    cancel: &CancellationToken,
) -> HashSet<Ipv4Addr> {
    let wanted: HashSet<Ipv4Addr> = hosts.iter().copied().collect();
    let stop = AtomicBool::new(false);

    std::thread::scope(|scope| {
        let receiver = scope.spawn(|| {
            let mut found = HashSet::new();
            while !stop.load(Ordering::Relaxed) {
                match rx.next() {
                    Ok(frame) => {
                        if let Some(ip) = parse_arp_reply(frame, mac)
                            && wanted.contains(&ip)
                        {
                            found.insert(ip);
                        }
                    }
                    // The read timeout elapsing is how the loop gets to recheck
                    // `stop`.
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                        ) => {}
                    Err(e) => {
                        warn!(error = %e, "Probe scan: ARP receive failed");
                        break;
                    }
                }
            }
            found
        });

        let mut frame = [0u8; ARP_FRAME_LEN];
        let mut send_error_logged = false;
        for (i, host) in hosts.iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            if i > 0 && i % ARP_BURST == 0 {
                std::thread::sleep(ARP_BURST_PAUSE);
            }
            build_arp_request(&mut frame, mac, source, *host);
            if let Some(Err(e)) = tx.send_to(&frame, None)
                && !send_error_logged
            {
                // Typically ENOBUFS under load. One host missing a request just
                // means it isn't found this pass; don't log 65k times.
                warn!(error = %e, "Probe scan: ARP send failed");
                send_error_logged = true;
            }
        }

        // Late replies: wait out the timeout, in slices so cancellation is
        // prompt.
        let deadline = Instant::now() + timeout;
        while !cancel.is_cancelled() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep((deadline - now).min(ARP_READ_TIMEOUT));
        }

        stop.store(true, Ordering::Relaxed);
        receiver.join().unwrap_or_default()
    })
}

/// Write a broadcast ARP who-has for `target` into `frame`.
fn build_arp_request(
    frame: &mut [u8; ARP_FRAME_LEN],
    mac: MacAddr,
    source: Ipv4Addr,
    target: Ipv4Addr,
) {
    // Neither constructor can fail: the buffer is exactly the right size.
    let mut ethernet = MutableEthernetPacket::new(frame).expect("ethernet frame fits");
    ethernet.set_destination(MacAddr::broadcast());
    ethernet.set_source(mac);
    ethernet.set_ethertype(EtherTypes::Arp);

    let mut arp = MutableArpPacket::new(ethernet.payload_mut()).expect("ARP payload fits");
    arp.set_hardware_type(ArpHardwareTypes::Ethernet);
    arp.set_protocol_type(EtherTypes::Ipv4);
    arp.set_hw_addr_len(6);
    arp.set_proto_addr_len(4);
    arp.set_operation(ArpOperations::Request);
    arp.set_sender_hw_addr(mac);
    arp.set_sender_proto_addr(source);
    arp.set_target_hw_addr(MacAddr::zero());
    arp.set_target_proto_addr(target);
}

/// If `frame` is an ARP reply addressed to `own_mac`, the IPv4 address it
/// announces.
fn parse_arp_reply(frame: &[u8], own_mac: MacAddr) -> Option<Ipv4Addr> {
    let ethernet = EthernetPacket::new(frame)?;
    if ethernet.get_ethertype() != EtherTypes::Arp {
        return None;
    }
    let arp = ArpPacket::new(ethernet.payload())?;
    if arp.get_operation() != ArpOperations::Reply
        || arp.get_protocol_type() != EtherTypes::Ipv4
        || arp.get_target_hw_addr() != own_mac
    {
        return None;
    }
    Some(arp.get_sender_proto_addr())
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
    // Collected up front: a lazy `flat_map` over borrowed slices held across
    // the awaits below trips rustc's higher-ranked lifetime check on the
    // `Service::run` future's `Send` bound.
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

    const OURS: MacAddr = MacAddr(0x02, 0, 0, 0, 0, 0x01);
    const THEIRS: MacAddr = MacAddr(0x02, 0, 0, 0, 0, 0x02);

    /// Turn a request built by [`build_arp_request`] into the reply its target
    /// would send back.
    fn reply_to(request: &[u8; ARP_FRAME_LEN], from: MacAddr) -> [u8; ARP_FRAME_LEN] {
        let mut reply = *request;
        let mut ethernet = MutableEthernetPacket::new(&mut reply).unwrap();
        let requester = ethernet.get_source();
        ethernet.set_destination(requester);
        ethernet.set_source(from);
        let mut arp = MutableArpPacket::new(ethernet.payload_mut()).unwrap();
        let (asked, asker) = (arp.get_target_proto_addr(), arp.get_sender_proto_addr());
        arp.set_operation(ArpOperations::Reply);
        arp.set_sender_hw_addr(from);
        arp.set_sender_proto_addr(asked);
        arp.set_target_hw_addr(requester);
        arp.set_target_proto_addr(asker);
        reply
    }

    #[test]
    fn arp_request_is_a_broadcast_who_has() {
        let mut frame = [0u8; ARP_FRAME_LEN];
        let (source, target) = (Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 7));
        build_arp_request(&mut frame, OURS, source, target);

        let ethernet = EthernetPacket::new(&frame).unwrap();
        assert_eq!(ethernet.get_destination(), MacAddr::broadcast());
        assert_eq!(ethernet.get_source(), OURS);
        assert_eq!(ethernet.get_ethertype(), EtherTypes::Arp);

        let arp = ArpPacket::new(ethernet.payload()).unwrap();
        assert_eq!(arp.get_operation(), ArpOperations::Request);
        assert_eq!(arp.get_sender_hw_addr(), OURS);
        assert_eq!(arp.get_sender_proto_addr(), source);
        assert_eq!(arp.get_target_proto_addr(), target);

        // Our own broadcast echoing back is not a reply.
        assert_eq!(parse_arp_reply(&frame, OURS), None);
    }

    #[test]
    fn arp_reply_yields_the_announced_address() {
        let mut request = [0u8; ARP_FRAME_LEN];
        let target = Ipv4Addr::new(10, 0, 0, 7);
        build_arp_request(&mut request, OURS, Ipv4Addr::new(10, 0, 0, 1), target);
        let reply = reply_to(&request, THEIRS);

        assert_eq!(parse_arp_reply(&reply, OURS), Some(target));
        // A reply to some other host's request isn't ours to count.
        assert_eq!(parse_arp_reply(&reply, THEIRS), None);
        // Truncated frames are ignored rather than panicking.
        assert_eq!(parse_arp_reply(&reply[..20], OURS), None);
    }
}
