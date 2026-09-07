use sandpolis_server::ServerUrl;
use serde::Deserialize;
use serde::Serialize;
use std::net::IpAddr;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct ProbeManagerConfig {
    pub devices: Vec<DeviceConfig>,
    /// Automatic network discovery. See [`ScanConfig`].
    pub scan: ScanConfig,
}

/// Settings for probe "scanning mode": the server periodically sweeps the
/// network for reachable devices and registers them without credentials, which
/// an operator fills in later by editing the device in the realm config.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct ScanConfig {
    /// Whether the discovery service runs at all. Off by default: it sends ARP
    /// traffic and opens TCP connections across the local network, which not
    /// every deployment wants.
    pub enabled: bool,

    /// IPv4 CIDR ranges to scan (e.g. `"10.0.0.0/24"`). When empty, the ranges
    /// are auto-detected from the server's own network interfaces. ARP
    /// discovery only reaches hosts on the same L2 segment, so these should be
    /// the server's local subnets.
    pub networks: Vec<String>,

    /// Seconds between discovery passes.
    pub interval: u64,

    /// Milliseconds to wait for an ARP reply from a host before treating it as
    /// absent.
    pub arp_timeout_ms: u64,

    /// Milliseconds to wait for a TCP connection to a candidate port.
    pub connect_timeout_ms: u64,

    /// How many port probes may be in flight at once.
    pub concurrency: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            networks: Vec::new(),
            interval: 15 * 60,
            arp_timeout_ms: 500,
            connect_timeout_ms: 750,
            concurrency: 256,
        }
    }
}

/// A single managed device. One device may expose several protocols (e.g. an IP
/// camera reachable over both RTSP and ONVIF), so each protocol is an optional
/// sub-config. This is both the authoring format and the persisted format.
///
/// The device's [`ip`](DeviceConfig::ip) is the single source of the device's
/// address; per-protocol configs reference it rather than repeating an address.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DeviceConfig {
    /// Human-readable name for the device.
    pub name: Option<String>,
    /// The server this device is associated with (the server that reaches it).
    /// `None` on hand-authored/legacy configs; resolved to the local server when
    /// loaded. Persisted so the global-stratum config records each probe's owner.
    #[serde(default)]
    pub server: Option<ServerUrl>,
    /// The device's network address. Protocols that connect to the device (e.g.
    /// RTSP) use this address.
    pub ip: IpAddr,
    pub rtsp: Option<RtspProbeConfig>,
    pub wol: Option<WolProbeConfig>,
    pub ssh: Option<SshProbeConfig>,
    pub rdp: Option<RdpProbeConfig>,
    pub vnc: Option<VncProbeConfig>,
    pub http: Option<HttpProbeConfig>,
    pub ipmi: Option<IpmiProbeConfig>,
    pub snmp: Option<SnmpProbeConfig>,
    pub onvif: Option<OnvifProbeConfig>,
    pub docker: Option<DockerProbeConfig>,
    pub libvirt: Option<LibvirtProbeConfig>,
    pub ups: Option<UpsProbeConfig>,
    pub nfs: Option<NfsProbeConfig>,
    pub smb: Option<SmbProbeConfig>,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            name: None,
            server: None,
            ip: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            rtsp: None,
            wol: None,
            ssh: None,
            rdp: None,
            vnc: None,
            http: None,
            ipmi: None,
            snmp: None,
            onvif: None,
            docker: None,
            libvirt: None,
            ups: None,
            nfs: None,
            smb: None,
        }
    }
}

/// RDP probe configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RdpProbeConfig {
    /// Hostname or IP address.
    pub host: String,
    /// Port (default: 3389).
    pub port: Option<u16>,
    /// Username for authentication.
    pub username: Option<String>,
    /// Password for authentication.
    pub password: Option<String>,
    /// Domain for Windows authentication.
    pub domain: Option<String>,
}

