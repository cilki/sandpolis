use super::RealmCert;
use super::RealmCertType;
use super::RealmName;
use super::RealmManager;
use super::url::ServerUrl;
use crate::ClusterId;
use crate::ServerId;
use crate::InstanceManager;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use axum::{
    Extension,
    extract::{ConnectInfo, Request, State},
    middleware::AddExtension,
    middleware::Next,
    response::Response,
};
use axum_server::tls_rustls::RustlsConfig;
use axum_server::{accept::Accept, tls_rustls::RustlsAcceptor};
use futures_util::future::BoxFuture;
use headers::{Header, HeaderName, HeaderValue};
use rcgen::BasicConstraints;
use rcgen::CertificateParams;
use rcgen::DnType;
use rcgen::ExtendedKeyUsagePurpose;
use rcgen::IsCa;
use rcgen::Issuer;
use rcgen::KeyPair;
use rcgen::SanType;
use rustls::RootCertStore;
use rustls::ServerConfig;
use rustls::server::ResolvesServerCertUsingSni;
use rustls::server::WebPkiClientVerifier;
use rustls_pki_types::CertificateDer;
use std::io;
use std::sync::Arc;
use time::Duration;
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::server::TlsStream;
use tower::Layer;
use tracing::debug;
use tracing::trace;
use tracing::warn;
use x509_parser::prelude::{FromDer, X509Certificate};

impl RealmCert {
    /// Generate a new realm CA certificate.
    pub fn new_cluster(cluster_id: ClusterId, name: RealmName) -> Result<Self> {
        // Generate key
        let keypair = KeyPair::generate()?;

        // Generate certificate
        let mut cert_params = CertificateParams::default();
        cert_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        cert_params.not_before = OffsetDateTime::now_utc();
        cert_params.not_after = OffsetDateTime::now_utc().saturating_add(Duration::days(36780));
        cert_params.subject_alt_names = vec![SanType::DnsName(cluster_id.to_string().try_into()?)];

        // Without a subject the certificates this CA signs have the same
        // (empty) subject and issuer, which reads as self-signed.
        cert_params
            .distinguished_name
            .push(DnType::CommonName, format!("{cluster_id} {name}"));

        // Generate the certificate
        let cert = cert_params.self_signed(&keypair)?;

        debug!(cert = ?cert_params, "Generated new realm CA certificate");
        Ok(Self {
            cert_type: RealmCertType::Cluster,
            name,
            cert: cert.der().to_vec(),
            key: Some(keypair.serialize_der()),
            ..Default::default()
        })
    }

    /// This CA, ready to sign. Only a CA that still holds its private key can
    /// issue, which is what keeps a local stratum server from doing so.
    pub fn issuer(&self) -> Result<Issuer<'static, KeyPair>> {
        if self.cert_type != RealmCertType::Cluster {
            bail!("Only a realm CA can issue certificates");
        }

