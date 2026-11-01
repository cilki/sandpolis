//! A protocol-agnostic filesystem interface over probe devices.
//!
//! The filesystem subsystem drives this; everything underneath — NFSv3 RPC, SMB2 —
//! is the probe subsystem's business. A caller names a device and one of its
//! filesystem protocols and gets directory entries back; it never learns which
//! wire protocol answered.
//!
//! Every request is self-contained (device + protocol + operation) rather than
//! opening a session and issuing operations against it. That keeps callers free of
//! session bookkeeping; the server reuses the underlying connection through
//! [`CONNECTIONS`], since mounting is the expensive part.
//!
//! Credentials never leave the server. Callers send a device id, which the server
//! resolves against [`REGISTERED_DEVICES`](crate::REGISTERED_DEVICES).

use crate::ProbeType;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What a directory entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One entry in a directory listing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    /// Modification time as seconds since the Unix epoch.
    pub modified: Option<i64>,
    /// POSIX mode bits, when the protocol reports them.
    pub mode: Option<u32>,
}

/// Attributes of a single file or directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileAttr {
    pub kind: FileKind,
    pub size: u64,
    pub modified: Option<i64>,
    pub mode: Option<u32>,
}

/// Space totals for a mounted filesystem.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct FsUsage {
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

/// One NFS export or SMB share a device offers.
///
/// Enumerating these needs no mount, so it works before anything is configured —
/// it's how the probe panel reports what a device actually serves.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShareEntry {
    /// Export path (NFS) or share name (SMB).
    pub name: String,
    /// Protocol-specific kind, e.g. "Disk" or "export".
    pub kind: String,
    /// Allowed groups (NFS) or the share's remark (SMB).
    pub comment: Option<String>,
}

/// An operation against a probe device's filesystem.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProbeFsOp {
    /// List the exports/shares the device offers. Needs no mount.
    Enumerate,
    List { path: PathBuf },
    Stat { path: PathBuf },
    Read { path: PathBuf, offset: u64, len: u32 },
    Write { path: PathBuf, offset: u64, data: Vec<u8> },
    CreateDir { path: PathBuf },
    Remove { path: PathBuf, recursive: bool },
    Rename { from: PathBuf, to: PathBuf },
    Statfs,
}

/// A request naming the device, the protocol to reach it by, and the operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeFsRequest {
    pub device_id: u64,
    pub protocol: ProbeType,
    pub op: ProbeFsOp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProbeFsResponse {
    Shares(Vec<ShareEntry>),
    /// The listing, echoing the path it belongs to so a caller that issued
    /// several can tell them apart.
    Listing {
        path: PathBuf,
        entries: Vec<DirEntry>,
    },
    Attr(FileAttr),
    Data(Vec<u8>),
    Written(u32),
    Usage(FsUsage),
    /// An operation with no return value succeeded.
    Done,
    Failed(String),
}

/// Normalize a caller-supplied path into components, rejecting anything that
/// tries to climb out of the mounted export/share.
pub fn normalize(path: &std::path::Path) -> anyhow::Result<Vec<String>> {
    use std::path::Component;

    let mut out = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                anyhow::bail!("path escapes the exported root: {}", path.display())
            }
            Component::Normal(part) => out.push(
                part.to_str()
                    .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8"))?
                    .to_string(),
            ),
        }
    }
    Ok(out)
}