/// SSH probe configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SshProbeConfig {
    /// Hostname or IP address.
    pub host: String,
    /// Port (default: 22).
    pub port: Option<u16>,
    /// Username for authentication.
    pub username: Option<String>,
    /// Password for authentication (if not using key).
    pub password: Option<String>,
    /// Path to private key file.
    pub private_key_path: Option<String>,
    /// SSH fingerprint for verification.
    pub fingerprint: Option<String>,
}

/// UPS probe configuration (via NUT).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UpsProbeConfig {
    /// NUT server hostname.
    pub host: String,
    /// NUT server port (default: 3493).
    pub port: Option<u16>,
    /// UPS name on the NUT server.
    pub ups_name: String,
    /// Username for NUT authentication.
    pub username: Option<String>,
    /// Password for NUT authentication.
    pub password: Option<String>,
}

/// VNC probe configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VncProbeConfig {
    /// Hostname or IP address.
    pub host: String,
    /// Port (default: 5900).
    pub port: Option<u16>,
    /// Password for VNC authentication.
    pub password: Option<String>,
}

/// Wake-on-LAN probe configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WolProbeConfig {
    /// MAC address of the target device.
    pub mac_address: String,
    /// Broadcast address to send the packet to.
    pub broadcast_address: Option<String>,
    /// Port (default: 9).
    pub port: Option<u16>,
    /// Hostname for status checking (optional).
    pub hostname: Option<String>,
}

/// HTTP probe configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HttpProbeConfig {
    /// URL to probe.
    pub url: String,
    /// Expected HTTP status code.
    pub expected_status: Option<u16>,
    /// Request timeout in seconds.
    pub timeout_secs: Option<u32>,
    /// Whether to verify TLS certificates.
    pub verify_tls: Option<bool>,
    /// HTTP method (GET, POST, etc.).
    pub method: Option<String>,
    /// Basic auth username.
    pub username: Option<String>,
    /// Basic auth password.
    pub password: Option<String>,
}

/// IPMI probe configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IpmiProbeConfig {
    /// BMC hostname or IP address.
    pub host: String,
    /// IPMI port (default: 623).
    pub port: Option<u16>,
    /// Username for IPMI authentication.
    pub username: String,
    /// Password for IPMI authentication.
    pub password: String,
    /// IPMI interface type (lanplus, lan, etc.).
    pub interface_type: Option<String>,
}

/// RTSP probe configuration. The host comes from the device's
/// [`ip`](DeviceConfig::ip); the streaming URL is built as
/// `rtsp://[user:pass@]<device ip>:<port>/<path>`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RtspProbeConfig {
    /// RTSP port (default: 554).
    pub port: Option<u16>,
    /// Stream path (e.g. `cam/realmonitor?channel=1&subtype=1` or `stream1`).
    pub path: String,
    /// Username for RTSP authentication.
    pub username: Option<String>,
    /// Password for RTSP authentication.
    pub password: Option<String>,
    /// Transport protocol (udp, tcp).
    pub transport: Option<String>,
}

/// SNMP probe configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SnmpProbeConfig {
    /// Hostname or IP address.
    pub host: String,
    /// SNMP port (default: 161).
    pub port: Option<u16>,
    /// SNMP version (1, 2c, or 3).
    pub version: SnmpVersion,
    /// Community string (for v1/v2c).
    pub community: Option<String>,
    /// Username (for v3).
    pub username: Option<String>,
    /// Auth protocol (for v3).
    pub auth_protocol: Option<String>,
    /// Auth password (for v3).
    pub auth_password: Option<String>,
    /// Privacy protocol (for v3).
    pub priv_protocol: Option<String>,
    /// Privacy password (for v3).
    pub priv_password: Option<String>,
}

/// SNMP version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnmpVersion {
    #[default]
    V1,
    V2c,
    V3,
}

/// ONVIF probe configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OnvifProbeConfig {
    /// Camera hostname or IP address.
    pub host: String,
    /// HTTP port (default: 80).
    pub port: Option<u16>,
    /// Username for ONVIF authentication.
    pub username: Option<String>,
    /// Password for ONVIF authentication.
    pub password: Option<String>,
    /// Specific profile token to use.
    pub profile_token: Option<String>,
}

