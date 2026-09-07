//! Probe subsystem for monitoring and managing various device types.
//!
//! Probes are lightweight monitoring endpoints that can be registered on agents
//! to represent devices such as SSH hosts, IPMI-enabled servers, UPS devices,
//! cameras, and more.

use config::{
    DeviceConfig, DockerProbeConfig, HttpProbeConfig, NfsProbeConfig, OnvifProbeConfig,
    ProbeManagerConfig, RdpProbeConfig, RtspProbeConfig, SmbProbeConfig, SnmpVersion,
    SshProbeConfig, VncProbeConfig,
};
use sandpolis_instance::InstanceId;
use sandpolis_instance::config::ConfigPersistHook;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock, OnceLock, RwLock};

pub mod config;
pub mod docker;
pub mod filesystem;
pub mod http;
pub mod ipmi;
pub mod libvirt;
pub mod management;
pub mod nfs;
pub mod onvif;
pub mod rdp;
pub mod rtsp;
#[cfg(feature = "server")]
pub mod scan;
pub mod service;
pub mod smb;
pub mod snmp;
pub mod ssh;
pub mod ups;
pub mod vnc;
pub mod wol;

#[cfg(all(feature = "client", not(target_os = "android")))]
pub mod cli;
#[cfg(feature = "client")]
pub mod client;

/// Devices registered on this instance (populated from config at startup, kept
/// in sync over the management stream).
///
/// This is a global because GUI extension trait methods have no access to manager
/// state when rendering, and the server-side management responder is constructed
/// by a stateless factory.
pub static REGISTERED_DEVICES: LazyLock<Arc<RwLock<Vec<RegisteredDevice>>>> =
    LazyLock::new(Default::default);

static DEVICE_PERSIST: ConfigPersistHook<RegisteredDevice> = ConfigPersistHook::new("probe");

/// This instance's own id, captured when [`ProbeManager`] is constructed. Probes are
/// accessed only from servers, so the server's management responder stamps this as
/// the gateway of every registered device.
static GATEWAY: OnceLock<InstanceId> = OnceLock::new();

/// The gateway instance for devices registered on this instance (the server's own
/// id). `None` before [`ProbeManager::new`] has run.
pub fn gateway() -> Option<InstanceId> {
    GATEWAY.get().copied()
}

/// Install the persistence hook (see [`DEVICE_PERSIST`]). Idempotent: the first
/// caller wins.
pub fn set_device_persist(
    f: impl Fn(&[RegisteredDevice]) -> anyhow::Result<()> + Send + Sync + 'static,
) {
    DEVICE_PERSIST.set(f);
}

/// Persist the current device list if a hook is installed.
pub fn persist_devices(devices: &[RegisteredDevice]) {
    DEVICE_PERSIST.persist(devices);
}

/// Rebuild the on-disk config from the current device list. Only the device
/// list is reconstructed here; the persist hook writes it into the realm's
/// existing probe section, so the `scan` settings on disk are left untouched.
pub fn devices_to_config(devices: &[RegisteredDevice]) -> ProbeManagerConfig {
    ProbeManagerConfig {
        devices: devices.iter().map(|d| d.device.clone()).collect(),
        scan: Default::default(),
    }
}

/// Register the probe subsystem's server-side background services on `runner`.
/// Currently just the discovery scanner, and only when it's enabled in config.
///
/// `server` is stamped onto discovered devices so persistence routes them to the
/// right realm; pass the server's own URL, or `None` for the single-realm case.
#[cfg(feature = "server")]
pub fn register_server_services(
    scan: &config::ScanConfig,
    server: Option<sandpolis_server::ServerUrl>,
    runner: &mut sandpolis_instance::service::ServiceRunner,
) {
    if !scan.enabled {
        tracing::info!("Probe scanning mode is disabled");
        return;
    }
    runner.register(scan::ScanService::new(scan.clone(), server));
}

/// Manages device registrations and streaming state.
#[derive(Clone)]
#[cfg_attr(feature = "client", derive(bevy::prelude::Resource))]
pub struct ProbeManager {
    pub devices: Arc<RwLock<Vec<RegisteredDevice>>>,
}

impl ProbeManager {
    pub fn new(config: ProbeManagerConfig, gateway: InstanceId) -> Self {
        let _ = GATEWAY.set(gateway);
        let devices: Vec<RegisteredDevice> = config
            .devices
            .into_iter()
            .map(|device| RegisteredDevice {
                id: ProbeId::random(),
                gateway,
                device,
                online: false,
                status_message: None,
            })
            .collect();

        *REGISTERED_DEVICES.write().unwrap() = devices;
        Self {
            devices: REGISTERED_DEVICES.clone(),
        }
    }
}

