use crate::InstanceId;
use crate::ClusterId;
use crate::InstanceManager;
use crate::InstanceType;
use crate::database::Data;
use crate::database::RealmDatabase;
use crate::database::ResidentVec;
use crate::database::{DatabaseManager, Resident};
use crate::realm::config::CERTIFICATE_TAG;
use crate::realm::config::PRIVATE_KEY_TAG;
use crate::realm::config::RealmBootstrap;
use crate::realm::url::ServerUrl;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use native_db::ToKey;
use native_model::Model;
use pem::Pem;
use pem::encode;
use regex::Regex;
use sandpolis_macros::data;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Display;
use std::ops::Deref;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::LazyLock;
use tracing::debug;
use tracing::info;
use validator::{Validate, ValidationError, ValidationErrors};
use x509_parser::asn1_rs::Oid;
use x509_parser::prelude::{FromDer, GeneralName};
use x509_parser::prelude::{ParsedExtension, X509Certificate};

pub mod config;
#[cfg(feature = "server")]
pub mod server;
pub mod url;

static REALM_NAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^[a-z0-9]{4,32}$").unwrap());

/// Realms have unique names and are shared across the entire cluster. Realm
/// names cannot be changed after they are created.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RealmName(String);

impl Default for RealmName {
    fn default() -> Self {
        Self("default".into())
    }
}

impl Display for RealmName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Deref for RealmName {
    type Target = String;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FromStr for RealmName {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let name = RealmName(s.to_string());
        name.validate()?;
        Ok(name)
    }
}

impl Validate for RealmName {
    fn validate(&self) -> Result<(), ValidationErrors> {
        if REALM_NAME_REGEX.is_match(&self.0) {
            Ok(())
        } else {
            Err(ValidationErrors::new())
        }
    }
}

impl ToKey for RealmName {
    fn to_key(&self) -> native_db::Key {
        native_db::Key::new(self.0.as_bytes().to_vec())
    }

    fn key_names() -> Vec<String> {
        vec!["RealmName".to_string()]
    }
}

#[cfg(test)]
mod test_realm_name {
    use super::*;

    #[test]
    fn test_valid() {
        assert!("test".parse::<RealmName>().is_ok());
        assert!("1default".parse::<RealmName>().is_ok());
        assert!("default".parse::<RealmName>().is_ok());
        assert!("default99".parse::<RealmName>().is_ok());
    }

    #[test]
    fn test_invalid() {
        assert!("t".parse::<RealmName>().is_err());
        assert!("".parse::<RealmName>().is_err());
        assert!("test*".parse::<RealmName>().is_err());
        assert!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse::<RealmName>()
                .is_err()
        );
    }
}

/// The realms named by a set of endpoint certificates.
///
/// An instance with no certificates yet — a client before login, the mobile app
/// — still needs the default realm, which is where its own local data lives.
fn endpoint_realm_names(certs: &[RealmCert]) -> Vec<RealmName> {
    let mut names: Vec<RealmName> = certs.iter().map(|cert| cert.name.clone()).collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        names.push(RealmName::default());
    }
    names
}

/// Add the realms named by `endpoint_certs` to `inner`, creating a registry row
/// for any this instance hasn't recorded yet.
///
/// Shared between [`RealmManager::new`] and [`RealmManager::for_endpoint`]: a server with no
/// realm configs of its own and an endpoint attaching to one learn their realms
/// exactly the same way, from the certificates they hold.
fn insert_endpoint_realms(
    inner: &mut BTreeMap<RealmName, Realm>,
    registry: &ResidentVec<RealmData>,
    database: &DatabaseManager,
    endpoint_certs: &[RealmCert],
) -> Result<()> {
    for name in endpoint_realm_names(endpoint_certs) {
        let realm_db = database.realm(name.clone())?;
        let data = match registry.iter().find(|row| row.read().name == name) {
            Some(existing) => existing,
            None => registry.push_local(RealmData {
                name: name.clone(),
                ..Default::default()
            })?,
        };
        inner.insert(
            name.clone(),
            Realm {
                name,
                database: realm_db,
                data,
            },
        );
    }
    Ok(())
}

/// A realm this instance serves or connects to: its name, the database holding
/// its data, and its row in the realm registry.
#[derive(Clone)]
pub struct Realm {
    pub name: RealmName,
    pub database: RealmDatabase,
    pub data: Resident<RealmData>,
}

/// A realm CA that the caller should write back into the realm config it came
/// from, so the file stays the durable source of truth.
#[derive(Debug, Clone)]
pub struct MintedCa {
    pub name: RealmName,
    pub cert_pem: String,
    pub key_pem: String,
}