        Ok(Issuer::from_ca_cert_der(
            &self.cert.clone().try_into()?,
            KeyPair::try_from(self.key.clone().ok_or_else(|| anyhow!("No key"))?)?,
        )?)
    }

    /// Generate a new realm certificate for an endpoint: a client or an agent.
    ///
    /// `url` names the server the holder will connect to; it becomes the
    /// certificate's common name, so one certificate authenticates against
    /// exactly one server and realm.
    pub fn endpoint_cert(&self, url: &ServerUrl) -> Result<RealmCert> {
        let issuer = self.issuer()?;

        // Generate key
        let keypair = KeyPair::generate()?;

        // Generate certificate
        let mut cert_params = CertificateParams::default();
        cert_params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::ClientAuth);
        cert_params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::Other(
                RealmCertType::Endpoint.oid().unwrap().to_vec(),
            ));
        cert_params.not_before = OffsetDateTime::now_utc();
        cert_params.not_after = OffsetDateTime::now_utc().saturating_add(Duration::days(365));
        cert_params
            .distinguished_name
            .push(DnType::CommonName, url.canonical());

        // Generate the certificate signed by the CA
        let cert = cert_params.signed_by(&keypair, &issuer)?;

        debug!(cert = ?cert_params, "Generated new realm endpoint certificate");
        Ok(RealmCert {
            cert_type: RealmCertType::Endpoint,
            name: url.realm.clone(),
            ca: self.cert.clone(),
            cert: cert.der().to_vec(),
            key: Some(keypair.serialize_der()),
            ..Default::default()
        })
    }

    /// Generate a new realm certificate for server instances.
    pub fn server_cert(&self, server_id: ServerId) -> Result<RealmCert> {
        let issuer = self.issuer()?;

        // Generate key
        let keypair = KeyPair::generate()?;

        // Generate certificate
        let mut cert_params = CertificateParams::default();
        cert_params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::ServerAuth);

        // TODO add server id?

        // Can also do client auth when connecting to other servers
        cert_params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::ClientAuth);
        cert_params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::Other(
                RealmCertType::Server.oid().unwrap().to_vec(),
            ));
        cert_params.not_before = OffsetDateTime::now_utc();
        cert_params.not_after = OffsetDateTime::now_utc().saturating_add(Duration::days(365));
        cert_params.subject_alt_names = vec![SanType::DnsName(
            format!("{}.{}", self.cluster_id()?, self.name).try_into()?,
        )];

        // Generate the certificate signed by the CA
        let cert = cert_params.signed_by(&keypair, &issuer)?;

        debug!(cert = ?cert_params, "Generated new realm server certificate");
        Ok(RealmCert {
            cert_type: RealmCertType::Server,
            name: self.name.clone(),
            ca: self.cert.clone(),
            cert: cert.der().to_vec(),
            key: Some(keypair.serialize_der()),
            _instance_id: Some(server_id.into()),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod test_realm_ca {
    use super::*;
    use io::Write;
    use openssl::{
        pkey::PKey,
        ssl::{SslAcceptor, SslConnector, SslMethod, SslVerifyMode},
        x509::X509,
    };
    use pem::{Pem, encode};
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn test_generate_and_authenticate() -> Result<()> {
        let ca = RealmCert::new_cluster(ClusterId::default(), "default".parse()?)?;
        let client = ca.endpoint_cert(&"127.0.0.1:9999/default".parse()?)?;
        let server = ca.server_cert(ServerId::random())?;

        // Write CA cert to temp file
        let mut ca_file = tempfile::NamedTempFile::new()?;
        ca_file.write_all(encode(&Pem::new("CERTIFICATE", ca.cert)).as_bytes())?;

        let mut server_context = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())?;
        server_context.set_verify(SslVerifyMode::PEER);
        server_context.set_ca_file(&ca_file)?;
        server_context.set_certificate(&&X509::from_der(&server.cert)?)?;
        server_context
            .set_private_key(&&PKey::private_key_from_der(server.key.as_ref().unwrap())?)?;
        let server_context = server_context.build();

        let mut client_context = SslConnector::builder(SslMethod::tls_client())?;
        client_context.set_verify(SslVerifyMode::PEER);
        client_context.set_ca_file(&ca_file)?;
        client_context.set_certificate(&&X509::from_der(&client.cert)?)?;
        client_context
            .set_private_key(&&PKey::private_key_from_der(client.key.as_ref().unwrap())?)?;
        let client_context = client_context.build();

        // Start temporary server and listen for one connection
        let server_handle = std::thread::spawn(move || -> Result<()> {
            let listener = TcpListener::bind("127.0.0.1:9999")?;
            for stream in listener.incoming() {
                let mut ssl = server_context.accept(stream?)?;
                ssl.do_handshake()?;
                break;
            }
            Ok(())
        });

        // Give server time to start
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Make connection
        let stream = TcpStream::connect("127.0.0.1:9999")?;
        let mut ssl = client_context.connect(&server.subject_name()?, stream)?;
        ssl.do_handshake()?;

        // Wait for server thread to complete
        server_handle.join().unwrap()?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct TlsData {
    peer_certificates: Option<Vec<CertificateDer<'static>>>,
}

/// Accepts TLS connections with realm certificates.
#[derive(Debug, Clone)]
pub struct RealmAcceptor {
    acceptor: RustlsAcceptor,
    /// Whether a rejected handshake raises a user-facing notification, from the
    /// realm configs' `server.notify_cert_failures`.
    notify_cert_failures: bool,
}

impl RealmAcceptor {
    pub async fn new(
        instance_manager: InstanceManager,
        realms: RealmManager,
        notify_cert_failures: bool,
    ) -> Result<Self> {
        let mut roots = RootCertStore::empty();
        let mut sni_resolver = ResolvesServerCertUsingSni::new();

        let config = ServerConfig::builder();

        for realm in realms.iter() {
            let db = &realm.database;
            trace!(name = *realm.name, "Registering realm with server acceptor");

            // These certificates are written once at startup and never change,
            // so read them directly rather than holding residents on them.
            let r = db.r_transaction()?;
            let certs: Vec<RealmCert> =
                r.scan().primary()?.all()?.collect::<Result<Vec<_>, _>>()?;
            drop(r);

            // Add cluster cert as a CA cert to the root store
            let cluster_cert = certs
                .iter()
                .find(|cert| cert.cert_type == RealmCertType::Cluster)
                .ok_or_else(|| anyhow!("No CA certificate for realm: {}", realm.name))?;
            roots.add(cluster_cert.cert.clone().try_into()?)?;

            // Add server cert to the SNI resolver
            let server_cert = certs
                .iter()
                .find(|cert| {
                    cert.cert_type == RealmCertType::Server
                        && cert._instance_id == Some(instance_manager.instance_id)
                })
                .ok_or_else(|| anyhow!("No server certificate for realm: {}", realm.name))?;

            let private_key = config.crypto_provider().key_provider.load_private_key(
                server_cert
                    .key
                    .clone()
                    .ok_or_else(|| anyhow!("No server key"))?
                    .try_into()
                    .map_err(|_| anyhow!("Failed to parse key"))?,
            )?;

            let subject_name = server_cert.subject_name()?;

            trace!(subject_name = %subject_name, "Adding SNI resolver");
            sni_resolver.add(
                &subject_name,
                rustls::sign::CertifiedKey::new(
                    vec![server_cert.cert.clone().try_into()?],
                    private_key,
                ),
            )?;
        }

        Ok(Self {
            acceptor: RustlsAcceptor::new(RustlsConfig::from_config(Arc::new(
                config
                    .with_client_cert_verifier(
                        WebPkiClientVerifier::builder(Arc::new(roots)).build()?,
                    )
                    .with_cert_resolver(Arc::new(sni_resolver)),
            ))),
            notify_cert_failures,
        })
    }
}

/// Whether a failed TLS accept was the peer failing *certificate*
/// authentication, as opposed to unrelated handshake noise (port scanners,
/// protocol mismatches) that shouldn't reach the user.
fn is_cert_auth_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        .is_some_and(|error| {
            matches!(
                error,
                rustls::Error::NoCertificatesPresented | rustls::Error::InvalidCertificate(_)
            )
        })
}
impl<I, S> Accept<I, S> for RealmAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = TlsStream<I>;
    type Service = AddExtension<S, TlsData>;
    type Future = BoxFuture<'static, io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let acceptor = self.acceptor.clone();
        let notify_cert_failures = self.notify_cert_failures;

        Box::pin(async move {
            let (stream, service) = match acceptor.accept(stream, service).await {
                Ok(result) => result,
                Err(e) => {
                    debug!("TLS accept failed: {}", e);
                    if notify_cert_failures && is_cert_auth_error(&e) {
                        crate::notification::notify(
                            crate::notification::Notification::warn(
                                "Network",
                                "Connection rejected: certificate authentication failed",
                            )
                            .body(e.to_string()),
                        );
                    }
                    return Err(e);
                }
            };
            let server_conn = stream.get_ref().1;
            let tls_data = TlsData {
                peer_certificates: server_conn.peer_certificates().map(From::from),
            };
            let service = Extension(tls_data).layer(service);

            // TODO check for revoked certificates

            Ok((stream, service))
        })
    }
}