/// An enumeration of all available probe types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProbeType {
    /// Remote Desktop Protocol (Windows)
    Rdp,
    /// Secure Shell
    Ssh,
    /// Uninterruptible Power Supply (via NUT)
    Ups,
    /// Virtual Network Computing
    Vnc,
    /// Wake-on-LAN
    Wol,
    /// HTTP/HTTPS web service
    Http,
    /// Intelligent Platform Management Interface
    Ipmi,
    /// Real Time Streaming Protocol
    Rtsp,
    /// Simple Network Management Protocol
    Snmp,
    /// Open Network Video Interface Forum (IP cameras)
    Onvif,
    /// Docker container engine
    Docker,
    /// libvirt virtualization
    Libvirt,
    /// Network File System (v3)
    Nfs,
    /// Server Message Block / CIFS
    Smb,
}

impl ProbeType {
    /// Get a human-readable display name for this probe type.
    pub fn display_name(&self) -> &'static str {
        match self {
            ProbeType::Rdp => "RDP",
            ProbeType::Ssh => "SSH",
            ProbeType::Ups => "UPS",
            ProbeType::Vnc => "VNC",
            ProbeType::Wol => "Wake-on-LAN",
            ProbeType::Http => "HTTP",
            ProbeType::Ipmi => "IPMI",
            ProbeType::Rtsp => "RTSP",
            ProbeType::Snmp => "SNMP",
            ProbeType::Onvif => "ONVIF",
            ProbeType::Docker => "Docker",
            ProbeType::Libvirt => "libvirt",
            ProbeType::Nfs => "NFS",
            ProbeType::Smb => "SMB",
        }
    }

    /// Get a short description of this probe type.
    pub fn description(&self) -> &'static str {
        match self {
            ProbeType::Rdp => "Windows Remote Desktop Protocol",
            ProbeType::Ssh => "Secure Shell access",
            ProbeType::Ups => "UPS monitoring via Network UPS Tools",
            ProbeType::Vnc => "Virtual Network Computing",
            ProbeType::Wol => "Wake-on-LAN capable device",
            ProbeType::Http => "HTTP/HTTPS web service",
            ProbeType::Ipmi => "Intelligent Platform Management Interface",
            ProbeType::Rtsp => "Real Time Streaming Protocol",
            ProbeType::Snmp => "Simple Network Management Protocol",
            ProbeType::Onvif => "ONVIF-compatible IP camera",
            ProbeType::Docker => "Docker container engine",
            ProbeType::Libvirt => "libvirt virtualization host",
            ProbeType::Nfs => "NFSv3 file server",
            ProbeType::Smb => "SMB/CIFS file server",
        }
    }

    /// Get all probe types.
    pub fn all() -> &'static [ProbeType] {
        &[
            ProbeType::Rdp,
            ProbeType::Ssh,
            ProbeType::Ups,
            ProbeType::Vnc,
            ProbeType::Wol,
            ProbeType::Http,
            ProbeType::Ipmi,
            ProbeType::Rtsp,
            ProbeType::Snmp,
            ProbeType::Onvif,
            ProbeType::Docker,
            ProbeType::Libvirt,
            ProbeType::Nfs,
            ProbeType::Smb,
        ]
    }

    /// Whether this protocol exposes a filesystem, and so can back the
    /// filesystem subsystem via [`crate::filesystem`].
    pub fn is_filesystem(&self) -> bool {
        matches!(self, ProbeType::Nfs | ProbeType::Smb)
    }

    /// Whether this protocol manages service instances (containers or virtual
    /// machines), and so can back the health subsystem via [`crate::service`].
    pub fn is_service(&self) -> bool {
        matches!(self, ProbeType::Docker | ProbeType::Libvirt)
    }

    /// The default TCP port this protocol listens on, for protocols the scanner
    /// can detect with a TCP connection. Returns `None` for UDP-only or
    /// connectionless protocols (SNMP, IPMI, UPS, Wake-on-LAN), which discovery
    /// doesn't probe.
    pub fn default_port(&self) -> Option<u16> {
        Some(match self {
            ProbeType::Ssh => 22,
            ProbeType::Rdp => 3389,
            ProbeType::Vnc => 5900,
            ProbeType::Http => 80,
            ProbeType::Rtsp => 554,
            ProbeType::Onvif => 80,
            ProbeType::Docker => 2375,
            ProbeType::Smb => 445,
            ProbeType::Nfs => 111,
            ProbeType::Snmp | ProbeType::Ipmi | ProbeType::Ups | ProbeType::Libvirt | ProbeType::Wol => {
                return None;
            }
        })
    }

    /// Whether a live host can be detected as speaking this protocol by opening
    /// a TCP connection to its [`default_port`](Self::default_port).
    pub fn is_tcp_scannable(&self) -> bool {
        self.default_port().is_some()
    }
}

