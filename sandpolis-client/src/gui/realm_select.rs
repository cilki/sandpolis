//! Realm selection dialog, shown before the login dialog when the client holds
//! no realm certificate.
//!
//! Without an endpoint certificate the client has no server it could ever
//! connect to, so this dialog is the first thing a fresh install sees. The
//! user picks a `.realm.pem` file with the platform's native file picker (an
//! `rfd` dialog on desktop, the Storage Access Framework document picker on
//! Android), the certificate is installed into the running [`RealmManager`]
//! and persisted for future starts, and a connection to the server it names is
//! established — at which point the ordinary login flow takes over.

use crate::gui::ui::panel::modal_scrim;
use crate::gui::ui::theme::{Role, Theme, ThemedBg, ThemedBorder};
use crate::gui::ui::widgets::{button, heading, muted, text};
use bevy::input_focus::InputFocus;
use bevy::prelude::*;
use bevy_ui_widgets::Activate;
use sandpolis_instance::realm::RealmCert;
use sandpolis_instance::realm::config::REALM_CERT_SUFFIX;
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, warn};

/// What came back from the platform's file picker.
pub enum PickedRealmFile {
    Picked { name: String, contents: Vec<u8> },
    Cancelled,
    Error(String),
}

#[derive(Resource, Default)]
pub struct RealmSelectState {
    pub show: bool,
    /// A pick or connection attempt is in flight.
    pub loading: bool,
    pub error_message: Option<String>,
    /// An imported certificate whose server hasn't answered yet; kept so a
    /// failed connection can be retried without picking the file again.
    pub pending_cert: Option<RealmCert>,
}

/// In-flight async work for the realm selection dialog.
#[derive(Resource, Default)]
pub struct RealmSelectOperation {
    #[cfg(not(target_os = "android"))]
    pub pick_task: Option<bevy::tasks::Task<PickedRealmFile>>,
    /// The SAF picker is out; its result arrives through the JNI callback.
    #[cfg(target_os = "android")]
    pub pick_pending: bool,
    pub connect_task: Option<bevy::tasks::Task<Result<sandpolis_server::ServerConnection, String>>>,
}

impl RealmSelectOperation {
    fn pick_outstanding(&self) -> bool {
        #[cfg(not(target_os = "android"))]
        return self.pick_task.is_some();
        #[cfg(target_os = "android")]
        return self.pick_pending;
    }
}

/// Where an imported realm cert is persisted so the next start finds it
/// without `--realm`. `None` only when the platform offers nowhere durable.
#[derive(Resource, Default)]
pub struct RealmCertDir(pub Option<PathBuf>);

/// Modal root; tracks which phase the form was built for so it can be rebuilt
/// when the flow advances (mirrors [`super::login::LoginRoot`]).
#[derive(Component)]
pub struct RealmSelectRoot {
    pub phase: u8,
}

#[derive(Component)]
pub struct RealmSelectErrorText;

fn phase_id(state: &RealmSelectState) -> u8 {
    if state.pending_cert.is_some() { 1 } else { 0 }
}

/// Open the dialog on startup when no endpoint certificate is loaded. With a
/// certificate — `--realm`, a cert in the data dir, or one imported on a
/// previous start — this never fires and the login flow proceeds as always.
pub fn prompt_realm_select_on_start(
    mut state: ResMut<RealmSelectState>,
    server_manager: Res<sandpolis_server::ServerManager>,
    mut prompted: Local<bool>,
) {
    if *prompted {
        return;
    }
    *prompted = true;

    if !server_manager.realms.has_endpoint_certs() {
        debug!("No realm certificate loaded; prompting for one");
        state.show = true;
    }
}

/// Spawn/despawn the realm selection modal, rebuilding it when the phase
/// changes.
pub fn manage_realm_select(
    mut commands: Commands,
    theme: Res<Theme>,
    state: Res<RealmSelectState>,
    root: Query<(Entity, &RealmSelectRoot)>,
    mut focus: ResMut<InputFocus>,
) {
    let existing = root.iter().next();
    if !state.show {
        if let Some((entity, _)) = existing {
            commands.entity(entity).despawn();
            focus.clear();
        }
        return;
    }

    let phase = phase_id(&state);
    let needs_rebuild = match existing {
        Some((_, root)) => root.phase != phase,
        None => true,
    };
    if needs_rebuild {
        if let Some((entity, _)) = existing {
            commands.entity(entity).despawn();
        }
        spawn_realm_select_modal(&mut commands, &theme, &state, phase);
    }
}