/// The realm a request is authenticated for.
///
/// [`auth_middleware`] derives this from the certificate the connection
/// presented and inserts it as a request extension. Handlers must take the
/// realm from here and never from the request itself: a realm certificate
/// authenticates its holder against exactly one realm, but one server listens
/// for every realm it serves behind a single client-certificate verifier, so a
/// realm the *peer* names is a realm the peer chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedRealm(pub RealmName);

impl<S> axum::extract::FromRequestParts<S> for AuthenticatedRealm
where
    S: Send + Sync,
{
    type Rejection = (axum::http::StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthenticatedRealm>()
            .cloned()
            // Only reachable if a route escaped `auth_middleware`, which would
            // be an unauthenticated request rather than a bad one.
            .ok_or((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "request was not authenticated",
            ))
    }
}

/// Work out which realm a request is authenticated for.
///
/// The realm comes from the common name of the certificate the connection
/// presented, which is the server URL it was minted for and therefore names
/// exactly one realm.
///
/// A peer may also state a realm in the `x-realm` header, and a legitimate one
/// always states the realm of the very certificate it is connecting with (both
/// come from the same realm cert). Stating a different one is refused rather
/// than ignored, so a peer can never operate on a realm its certificate was not
/// issued by.
fn authenticated_realm(
    tls_data: &TlsData,
    headers: &axum::http::HeaderMap,
) -> Result<RealmName, &'static str> {
    let peer_certificates = tls_data
        .peer_certificates
        .as_ref()
        .ok_or("missing client certificate")?;

    // Take first client certificate
    let cert = X509Certificate::from_der(
        peer_certificates
            .first()
            .ok_or("missing client certificate")?,
    )
    .map_err(|_| "invalid client certificate")?
    .1;

    // Take first common name from certificate
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .ok_or("missing common name in client certificate")?
        .as_str()
        .map_err(|_| "invalid common name in client certificate")?;

    // The common name is the server URL the certificate was minted for, so
    // the realm comes from its path component.
    let url = cn
        .parse::<ServerUrl>()
        .map_err(|_| "invalid common name in client certificate")?;

    if let Some(stated) = headers.get(RealmName::name()) {
        let stated = stated
            .to_str()
            .ok()
            .and_then(|stated| stated.parse::<RealmName>().ok())
            .ok_or("invalid realm header")?;

        if stated != url.realm {
            return Err("realm header does not match the client certificate");
        }
    }

    Ok(url.realm)
}