sandpolis_instance::typed_id!(
    /// Identifies a registered device (stable for the process lifetime).
    ProbeId,
    "probe",
    4
);

/// A registered device, the runtime counterpart of a [`DeviceConfig`]. Each
/// device maps to exactly one graph node; the protocols it exposes become tabs in
/// its controller.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegisteredDevice {
    /// Unique identifier for this device (stable for the process lifetime).
    pub id: ProbeId,

    /// The gateway instance that reaches this device.
    pub gateway: InstanceId,

    /// Name/address plus the per-protocol configuration.
    pub device: DeviceConfig,

    /// Whether the device is currently online/reachable.
    pub online: bool,

    /// Last status message.
    pub status_message: Option<String>,
}

impl RegisteredDevice {
    /// Display name: the configured name, otherwise the IP.
    pub fn display_name(&self) -> String {
        self.device
            .name
            .clone()
            .unwrap_or_else(|| self.device.ip.to_string())
    }
}

impl DeviceConfig {
    /// The protocols this device exposes, in a stable display order.
    pub fn protocols(&self) -> Vec<ProbeType> {
        let mut out = Vec::new();
        if self.rtsp.is_some() {
            out.push(ProbeType::Rtsp);
        }
        if self.onvif.is_some() {
            out.push(ProbeType::Onvif);
        }
        if self.vnc.is_some() {
            out.push(ProbeType::Vnc);
        }
        if self.rdp.is_some() {
            out.push(ProbeType::Rdp);
        }
        if self.ssh.is_some() {
            out.push(ProbeType::Ssh);
        }
        if self.nfs.is_some() {
            out.push(ProbeType::Nfs);
        }
        if self.smb.is_some() {
            out.push(ProbeType::Smb);
        }
        if self.http.is_some() {
            out.push(ProbeType::Http);
        }
        if self.ipmi.is_some() {
            out.push(ProbeType::Ipmi);
        }
        if self.snmp.is_some() {
            out.push(ProbeType::Snmp);
        }
        if self.docker.is_some() {
            out.push(ProbeType::Docker);
        }
        if self.libvirt.is_some() {
            out.push(ProbeType::Libvirt);
        }
        if self.ups.is_some() {
            out.push(ProbeType::Ups);
        }
        if self.wol.is_some() {
            out.push(ProbeType::Wol);
        }
        out
    }

    /// The protocol used to represent the device's graph node icon.
    pub fn primary(&self) -> Option<ProbeType> {
        self.protocols().first().copied()
    }

    /// The subset of [`protocols`](Self::protocols) that expose a filesystem.
    /// This is what the filesystem subsystem offers to browse.
    pub fn filesystem_protocols(&self) -> Vec<ProbeType> {
        self.protocols()
            .into_iter()
            .filter(ProbeType::is_filesystem)
            .collect()
    }

    /// The subset of [`protocols`](Self::protocols) that manage service
    /// instances. This is what the health subsystem offers to control.
    pub fn service_protocols(&self) -> Vec<ProbeType> {
        self.protocols()
            .into_iter()
            .filter(ProbeType::is_service)
            .collect()
    }

    /// Record that this device speaks `protocol`, populating a credential-less
    /// sub-config addressed at the device's [`ip`](DeviceConfig::ip). Used by
    /// the discovery scanner; an existing sub-config for the protocol is left
    /// untouched. A no-op for protocols the scanner can't detect.
    pub fn add_detected(&mut self, protocol: ProbeType) {
        let host = self.ip.to_string();
        match protocol {
            ProbeType::Ssh if self.ssh.is_none() => {
                self.ssh = Some(SshProbeConfig {
                    host,
                    ..Default::default()
                });
            }
            ProbeType::Rdp if self.rdp.is_none() => {
                self.rdp = Some(RdpProbeConfig {
                    host,
                    ..Default::default()
                });
            }
            ProbeType::Vnc if self.vnc.is_none() => {
                self.vnc = Some(VncProbeConfig {
                    host,
                    ..Default::default()
                });
            }
            ProbeType::Http if self.http.is_none() => {
                self.http = Some(HttpProbeConfig {
                    url: format!("http://{host}/"),
                    ..Default::default()
                });
            }
            ProbeType::Rtsp if self.rtsp.is_none() => {
                self.rtsp = Some(RtspProbeConfig::default());
            }
            ProbeType::Onvif if self.onvif.is_none() => {
                self.onvif = Some(OnvifProbeConfig {
                    host,
                    ..Default::default()
                });
            }
            ProbeType::Docker if self.docker.is_none() => {
                self.docker = Some(DockerProbeConfig {
                    host: format!("tcp://{host}:2375"),
                    ..Default::default()
                });
            }
            ProbeType::Smb if self.smb.is_none() => {
                self.smb = Some(SmbProbeConfig::default());
            }
            ProbeType::Nfs if self.nfs.is_none() => {
                self.nfs = Some(NfsProbeConfig::default());
            }
            _ => {}
        }
    }

