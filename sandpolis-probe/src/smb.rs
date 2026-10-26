//! SMB2/3 backend for the probe filesystem interface.
//!
//! Everything SMB-specific lives here: session setup, share connection, and the
//! create/query/set round trips each operation costs. Callers go through
//! [`crate::filesystem`] and never see any of it.
//!
//! Unlike NFS, a share is reached by name rather than by path, and the server
//! authenticates the session — so [`SmbFs::mount`] carries credentials, which
//! stay on the server that owns the device.

/// Default SMB port. Also the crate's own default, repeated here so the config's
/// `port` field has a documented meaning.
pub const SMB_PORT: u16 = 445;

#[cfg(feature = "server")]
mod server {
    use super::*;
    use crate::config::SmbProbeConfig;
    use crate::filesystem::{DirEntry, FileAttr, FileKind, FsUsage, ShareEntry, normalize};
    use anyhow::{Context, Result, anyhow, bail};
    use futures::StreamExt;
    use smb::binrw_util::prelude::{FileTime, SizedWideString};
    use smb::connection::AuthMethodsConfig;
    use smb::{
        Client, ClientConfig, ConnectionConfig, CreateOptions, Directory, FileAccessMask,
        FileAllInformation, FileAttributes, FileCreateArgs, FileDispositionInformation,
        FileFsFullSizeInformation, FileIdBothDirectoryInformation, FileRenameInformation, ReadAt,
        Resource, UncPath, WriteAt,
    };
    use smb_rpc::interface::{ShareInfo1, ShareKind};
    use std::net::IpAddr;
    use std::path::Path;
    use std::sync::Arc;

    /// A connected SMB share.
    ///
    /// No lock around the client, unlike [`NfsFs`](crate::nfs::NfsFs): SMB
    /// multiplexes requests over one connection and [`Client`] takes `&self`, so
    /// it serializes what it must internally. It also caches the connection,
    /// session, and tree, which is why mounting is worth keeping alive in
    /// [`CONNECTIONS`](crate::filesystem::open).
    pub struct SmbFs {
        client: Client,
        /// `\\<ip>\<share>`. Every operation hangs a path off this.
        root: UncPath,
    }

