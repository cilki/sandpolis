//! A live file-browsing session against an agent's local filesystem.
//!
//! The client opens one relayed [`FsSessionStream`] per browsed agent and sends
//! operations over it; the agent answers with listings, space totals, or
//! errors. A [`List`](FsSessionRequest::List) also arms a directory watcher on
//! the agent, so changes to the listed directory push fresh listings without
//! the client asking again — which is what makes this a session rather than a
//! sequence of one-shot requests.
//!
//! Probe devices are browsed through `sandpolis_probe::filesystem` instead;
//! this stream only ever touches the agent's own disks.

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
pub struct FileEntry {
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    /// Modification time as seconds since the Unix epoch.
    pub modified: Option<i64>,
    /// POSIX mode bits on unix agents.
    pub mode: Option<u32>,
}

/// Space totals for the filesystem holding the browsed directory.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct FsUsage {
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

/// An operation against the agent's filesystem.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FsSessionRequest {
    /// List a directory and watch it: subsequent changes push fresh
    /// [`Listing`](FsSessionResponse::Listing)s until another `List` moves the
    /// watch elsewhere.
    List { path: PathBuf },
    /// Report space totals for the filesystem containing `path`.
    Statfs { path: PathBuf },
    CreateDir { path: PathBuf },
    Remove {
        targets: Vec<PathBuf>,
        /// Whether to recursively delete directories
        recursive: bool,
    },
    Rename { from: PathBuf, to: PathBuf },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FsSessionResponse {
    /// A directory listing, echoing the path it belongs to: watcher pushes and
    /// answered `List`s arrive on the same stream.
    Listing {
        path: PathBuf,
        entries: Vec<FileEntry>,
    },
    Usage(FsUsage),
    /// An operation with no return value succeeded.
    Done,
    Failed(String),
}

#[cfg(feature = "agent")]
mod agent {
    use super::*;
    use anyhow::Result;
    use notify::Watcher;
    use sandpolis_instance::network::StreamResponder;
    use sandpolis_macros::Stream;
    use tokio::sync::Mutex;
    use tokio::sync::mpsc::Sender;

    /// How long to sit on a watcher event before re-listing, so a burst of
    /// changes (an unpacking archive, a compile) costs one listing, not one per
    /// file.
    const WATCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