/// What bringing the realms up produced that belongs on disk rather than in the
/// database.
///
/// Both are written by the caller, which is the only thing that knows where this
/// server's files live.
#[derive(Debug, Clone, Default)]
pub struct RealmStartupOutput {
    /// CAs to write back into the realm configs the bootstraps came from.
    pub minted_cas: Vec<MintedCa>,

    /// One freshly minted endpoint certificate per realm, to be written out as
    /// `<realm>.realm.pem`.
    pub endpoint_certs: Vec<RealmCert>,
}

/// Every realm known to this instance.
///
/// Server-side realms are frozen at startup: they only ever come from realm
/// configs. Endpoint realms can also be added at runtime via
/// [`add_endpoint_cert`](Self::add_endpoint_cert), which is how a client that
/// started without a certificate attaches to a realm the user picks in the
/// GUI. Both maps are shared across clones so an addition is visible
/// everywhere.
#[derive(Clone)]
pub struct RealmManager {
    database: DatabaseManager,

    /// The realms themselves, keyed by name.
    inner: Arc<std::sync::RwLock<BTreeMap<RealmName, Realm>>>,

    /// Registry rows, which live in the default realm's database so an instance
    /// knows what other realms exist.
    pub realms: ResidentVec<RealmData>,

    /// Endpoint certificates loaded from realm certs. Kept in memory only.
    ///
    /// Clients and agents hold the same kind of certificate, and so does a local
    /// stratum server authenticating to its global stratum server — there is no
    /// separate server-to-server certificate type.
    #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
    endpoint_certs: Arc<std::sync::RwLock<Vec<RealmCert>>>,
}