fn spawn_realm_select_modal(
    commands: &mut Commands,
    theme: &Theme,
    state: &RealmSelectState,
    phase: u8,
) {
    commands
        .spawn((RealmSelectRoot { phase }, modal_scrim()))
        .with_children(|scrim| {
            scrim
                .spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        width: Val::Px(360.0),
                        padding: UiRect::all(Val::Px(16.0)),
                        row_gap: Val::Px(6.0),
                        border: UiRect::all(Val::Px(1.0)),
                        ..default()
                    },
                    BackgroundColor(theme.color(Role::Panel)),
                    ThemedBg(Role::Panel),
                    BorderColor::all(theme.color(Role::Border)),
                    ThemedBorder(Role::Border),
                ))
                .with_children(|p| {
                    p.spawn(heading(theme, "Select Realm"));
                    match &state.pending_cert {
                        None => {
                            p.spawn(muted(
                                theme,
                                "Choose the realm certificate (.realm.pem) that names \
                                 your server",
                                theme.metrics.font_sm,
                            ));
                        }
                        Some(cert) => {
                            p.spawn(text(
                                theme,
                                format!("Realm: {}", cert.name),
                                theme.metrics.font_md,
                                Role::Text,
                            ));
                            if let Ok(url) = cert.url() {
                                p.spawn(muted(
                                    theme,
                                    format!("Server: {url}"),
                                    theme.metrics.font_sm,
                                ));
                            }
                        }
                    }
                    p.spawn((
                        RealmSelectErrorText,
                        text(theme, String::new(), theme.metrics.font_sm, Role::Error),
                    ));
                    p.spawn(Node {
                        column_gap: Val::Px(8.0),
                        ..default()
                    })
                    .with_children(|row| {
                        let primary = if phase == 1 {
                            "Retry"
                        } else {
                            "Choose File…"
                        };
                        row.spawn(button(theme, primary)).observe(on_realm_primary);
                        // A mobile client is useless without a realm, so there
                        // the dialog can't be dismissed; a failed connection
                        // backs up to the picker phase instead.
                        if cfg!(any(target_os = "android", target_os = "ios")) {
                            if phase == 1 {
                                row.spawn(button(theme, "Back")).observe(on_realm_back);
                            }
                        } else {
                            row.spawn(button(theme, "Cancel")).observe(on_realm_cancel);
                        }
                    });
                });
        });
}

/// Mirror the error message into the form's error label.
pub fn update_realm_select_error(
    state: Res<RealmSelectState>,
    mut label: Query<&mut Text, With<RealmSelectErrorText>>,
) {
    if let Ok(mut text) = label.single_mut() {
        let message = state.error_message.clone().unwrap_or_default();
        if text.0 != message {
            text.0 = message;
        }
    }
}

fn on_realm_primary(_activate: On<Activate>, mut state: ResMut<RealmSelectState>) {
    if state.loading {
        return;
    }
    // With a cert already imported the button retries the connection; otherwise
    // it opens the file picker.
    state.loading = true;
    state.error_message = None;
}

/// Drop the imported-but-unreachable cert and return to the picker phase,
/// keeping the dialog up (mobile's replacement for Cancel).
fn on_realm_back(_activate: On<Activate>, mut state: ResMut<RealmSelectState>) {
    state.loading = false;
    state.error_message = None;
    state.pending_cert = None;
}

fn on_realm_cancel(_activate: On<Activate>, mut state: ResMut<RealmSelectState>) {
    // Dismissing leaves the client certless (a browsable local view), exactly
    // as if it had started that way and the prompt never fired.
    state.show = false;
    state.loading = false;
    state.error_message = None;
    state.pending_cert = None;
}

