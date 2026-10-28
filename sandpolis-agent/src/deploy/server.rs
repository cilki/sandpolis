//! Server side of deployment: the half that actually talks SSH.
//!
//! The deploy runs in a task spawned by [`DeployStreamResponder::on_message`]
//! rather than inline. Responder handlers run on the connection's receive path,
//! and that same loop is what flushes outbound messages — deploying inline
//! would stall the whole connection and not one progress message would reach
//! the client until it finished.
//!
//! Every command below is a fixed string against fixed paths, so the target's
//! shell never sees anything the operator typed. The account deployment
//! authenticates as must be able to write `/opt`, `/var/lib`, and
//! `/etc/systemd/system` — in practice, root.

use super::*;
use crate::deploy::binary::AgentBinaryKey;
use crate::deploy::os::{TargetOs, parse_os_release};
use anyhow::{Context, Result, anyhow, bail};
use russh::ChannelMsg;
use russh::client::{self, AuthResult, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, decode_secret_key};
use sandpolis_instance::network::{
    RegisterResponders, ResponderRegistration, StreamRegistry, StreamResponder,
};
use sandpolis_instance::realm::url::ServerUrl;
use sandpolis_instance::realm::{RealmCert, RealmCertType, RealmManager};
use sandpolis_macros::Stream;
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// The realms this server serves, installed at startup.
///
/// Held in a static so [`DeployStreamResponder`] can be constructed by the
/// stateless `inventory` factory, the same arrangement the account subsystem's
/// management responder uses.
static REALMS: OnceLock<RealmManager> = OnceLock::new();

/// Give the deployer access to the realm CAs. Called once at startup.
pub fn install_realms(realms: RealmManager) {
    let _ = REALMS.set(realms);
}

/// Server side of the deploy stream.
#[derive(Stream, Default)]
pub struct DeployStreamResponder {
    /// Cancels the running deployment when the client goes away (on drop).
    cancel: CancellationToken,
}

impl StreamResponder for DeployStreamResponder {
    type In = DeployStreamRequest;
    type Out = DeployStreamResponse;

    async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
        let DeployStreamRequest::Start {
            target,
            server,
            dry_run,
        } = request;

        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let mut progress = Progress {
                sender,
                step: DeployStep::Connect,
            };

            let deploy = deploy(&target, &server, dry_run, &mut progress);
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!(host = %target.host, "Deployment cancelled");
                }
                result = deploy => match result {
                    Ok(Outcome::Deployed { reconfigured }) => {
                        info!(host = %target.host, reconfigured, "Deployment finished");
                        progress.finished(reconfigured).await;
                    }
                    Ok(outcome @ Outcome::Planned { .. }) => {
                        info!(host = %target.host, "Dry run finished");
                        progress.planned(outcome).await;
                    }
                    Err(e) => {
                        warn!(host = %target.host, error = %e, "Deployment failed");
                        progress.failed(e).await;
                    }
                },
            }
        });
        Ok(())
    }
}

impl Drop for DeployStreamResponder {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// How a deployment ended when it didn't fail.
enum Outcome {
    Deployed {
        reconfigured: bool,
    },
    /// A dry run's findings; nothing was changed on the target.
    Planned {
        os: TargetOs,
        arch: String,
        installed: bool,
        actions: Vec<String>,
        blocker: Option<String>,
    },
}

/// Reports step transitions back to the client, remembering which step is
/// running so a failure can name it without every call site passing it along.
struct Progress {
    sender: Sender<DeployStreamResponse>,
    step: DeployStep,
}

impl Progress {
    /// Enter `step`, describing what it's about to do.
    async fn begin(&mut self, step: DeployStep, message: impl Into<String>) {
        self.step = step;
        let _ = self
            .sender
            .send(DeployStreamResponse::Step {
                step,
                message: message.into(),
            })
            .await;
    }

    /// The current step succeeded.
    async fn done(&self) {
        let _ = self
            .sender
            .send(DeployStreamResponse::Done { step: self.step })
            .await;
    }

    async fn finished(&self, reconfigured: bool) {
        let _ = self
            .sender
            .send(DeployStreamResponse::Finished { reconfigured })
            .await;
    }

    async fn planned(&self, outcome: Outcome) {
        let Outcome::Planned {
            os,
            arch,
            installed,
            actions,
            blocker,
        } = outcome
        else {
            return;
        };
        let _ = self
            .sender
            .send(DeployStreamResponse::Planned {
                os: os.to_string(),
                arch,
                installed,
                actions,
                blocker,
            })
            .await;
    }