#[cfg(feature = "server")]
mod server {
    use super::*;
    use crate::{REGISTERED_DEVICES, RegisteredDevice};
    use anyhow::{Result, bail};
    use sandpolis_instance::network::{
        RegisterResponders, ResponderRegistration, StreamRegistry, StreamResponder,
    };
    use sandpolis_macros::Stream;
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock};
    use tokio::sync::{Mutex, mpsc::Sender};

    /// A mounted filesystem on a probe device.
    ///
    /// One variant per protocol. Adding a protocol means adding a variant and its
    /// arm below; nothing outside this module changes.
    pub enum ProbeFs {
        Nfs(crate::nfs::NfsFs),
        Smb(crate::smb::SmbFs),
    }

    impl ProbeFs {
        pub async fn list(&self, path: &std::path::Path) -> Result<Vec<DirEntry>> {
            match self {
                ProbeFs::Nfs(fs) => fs.list(path).await,
                ProbeFs::Smb(fs) => fs.list(path).await,
            }
        }

        pub async fn stat(&self, path: &std::path::Path) -> Result<FileAttr> {
            match self {
                ProbeFs::Nfs(fs) => fs.stat(path).await,
                ProbeFs::Smb(fs) => fs.stat(path).await,
            }
        }

        pub async fn read(&self, path: &std::path::Path, offset: u64, len: u32) -> Result<Vec<u8>> {
            match self {
                ProbeFs::Nfs(fs) => fs.read(path, offset, len).await,
                ProbeFs::Smb(fs) => fs.read(path, offset, len).await,
            }
        }

        pub async fn write(&self, path: &std::path::Path, offset: u64, data: &[u8]) -> Result<u32> {
            match self {
                ProbeFs::Nfs(fs) => fs.write(path, offset, data).await,
                ProbeFs::Smb(fs) => fs.write(path, offset, data).await,
            }
        }

        pub async fn create_dir(&self, path: &std::path::Path) -> Result<()> {
            match self {
                ProbeFs::Nfs(fs) => fs.create_dir(path).await,
                ProbeFs::Smb(fs) => fs.create_dir(path).await,
            }
        }

        pub async fn remove(&self, path: &std::path::Path, recursive: bool) -> Result<()> {
            match self {
                ProbeFs::Nfs(fs) => fs.remove(path, recursive).await,
                ProbeFs::Smb(fs) => fs.remove(path, recursive).await,
            }
        }

        pub async fn rename(&self, from: &std::path::Path, to: &std::path::Path) -> Result<()> {
            match self {
                ProbeFs::Nfs(fs) => fs.rename(from, to).await,
                ProbeFs::Smb(fs) => fs.rename(from, to).await,
            }
        }

        pub async fn statfs(&self) -> Result<FsUsage> {
            match self {
                ProbeFs::Nfs(fs) => fs.statfs().await,
                ProbeFs::Smb(fs) => fs.statfs().await,
            }
        }
    }

    /// Mounted filesystems, keyed by device and protocol.
    type Connections = Mutex<HashMap<(u64, ProbeType), Arc<ProbeFs>>>;

    /// Mounting costs a portmapper round trip plus a MNT call, so connections
    /// outlive the one-shot streams that use them; a failed operation evicts its
    /// entry so the next request reconnects.
    static CONNECTIONS: LazyLock<Connections> = LazyLock::new(Default::default);

    /// Look a registered device up by id.
    fn device(device_id: u64) -> Result<RegisteredDevice> {
        REGISTERED_DEVICES
            .read()
            .ok()
            .and_then(|devices| devices.iter().find(|d| d.id.body() == device_id).cloned())
            .ok_or_else(|| anyhow::anyhow!("device {device_id} is not registered"))
    }

    /// Mount `protocol` on `device_id`, reusing an existing connection if one is
    /// already open.
    pub async fn open(device_id: u64, protocol: ProbeType) -> Result<Arc<ProbeFs>> {
        if let Some(fs) = CONNECTIONS.lock().await.get(&(device_id, protocol)) {
            return Ok(fs.clone());
        }

        let device = device(device_id)?;
        let fs = Arc::new(match protocol {
            ProbeType::Nfs => {
                let config = device
                    .device
                    .nfs
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("device has no NFS configuration"))?;
                ProbeFs::Nfs(crate::nfs::NfsFs::mount(device.device.ip, &config).await?)
            }
            ProbeType::Smb => {
                let config = device
                    .device
                    .smb
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("device has no SMB configuration"))?;
                ProbeFs::Smb(crate::smb::SmbFs::mount(device.device.ip, &config).await?)
            }
            other => bail!("{} is not a filesystem protocol", other.display_name()),
        });

        CONNECTIONS
            .lock()
            .await
            .insert((device_id, protocol), fs.clone());
        Ok(fs)
    }

    /// List the exports/shares `device_id` offers over `protocol`. Deliberately
    /// independent of [`open`]: enumeration is what you do *before* you know
    /// which export to mount.
    pub async fn enumerate(device_id: u64, protocol: ProbeType) -> Result<Vec<ShareEntry>> {
        let device = device(device_id)?;
        match protocol {
            ProbeType::Nfs => {
                crate::nfs::enumerate(device.device.ip, device.device.nfs.as_ref()).await
            }
            ProbeType::Smb => {
                crate::smb::enumerate(device.device.ip, device.device.smb.as_ref()).await
            }
            other => bail!("{} is not a filesystem protocol", other.display_name()),
        }
    }

    /// Drop any cached connection for this device/protocol, so the next request
    /// reconnects rather than reusing a broken mount.
    async fn evict(device_id: u64, protocol: ProbeType) {
        CONNECTIONS.lock().await.remove(&(device_id, protocol));
    }

    /// Run one operation, translating errors into [`ProbeFsResponse::Failed`].
    async fn dispatch(request: ProbeFsRequest) -> ProbeFsResponse {
        let ProbeFsRequest {
            device_id,
            protocol,
            op,
        } = request;

        // Enumeration doesn't need (and must not require) a mount.
        if matches!(op, ProbeFsOp::Enumerate) {
            return match enumerate(device_id, protocol).await {
                Ok(shares) => ProbeFsResponse::Shares(shares),
                Err(e) => ProbeFsResponse::Failed(e.to_string()),
            };
        }

        let fs = match open(device_id, protocol).await {
            Ok(fs) => fs,
            Err(e) => return ProbeFsResponse::Failed(e.to_string()),
        };

        let result = match &op {
            ProbeFsOp::Enumerate => unreachable!("handled above"),
            ProbeFsOp::List { path } => fs.list(path).await.map(|entries| {
                ProbeFsResponse::Listing {
                    path: path.clone(),
                    entries,
                }
            }),
            ProbeFsOp::Stat { path } => fs.stat(path).await.map(ProbeFsResponse::Attr),
            ProbeFsOp::Read { path, offset, len } => {
                fs.read(path, *offset, *len).await.map(ProbeFsResponse::Data)
            }
            ProbeFsOp::Write { path, offset, data } => fs
                .write(path, *offset, data)
                .await
                .map(ProbeFsResponse::Written),
            ProbeFsOp::CreateDir { path } => {
                fs.create_dir(path).await.map(|()| ProbeFsResponse::Done)
            }
            ProbeFsOp::Remove { path, recursive } => fs
                .remove(path, *recursive)
                .await
                .map(|()| ProbeFsResponse::Done),
            ProbeFsOp::Rename { from, to } => {
                fs.rename(from, to).await.map(|()| ProbeFsResponse::Done)
            }
            ProbeFsOp::Statfs => fs.statfs().await.map(ProbeFsResponse::Usage),
        };

        match result {
            Ok(response) => response,
            Err(e) => {
                // The mount may be what's broken, so don't hand it to the next
                // request.
                evict(device_id, protocol).await;
                ProbeFsResponse::Failed(e.to_string())
            }
        }
    }

    /// Server side of the probe filesystem stream.
    #[derive(Stream, Default)]
    pub struct ProbeFsStreamResponder;

    impl StreamResponder for ProbeFsStreamResponder {
        type In = ProbeFsRequest;
        type Out = ProbeFsResponse;

        async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
            let _ = sender.send(dispatch(request).await).await;
            Ok(())
        }
    }

    /// Registers [`ProbeFsStreamResponder`] on each connection.
    pub struct ProbeFsResponderRegistration;

    impl RegisterResponders for ProbeFsResponderRegistration {
        fn register_responders(&self, registry: &StreamRegistry) {
            registry.register_responder(ProbeFsStreamResponder::default);
        }
    }

    inventory::submit!(ResponderRegistration(&ProbeFsResponderRegistration));
}