    /// Read one directory into wire entries.
    async fn list(path: &std::path::Path) -> Result<Vec<FileEntry>> {
        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(path).await?;
        while let Some(entry) = dir.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            // An entry whose metadata can't be read (dangling symlink, TOCTOU
            // delete) still belongs in the listing.
            let metadata = entry.metadata().await.ok();
            let kind = match entry.file_type().await {
                Ok(t) if t.is_dir() => FileKind::Dir,
                Ok(t) if t.is_file() => FileKind::File,
                Ok(t) if t.is_symlink() => FileKind::Symlink,
                _ => FileKind::Other,
            };
            entries.push(FileEntry {
                name,
                kind,
                size: metadata.as_ref().map(|m| m.len()).unwrap_or(0),
                modified: metadata.as_ref().and_then(|m| {
                    m.modified()
                        .ok()?
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs() as i64)
                }),
                #[cfg(unix)]
                mode: metadata.as_ref().map(|m| {
                    use std::os::unix::fs::PermissionsExt;
                    m.permissions().mode()
                }),
                #[cfg(not(unix))]
                mode: None,
            });
        }
        entries.sort_by(|a, b| {
            // Directories first, then by name.
            (a.kind != FileKind::Dir)
                .cmp(&(b.kind != FileKind::Dir))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(entries)
    }

    /// Space totals for the filesystem containing `path`: the mounted disk
    /// whose mount point is the longest prefix of the (canonicalized) path.
    fn statfs(path: &std::path::Path) -> Result<FsUsage> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let disk = disks
            .list()
            .iter()
            .filter(|disk| path.starts_with(disk.mount_point()))
            .max_by_key(|disk| disk.mount_point().as_os_str().len())
            .ok_or_else(|| anyhow::anyhow!("no mounted filesystem contains {}", path.display()))?;
        let total = disk.total_space();
        let free = disk.available_space();
        Ok(FsUsage {
            total,
            used: total.saturating_sub(free),
            free,
        })
    }

    /// Stream that browses this agent's filesystem.
    ///
    /// One responder is one session: the most recent `List` names its current
    /// directory, which a [`notify`] watcher re-lists on change. Re-arming the
    /// watch drops the previous watcher, whose event channel closing is what
    /// stops the previous push task.
    #[derive(Stream, Default)]
    pub struct FsSessionStreamResponder {
        watcher: Mutex<Option<notify::RecommendedWatcher>>,
    }

    impl FsSessionStreamResponder {
        /// Watch `path` and push a fresh listing on every (debounced) change.
        async fn watch(&self, path: PathBuf, sender: Sender<FsSessionResponse>) -> Result<()> {
            let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut watcher = notify::recommended_watcher(move |event| {
                if let Ok(event) = event
                    && matches!(
                        event,
                        notify::Event {
                            kind: notify::EventKind::Create(_)
                                | notify::EventKind::Modify(_)
                                | notify::EventKind::Remove(_),
                            ..
                        }
                    )
                {
                    let _ = event_tx.send(());
                }
            })?;
            watcher.watch(&path, notify::RecursiveMode::NonRecursive)?;
            *self.watcher.lock().await = Some(watcher);

            tokio::spawn(async move {
                while event_rx.recv().await.is_some() {
                    tokio::time::sleep(WATCH_DEBOUNCE).await;
                    while event_rx.try_recv().is_ok() {}
                    // The directory itself may be gone; the requester finds out
                    // from its next List rather than a push.
                    let Ok(entries) = list(&path).await else {
                        continue;
                    };
                    let response = FsSessionResponse::Listing {
                        path: path.clone(),
                        entries,
                    };
                    if sender.send(response).await.is_err() {
                        break;
                    }
                }
            });
            Ok(())
        }

        async fn dispatch(
            &self,
            request: FsSessionRequest,
            sender: &Sender<FsSessionResponse>,
        ) -> Result<FsSessionResponse> {
            Ok(match request {
                FsSessionRequest::List { path } => {
                    let entries = list(&path).await?;
                    self.watch(path.clone(), sender.clone()).await?;
                    FsSessionResponse::Listing { path, entries }
                }
                FsSessionRequest::Statfs { path } => FsSessionResponse::Usage(statfs(&path)?),
                FsSessionRequest::CreateDir { path } => {
                    tokio::fs::create_dir_all(&path).await?;
                    FsSessionResponse::Done
                }
                FsSessionRequest::Remove { targets, recursive } => {
                    for target in targets {
                        let metadata = tokio::fs::symlink_metadata(&target).await?;
                        if metadata.is_dir() {
                            if recursive {
                                tokio::fs::remove_dir_all(&target).await?;
                            } else {
                                tokio::fs::remove_dir(&target).await?;
                            }
                        } else {
                            tokio::fs::remove_file(&target).await?;
                        }
                    }
                    FsSessionResponse::Done
                }
                FsSessionRequest::Rename { from, to } => {
                    tokio::fs::rename(&from, &to).await?;
                    FsSessionResponse::Done
                }
            })
        }
    }

    impl StreamResponder for FsSessionStreamResponder {
        type In = FsSessionRequest;
        type Out = FsSessionResponse;

        async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
            let response = self
                .dispatch(request, &sender)
                .await
                .unwrap_or_else(|e| FsSessionResponse::Failed(e.to_string()));
            let _ = sender.send(response).await;
            Ok(())
        }
    }
}

#[cfg(feature = "agent")]
pub use agent::FsSessionStreamResponder;

#[cfg(all(test, feature = "agent", unix))]
mod test_fs_session {
    use super::*;
    use sandpolis_instance::network::StreamResponder;
    use tokio::sync::mpsc;

    /// Send one request and wait for its (first) response.
    async fn roundtrip(
        responder: &FsSessionStreamResponder,
        request: FsSessionRequest,
    ) -> (FsSessionResponse, mpsc::Receiver<FsSessionResponse>) {
        let (tx, mut rx) = mpsc::channel(16);
        responder.on_message(request, tx).await.unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for response")
            .expect("stream closed without a response");
        (response, rx)
    }

    #[tokio::test]
    async fn list_sorts_directories_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a-file"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("z-dir")).unwrap();

        let responder = FsSessionStreamResponder::default();
        let (response, _rx) = roundtrip(
            &responder,
            FsSessionRequest::List {
                path: dir.path().to_path_buf(),
            },
        )
        .await;