impl RealmManager {
    /// `authoritative` is true only on the global stratum server, which owns the
    /// realm CA.
    ///
    /// A local stratum server must never mint a CA of its own — that would make
    /// it a separate trust root and its agents unreachable from the rest of the
    /// network. It starts with no certificates and is issued a server
    /// certificate by the global stratum server (see
    /// [`install_enrollment`](Self::install_enrollment)).
    ///
    /// Returns what the caller has to write out: any realm CAs that belong back
    /// in the realm configs the bootstraps came from, and the endpoint
    /// certificate minted for each realm.
    ///
    /// `default_address` names the host and port certificates carry when a
    /// realm declares no address of its own; its realm field is ignored (each
    /// bootstrap's realm name is substituted).
    #[allow(unused_variables)]
    pub async fn new(
        bootstraps: Vec<RealmBootstrap>,
        endpoint_certs: Vec<RealmCert>,
        database: DatabaseManager,
        instance: InstanceManager,
        authoritative: bool,
        default_address: ServerUrl,
    ) -> Result<(Self, RealmStartupOutput)> {
        debug!("Initializing realms");

        let registry: ResidentVec<RealmData> =
            database.realm(RealmName::default())?.resident_vec(())?;

        #[allow(unused_mut)]
        let mut output = RealmStartupOutput::default();
        #[allow(unused_mut)]
        let mut inner: BTreeMap<RealmName, Realm> = BTreeMap::new();

        // Only the global stratum server holds the realm CA and can issue from
        // it. A local stratum server gets its server certificate from the GS
        // instead, so that the whole network shares one trust root.
        #[allow(unused_mut, unused_assignments)]
        let mut authored_from_files = false;

        #[cfg(feature = "server")]
        if authoritative {
            for bootstrap in &bootstraps {
                let name = bootstrap.name.clone();

                // Realm databases are only ever created here, from a file.
                let realm_db = database.realm(name.clone())?;

                let data = match registry.iter().find(|row| row.read().name == name) {
                    Some(existing) => existing,
                    None => registry.push_local(RealmData {
                        name: name.clone(),
                        ..Default::default()
                    })?,
                };

                // This instance's own certificates are local state, not estate
                // data replicated from the global stratum server.
                let rw = realm_db.local_write()?;
                let certs: Vec<RealmCert> =
                    rw.scan().primary()?.all()?.collect::<Result<Vec<_>, _>>()?;

                // The file is the source of truth: whatever it declares replaces
                // what the database held.
                let ca = if let Some((cert, key)) = bootstrap.ca.clone() {
                    for old in certs
                        .iter()
                        .filter(|c| c.cert_type == RealmCertType::Cluster)
                    {
                        rw.remove(old.clone())?;
                    }
                    let ca = RealmCert {
                        cert_type: RealmCertType::Cluster,
                        name: name.clone(),
                        cert,
                        key: Some(key),
                        ..Default::default()
                    };
                    rw.insert(ca.clone())?;
                    ca
                } else {
                    // The file declared no CA, so reuse the database's copy or
                    // mint one. Either way it goes back into the file.
                    let ca = match certs
                        .iter()
                        .find(|c| c.cert_type == RealmCertType::Cluster)
                    {
                        Some(existing) => existing.clone(),
                        None => {
                            let ca = RealmCert::new_cluster(instance.cluster_id, name.clone())?;
                            rw.insert(ca.clone())?;
                            ca
                        }
                    };
                    if let Some(key) = ca.key.as_ref() {
                        output.minted_cas.push(MintedCa {
                            name: name.clone(),
                            cert_pem: encode(&Pem::new(CERTIFICATE_TAG, ca.cert.clone())),
                            key_pem: encode(&Pem::new(PRIVATE_KEY_TAG, key.clone())),
                        });
                    }
                    ca
                };

                // Get or create this instance's server cert
                if !certs.iter().any(|c| {
                    c.cert_type == RealmCertType::Server
                        && c._instance_id == Some(instance.instance_id)
                }) {
                    rw.insert(ca.server_cert(instance.instance_id.try_into()?)?)?;
                }

                rw.commit()?;

                // Certificates minted here name the address this realm is
                // reachable at, falling back to the caller's default for a
                // realm that declares none.
                let mut url: ServerUrl = bootstrap
                    .address
                    .clone()
                    .unwrap_or_else(|| default_address.clone());
                url.realm = name.clone();

                // X.509 ub-common-name caps the encoded identity at 64
                // characters; a longer one would produce a certificate
                // instances can't parse back.
                if url.canonical().len() > 64 {
                    bail!(
                        "realm address `{}` exceeds 64 characters and can't be \
                         encoded into a certificate; use a shorter hostname or \
                         realm name",
                        url.canonical()
                    );
                }

                // The caller writes this out as `<realm>.realm.pem`. Clients and
                // agents both attach with it, so a realm is usable as soon as
                // its server has started once.
                output.endpoint_certs.push(ca.endpoint_cert(&url)?);

                inner.insert(
                    name.clone(),
                    Realm {
                        name,
                        database: realm_db,
                        data,
                    },
                );
            }

            // A realm whose file is gone this run leaves the registry, but its
            // database file stays on disk — dropping data because a path changed
            // would be the wrong default.
            for row in registry.iter() {
                let name = row.read().name.clone();
                if !inner.contains_key(&name) {
                    tracing::warn!(
                        realm = %name,
                        "No realm config was given for this realm; removing it from the registry \
                         and leaving its database on disk"
                    );
                    let id = row.read().id();
                    registry.remove_local(id)?;
                }
            }

            authored_from_files = true;
        }

        if !authored_from_files {
            // Everyone else learns which realms exist from the certificates they
            // hold, since those name exactly what they can authenticate against.
            insert_endpoint_realms(&mut inner, &registry, &database, &endpoint_certs)?;
        }

        Ok((
            Self {
                database,
                inner: Arc::new(std::sync::RwLock::new(inner)),
                realms: registry,
                #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
                endpoint_certs: Arc::new(std::sync::RwLock::new(endpoint_certs)),
            },
            output,
        ))
    }

    /// Build the realm set for an instance that only ever connects out — an
    /// agent, a standalone client, the mobile app, an example.
    ///
    /// Such an instance serves no realms of its own, so its certificates are the
    /// whole story: it knows exactly the realms they name and nothing else.
    pub fn for_endpoint(endpoint_certs: Vec<RealmCert>, database: DatabaseManager) -> Result<Self> {
        let registry: ResidentVec<RealmData> =
            database.realm(RealmName::default())?.resident_vec(())?;

        let mut inner = BTreeMap::new();
        insert_endpoint_realms(&mut inner, &registry, &database, &endpoint_certs)?;

        Ok(Self {
            database,
            inner: Arc::new(std::sync::RwLock::new(inner)),
            realms: registry,
            #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
            endpoint_certs: Arc::new(std::sync::RwLock::new(endpoint_certs)),
        })
    }

    /// The realm called `name`, or `None` if this instance doesn't know it.
    pub fn get(&self, name: &RealmName) -> Option<Realm> {
        self.inner.read().unwrap().get(name).cloned()
    }