#[cfg(feature = "server")]
pub use server::{ProbeFs, ProbeFsStreamResponder, enumerate, open};

/// Client-side access to the interface above.
///
/// Kept as a module rather than flattened because an all-in-one build enables
/// both features, and the client's `enumerate` would otherwise collide with the
/// server's.
#[cfg(feature = "client")]
pub mod client {
    use super::*;
    use anyhow::Result;
    use sandpolis_instance::network::StreamRequester;
    use sandpolis_instance::network::stream::StreamMessage;
    use sandpolis_instance::network::InstanceConnection;
    use sandpolis_macros::Stream;
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, RwLock};
    use tokio::sync::mpsc::Sender;

    /// What the client knows about one device's filesystem, keyed by device id.
    ///
    /// A global rather than a bevy resource because `bind_text` projections get no
    /// world access — the same reason [`REGISTERED_DEVICES`](crate::REGISTERED_DEVICES)
    /// is one.
    pub static PROBE_FS_VIEWS: LazyLock<Arc<RwLock<HashMap<u64, ProbeFsView>>>> =
        LazyLock::new(Default::default);

    /// The last thing a device's filesystem told us.
    #[derive(Clone, Debug, Default)]
    pub struct ProbeFsView {
        /// Exports/shares the device offers.
        pub shares: Option<Vec<ShareEntry>>,
        /// The directory most recently listed, and its entries.
        pub cwd: PathBuf,
        pub entries: Option<Vec<DirEntry>>,
        pub usage: Option<FsUsage>,
        /// Set while a request is outstanding, cleared when one answers.
        pub busy: bool,
        /// Why the last request failed, if it did.
        pub error: Option<String>,
    }

    /// Read one device's view.
    pub fn view(device_id: u64) -> Option<ProbeFsView> {
        PROBE_FS_VIEWS.read().ok()?.get(&device_id).cloned()
    }

    fn update(device_id: u64, f: impl FnOnce(&mut ProbeFsView)) {
        if let Ok(mut views) = PROBE_FS_VIEWS.write() {
            f(views.entry(device_id).or_default());
        }
    }

    /// Client side of the probe filesystem stream: folds responses into
    /// [`PROBE_FS_VIEWS`] so the GUI can render them without holding a session.
    #[derive(Stream)]
    pub struct ProbeFsStreamRequester {
        /// Which device's view to fold responses into. The response carries no
        /// device id, so the requester remembers what it asked about.
        pub device_id: u64,
    }

    impl StreamRequester for ProbeFsStreamRequester {
        type In = ProbeFsResponse;
        type Out = ProbeFsRequest;

        async fn on_message(&self, response: Self::In, _: Sender<Self::Out>) -> Result<()> {
            let device_id = self.device_id;
            update(device_id, |view| {
                view.busy = false;
                match response {
                    ProbeFsResponse::Shares(shares) => {
                        view.error = None;
                        view.shares = Some(shares);
                    }
                    ProbeFsResponse::Listing { path, entries } => {
                        view.error = None;
                        view.cwd = path;
                        view.entries = Some(entries);
                    }
                    ProbeFsResponse::Usage(usage) => {
                        view.error = None;
                        view.usage = Some(usage);
                    }
                    ProbeFsResponse::Failed(reason) => {
                        tracing::warn!(device_id, %reason, "Probe filesystem request failed");
                        view.error = Some(reason);
                    }
                    // Reads and writes are driven by callers that want the bytes
                    // back directly rather than through the shared view; nothing
                    // to fold in here yet.
                    ProbeFsResponse::Attr(_)
                    | ProbeFsResponse::Data(_)
                    | ProbeFsResponse::Written(_)
                    | ProbeFsResponse::Done => {
                        view.error = None;
                    }
                }
            });
            Ok(())
        }
    }

    /// How long to keep a one-shot stream registered so its answer arrives before
    /// the stream is released.
    const RESPONSE_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);

    /// Send one filesystem operation to the server that reaches `device_id`.
    ///
    /// One-shot, like [`send_wake`](crate::wol::send_wake): the stream lives just
    /// long enough to carry the answer back into [`PROBE_FS_VIEWS`].
    pub fn request(conn: Arc<InstanceConnection>, device_id: u64, protocol: ProbeType, op: ProbeFsOp) {
        update(device_id, |view| {
            view.busy = true;
        });
        sandpolis_client::sync::spawn(async move {
            let (id, tx) = conn.register_stream(ProbeFsStreamRequester { device_id });
            let payload = match serde_cbor::to_vec(&ProbeFsRequest {
                device_id,
                protocol,
                op,
            }) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to encode probe filesystem request");
                    update(device_id, |view| view.busy = false);
                    return;
                }
            };
            let _ = tx.send(StreamMessage::local(id, payload)).await;
            tokio::time::sleep(RESPONSE_WINDOW).await;
            conn.close_stream(id);
        });
    }

    /// The connection that reaches `device_id`: the server that owns it, else the
    /// primary.
    pub fn connection_for(device_id: u64) -> Option<Arc<InstanceConnection>> {
        let device = crate::REGISTERED_DEVICES
            .read()
            .ok()?
            .iter()
            .find(|d| d.id.body() == device_id)
            .cloned()?;
        device
            .device
            .server
            .as_ref()
            .and_then(sandpolis_client::sync::connection_for)
            .or_else(sandpolis_client::sync::connection)
    }

    /// Ask for the device's exports/shares, if a connection is available.
    pub fn enumerate(device_id: u64, protocol: ProbeType) {
        if let Some(conn) = connection_for(device_id) {
            request(conn, device_id, protocol, ProbeFsOp::Enumerate);
        } else {
            tracing::warn!(device_id, "No server connection; cannot enumerate shares");
        }
    }

    /// Ask for a directory listing plus the filesystem's space totals.
    pub fn browse(device_id: u64, protocol: ProbeType, path: PathBuf) {
        let Some(conn) = connection_for(device_id) else {
            tracing::warn!(device_id, "No server connection; cannot browse filesystem");
            return;
        };
        request(
            conn.clone(),
            device_id,
            protocol,
            ProbeFsOp::List { path },
        );
        request(conn, device_id, protocol, ProbeFsOp::Statfs);
    }
}
