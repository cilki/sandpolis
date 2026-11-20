use anyhow::{Context, Result, bail};
use pem::{Pem, encode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Realms have unique names and are shared across the entire cluster. Realm
/// names cannot be changed after they are created.
///
/// A newtype rather than a string as far as serde is concerned, so a realm
/// config spells it `("default")`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RealmName(pub(super) String);

/// Locates a server instance over the network. These have a format like:
///
/// ```text
/// https://example.com:8768/default
/// ```
///
/// With default information omitted, the URL can be as simple as:
///
/// ```text
/// https://example.com
/// ```
///
/// That's its string form; serde writes it as a struct, which is how a realm
/// config spells it (`(host: "example.com", port: 8768, realm: ("default"))`).
#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub struct ServerUrl {
    pub host: String,
    pub port: u16,
    pub realm: RealmName,
}

/// PEM tag for an X.509 certificate.
pub const CERTIFICATE_TAG: &str = "CERTIFICATE";

/// PEM tag for a PKCS#8 private key.
pub const PRIVATE_KEY_TAG: &str = "PRIVATE KEY";

/// A certificate or private key as it appears in a realm config.
///
/// Both forms hold PEM: either written out in the file itself, or in a separate
/// file next to it. Inline is what the server writes back when it generates a
/// realm CA; a path is convenient when the material is managed elsewhere (a
/// secret manager, an ACME client, etc).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum CertSource {
    Inline(String),
    Path(PathBuf),
}

impl CertSource {
    /// Wrap DER bytes as inline PEM with `tag`.
    pub fn inline_der(der: &[u8], tag: &str) -> Self {
        Self::Inline(encode(&Pem::new(tag, der.to_vec())))
    }

    /// Decode this source into DER bytes, requiring the PEM block to carry
    /// `tag`.
    ///
    /// A relative [`Path`](Self::Path) resolves against `base_dir`, which is the
    /// directory holding the file this source was read from.
    pub fn load_der(&self, base_dir: Option<&Path>, tag: &str) -> Result<Vec<u8>> {
        let pem = match self {
            Self::Inline(contents) => contents.clone(),
            Self::Path(path) => {
                let path = match base_dir {
                    Some(base) if path.is_relative() => base.join(path),
                    _ => path.clone(),
                };
                std::fs::read_to_string(&path)
                    .with_context(|| format!("Reading {}", path.display()))?
            }
        };

        let parsed = pem::parse(pem.as_bytes()).context("Parsing PEM")?;
        if parsed.tag() != tag {
            bail!("Expected a {tag} PEM block but found {}", parsed.tag());
        }
        Ok(parsed.into_contents())
    }
}

/// A realm's root certificate authority, as declared in a realm config.
///
/// The global stratum server needs the key to issue certificates; a CA without
/// one can only verify.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CaConfig {
    pub cert: CertSource,
    pub key: Option<CertSource>,
}

impl CaConfig {
    /// Decode the CA certificate and (if present) its private key into DER.
    pub fn load_der(&self, base_dir: Option<&Path>) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
        let cert = self.load_cert_der(base_dir)?;
        let key = match &self.key {
            Some(key) => Some(key.load_der(base_dir, PRIVATE_KEY_TAG)?),
            None => None,
        };
        Ok((cert, key))
    }

    /// Decode just the CA certificate into DER.
    fn load_cert_der(&self, base_dir: Option<&Path>) -> Result<Vec<u8>> {
        self.cert.load_der(base_dir, CERTIFICATE_TAG)
    }
}

/// Everything a server needs to bring one realm up: what it's called, the trust
/// root it serves under, and the address the certificates it mints will name.
///
/// A realm only ever comes from a realm config (or the implicit default of a
/// zero-flag run), so this is the complete set of realms for a process.
#[derive(Debug, Clone, Default)]
pub struct RealmBootstrap {
    pub name: RealmName,

    /// The realm CA as `(certificate, key)` DER. Absent when the file declared
    /// no CA, in which case the server reuses the copy in the realm database or
    /// mints a fresh one and writes it back to the file.
    pub ca: Option<(Vec<u8>, Vec<u8>)>,

    /// Address clients and agents will use to reach this realm. Certificates
    /// minted for the realm carry it in their common name, so it must be how the
    /// server is actually reachable.
    pub address: Option<ServerUrl>,
}

/// RON parsing options for the realm config: allow optional fields without an
/// explicit `Some`.
pub fn ron_options() -> ron::Options {
    ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
}

/// Filename suffix of a realm cert, which is also how a directory full of them
/// is recognized.
pub const REALM_CERT_SUFFIX: &str = ".realm.pem";

/// Render an endpoint certificate as a realm cert.
///
/// Three PEM blocks: the endpoint certificate, the realm CA that signed it, and
/// the endpoint's private key. Leaf first, as a TLS chain is conventionally
/// written.
pub fn to_pem(cert: &super::RealmCert) -> String {
    let mut out = encode(&Pem::new(CERTIFICATE_TAG, cert.cert.clone()));
    out.push_str(&encode(&Pem::new(CERTIFICATE_TAG, cert.ca.clone())));
    if let Some(key) = cert.key.as_ref() {
        out.push_str(&encode(&Pem::new(PRIVATE_KEY_TAG, key.clone())));
    }
    out
}

/// Decode a realm cert into the endpoint certificate it holds.
///
/// The realm it authenticates against comes from the certificate's common name,
/// so the file never has to name it separately. `source` only names the file in
/// error messages.
pub fn from_pem(contents: &str, source: &Path) -> Result<super::RealmCert> {
    use validator::Validate;

    let blocks =
        pem::parse_many(contents).with_context(|| format!("Parsing {}", source.display()))?;

    let mut certs = Vec::new();
    let mut keys = Vec::new();
    for block in blocks {
        match block.tag() {
            CERTIFICATE_TAG => certs.push(block.into_contents()),
            PRIVATE_KEY_TAG => keys.push(block.into_contents()),
            tag => bail!("{} holds an unexpected {tag} block", source.display()),
        }
    }

    if certs.len() != 2 {
        bail!(
            "{} holds {} certificates; a realm cert holds exactly two, the \
             endpoint's and the realm CA's",
            source.display(),
            certs.len()
        );
    }
    if keys.len() > 1 {
        bail!("{} holds more than one private key", source.display());
    }

    // Written leaf first, but the two are told apart by which one signed
    // itself, so a hand-assembled file in either order still loads.
    let ca_first = is_self_signed(&certs[0])?;
    if ca_first == is_self_signed(&certs[1])? {
        bail!(
            "{} holds two {} certificates; a realm cert pairs one endpoint \
             certificate with the realm CA that signed it",
            source.display(),
            if ca_first { "self-signed" } else { "signed" }
        );
    }
    let (ca, cert) = if ca_first {
        (certs[0].clone(), certs[1].clone())
    } else {
        (certs[1].clone(), certs[0].clone())
    };

    let mut endpoint = super::RealmCert {
        cert_type: super::RealmCertType::Endpoint,
        ca,
        cert,
        key: keys.pop(),
        ..Default::default()
    };
    endpoint
        .validate()
        .with_context(|| format!("{} is not an endpoint realm certificate", source.display()))?;
    endpoint.name = endpoint.url()?.realm;

    Ok(endpoint)
}

/// Whether a certificate issued itself, which is what makes it the CA of the
/// pair.
fn is_self_signed(der: &[u8]) -> Result<bool> {
    use x509_parser::prelude::FromDer;

    let (_, cert) =
        x509_parser::prelude::X509Certificate::from_der(der).context("Parsing certificate")?;
    Ok(cert.issuer() == cert.subject())
}