/// Docker probe configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DockerProbeConfig {
    /// Docker host URL (e.g., unix:///var/run/docker.sock or tcp://host:2375).
    pub host: String,
    /// TLS CA certificate path.
    pub tls_ca_cert: Option<String>,
    /// TLS client certificate path.
    pub tls_cert: Option<String>,
    /// TLS client key path.
    pub tls_key: Option<String>,
    /// Whether to verify TLS.
    pub tls_verify: Option<bool>,
}

/// NFS probe configuration. The host comes from the device's
/// [`ip`](DeviceConfig::ip); the mount and NFS ports are resolved from the
/// device's portmapper unless pinned here.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NfsProbeConfig {
    /// Exported path to mount (e.g. `/export/media`).
    pub export: String,
    /// Portmapper port (default: 111).
    pub portmapper_port: Option<u16>,
    /// Mount service port. Resolved via the portmapper when absent.
    pub mount_port: Option<u16>,
    /// NFS service port (default: 2049 once resolved via the portmapper).
    pub nfs_port: Option<u16>,
    /// UID presented in the AUTH_UNIX credential (default: 0).
    pub uid: Option<u32>,
    /// GID presented in the AUTH_UNIX credential (default: 0).
    pub gid: Option<u32>,
    /// Connect from a privileged source port, which Linux `nfsd` requires by
    /// default (default: true).
    pub privileged_port: Option<bool>,
}

/// SMB probe configuration. The host comes from the device's
/// [`ip`](DeviceConfig::ip).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SmbProbeConfig {
    /// Share name to connect to (e.g. `media`).
    pub share: String,
    /// SMB port (default: 445).
    pub port: Option<u16>,
    /// Username for authentication.
    pub username: Option<String>,
    /// Password for authentication.
    pub password: Option<String>,
    /// Workgroup or domain for authentication.
    pub domain: Option<String>,
}

/// libvirt probe configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LibvirtProbeConfig {
    /// libvirt connection URI (e.g., qemu:///system, qemu+ssh://user@host/system).
    pub uri: String,
    /// Username for SSH-based connections.
    pub username: Option<String>,
    /// Private key path for SSH-based connections.
    pub private_key_path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_config_ron_round_trip() {
        let config = ProbeManagerConfig {
            scan: ScanConfig {
                enabled: true,
                networks: vec!["10.0.0.0/24".into(), "192.168.1.0/24".into()],
                interval: 600,
                ..Default::default()
            },
            devices: vec![
                DeviceConfig {
                    name: Some("Front door camera".into()),
                    server: Some("https://gs.example.com:8768/default".parse().unwrap()),
                    ip: "10.0.0.220".parse().unwrap(),
                    rtsp: Some(RtspProbeConfig {
                        port: Some(554),
                        path: "stream1".into(),
                        username: Some("admin".into()),
                        password: Some("hunter2".into()),
                        transport: None,
                    }),
                    wol: Some(WolProbeConfig {
                        mac_address: "f4:4d:30:62:c0:45".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                DeviceConfig {
                    name: Some("Rack switch".into()),
                    ip: "10.0.0.2".parse().unwrap(),
                    ssh: Some(SshProbeConfig {
                        host: "10.0.0.2".into(),
                        port: Some(22),
                        username: Some("root".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                DeviceConfig {
                    name: Some("File server".into()),
                    ip: "10.0.0.30".parse().unwrap(),
                    nfs: Some(NfsProbeConfig {
                        export: "/export/media".into(),
                        uid: Some(1000),
                        gid: Some(1000),
                        ..Default::default()
                    }),
                    smb: Some(SmbProbeConfig {
                        share: "media".into(),
                        username: Some("probe".into()),
                        password: Some("hunter2".into()),
                        domain: Some("WORKGROUP".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
        };

        let serialized = ron::ser::to_string(&config).unwrap();
        let parsed: ProbeManagerConfig = ron::from_str(&serialized).unwrap();
        assert_eq!(config, parsed);
    }
}