        let FsSessionResponse::Listing { path, entries } = response else {
            panic!("expected a listing");
        };
        assert_eq!(path, dir.path());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "z-dir");
        assert_eq!(entries[0].kind, FileKind::Dir);
        assert_eq!(entries[1].name, "a-file");
        assert_eq!(entries[1].kind, FileKind::File);
        assert_eq!(entries[1].size, 5);
    }

    #[tokio::test]
    async fn list_of_missing_path_fails() {
        let responder = FsSessionStreamResponder::default();
        let (response, _rx) = roundtrip(
            &responder,
            FsSessionRequest::List {
                path: PathBuf::from("/definitely/not/a/real/path"),
            },
        )
        .await;
        assert!(matches!(response, FsSessionResponse::Failed(_)));
    }

    #[tokio::test]
    async fn statfs_reports_space() {
        let dir = tempfile::tempdir().unwrap();
        let responder = FsSessionStreamResponder::default();
        let (response, _rx) = roundtrip(
            &responder,
            FsSessionRequest::Statfs {
                path: dir.path().to_path_buf(),
            },
        )
        .await;
        let FsSessionResponse::Usage(usage) = response else {
            panic!("expected usage");
        };
        assert!(usage.total > 0);
        assert_eq!(usage.used, usage.total - usage.free);
    }

    #[tokio::test]
    async fn mutations_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let responder = FsSessionStreamResponder::default();

        let made = dir.path().join("made");
        let (response, _rx) = roundtrip(
            &responder,
            FsSessionRequest::CreateDir { path: made.clone() },
        )
        .await;
        assert!(matches!(response, FsSessionResponse::Done));
        assert!(made.is_dir());

        let renamed = dir.path().join("renamed");
        let (response, _rx) = roundtrip(
            &responder,
            FsSessionRequest::Rename {
                from: made,
                to: renamed.clone(),
            },
        )
        .await;
        assert!(matches!(response, FsSessionResponse::Done));
        assert!(renamed.is_dir());

        std::fs::write(renamed.join("occupant"), b"x").unwrap();
        let (response, _rx) = roundtrip(
            &responder,
            FsSessionRequest::Remove {
                targets: vec![renamed.clone()],
                recursive: true,
            },
        )
        .await;
        assert!(matches!(response, FsSessionResponse::Done));
        assert!(!renamed.exists());
    }

    #[tokio::test]
    async fn watcher_pushes_fresh_listing() {
        let dir = tempfile::tempdir().unwrap();
        let responder = FsSessionStreamResponder::default();
        let (response, mut rx) = roundtrip(
            &responder,
            FsSessionRequest::List {
                path: dir.path().to_path_buf(),
            },
        )
        .await;
        let FsSessionResponse::Listing { entries, .. } = response else {
            panic!("expected a listing");
        };
        assert!(entries.is_empty());

        std::fs::write(dir.path().join("appeared"), b"x").unwrap();

        // Pushes are debounced; wait past the window for one that names the
        // new file.
        let pushed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match rx.recv().await.expect("stream closed without a push") {
                    FsSessionResponse::Listing { entries, .. }
                        if entries.iter().any(|e| e.name == "appeared") =>
                    {
                        break entries;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("no listing was pushed for the new file");
        assert_eq!(pushed.len(), 1);
    }
}

#[cfg(feature = "client")]
pub mod client {
    use super::*;
    use anyhow::Result;
    use sandpolis_instance::InstanceId;
    use sandpolis_instance::network::stream::StreamMessage;
    use sandpolis_instance::network::StreamRequester;
    use sandpolis_macros::Stream;
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex, RwLock};
    use tokio::sync::mpsc::Sender;
    use tracing::warn;

    /// What the client knows about each browsed agent's filesystem.
    ///
    /// A global rather than a bevy resource for the same reason the probe
    /// views are one: `bind_text` projections get no world access.
    pub static FS_VIEWS: LazyLock<Arc<RwLock<HashMap<InstanceId, FsView>>>> =
        LazyLock::new(Default::default);

    /// The last thing an agent's filesystem told us.
    #[derive(Clone, Debug, Default)]
    pub struct FsView {
        /// The directory most recently listed, and its entries.
        pub cwd: PathBuf,
        pub entries: Option<Vec<FileEntry>>,
        pub usage: Option<FsUsage>,
        /// Set while a request is outstanding, cleared when one answers.
        pub busy: bool,
        /// Why the last request failed, if it did.
        pub error: Option<String>,
    }

    /// Read one agent's view.
    pub fn view(instance: InstanceId) -> Option<FsView> {
        FS_VIEWS.read().ok()?.get(&instance).cloned()
    }

    fn update(instance: InstanceId, f: impl FnOnce(&mut FsView)) {
        if let Ok(mut views) = FS_VIEWS.write() {
            f(views.entry(instance).or_default());
        }
    }

    /// Open sessions, keyed by agent. Holding the outbound sender is what
    /// keeps a session alive: dropping it ends the forwarding task, which
    /// closes the stream.
    static SESSIONS: LazyLock<Mutex<HashMap<InstanceId, Sender<FsSessionRequest>>>> =
        LazyLock::new(Default::default);

    /// Client side of the session: folds listings and space totals into
    /// [`FS_VIEWS`] so the GUI can render them without holding the stream.
    #[derive(Stream)]
    pub struct FsSessionStreamRequester {
        /// Which agent's view to fold responses into.
        instance: InstanceId,
    }

    impl StreamRequester for FsSessionStreamRequester {
        type In = FsSessionResponse;
        type Out = FsSessionRequest;

        async fn new(_: Self::Out, _: Sender<Self::Out>) -> Result<Self> {
            anyhow::bail!("FsSessionStreamRequester must be constructed directly")
        }

        async fn on_message(&self, response: Self::In, _: Sender<Self::Out>) -> Result<()> {
            let instance = self.instance;
            update(instance, |view| {
                view.busy = false;
                match response {
                    FsSessionResponse::Listing { path, entries } => {
                        view.error = None;
                        view.cwd = path;
                        view.entries = Some(entries);
                    }
                    FsSessionResponse::Usage(usage) => {
                        view.error = None;
                        view.usage = Some(usage);
                    }
                    FsSessionResponse::Failed(reason) => {
                        warn!(%instance, %reason, "Filesystem session request failed");
                        view.error = Some(reason);
                    }
                    FsSessionResponse::Done => {
                        view.error = None;
                    }
                }
            });
            Ok(())
        }
    }

    /// Whether a session to `instance` is currently open.
    pub fn is_open(instance: InstanceId) -> bool {
        SESSIONS
            .lock()
            .map(|sessions| sessions.contains_key(&instance))
            .unwrap_or(false)
    }

    /// List `path` on `instance`, opening a session if none is open yet.
    ///
    /// Every listing is chased by a `Statfs` of the same path, which is what
    /// keeps the disk gauge honest as the user navigates across mounts.
    pub fn browse(instance: InstanceId, path: PathBuf) -> Result<()> {
        update(instance, |view| view.busy = true);

        let mut sessions = SESSIONS
            .lock()
            .map_err(|_| anyhow::anyhow!("session table poisoned"))?;
        if let Some(outbound) = sessions.get(&instance) {
            let sent = outbound
                .try_send(FsSessionRequest::List { path: path.clone() })
                .and_then(|()| outbound.try_send(FsSessionRequest::Statfs { path: path.clone() }));
            if sent.is_ok() {
                return Ok(());
            }
            // The forwarding task is gone (stream died) or the channel is
            // backed up; either way, start over with a fresh session.
            sessions.remove(&instance);
        }

        let conn = sandpolis_client::sync::connection()
            .ok_or_else(|| anyhow::anyhow!("no server connection"))?;

        let (outbound, mut outbound_rx) = tokio::sync::mpsc::channel::<FsSessionRequest>(16);
        // Queued behind the initial List sent by open_stream_to below.
        let _ = outbound.try_send(FsSessionRequest::Statfs { path: path.clone() });
        // A weak handle, so the task can recognize its own table entry without
        // holding the channel open.
        let this_session = outbound.downgrade();
        sessions.insert(instance, outbound);
        drop(sessions);

        sandpolis_client::sync::spawn(async move {
            let requester = FsSessionStreamRequester { instance };
            let initial = FsSessionRequest::List { path };
            let (id, msg_tx) = match conn.open_stream_to(instance, requester, initial).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(%instance, error = %e, "Failed to open filesystem session");
                    update(instance, |view| {
                        view.busy = false;
                        view.error = Some(e.to_string());
                    });
                    remove_session(instance, &this_session);
                    return;
                }
            };
            while let Some(request) = outbound_rx.recv().await {
                let payload = match serde_cbor::to_vec(&request) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if msg_tx
                    .send(StreamMessage::to(id, payload, instance))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            conn.close_stream(id);
            // Sending failed above, or `close` already removed us; either way
            // the next browse must open fresh.
            remove_session(instance, &this_session);
        });
        Ok(())
    }

    /// Remove `instance`'s table entry, but only if it still belongs to the
    /// session identified by `handle` — a newer session may have replaced it.
    fn remove_session(
        instance: InstanceId,
        handle: &tokio::sync::mpsc::WeakSender<FsSessionRequest>,
    ) {
        if let Ok(mut sessions) = SESSIONS.lock()
            && let Some(current) = sessions.get(&instance)
            && handle
                .upgrade()
                .map(|mine| mine.same_channel(current))
                .unwrap_or(false)
        {
            sessions.remove(&instance);
        }
    }

    /// End the session to `instance`, keeping its view for the next open.
    pub fn close(instance: InstanceId) {
        if let Ok(mut sessions) = SESSIONS.lock() {
            sessions.remove(&instance);
        }
        update(instance, |view| view.busy = false);
    }
}