    async fn failed(&self, error: anyhow::Error) {
        let _ = self
            .sender
            .send(DeployStreamResponse::Failed {
                step: self.step,
                // The chain carries the context each step attaches, which is
                // usually more useful than the innermost IO error alone.
                message: format!("{error:#}"),
            })
            .await;
    }
}

/// Run a deployment start to finish. A dry run stops after the read-only
/// steps and reports what a real deployment would have done.
async fn deploy(
    target: &DeployTarget,
    server: &ServerUrl,
    dry_run: bool,
    progress: &mut Progress,
) -> Result<Outcome> {
    progress
        .begin(
            DeployStep::Connect,
            format!(
                "Connecting to {}@{}:{}",
                target.username, target.host, target.port
            ),
        )
        .await;
    let session = connect(target).await?;
    progress.done().await;

    progress
        .begin(DeployStep::Probe, "Identifying the target system")
        .await;
    let (os, arch, installed) = probe(&session).await?;
    if os == TargetOs::NixOs {
        bail!(
            "{} runs NixOS, where an agent installed into /opt would be outside \
             the system configuration that owns it. Add the agent to the host's \
             NixOS configuration instead.",
            target.host
        );
    }
    debug!(host = %target.host, os = %os, arch = %arch, installed, "Probed deploy target");
    progress.done().await;

    progress
        .begin(
            DeployStep::Certificate,
            format!("Issuing an agent certificate for realm {}", server.realm),
        )
        .await;
    let realm_cert = certificate(server)?;
    progress.done().await;

    if dry_run {
        let mut actions = vec![format!("write the realm certificate to {REALM_FILE}")];
        let mut blocker = None;
        if !installed {
            let key = AgentBinaryKey {
                os: os.clone(),
                arch: arch.clone(),
            };
            match crate::deploy::binary::check(&key) {
                Ok(()) => {
                    actions.push(format!(
                        "upload the agent binary for {key} to {INSTALL_PATH}"
                    ));
                    actions.push(format!(
                        "install {} and reload systemd",
                        systemd::UNIT_PATH
                    ));
                    actions.push(format!("enable and start {}", systemd::UNIT_NAME));
                }
                Err(e) => blocker = Some(format!("{e:#}")),
            }
        }
        return Ok(Outcome::Planned {
            os,
            arch,
            installed,
            actions,
            blocker,
        });
    }

    progress
        .begin(DeployStep::Upload, format!("Writing {REALM_FILE}"))
        .await;
    upload(&session, REALM_FILE, "600", realm_cert.into_bytes()).await?;
    progress.done().await;

    // An agent that's already installed picks up the certificate we just wrote,
    // so there is nothing left to install.
    if installed {
        return Ok(Outcome::Deployed { reconfigured: true });
    }

    progress
        .begin(DeployStep::Service, "Installing the agent and its service")
        .await;
    let key = AgentBinaryKey {
        os: os.clone(),
        arch: arch.clone(),
    };
    let binary = crate::deploy::binary::resolve(&key)?;
    upload(&session, INSTALL_PATH, "755", binary).await?;
    upload(
        &session,
        systemd::UNIT_PATH,
        "644",
        systemd::unit_file().into_bytes(),
    )
    .await?;
    run_checked(&session, "systemctl daemon-reload").await?;
    progress.done().await;

    progress
        .begin(DeployStep::Start, "Starting the agent")
        .await;
    run_checked(
        &session,
        &format!("systemctl enable --now {}", systemd::UNIT_NAME),
    )
    .await?;
    run_checked(
        &session,
        &format!("systemctl is-active --quiet {}", systemd::UNIT_NAME),
    )
    .await
    .context("the agent service did not stay running")?;
    progress.done().await;

    Ok(Outcome::Deployed {
        reconfigured: false,
    })
}

/// Mint an agent certificate from `server`'s realm CA and render the realm cert
/// the deployed agent will read.
fn certificate(server: &ServerUrl) -> Result<String> {
    let realms = REALMS
        .get()
        .ok_or_else(|| anyhow!("the deploy responder is not initialized"))?;

    let database = realms.realm(server.realm.clone())?;
    let r = database.r_transaction()?;
    let ca: RealmCert = r
        .scan()
        .primary()?
        .all()?
        .collect::<std::result::Result<Vec<RealmCert>, _>>()?
        .into_iter()
        .find(|cert| cert.cert_type == RealmCertType::Cluster)
        .ok_or_else(|| anyhow!("no realm CA for {}", server.realm))?;
    drop(r);

    // Only the global stratum server holds the CA's private key, so this is
    // also where a local stratum server finds out it can't deploy.
    if ca.key.is_none() {
        bail!(
            "this server holds the {} CA certificate but not its private key, \
             so it cannot issue agent certificates. Deploy from the global \
             stratum server.",
            server.realm
        );
    }

    Ok(sandpolis_instance::realm::config::to_pem(
        &ca.endpoint_cert(server)?,
    ))
}

/// Verifies the target's host key against the configured fingerprint.
struct DeployHandler {
    /// Expected SHA256 fingerprint, with or without the `SHA256:` prefix.
    fingerprint: Option<String>,
}

impl client::Handler for DeployHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // A host certificate carries the host key; the fingerprint is of that
        // key either way, so an operator configures the same value regardless.
        let actual = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        let Some(expected) = self.fingerprint.as_deref() else {
            // Trust on first use. Loud, because it's the one case where a man in
            // the middle goes unnoticed.
            warn!(
                fingerprint = %actual,
                "Deploy target has no configured SSH fingerprint; accepting host key unverified"
            );
            return Ok(true);
        };
        let matches = expected == actual
            || actual
                .strip_prefix("SHA256:")
                .is_some_and(|stripped| expected == stripped);
        if !matches {
            warn!(expected = %expected, actual = %actual, "SSH host key mismatch");
        }
        Ok(matches)
    }
}