    /// Whether any protocol this device exposes can authenticate but hasn't been
    /// given credentials yet — the state a freshly discovered device is in until
    /// an operator fills them in. Drives the client's credential node effect.
    ///
    /// Protocols that don't authenticate (Wake-on-LAN, NFS's AUTH_UNIX, plain
    /// HTTP, unauthenticated Docker, libvirt) never contribute.
    pub fn needs_credentials(&self) -> bool {
        if let Some(c) = &self.ssh
            && c.password.is_none()
            && c.private_key_path.is_none()
        {
            return true;
        }
        if let Some(c) = &self.rdp
            && c.password.is_none()
        {
            return true;
        }
        if let Some(c) = &self.vnc
            && c.password.is_none()
        {
            return true;
        }
        if let Some(c) = &self.rtsp
            && c.password.is_none()
        {
            return true;
        }
        if let Some(c) = &self.onvif
            && c.password.is_none()
        {
            return true;
        }
        if let Some(c) = &self.smb
            && c.password.is_none()
        {
            return true;
        }
        if let Some(c) = &self.ipmi
            && (c.username.is_empty() || c.password.is_empty())
        {
            return true;
        }
        if let Some(c) = &self.snmp {
            let missing = match c.version {
                SnmpVersion::V1 | SnmpVersion::V2c => c.community.is_none(),
                SnmpVersion::V3 => c.username.is_none(),
            };
            if missing {
                return true;
            }
        }
        false
    }
}

// What a client must be granted to open this layer's streams.
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(DeviceMgmt), "probe:manage")
}
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(WolStream), "probe:wake")
}
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(RtspSessionStream), "probe:rtsp")
}
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(ProbeFsStream), "probe:filesystem")
}
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(ProbeServiceStream), "probe:service")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn device() -> DeviceConfig {
        DeviceConfig {
            ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
            ..Default::default()
        }
    }

    #[test]
    fn scannable_types_have_a_port() {
        for t in ProbeType::all() {
            assert_eq!(t.is_tcp_scannable(), t.default_port().is_some());
        }
        assert_eq!(ProbeType::Ssh.default_port(), Some(22));
        assert_eq!(ProbeType::Smb.default_port(), Some(445));
        assert!(ProbeType::Snmp.default_port().is_none());
        assert!(ProbeType::Wol.default_port().is_none());
    }

    #[test]
    fn add_detected_populates_only_its_own_field() {
        let mut d = device();
        d.add_detected(ProbeType::Ssh);
        assert_eq!(d.protocols(), vec![ProbeType::Ssh]);
        assert_eq!(d.ssh.as_ref().unwrap().host, "10.0.0.5");

        d.add_detected(ProbeType::Smb);
        assert!(d.protocols().contains(&ProbeType::Smb));
        // Detecting the same protocol again doesn't clobber an existing config.
        d.ssh.as_mut().unwrap().password = Some("secret".into());
        d.add_detected(ProbeType::Ssh);
        assert_eq!(d.ssh.as_ref().unwrap().password.as_deref(), Some("secret"));
    }

    #[test]
    fn discovered_device_needs_credentials_until_filled() {
        let mut d = device();
        d.add_detected(ProbeType::Ssh);
        assert!(d.needs_credentials());

        d.ssh.as_mut().unwrap().password = Some("hunter2".into());
        assert!(!d.needs_credentials());
    }

    #[test]
    fn ssh_key_counts_as_credentials() {
        let mut d = device();
        d.add_detected(ProbeType::Ssh);
        d.ssh.as_mut().unwrap().private_key_path = Some("/root/.ssh/id_ed25519".into());
        assert!(!d.needs_credentials());
    }

    #[test]
    fn credential_free_protocols_never_flag() {
        // NFS (AUTH_UNIX), Wake-on-LAN, and plain HTTP don't authenticate.
        let mut d = device();
        d.add_detected(ProbeType::Nfs);
        d.add_detected(ProbeType::Http);
        d.wol = Some(config::WolProbeConfig {
            mac_address: "f4:4d:30:62:c0:45".into(),
            ..Default::default()
        });
        assert!(!d.needs_credentials());
    }
}