pub async fn auth_middleware(
    State(notify_cert_failures): State<bool>,
    Extension(tls_data): Extension<TlsData>,
    mut request: Request,
    next: Next,
) -> Result<Response, &'static str> {
    let realm = authenticated_realm(&tls_data, request.headers()).inspect_err(|reason| {
        let peer = request
            .extensions()
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|ConnectInfo(peer)| peer.ip());

        // Canonical line matched by the shipped fail2ban filter
        if let Some(peer) = peer {
            warn!(peer = %peer, reason = %reason, "Authentication failure");
        }

        if notify_cert_failures {
            let mut notification = crate::notification::Notification::warn(
                "Network",
                "Connection rejected: invalid client certificate",
            );
            if let Some(peer) = peer {
                notification = notification.body(format!("{reason} (peer {peer})"));
            } else {
                notification = notification.body(*reason);
            }
            crate::notification::notify(notification);
        }
    })?;

    // Pass authentication to routes
    request.extensions_mut().insert(AuthenticatedRealm(realm));

    Ok(next.run(request).await)
}

#[cfg(test)]
mod test_auth_middleware {
    use super::*;
    use axum::http::HeaderMap;

    /// A realm cert as a peer presents it: the endpoint certificate `realm` at
    /// `url` would hand to a server.
    fn endpoint(realm: &str) -> Result<TlsData> {
        let url: ServerUrl = format!("127.0.0.1:8768/{realm}").parse()?;
        let ca = RealmCert::new_cluster(ClusterId::default(), realm.parse()?)?;
        Ok(TlsData {
            peer_certificates: Some(vec![ca.endpoint_cert(&url)?.cert.try_into()?]),
        })
    }