    /// Every known realm. Owned snapshots, since the underlying map is shared
    /// and can grow at runtime.
    pub fn iter(&self) -> impl Iterator<Item = Realm> {
        self.inner
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// The database for `name`. Server-side realms are never created on the
    /// fly, so an unknown name is an error rather than a new realm.
    pub fn realm(&self, name: RealmName) -> Result<RealmDatabase> {
        match self.inner.read().unwrap().get(&name) {
            Some(realm) => Ok(realm.database.clone()),
            None => bail!("Realm does not exist: {name}"),
        }
    }

    /// Attach to the realm named by `cert` at runtime: create or open the
    /// realm's database, register it, and make the certificate available to
    /// [`find_endpoint_cert`](Self::find_endpoint_cert). Replaces any
    /// previously loaded certificate for the same realm.
    ///
    /// This is how a client that started with no certificate attaches to a
    /// realm the user picks in the GUI without a restart.
    #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
    pub fn add_endpoint_cert(&self, cert: RealmCert) -> Result<()> {
        {
            let mut inner = self.inner.write().unwrap();
            insert_endpoint_realms(
                &mut inner,
                &self.realms,
                &self.database,
                std::slice::from_ref(&cert),
            )?;
        }

        let mut certs = self.endpoint_certs.write().unwrap();
        certs.retain(|existing| existing.name != cert.name);
        certs.push(cert);
        Ok(())
    }

    /// Whether any endpoint certificate is loaded. A client without one has no
    /// server it could ever connect to, which is what prompts the realm
    /// selection dialog.
    #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
    pub fn has_endpoint_certs(&self) -> bool {
        !self.endpoint_certs.read().unwrap().is_empty()
    }

    /// Whether this instance already holds the certificates it needs to serve
    /// `realm`: the realm CA (to verify peers) and its own server certificate
    /// (to present).
    #[cfg(feature = "server")]
    pub fn has_server_cert(&self, realm: RealmName, instance_id: crate::InstanceId) -> bool {
        let Ok(db) = self.database.realm(realm) else {
            return false;
        };
        let Ok(r) = db.r_transaction() else {
            return false;
        };

        let Ok(certs) = (|| -> Result<Vec<RealmCert>> {
            Ok(r.scan().primary()?.all()?.collect::<Result<Vec<_>, _>>()?)
        })() else {
            return false;
        };

        let has_ca = certs
            .iter()
            .any(|c| c.cert_type == RealmCertType::Cluster);
        let has_cert = certs
            .iter()
            .any(|c| c.cert_type == RealmCertType::Server && c._instance_id == Some(instance_id));

        has_ca && has_cert
    }

    /// Store the realm CA and server certificate issued by the global stratum
    /// server.
    ///
    /// `ca` is the CA certificate **without** its private key — a local stratum
    /// server can verify peers against the realm's trust root but can never
    /// issue from it. These are this instance's own credentials, so they are
    /// written locally even though the database is a replica.
    #[cfg(feature = "server")]
    pub fn install_enrollment(
        &self,
        realm: RealmName,
        ca: Vec<u8>,
        cert: Vec<u8>,
        key: Vec<u8>,
        instance_id: crate::InstanceId,
    ) -> Result<()> {
        let db = self.database.realm(realm.clone())?;
        let rw = db.local_write()?;

        let existing: Vec<RealmCert> =
            rw.scan().primary()?.all()?.collect::<Result<Vec<_>, _>>()?;
        for old in existing {
            match old.cert_type {
                RealmCertType::Cluster => rw.remove(old)?,
                RealmCertType::Server if old._instance_id == Some(instance_id) => rw.remove(old)?,
                _ => continue,
            };
        }

        rw.insert(RealmCert {
            cert_type: RealmCertType::Cluster,
            name: realm.clone(),
            cert: ca.clone(),
            key: None,
            ..Default::default()
        })?;
        rw.insert(RealmCert {
            cert_type: RealmCertType::Server,
            name: realm.clone(),
            ca,
            cert,
            key: Some(key),
            _instance_id: Some(instance_id),
            ..Default::default()
        })?;

        rw.commit()?;
        info!(realm = %realm, "Installed server certificate issued by the global stratum server");
        Ok(())
    }

    /// The certificate this process presents when it dials a server.
    ///
    /// Clients, agents, and a local stratum server dialing its global stratum
    /// server all present the same kind of certificate, so what this process is
    /// doesn't come into it — only which realm it's dialing.
    #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
    pub fn find_endpoint_cert(&self, realm: RealmName) -> Result<RealmCert> {
        for cert in self.endpoint_certs.read().unwrap().iter() {
            if cert.name == realm {
                return Ok(cert.clone());
            }
        }

        bail!("No realm certificate loaded for realm: {realm}");
    }
}

/// Every realm cert in `dir` (`*.realm.pem`), which is how an instance is
/// attached without naming a file on the command line.
///
/// A directory that holds none is not an error — a client with nowhere to
/// connect starts at its realm selection dialog.
pub fn load_realm_certs_dir(dir: &Path) -> Result<Vec<RealmCert>> {
    use crate::realm::config::REALM_CERT_SUFFIX;

    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("Reading {}", dir.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(REALM_CERT_SUFFIX))
        })
        .collect();

    // Sorted so the first cert — the one the primary connection is made to — is
    // the same on every start.
    paths.sort();

    paths.iter().map(RealmCert::read_pem).collect()
}