    /// Client configuration for a device.
    ///
    /// DFS resolution is off: the probe was told which share to use, and a
    /// referral would silently send it to a different server.
    fn client_config(config: Option<&SmbProbeConfig>) -> ClientConfig {
        ClientConfig {
            dfs: false,
            connection: ConnectionConfig {
                port: Some(config.and_then(|c| c.port).unwrap_or(SMB_PORT)),
                auth_methods: AuthMethodsConfig {
                    ntlm: true,
                    // Kerberos needs a KDC and a service ticket for a hostname;
                    // probes are addressed by IP.
                    kerberos: false,
                },
                // A server that only offers guest access won't sign, and refusing
                // that would leave public shares unreachable.
                allow_unsigned_guest_access: config
                    .map(|c| c.username.is_none())
                    .unwrap_or(true),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// The account to authenticate as. An empty username is an anonymous session,
    /// which is what public shares expect.
    fn credentials(config: Option<&SmbProbeConfig>) -> (String, String) {
        let username = config
            .and_then(|c| c.username.clone())
            .unwrap_or_default();
        let password = config
            .and_then(|c| c.password.clone())
            .unwrap_or_default();

        match config.and_then(|c| c.domain.as_deref()) {
            Some(domain) if !username.is_empty() => (format!("{domain}\\{username}"), password),
            _ => (username, password),
        }
    }

    impl SmbFs {
        /// Connect to `config.share` on `ip`.
        pub async fn mount(ip: IpAddr, config: &SmbProbeConfig) -> Result<Self> {
            let server = ip.to_string();
            let root = UncPath::new(&server)
                .and_then(|path| path.with_share(&config.share))
                .map_err(|e| anyhow!("{server} and {} are not a UNC path: {e}", config.share))?;

            let client = Client::new(client_config(Some(config)));
            let (username, password) = credentials(Some(config));
            client
                .share_connect(&root, &username, password)
                .await
                .with_context(|| format!("failed to connect to {root}"))?;

            Ok(Self { client, root })
        }

        pub async fn list(&self, path: &Path) -> Result<Vec<DirEntry>> {
            let components = normalize(path)?;
            let dir = Arc::new(self.open_dir(&components).await?);

            let mut entries = Vec::new();
            {
                let mut stream =
                    Directory::query::<FileIdBothDirectoryInformation>(&dir, "*").await?;
                while let Some(entry) = stream.next().await {
                    let entry = entry.context("failed to read a directory entry")?;
                    let name = text(&entry.file_name);
                    if name == "." || name == ".." {
                        continue;
                    }
                    entries.push(dir_entry(name, &entry));
                }
            }

            dir.close().await?;
            Ok(entries)
        }

        pub async fn stat(&self, path: &Path) -> Result<FileAttr> {
            let components = normalize(path)?;
            let resource = self
                .open(
                    &components,
                    FileCreateArgs::make_open_existing(
                        FileAccessMask::new().with_generic_read(true),
                    ),
                )
                .await?;

            let info: FileAllInformation = match &resource {
                Resource::File(file) => file.query_info().await?,
                Resource::Directory(dir) => dir.query_info().await?,
                Resource::Pipe(_) => bail!("{} is a pipe", path.display()),
            };
            close(resource).await?;

            Ok(file_attr(&info))
        }

        pub async fn read(&self, path: &Path, offset: u64, len: u32) -> Result<Vec<u8>> {
            let components = normalize(path)?;
            let file = self
                .open_file(
                    &components,
                    FileCreateArgs::make_open_existing(
                        FileAccessMask::new().with_generic_read(true),
                    ),
                )
                .await?;

            let mut data = vec![0u8; len as usize];
            let read = file.read_at(&mut data, offset).await?;
            data.truncate(read);

            file.close().await?;
            Ok(data)
        }

        pub async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> Result<u32> {
            let components = normalize(path)?;
            let file = self
                .open_file(
                    &components,
                    FileCreateArgs::make_open_existing(
                        FileAccessMask::new().with_generic_write(true),
                    ),
                )
                .await?;

            let written = file.write_at(data, offset).await?;
            file.close().await?;

            u32::try_from(written).context("write is larger than one SMB call")
        }

        pub async fn create_dir(&self, path: &Path) -> Result<()> {
            let components = normalize(path)?;
            let resource = self
                .open(
                    &components,
                    FileCreateArgs::make_create_new(
                        FileAttributes::new().with_directory(true),
                        CreateOptions::new().with_directory_file(true),
                    ),
                )
                .await?;
            close(resource).await
        }

        pub async fn remove(&self, path: &Path, recursive: bool) -> Result<()> {
            let components = normalize(path)?;
            if components.is_empty() {
                bail!("refusing to remove the share root");
            }

            if recursive && matches!(self.stat(path).await?.kind, FileKind::Dir) {
                // SMB has no recursive delete; walk the tree depth-first.
                return Box::pin(self.remove_tree(path)).await;
            }

            self.delete(&components).await
        }

        pub async fn rename(&self, from: &Path, to: &Path) -> Result<()> {
            let from_components = normalize(from)?;
            let to_components = normalize(to)?;
            if from_components.is_empty() || to_components.is_empty() {
                bail!("rename requires a path below the share root");
            }

            let resource = self
                .open(
                    &from_components,
                    FileCreateArgs::make_open_existing(FileAccessMask::new().with_delete(true)),
                )
                .await?;

            // MS-SMB2 wants the destination relative to the share root, with no
            // leading separator.
            let rename = FileRenameInformation {
                // Replaces an existing target, matching the NFS backend (and
                // POSIX), so callers get one behavior across protocols.
                replace_if_exists: true.into(),
                root_directory: 0,
                file_name: SizedWideString::from(join(&to_components).as_str()),
            };
            let result = match &resource {
                Resource::File(file) => file.set_info(rename).await,
                Resource::Directory(dir) => dir.set_info(rename).await,
                Resource::Pipe(_) => bail!("{} is a pipe", from.display()),
            };
            close(resource).await?;
            result.with_context(|| format!("failed to rename {}", from.display()))?;

            Ok(())
        }

        pub async fn statfs(&self) -> Result<FsUsage> {
            let root = self.open_dir(&[]).await?;
            let info: FileFsFullSizeInformation = root.query_fs_info().await?;
            root.close().await?;

            Ok(usage(&info))
        }

        /// Open a path under the share, whatever kind of thing it turns out to be.
        async fn open(&self, components: &[String], args: FileCreateArgs) -> Result<Resource> {
            let path = self.path_of(components);
            self.client
                .create_file(&path, &args)
                .await
                .with_context(|| format!("failed to open {path}"))
        }

        async fn open_dir(&self, components: &[String]) -> Result<Directory> {
            let resource = self
                .open(
                    components,
                    FileCreateArgs::make_open_existing(
                        FileAccessMask::new().with_generic_read(true),
                    ),
                )
                .await?;
            match resource {
                Resource::Directory(dir) => Ok(dir),
                other => {
                    close(other).await?;
                    bail!("{} is not a directory", self.path_of(components))
                }
            }
        }

        async fn open_file(
            &self,
            components: &[String],
            args: FileCreateArgs,
        ) -> Result<smb::File> {
            let resource = self.open(components, args).await?;
            match resource {
                Resource::File(file) => Ok(file),
                other => {
                    close(other).await?;
                    bail!("{} is not a file", self.path_of(components))
                }
            }
        }

        /// Mark an entry for deletion and close it, which is how SMB deletes.
        async fn delete(&self, components: &[String]) -> Result<()> {
            let resource = self
                .open(
                    components,
                    FileCreateArgs::make_open_existing(FileAccessMask::new().with_delete(true)),
                )
                .await?;

            let result = match &resource {
                Resource::File(file) => file.set_info(FileDispositionInformation::default()).await,
                Resource::Directory(dir) => {
                    dir.set_info(FileDispositionInformation::default()).await
                }
                Resource::Pipe(_) => bail!("{} is a pipe", self.path_of(components)),
            };
            close(resource).await?;
            result.with_context(|| format!("failed to delete {}", self.path_of(components)))?;

            Ok(())
        }

        /// Depth-first delete of the subtree at `path`.
        async fn remove_tree(&self, path: &Path) -> Result<()> {
            for entry in self.list(path).await? {
                let child = path.join(&entry.name);
                if entry.kind == FileKind::Dir {
                    Box::pin(self.remove_tree(&child)).await?;
                } else {
                    self.delete(&normalize(&child)?).await?;
                }
            }
            self.delete(&normalize(path)?).await
        }

        /// The UNC path of a normalized path under the share.
        fn path_of(&self, components: &[String]) -> UncPath {
            match components.is_empty() {
                true => self.root.clone().with_no_path(),
                false => self.root.clone().with_path(&join(components)),
            }
        }
    }

    /// List the shares a server offers.
    ///
    /// Connects to IPC$ and asks the server service, so it needs no share to be
    /// configured — which is the point: it's how you find out what to configure.
    pub async fn enumerate(ip: IpAddr, config: Option<&SmbProbeConfig>) -> Result<Vec<ShareEntry>> {
        let server = ip.to_string();
        let client = Client::new(client_config(config));
        let (username, password) = credentials(config);

        client
            .ipc_connect(&server, &username, password)
            .await
            .with_context(|| format!("failed to connect to IPC$ on {server}"))?;
        let shares = client
            .list_shares(&server)
            .await
            .with_context(|| format!("failed to list shares on {server}"))?;
        client.close().await.ok();

        Ok(shares.iter().map(share_entry).collect())
    }

    /// Close a resource whichever kind it is. Closing is not optional: the server
    /// holds the handle open until it hears otherwise.
    async fn close(resource: Resource) -> Result<()> {
        match resource {
            Resource::File(file) => file.close().await?,
            Resource::Directory(dir) => dir.close().await?,
            Resource::Pipe(pipe) => pipe.close().await?,
        }
        Ok(())
    }

    /// Join normalized components into a share-relative SMB path.
    fn join(components: &[String]) -> String {
        components.join("\\")
    }

    /// A wide string as plain text. RPC strings are NUL-terminated and the
    /// terminator survives the conversion.
    fn text(value: &impl std::fmt::Display) -> String {
        value.to_string().trim_end_matches('\0').to_string()
    }

    fn kind(attributes: FileAttributes) -> FileKind {
        if attributes.reparse_point() {
            FileKind::Symlink
        } else if attributes.directory() {
            FileKind::Dir
        } else {
            FileKind::File
        }
    }

    /// Seconds since the Unix epoch, or `None` when the server reports no time.
    fn modified(time: FileTime) -> Option<i64> {
        match time.is_zero() {
            true => None,
            false => Some(time.date_time().assume_utc().unix_timestamp()),
        }
    }

    /// SMB reports attributes rather than POSIX mode bits, so `mode` stays `None`.
    fn file_attr(info: &FileAllInformation) -> FileAttr {
        FileAttr {
            kind: kind(info.basic.file_attributes),
            size: info.standard.end_of_file,
            modified: modified(info.basic.last_write_time),
            mode: None,
        }
    }

    fn dir_entry(name: String, info: &FileIdBothDirectoryInformation) -> DirEntry {
        DirEntry {
            name,
            kind: kind(info.file_attributes),
            size: info.end_of_file,
            modified: modified(info.last_write_time),
            mode: None,
        }
    }

    /// Space totals, in bytes. The server counts in allocation units, and reports
    /// two kinds of free space: what this caller may use (quota-limited) and what
    /// the volume actually has. `used` is derived from the latter so it matches
    /// what the server would report to anyone.
    fn usage(info: &FileFsFullSizeInformation) -> FsUsage {
        let unit = u64::from(info.sectors_per_allocation_unit) * u64::from(info.bytes_per_sector);
        let total = info.total_allocation_units.saturating_mul(unit);

        FsUsage {
            total,
            used: total.saturating_sub(info.actual_available_allocation_units.saturating_mul(unit)),
            free: info.caller_available_allocation_units.saturating_mul(unit),
        }
    }

    fn share_entry(info: &ShareInfo1) -> ShareEntry {
        // The RPC strings arrive behind an NDR pointer and its alignment wrapper.
        let name = info
            .netname
            .as_ref()
            .map(|netname| text(&**netname))
            .unwrap_or_default();
        let comment = info
            .remark
            .as_ref()
            .map(|remark| text(&**remark))
            .filter(|remark| !remark.is_empty());

        ShareEntry {
            name,
            kind: share_kind(info.share_type.kind()).to_string(),
            comment,
        }
    }

    fn share_kind(kind: ShareKind) -> &'static str {
        match kind {
            ShareKind::Disk => "Disk",
            ShareKind::PrintQ => "Printer",
            ShareKind::Device => "Device",
            ShareKind::IPC => "IPC",
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::PathBuf;

        fn components(path: &str) -> Vec<String> {
            normalize(Path::new(path)).expect("path should normalize")
        }

        #[test]
        fn joins_paths_with_smb_separators() {
            assert_eq!(join(&components("/")), "");
            assert_eq!(join(&components("/top.txt")), "top.txt");
            assert_eq!(
                join(&components("/docs/nested/deep.txt")),
                "docs\\nested\\deep.txt"
            );
        }

        #[test]
        fn rejects_paths_escaping_the_share() {
            assert!(normalize(&PathBuf::from("/docs/../../etc/passwd")).is_err());
        }

        #[test]
        fn maps_attributes_to_kinds() {
            assert_eq!(kind(FileAttributes::new()), FileKind::File);
            assert_eq!(
                kind(FileAttributes::new().with_directory(true)),
                FileKind::Dir
            );
            assert_eq!(
                kind(FileAttributes::new().with_reparse_point(true)),
                FileKind::Symlink
            );
            // A reparse point that is also a directory is still a link.
            assert_eq!(
                kind(FileAttributes::new()
                    .with_directory(true)
                    .with_reparse_point(true)),
                FileKind::Symlink
            );
        }

        #[test]
        fn converts_file_times_to_unix_seconds() {
            assert_eq!(modified(FileTime::ZERO), None);

            let stamp = time::macros::datetime!(2025-04-11 17:24:47);
            assert_eq!(
                modified(FileTime::from(stamp)),
                Some(stamp.assume_utc().unix_timestamp())
            );
        }

        #[test]
        fn derives_usage_from_allocation_units() {
            let info = FileFsFullSizeInformation {
                total_allocation_units: 1000,
                caller_available_allocation_units: 200,
                actual_available_allocation_units: 400,
                sectors_per_allocation_unit: 8,
                bytes_per_sector: 512,
            };
            let unit = 8 * 512;

            let usage = usage(&info);
            assert_eq!(usage.total, 1000 * unit);
            // Derived from what the volume has free, not from the caller's quota.
            assert_eq!(usage.used, 600 * unit);
            assert_eq!(usage.free, 200 * unit);
            assert!(usage.used + usage.free <= usage.total);
        }

        #[test]
        fn names_share_kinds() {
            assert_eq!(share_kind(ShareKind::Disk), "Disk");
            assert_eq!(share_kind(ShareKind::IPC), "IPC");
        }

        #[test]
        fn trims_the_terminator_off_rpc_strings() {
            assert_eq!(text(&"media\0"), "media");
            assert_eq!(text(&"media"), "media");
        }

        /// End-to-end against the probe test container's Samba
        /// (`sandpolis-probe/tests/`): NTLM session setup with Kerberos off,
        /// then list/stat/read on the seeded tree.
        ///
        /// [`enumerate`] is deliberately not covered: `smb` speaks DCE/RPC over
        /// NDR64 only, which Samba rejects at bind time, so share listing only
        /// works against Windows servers.
        ///
        /// ```shell
        /// docker run --rm --network host -e SERVICES=smb sandpolis/test-probe
        /// cargo test -p sandpolis-probe --features server smb_against_test_container -- --ignored
        /// ```
        ///
        /// `SMB_TEST_HOST` / `SMB_TEST_PORT` point it elsewhere, e.g. port 4445
        /// when the container runs rootless with host networking. Runs on the
        /// multi-threaded runtime the server uses, under a timeout, so a hung
        /// directory stream fails instead of blocking forever.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        #[ignore = "needs the probe test container's Samba"]
        async fn smb_against_test_container() {
            let ip: IpAddr = std::env::var("SMB_TEST_HOST")
                .unwrap_or_else(|_| "127.0.0.1".into())
                .parse()
                .expect("SMB_TEST_HOST is an IP address");
            let port = std::env::var("SMB_TEST_PORT")
                .ok()
                .map(|p| p.parse().expect("SMB_TEST_PORT is a port"));
            let config = SmbProbeConfig {
                share: "media".into(),
                port,
                username: Some("probe".into()),
                password: Some("password".into()),
                domain: Some("WORKGROUP".into()),
            };

            let run = async {
                let fs = SmbFs::mount(ip, &config).await.expect("mount share");

                let mut root: Vec<String> =
                    fs.list(Path::new("/")).await.expect("list /").into_iter().map(|e| e.name).collect();
                root.sort();
                assert_eq!(root, ["docs", "top.txt"]);

                let mut docs: Vec<(String, FileKind)> = fs
                    .list(Path::new("/docs"))
                    .await
                    .expect("list /docs")
                    .into_iter()
                    .map(|e| (e.name, e.kind))
                    .collect();
                docs.sort_by(|a, b| a.0.cmp(&b.0));
                assert_eq!(
                    docs,
                    [
                        ("empty.txt".to_string(), FileKind::File),
                        ("nested".to_string(), FileKind::Dir),
                        ("readme.txt".to_string(), FileKind::File),
                    ]
                );

                let attr = fs.stat(Path::new("/docs/readme.txt")).await.expect("stat");
                assert_eq!(attr.kind, FileKind::File);
                let content = fs
                    .read(Path::new("/docs/readme.txt"), 0, 4096)
                    .await
                    .expect("read readme.txt");
                assert_eq!(String::from_utf8_lossy(&content).trim_end(), "hello probe");
                assert_eq!(attr.size, content.len() as u64);

                let deep = fs
                    .read(Path::new("/docs/nested/deep.txt"), 0, 4096)
                    .await
                    .expect("read deep.txt");
                assert_eq!(String::from_utf8_lossy(&deep).trim_end(), "deep");
            };
            tokio::time::timeout(std::time::Duration::from_secs(60), run)
                .await
                .expect("SMB round trips finished within 60s");
        }
    }
}

#[cfg(feature = "server")]
pub use server::{SmbFs, enumerate};