/// Connect and authenticate.
async fn connect(target: &DeployTarget) -> Result<Handle<DeployHandler>> {
    let mut handle = client::connect(
        Arc::new(client::Config::default()),
        (target.host.as_str(), target.port),
        DeployHandler {
            fingerprint: target.fingerprint.clone(),
        },
    )
    .await
    .with_context(|| format!("connecting to {}:{}", target.host, target.port))?;

    let auth = match &target.auth {
        DeployAuth::Password(password) => {
            handle
                .authenticate_password(&target.username, password)
                .await?
        }
        DeployAuth::PrivateKey { pem, passphrase } => {
            // The key arrives as text rather than a path: it was read on the
            // client, which is where the operator's key actually lives.
            let key = decode_secret_key(pem, passphrase.as_deref())
                .context("reading the supplied private key")?;
            let hash_alg = handle.best_supported_rsa_hash().await?.flatten();
            handle
                .authenticate_publickey(
                    &target.username,
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
                )
                .await?
        }
    };
    if !matches!(auth, AuthResult::Success) {
        bail!(
            "authentication failed for {}@{}",
            target.username,
            target.host
        );
    }

    Ok(handle)
}

/// Identify the target's OS and architecture, and report whether an agent is
/// already installed there.
async fn probe(session: &Handle<DeployHandler>) -> Result<(TargetOs, String, bool)> {
    // A host with no /etc/os-release leaves stdout empty, which classifies as
    // unknown rather than failing the whole deployment.
    let (_, release) = run_command(session, "cat /etc/os-release 2>/dev/null").await?;
    let os = parse_os_release(&release);

    let (status, arch) = run_command(session, "uname -m").await?;
    if status != 0 {
        bail!("could not determine the target's architecture");
    }
    let arch = arch.trim().to_string();

    let (status, _) = run_command(session, &format!("test -x {INSTALL_PATH}")).await?;

    Ok((os, arch, status == 0))
}

/// Run `command` and return its exit status and stdout.
async fn run_command(session: &Handle<DeployHandler>, command: &str) -> Result<(u32, String)> {
    let mut channel = session.channel_open_session().await?;
    channel.exec(true, command).await?;

    let mut status = None;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            // Not leaving the loop here: more output can still arrive after the
            // exit status.
            _ => {}
        }
    }

    let status = status.ok_or_else(|| anyhow!("`{command}` exited without a status"))?;
    if status != 0 && !stderr.is_empty() {
        debug!(
            command,
            status,
            stderr = %String::from_utf8_lossy(&stderr),
            "Remote command failed"
        );
    }
    Ok((status, String::from_utf8_lossy(&stdout).into_owned()))
}

/// Run `command`, failing if it exits non-zero.
async fn run_checked(session: &Handle<DeployHandler>, command: &str) -> Result<()> {
    let (status, _) = run_command(session, command).await?;
    if status != 0 {
        bail!("`{command}` exited with status {status}");
    }
    Ok(())
}

/// Write `content` to `path` on the target with mode `mode`.
///
/// `install` rather than a redirect: it creates the parent directory, sets the
/// mode as it writes (so a key file is never briefly world-readable), and
/// replaces a running binary by unlinking rather than truncating it.
async fn upload(
    session: &Handle<DeployHandler>,
    path: &str,
    mode: &str,
    content: Vec<u8>,
) -> Result<()> {
    let mut channel = session.channel_open_session().await?;
    channel
        .exec(true, format!("install -D -m {mode} /dev/stdin {path}"))
        .await?;
    channel.data_bytes(content).await?;
    channel.eof().await?;

    let mut status = None;
    let mut stderr = Vec::new();
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            _ => {}
        }
    }

    match status {
        Some(0) => Ok(()),
        Some(status) => bail!(
            "writing {path} failed with status {status}: {}",
            String::from_utf8_lossy(&stderr).trim()
        ),
        None => bail!("writing {path} ended without an exit status"),
    }
}

/// Registers [`DeployStreamResponder`] on each connection.
pub struct DeployResponderRegistration;

impl RegisterResponders for DeployResponderRegistration {
    fn register_responders(&self, registry: &StreamRegistry) {
        registry.register_responder(DeployStreamResponder::default);
    }
}

inventory::submit!(ResponderRegistration(&DeployResponderRegistration));