/// A realm is a set of clients and agents that can interact. Each realm has a
/// global CA certificate that signs certificates used to connect to the server.
///
/// All servers have a default realm called "default". All `RealmData` entries
/// are stored within this realm.
#[data]
#[derive(Default, Validate)]
pub struct RealmData {
    #[secondary_key(unique)]
    pub name: RealmName,
    pub owner: String,
}

/// What a realm certificate is for, which decides how it's issued and which of
/// its fields carry meaning.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmCertType {
    /// The realm's CA, which signs every other certificate in the realm. This is
    /// the only kind that can issue, and only where it still holds its key.
    #[default]
    Cluster,

    /// A server's listener identity.
    Server,

    /// A client or agent: anything that dials a server.
    Endpoint,
}

impl RealmCertType {
    /// Custom extended key usage OID marking what the certificate is for.
    ///
    /// A CA needs none — its basic constraints already say what it is. The
    /// endpoint arc is the agent and client masks together, because agents and
    /// clients hold the same certificate.
    pub fn oid(&self) -> Option<[u64; 4]> {
        match self {
            Self::Cluster => None,
            Self::Server => Some([1, 1, 1, InstanceType::Server.mask() as u64]),
            Self::Endpoint => Some([
                1,
                1,
                1,
                (InstanceType::Agent.mask() | InstanceType::Client.mask()) as u64,
            ]),
        }
    }
}

impl ToKey for RealmCertType {
    fn to_key(&self) -> native_db::Key {
        native_db::Key::new(vec![match self {
            Self::Cluster => 0,
            Self::Server => 1,
            Self::Endpoint => 2,
        }])
    }

    fn key_names() -> Vec<String> {
        vec!["RealmCertType".to_string()]
    }
}

/// A certificate belonging to one realm: the realm's CA, a server's listener
/// identity, or an endpoint's credential for dialing one.
///
/// Clients and agents hold the same kind of certificate. The server doesn't
/// distinguish them — what a client is allowed to do comes from the user it
/// logs in as, not from the certificate that got it onto the network.
#[data]
#[derive(Default)]
pub struct RealmCert {
    /// The instance whose credential this is. `None` for certificates that
    /// belong to no single instance: the realm CA, and endpoint certificates
    /// loaded from a file before this process has an identity.
    #[secondary_key(optional)]
    pub _instance_id: Option<InstanceId>,

    pub cert_type: RealmCertType,

    /// The realm this certificate belongs to.
    pub name: RealmName,

    /// The realm CA, for verifying the peer. Empty on a
    /// [`Cluster`](RealmCertType::Cluster) certificate, which is itself the CA.
    pub ca: Vec<u8>,

    pub cert: Vec<u8>,

    /// The private half, which is absent wherever the holder can only verify:
    /// the CA on a local stratum server, or a certificate handed out without it.
    pub key: Option<Vec<u8>>,
}

impl Validate for RealmCert {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();

        // Parse the certificate
        let cert = match X509Certificate::from_der(&self.cert) {
            Ok((_, cert)) => cert,
            Err(_) => {
                errors.add(
                    "cert",
                    ValidationError::new("Invalid X.509 certificate format"),
                );
                return Err(errors);
            }
        };

        // A CA carries no type OID and doesn't authenticate anything itself.
        let Some(oid) = self.cert_type.oid() else {
            return Ok(());
        };

        let mut client_auth = false;
        let mut typed = false;
        for ext in cert.iter_extensions() {
            if let ParsedExtension::ExtendedKeyUsage(eku) = ext.parsed_extension() {
                if eku.client_auth {
                    client_auth = true;
                }
                if eku.other.contains(&Oid::from(&oid).unwrap()) {
                    typed = true;
                }
            }
        }