/// Drive the file pick: launch the platform picker when the user asks for one
/// and, once a file arrives, validate it, install it into the running
/// [`RealmManager`], and persist it for future starts.
pub fn drive_realm_pick(
    mut state: ResMut<RealmSelectState>,
    mut operation: ResMut<RealmSelectOperation>,
    server_manager: Res<sandpolis_server::ServerManager>,
    cert_dir: Res<RealmCertDir>,
) {
    if state.show && state.loading && state.pending_cert.is_none() && !operation.pick_outstanding()
    {
        start_pick(&mut operation);
    }

    let Some(picked) = poll_pick(&mut operation) else {
        return;
    };

    match picked {
        PickedRealmFile::Picked { name, contents } => {
            let cert = match import_cert(&name, contents) {
                Ok(cert) => cert,
                Err(e) => {
                    error!(error = %e, "Realm certificate rejected");
                    state.error_message = Some(format!("{e:#}"));
                    state.loading = false;
                    return;
                }
            };

            if let Err(e) = server_manager.realms.add_endpoint_cert(cert.clone()) {
                error!(error = %e, "Failed to install realm certificate");
                state.error_message = Some(format!("{e:#}"));
                state.loading = false;
                return;
            }

            persist_cert(&cert, &cert_dir);

            info!(realm = %cert.name, "Imported realm certificate");
            state.pending_cert = Some(cert);
            state.error_message = None;
            // Still loading: drive_realm_connect takes over from here.
        }
        PickedRealmFile::Cancelled => {
            state.loading = false;
        }
        PickedRealmFile::Error(e) => {
            error!(error = %e, "File picker failed");
            state.error_message = Some(e);
            state.loading = false;
        }
    }
}

/// Validate picked file contents as a realm cert.
fn import_cert(name: &str, contents: Vec<u8>) -> anyhow::Result<RealmCert> {
    let text =
        String::from_utf8(contents).map_err(|_| anyhow::anyhow!("{name} is not a PEM file"))?;
    let cert = sandpolis_instance::realm::config::from_pem(&text, Path::new(name))?;
    if cert.key.is_none() {
        anyhow::bail!("{name} holds no private key, so it cannot authenticate this client");
    }
    Ok(cert)
}

/// Write the cert into the data directory, where the startup scan finds it the
/// next time `--realm` isn't passed. Failure costs only future convenience, so
/// it never blocks the import.
fn persist_cert(cert: &RealmCert, cert_dir: &RealmCertDir) {
    let Some(dir) = cert_dir.0.as_ref() else {
        warn!("No data directory; the realm certificate will not survive a restart");
        return;
    };

    let path = dir.join(format!("{}{}", cert.name, REALM_CERT_SUFFIX));
    let result = std::fs::create_dir_all(dir)
        .map_err(anyhow::Error::from)
        .and_then(|()| cert.write_pem(&path));
    match result {
        Ok(()) => info!(path = %path.display(), "Saved realm certificate"),
        Err(e) => warn!(error = %e, path = %path.display(), "Failed to save realm certificate"),
    }
}

/// Connect to the server the imported cert names and retain the connection,
/// after which `prompt_login_on_connect` opens the login dialog if the realm
/// has user accounts — the same state a `--realm` start reaches.
pub fn drive_realm_connect(
    mut state: ResMut<RealmSelectState>,
    mut operation: ResMut<RealmSelectOperation>,
    server_manager: Res<sandpolis_server::ServerManager>,
) {
    if state.show
        && state.loading
        && operation.connect_task.is_none()
        && let Some(cert) = state.pending_cert.clone()
    {
        let url = match cert.url() {
            Ok(url) => url,
            Err(e) => {
                state.error_message = Some(format!("Invalid server address: {e:#}"));
                state.loading = false;
                return;
            }
        };

        // Surface the server in the saved server list, deduplicating so it
        // isn't re-added on retry.
        let already_saved = server_manager
            .servers
            .iter()
            .any(|s| s.read().address == url);
        if !already_saved {
            use sandpolis_instance::database::{DataCreation, DataIdentifier, DataRevision};
            if let Err(e) = server_manager.save_server(sandpolis_server::client::SavedServerData {
                address: url.clone(),
                token: sandpolis_server::user::ClientAuthToken(String::new()),
                user: sandpolis_server::user::UserName::default(),
                _id: DataIdentifier::default(),
                _revision: DataRevision::Latest(0),
                _creation: DataCreation::default(),
            }) {
                debug!(error = %e, "Failed to save local server entry");
            }
        }

        debug!(address = %url, "Connecting to imported realm's server");
        let server_manager = server_manager.clone();
        operation.connect_task = Some(bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
            server_manager
                .connect(url)
                .await
                .map_err(|e| format!("Connection failed: {e}"))
        }));
    }

    if let Some(mut task) = operation.connect_task.take() {
        if let Some(result) = bevy::tasks::block_on(bevy::tasks::poll_once(&mut task)) {
            match result {
                Ok(connection) => {
                    info!(url = %connection.url, "Connected to imported realm's server");

                    // A token from an earlier login lets this connection skip
                    // the login dialog entirely.
                    if let Some(token) = server_manager.saved_token(&connection.url) {
                        *connection.token.write().unwrap() = Some(token);
                    }
                    server_manager
                        .outbound
                        .write()
                        .unwrap()
                        .push(std::sync::Arc::new(connection));

                    state.show = false;
                    state.loading = false;
                    state.error_message = None;
                    state.pending_cert = None;
                }
                Err(e) => {
                    error!(error = %e, "Connection to imported realm's server failed");
                    state.error_message = Some(e);
                    state.loading = false;
                }
            }
        } else {
            operation.connect_task = Some(task);
        }
    }
}