    fn header(realm: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(RealmName::name(), realm.parse().unwrap());
        headers
    }

    /// The realm is the one the certificate was issued for, whether or not the
    /// peer bothers to state it.
    #[test]
    fn realm_comes_from_the_certificate() -> Result<()> {
        let tls = endpoint("alpha")?;
        let expected: RealmName = "alpha".parse()?;

        assert_eq!(
            authenticated_realm(&tls, &HeaderMap::new()).unwrap(),
            expected
        );
        assert_eq!(
            authenticated_realm(&tls, &header("alpha")).unwrap(),
            expected
        );
        Ok(())
    }

    /// Naming a realm other than the certificate's is refused: one server
    /// listens for every realm it serves behind a single client-certificate
    /// verifier, so a certificate issued by one realm's CA is accepted on a
    /// connection to any of them. Were the stated realm taken at face value, it
    /// would be the whole of what decides which realm's database, users and
    /// certificate authority the request reaches.
    #[test]
    fn stated_realm_cannot_cross_realms() -> Result<()> {
        let tls = endpoint("alpha")?;

        assert_eq!(
            authenticated_realm(&tls, &header("default")),
            Err("realm header does not match the client certificate")
        );
        Ok(())
    }

    /// No certificate, no realm — the handshake is mutually authenticated, so
    /// this means a route escaped the acceptor rather than a peer opting out.
    #[test]
    fn missing_certificate_is_refused() {
        let tls = TlsData {
            peer_certificates: None,
        };
        assert_eq!(
            authenticated_realm(&tls, &HeaderMap::new()),
            Err("missing client certificate")
        );

        let tls = TlsData {
            peer_certificates: Some(Vec::new()),
        };
        assert_eq!(
            authenticated_realm(&tls, &HeaderMap::new()),
            Err("missing client certificate")
        );
    }

    /// A server certificate names its realm by SAN and leaves the common name
    /// at rcgen's placeholder, so it can't stand in for the realm cert that
    /// authenticates a dialer — least of all with a realm header to make up the
    /// difference.
    #[test]
    fn server_certificate_has_no_realm() -> Result<()> {
        let ca = RealmCert::new_cluster(ClusterId::default(), "alpha".parse()?)?;
        let tls = TlsData {
            peer_certificates: Some(vec![ca.server_cert(ServerId::random())?.cert.try_into()?]),
        };

        assert_eq!(
            authenticated_realm(&tls, &HeaderMap::new()),
            Err("invalid common name in client certificate")
        );
        assert_eq!(
            authenticated_realm(&tls, &header("alpha")),
            Err("invalid common name in client certificate")
        );
        Ok(())
    }
}

impl Header for RealmName {
    fn name() -> &'static HeaderName {
        static NAME: HeaderName = HeaderName::from_static("x-realm");
        &NAME
    }

    fn decode<'i, I>(values: &mut I) -> Result<Self, headers::Error>
    where
        I: Iterator<Item = &'i HeaderValue>,
    {
        values
            .next()
            .ok_or_else(headers::Error::invalid)?
            .to_str()
            .map_err(|_| headers::Error::invalid())?
            .parse()
            .map_err(|_| headers::Error::invalid())
    }

    fn encode<E>(&self, values: &mut E)
    where
        E: Extend<HeaderValue>,
    {
        values.extend(std::iter::once(
            HeaderValue::from_str(&self.to_string()).expect("Realm names only allow ascii 32-127"),
        ));
    }
}