        if !typed {
            errors.add(
                "cert",
                ValidationError::new("Certificate is not of the expected type"),
            );
        }
        if !client_auth {
            errors.add(
                "cert",
                ValidationError::new("Certificate must have clientAuth extended key usage"),
            );
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl RealmCert {
    /// The cluster this certificate's realm belongs to, which is named in the
    /// CA's subject alternative name.
    pub fn cluster_id(&self) -> Result<ClusterId> {
        // A CA certificate is its own trust root, so it names the cluster itself.
        let der = match self.cert_type {
            RealmCertType::Cluster => &self.cert,
            _ => &self.ca,
        };

        for ext in X509Certificate::from_der(der)?.1.iter_extensions() {
            if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
                for name in &san.general_names {
                    if let GeneralName::DNSName(s) = name {
                        return s.parse::<ClusterId>();
                    }
                }
            }
        }

        bail!("Subject name not found");
    }

    /// The name this certificate is served under, which is how the SNI resolver
    /// keys a server certificate.
    pub fn subject_name(&self) -> Result<String> {
        for ext in X509Certificate::from_der(&self.cert)?.1.iter_extensions() {
            if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
                for name in &san.general_names {
                    if let GeneralName::DNSName(s) = name {
                        return Ok(s.to_string());
                    }
                }
            }
        }

        bail!("Subject name not found");
    }

    /// Write the certificate out as a realm cert.
    pub fn write_pem<P>(&self, path: P) -> Result<()>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        std::fs::write(path, config::to_pem(self))
            .with_context(|| format!("Writing {}", path.display()))?;
        Ok(())
    }

    /// Read a realm cert.
    pub fn read_pem<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("Reading {}", path.display()))?;
        config::from_pem(&contents, path)
    }

    /// The realm CA, for verifying the server this certificate names.
    #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
    pub fn root_certificate(&self) -> Result<reqwest::Certificate> {
        Ok(reqwest::Certificate::from_der(&self.ca)?)
    }

    /// This certificate and its key, for authenticating to a server.
    #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
    pub fn identity(&self) -> Result<reqwest::Identity> {
        // Combine cert and key together
        let mut bundle = Vec::new();
        bundle.extend_from_slice(encode(&Pem::new("CERTIFICATE", self.cert.clone())).as_bytes());
        bundle.extend_from_slice(
            encode(&Pem::new(
                "PRIVATE KEY",
                self.key.as_ref().ok_or_else(|| anyhow!("No key"))?.clone(),
            ))
            .as_bytes(),
        );
        Ok(reqwest::Identity::from_pem(&bundle)?)
    }

    /// The TLS configuration for dialing the server this certificate names:
    /// trust the realm CA alone, and present this certificate.
    ///
    /// The `reqwest` client gets the same two things through
    /// [`root_certificate`](Self::root_certificate) and
    /// [`identity`](Self::identity); this is for the QUIC endpoint, which takes
    /// `rustls` directly.
    #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
    pub fn client_tls_config(&self) -> Result<rustls::ClientConfig> {
        use rustls_pki_types::{CertificateDer, PrivateKeyDer};

        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(self.ca.clone()))?;

        let key = PrivateKeyDer::try_from(self.key.clone().ok_or_else(|| anyhow!("No key"))?)
            .map_err(|e| anyhow!("Invalid private key: {e}"))?;

        Ok(rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![CertificateDer::from(self.cert.clone())], key)?)
    }

    /// Return when the certificate was generated.
    pub fn creation_time(&self) -> Result<i64> {
        Ok(X509Certificate::from_der(&self.cert)?
            .1
            .validity
            .not_before
            .timestamp())
    }

    /// The server this certificate was issued for, encoded in its common name.
    pub fn url(&self) -> Result<ServerUrl> {
        common_name_url(&self.cert)
    }
}

/// Parse the [`ServerUrl`] out of a certificate's common name.
pub(crate) fn common_name_url(der: &[u8]) -> Result<ServerUrl> {
    X509Certificate::from_der(der)?
        .1
        .subject()
        .iter_common_name()
        .next()
        .ok_or_else(|| anyhow!("no common name"))?
        .to_owned()
        .as_str()
        .map_err(|_| anyhow!("invalid common name"))?
        .parse()
}

#[cfg(all(test, feature = "server"))]
mod test_enrollment {
    use super::*;
    use crate::id::{AgentId, ServerId};
    use crate::database::{ScopeTable, WriteAuthority};
    use crate::InstanceId;