#[cfg(not(target_os = "android"))]
fn start_pick(operation: &mut RealmSelectOperation) {
    operation.pick_task = Some(bevy::tasks::AsyncComputeTaskPool::get().spawn(async {
        match rfd::AsyncFileDialog::new()
            .set_title("Select realm certificate")
            .add_filter("Realm certificate", &["pem"])
            .pick_file()
            .await
        {
            Some(handle) => PickedRealmFile::Picked {
                name: handle.file_name(),
                contents: handle.read().await,
            },
            None => PickedRealmFile::Cancelled,
        }
    }));
}

#[cfg(not(target_os = "android"))]
fn poll_pick(operation: &mut RealmSelectOperation) -> Option<PickedRealmFile> {
    let mut task = operation.pick_task.take()?;
    match bevy::tasks::block_on(bevy::tasks::poll_once(&mut task)) {
        Some(picked) => Some(picked),
        None => {
            operation.pick_task = Some(task);
            None
        }
    }
}

/// The SAF picker's result, dropped off by [`nativeOnRealmCertPicked`] on the
/// Android UI thread and collected by [`drive_realm_pick`] on the next frame.
#[cfg(target_os = "android")]
static PICK_RESULT: std::sync::Mutex<Option<PickedRealmFile>> = std::sync::Mutex::new(None);

#[cfg(target_os = "android")]
fn start_pick(operation: &mut RealmSelectOperation) {
    operation.pick_pending = true;
    if let Err(e) = launch_picker() {
        *PICK_RESULT.lock().unwrap() = Some(PickedRealmFile::Error(format!(
            "Failed to open file picker: {e}"
        )));
    }
}

#[cfg(target_os = "android")]
fn poll_pick(operation: &mut RealmSelectOperation) -> Option<PickedRealmFile> {
    if !operation.pick_pending {
        return None;
    }
    let picked = PICK_RESULT.lock().unwrap().take()?;
    operation.pick_pending = false;
    Some(picked)
}

/// Ask `MainActivity` to launch the Storage Access Framework document picker.
#[cfg(target_os = "android")]
fn launch_picker() -> anyhow::Result<()> {
    use jni::JavaVM;
    use ndk_context::android_context;

    // ndk-context holds the `Application` (android-activity registers
    // `activity.getApplication()`), which doesn't have the picker method; the
    // Activity instance itself is in the `AndroidApp` that `#[bevy_main]` saved.
    let app = bevy::android::ANDROID_APP
        .get()
        .ok_or_else(|| anyhow::anyhow!("AndroidApp not initialized"))?;

    let ctx = android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };

    vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let activity =
            unsafe { jni::objects::JObject::from_raw(env, app.activity_as_ptr().cast()) };
        env.call_method(
            activity,
            jni::jni_str!("openRealmCertPicker"),
            jni::jni_sig!(() -> void),
            &[],
        )?;
        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("{e:?}"))
}

/// Called by `MainActivity.onActivityResult` with the picked document's display
/// name and contents, or nulls when the user backed out of the picker.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_sandpolis_mobile_MainActivity_nativeOnRealmCertPicked<'local>(
    mut unowned_env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    name: jni::objects::JString<'local>,
    contents: jni::objects::JByteArray<'local>,
) {
    use jni::refs::Reference;

    let outcome = unowned_env.with_env(|env| -> jni::errors::Result<()> {
        let picked = if contents.is_null() {
            PickedRealmFile::Cancelled
        } else {
            let name = if name.is_null() {
                String::from("selected file")
            } else {
                env.get_string(&name)
                    .map(String::from)
                    .unwrap_or_else(|_| String::from("selected file"))
            };
            match env.convert_byte_array(&contents) {
                Ok(contents) => PickedRealmFile::Picked { name, contents },
                Err(e) => PickedRealmFile::Error(format!("Failed to read picked file: {e:?}")),
            }
        };

        *PICK_RESULT.lock().unwrap() = Some(picked);
        Ok(())
    });

    outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}