    fn models() -> &'static native_db::Models {
        static MODELS: std::sync::OnceLock<native_db::Models> = std::sync::OnceLock::new();
        MODELS.get_or_init(|| {
            let mut m = native_db::Models::new();
            m.define::<RealmData>().unwrap();
            m.define::<RealmCert>().unwrap();
            m
        })
    }

    fn replica() -> Result<DatabaseManager> {
        Ok(DatabaseManager::new(
            crate::database::config::DatabaseConfig {
                storage: None,
                key: Default::default(),
            },
            models(),
            WriteAuthority::Scoped(std::sync::Arc::new(ScopeTable::default())),
        )?)
    }

    /// A local stratum server's realm set: it knows the default realm but was
    /// issued nothing yet.
    fn manager(database: DatabaseManager) -> RealmManager {
        RealmManager::for_endpoint(Vec::new(), database).unwrap()
    }

    /// A local stratum server starts with nothing: it must not invent a CA, and
    /// it can't serve until the global stratum server has issued its cert.
    #[tokio::test]
    async fn replica_starts_without_certificates() -> Result<()> {
        let realms = manager(replica()?);
        let id = InstanceId::from(ServerId::random());
        assert!(!realms.has_server_cert(RealmName::default(), id));
        Ok(())
    }

    /// Installing what the global stratum server issued makes the server ready,
    /// and the CA arrives without its private key so a local stratum server can
    /// verify peers but never issue certificates of its own.
    #[tokio::test]
    async fn enrollment_installs_ca_without_its_key() -> Result<()> {
        let cluster_id = crate::ClusterId::default();
        let ca = RealmCert::new_cluster(cluster_id, RealmName::default())?;
        let server_id = ServerId::random();
        let id = InstanceId::from(server_id);
        let issued = ca.server_cert(server_id)?;

        let realms = manager(replica()?);
        realms.install_enrollment(
            RealmName::default(),
            ca.cert.clone(),
            issued.cert.clone(),
            issued.key.clone().expect("issued cert carries a key"),
            id,
        )?;

        assert!(realms.has_server_cert(RealmName::default(), id));

        let db = realms.database.realm(RealmName::default())?;
        let r = db.r_transaction()?;
        let stored: Vec<RealmCert> = r
            .scan()
            .primary()?
            .all()?
            .collect::<Result<Vec<RealmCert>, _>>()?
            .into_iter()
            .filter(|cert| cert.cert_type == RealmCertType::Cluster)
            .collect();

        assert_eq!(stored.len(), 1, "exactly one CA is stored");
        assert_eq!(stored[0].cert, ca.cert, "the CA certificate is the GS's");
        assert!(
            stored[0].key.is_none(),
            "the CA private key must never reach a local stratum server"
        );
        assert!(
            stored[0].issuer().is_err(),
            "without the key, a local stratum server cannot issue certificates"
        );
        Ok(())
    }

    /// Re-enrolling replaces the previous credentials rather than accumulating
    /// them, so `resident()` (which expects a singleton CA) keeps working.
    #[tokio::test]
    async fn re_enrolling_replaces_credentials() -> Result<()> {
        let server_id = ServerId::random();
        let id = InstanceId::from(server_id);
        let realms = manager(replica()?);

        for _ in 0..2 {
            let ca = RealmCert::new_cluster(crate::ClusterId::default(), RealmName::default())?;
            let issued = ca.server_cert(server_id)?;
            realms.install_enrollment(
                RealmName::default(),
                ca.cert.clone(),
                issued.cert,
                issued.key.expect("issued cert carries a key"),
                id,
            )?;
        }

        let db = realms.database.realm(RealmName::default())?;
        let r = db.r_transaction()?;
        let stored: Vec<RealmCert> =
            r.scan().primary()?.all()?.collect::<Result<Vec<_>, _>>()?;

        assert_eq!(
            stored
                .iter()
                .filter(|cert| cert.cert_type == RealmCertType::Cluster)
                .count(),
            1
        );
        assert_eq!(
            stored
                .iter()
                .filter(|cert| cert.cert_type == RealmCertType::Server)
                .count(),
            1
        );
        Ok(())
    }

    /// A certificate imported at runtime (the GUI realm-selection dialog) is
    /// visible through clones taken before the import, since the GUI and the
    /// connection layer each hold their own clone of the manager.
    #[tokio::test]
    async fn add_endpoint_cert_reaches_earlier_clones() -> Result<()> {
        let realms = manager(replica()?);
        let clone = realms.clone();

        let name: RealmName = "myrealm".parse()?;
        assert!(!clone.has_endpoint_certs());
        assert!(clone.find_endpoint_cert(name.clone()).is_err());
        assert!(clone.realm(name.clone()).is_err());

        let ca = RealmCert::new_cluster(crate::ClusterId::default(), name.clone())?;
        let cert = ca.endpoint_cert(&"gs.example.com:9000/myrealm".parse::<ServerUrl>()?)?;
        realms.add_endpoint_cert(cert)?;

        assert!(clone.has_endpoint_certs());
        assert!(clone.find_endpoint_cert(name.clone()).is_ok());
        assert!(clone.realm(name).is_ok());
        Ok(())
    }

    /// A server certificate is only ever issued to a server: a non-server id
    /// can't even be converted into the argument `server_cert` takes.
    #[tokio::test]
    async fn server_cert_requires_a_server_id() -> Result<()> {
        assert!(ServerId::try_from(InstanceId::from(AgentId::random())).is_err());
        Ok(())
    }
}

#[cfg(all(test, feature = "server"))]
mod test_realm_cert {
    use super::*;
    use crate::id::ServerId;
    use crate::realm::config::{CERTIFICATE_TAG, to_pem};
    

    fn url() -> ServerUrl {
        "gs.example.com:9000/myrealm".parse().unwrap()
    }

    /// An endpoint certificate survives the round trip through a realm cert
    /// with its server URL intact. The same file is what both a client and an
    /// agent are given.
    #[test]
    fn endpoint_cert_round_trips() -> Result<()> {
        let cluster_id = crate::ClusterId::default();
        let ca = RealmCert::new_cluster(cluster_id, "myrealm".parse()?)?;
        let original = ca.endpoint_cert(&url())?;

        let temp_file = tempfile::NamedTempFile::new()?;
        original.write_pem(temp_file.path())?;

        let read_cert = RealmCert::read_pem(temp_file.path())?;

        assert_eq!(read_cert.cert_type, RealmCertType::Endpoint);
        assert_eq!(original.ca, read_cert.ca);
        assert_eq!(original.cert, read_cert.cert);
        assert_eq!(original.key, read_cert.key);
        assert_eq!(read_cert.cluster_id()?, cluster_id);
        assert_eq!(read_cert.url()?.canonical(), url().canonical());
        assert_eq!(read_cert.name, "myrealm".parse()?);
        Ok(())
    }

    /// The two certificates are told apart by which one signed itself, so a
    /// file assembled by hand loads whichever order they were concatenated in.
    #[test]
    fn blocks_load_in_either_order() -> Result<()> {
        let ca = RealmCert::new_cluster(crate::ClusterId::default(), "myrealm".parse()?)?;
        let original = ca.endpoint_cert(&url())?;

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("reversed.realm.pem");
        std::fs::write(
            &path,
            format!(
                "{}{}{}",
                encode(&Pem::new(CERTIFICATE_TAG, original.ca.clone())),
                encode(&Pem::new(CERTIFICATE_TAG, original.cert.clone())),
                encode(&Pem::new(
                    crate::realm::config::PRIVATE_KEY_TAG,
                    original.key.clone().expect("minted certs carry a key"),
                )),
            ),
        )?;

        let read_cert = RealmCert::read_pem(&path)?;
        assert_eq!(original.cert, read_cert.cert);
        assert_eq!(original.ca, read_cert.ca);
        Ok(())
    }

    /// A realm cert only ever holds an endpoint certificate, so one holding
    /// anything else — a server certificate, say — is rejected rather than
    /// loaded as something it isn't.
    #[test]
    fn only_endpoint_certs_load() -> Result<()> {
        let ca = RealmCert::new_cluster(crate::ClusterId::default(), "myrealm".parse()?)?;
        let server_cert = ca.server_cert(ServerId::random())?;

        let temp_file = tempfile::NamedTempFile::new()?;
        std::fs::write(temp_file.path(), to_pem(&server_cert))?;

        assert!(RealmCert::read_pem(temp_file.path()).is_err());
        Ok(())
    }

    /// The realm CA has to be in the file too — without it there is nothing to
    /// verify the server with, so a lone certificate is an error rather than a
    /// half-usable credential.
    #[test]
    fn a_single_certificate_is_rejected() -> Result<()> {
        let ca = RealmCert::new_cluster(crate::ClusterId::default(), "myrealm".parse()?)?;
        let original = ca.endpoint_cert(&url())?;

        let temp_file = tempfile::NamedTempFile::new()?;
        std::fs::write(
            temp_file.path(),
            encode(&Pem::new(CERTIFICATE_TAG, original.cert.clone())),
        )?;

        assert!(RealmCert::read_pem(temp_file.path()).is_err());
        Ok(())
    }
}
