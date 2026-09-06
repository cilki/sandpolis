use crate::gui::input::{LoginDialogState, LoginPhase};
use crate::gui::ui::panel::modal_scrim;
use crate::gui::ui::text_input::text_input;
use crate::gui::ui::theme::{Role, Theme, ThemedBg, ThemedBorder};
use crate::gui::ui::widgets::{button, heading, muted, text};
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy_ui_widgets::Activate;
use sandpolis_server::ServerUrl;
use sandpolis_server::login::{LoginPassword, LoginRequest, LoginResponse};
use std::str::FromStr;
use tracing::{debug, error, info};

/// Resource to track ongoing login operations
#[derive(Resource, Default)]
pub struct LoginOperation {
    pub phase1_handle: Option<LoginPhase1Handle>,
    pub phase2_handle: Option<LoginPhase2Handle>,
}

pub struct LoginPhase1Handle {
    pub task: bevy::tasks::Task<Result<sandpolis_server::ServerConnection, String>>,
}

pub struct LoginPhase2Handle {
    pub task:
        bevy::tasks::Task<Result<(sandpolis_server::ServerConnection, LoginResponse), String>>,
    pub server_url: ServerUrl,
    pub username: sandpolis_server::user::UserName,
}

/// System to handle phase 1: connecting to server and fetching banner
pub fn handle_login_phase1(
    mut login_state: ResMut<LoginDialogState>,
    mut login_operation: ResMut<LoginOperation>,
    server_manager: Res<sandpolis_server::ServerManager>,
) {
    // Check if we need to start phase 1
    if matches!(login_state.phase, LoginPhase::ServerAddress)
        && login_state.loading
        && login_operation.phase1_handle.is_none()
    {
        // Parse server URL
        let server_url = match ServerUrl::from_str(&login_state.server_address) {
            Ok(url) => url,
            Err(e) => {
                login_state.error_message = Some(format!("Invalid server address: {}", e));
                login_state.loading = false;
                return;
            }
        };

        debug!(address = %server_url, "Starting phase 1: connecting to server");

        // Clone what we need for the async task
        let server_manager = server_manager.clone();

        // Spawn async task
        let task = bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
            server_manager
                .connect(server_url)
                .await
                .map_err(|e| format!("Connection failed: {}", e))
        });

        login_operation.phase1_handle = Some(LoginPhase1Handle { task });
    }

    // Check if phase 1 task is complete
    if let Some(mut handle) = login_operation.phase1_handle.take() {
        if let Some(result) = bevy::tasks::block_on(bevy::tasks::poll_once(&mut handle.task)) {
            match result {
                Ok(connection) => {
                    info!("Phase 1 complete: connected to server and fetched banner");
                    login_state.phase = LoginPhase::Credentials {
                        banner: connection.banner.clone(),
                    };
                    login_state.loading = false;
                    login_state.error_message = None;
                }
                Err(e) => {
                    error!(error = %e, "Phase 1 failed");
                    login_state.error_message = Some(e);
                    login_state.loading = false;
                }
            }
        } else {
            // Task still running, put handle back
            login_operation.phase1_handle = Some(handle);
        }
    }
}

/// System to handle phase 2: performing login with credentials. Also drives
/// the first-login follow-ups (setting an initial password, enrolling TOTP),
/// which are just logins with extra fields.
pub fn handle_login_phase2(
    mut login_state: ResMut<LoginDialogState>,
    mut login_operation: ResMut<LoginOperation>,
    server_manager: Res<sandpolis_server::ServerManager>,
) {
    // Check if we need to start phase 2
    if matches!(
        login_state.phase,
        LoginPhase::Credentials { .. }
            | LoginPhase::PasswordSetup { .. }
            | LoginPhase::TotpEnroll { .. }
    ) && login_state.loading
        && login_operation.phase2_handle.is_none()
    {
        // Parse server URL
        let server_url = match ServerUrl::from_str(&login_state.server_address) {
            Ok(url) => url,
            Err(e) => {
                login_state.error_message = Some(format!("Invalid server address: {}", e));
                login_state.loading = false;
                return;
            }
        };

        // Parse username
        let username = match sandpolis_server::user::UserName::from_str(&login_state.username) {
            Ok(u) => u,
            Err(e) => {
                login_state.error_message = Some(format!("Invalid username: {}", e));
                login_state.loading = false;
                return;
            }
        };

        debug!(username = %username, "Starting phase 2: logging in");

        // Clone what we need for the async task
        let server_layer_clone = server_manager.clone();
        let password = login_state.password.clone();
        let setup = matches!(login_state.phase, LoginPhase::PasswordSetup { .. });
        let totp_token = if login_state.otp.is_empty() {
            None
        } else {
            Some(login_state.otp.clone())
        };

        // Clone values for use outside the async block
        let server_url_clone = server_url.clone();
        let username_clone = username.clone();

        // Spawn async task
        let task = bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
            // First connect to server
            let connection = server_layer_clone
                .connect(server_url_clone)
                .await
                .map_err(|e| format!("Connection failed: {}", e))?;

            // Create login request with hashed password
            let login_request = LoginRequest {
                username: username_clone.clone(),
                password: LoginPassword::new(connection.cluster_id, &password),
                setup,
                totp_token,
                lifetime: None,
            };

            // Perform login
            let response = connection
                .login(login_request)
                .await
                .map_err(|e| format!("Login request failed: {}", e))?;

            Ok((connection, response))
        });

        login_operation.phase2_handle = Some(LoginPhase2Handle {
            task,
            server_url,
            username,
        });
    }

    // Check if phase 2 task is complete
    if let Some(mut handle) = login_operation.phase2_handle.take() {
        if let Some(result) = bevy::tasks::block_on(bevy::tasks::poll_once(&mut handle.task)) {
            match result {
                Ok((connection, LoginResponse::Ok(client_auth_token))) => {
                    info!("Phase 2 complete: login successful");

                    // Save the server (and its token) for future logins
                    if let Err(e) = server_manager.update_server_token(
                        &handle.server_url,
                        handle.username,
                        client_auth_token.clone(),
                    ) {
                        error!(error = %e, "Failed to save server");
                    }

                    // Hand the token to the retained connection the sync loop
                    // uses; if this server wasn't connected yet (added through
                    // this dialog), retain the connection we just logged in on.
                    let mut installed = false;
                    for existing in server_manager.server_connections() {
                        if existing.url == handle.server_url {
                            *existing.token.write().unwrap() = Some(client_auth_token.clone());
                            installed = true;
                        }
                    }
                    if !installed {
                        server_manager
                            .outbound
                            .write()
                            .unwrap()
                            .push(std::sync::Arc::new(connection));
                    }

                    // Close dialog and reset state
                    login_state.show = false;
                    login_state.phase = LoginPhase::ServerAddress;
                    login_state.server_address.clear();
                    login_state.username.clear();
                    login_state.password.clear();
                    login_state.password_confirm.clear();
                    login_state.otp.clear();
                    login_state.error_message = None;
                    login_state.loading = false;
                }
                Ok((_, LoginResponse::PasswordSetupRequired { totp_required })) => {
                    info!("First login: password setup required");
                    let banner = match &login_state.phase {
                        LoginPhase::Credentials { banner } => banner.clone(),
                        LoginPhase::PasswordSetup { banner, .. } => banner.clone(),
                        _ => Default::default(),
                    };
                    login_state.phase = LoginPhase::PasswordSetup {
                        banner,
                        totp_required,
                    };
                    login_state.password.clear();
                    login_state.password_confirm.clear();
                    login_state.error_message = None;
                    login_state.loading = false;
                }
                Ok((_, LoginResponse::TotpSetupRequired { otpauth_url })) => {
                    info!("First login: TOTP enrollment required");
                    login_state.phase = LoginPhase::TotpEnroll { otpauth_url };
                    login_state.otp.clear();
                    login_state.error_message = None;
                    login_state.loading = false;
                }
                Ok((_, LoginResponse::Denied)) => {
                    error!("Phase 2 failed: login denied");
                    login_state.error_message =
                        Some("Invalid username, password, or OTP".to_string());
                    login_state.loading = false;
                }
                Ok((_, LoginResponse::Expired)) => {
                    error!("Phase 2 failed: account expired");
                    login_state.error_message = Some("Account has expired".to_string());
                    login_state.loading = false;
                }
                Ok((_, LoginResponse::Invalid)) => {
                    error!("Phase 2 failed: invalid request");
                    login_state.error_message = Some("Invalid login request".to_string());
                    login_state.loading = false;
                }
                Err(e) => {
                    error!(error = %e, "Phase 2 failed");
                    login_state.error_message = Some(e);
                    login_state.loading = false;
                }
            }
        } else {
            // Task still running, put handle back
            login_operation.phase2_handle = Some(handle);
        }
    }
}

/// Open the login dialog on startup when a connected server requires a login
/// and no saved token covers it. Open realms and servers with a usable cached
/// token never show the dialog.
pub fn prompt_login_on_connect(
    mut login_state: ResMut<LoginDialogState>,
    server_manager: Res<sandpolis_server::ServerManager>,
    mut prompted: bevy::prelude::Local<bool>,
) {
    if *prompted || login_state.show {
        return;
    }

    for connection in server_manager.server_connections() {
        if !connection.banner.users_configured {
            continue;
        }
        if connection
            .token
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|token| token.is_usable())
        {
            continue;
        }

        debug!(url = %connection.url, "Server requires login");
        login_state.server_address = connection.url.to_string();

        // Pre-fill the username from the saved server entry, if any
        for server in server_manager.servers.iter() {
            let server = server.read();
            if server.address == connection.url && !server.user.is_empty() {
                login_state.username = server.user.to_string();
            }
        }

        login_state.phase = LoginPhase::Credentials {
            banner: connection.banner.clone(),
        };
        login_state.show = true;
        *prompted = true;
        return;
    }
}

/// System to check for saved servers and skip to phase 2 if applicable
pub fn check_saved_servers(
    mut login_state: ResMut<LoginDialogState>,
    server_manager: Res<sandpolis_server::ServerManager>,
) {
    // Only run when dialog is opened and in ServerAddress phase
    if !login_state.show || !matches!(login_state.phase, LoginPhase::ServerAddress) {
        return;
    }

    // Only run if server address is not empty
    if login_state.server_address.is_empty() {
        return;
    }

    // Parse server URL
    let server_url = match ServerUrl::from_str(&login_state.server_address) {
        Ok(url) => url,
        Err(_) => return, // Invalid URL, don't try to match
    };

    // Check if we have a saved server with this address
    for server_resident in server_manager.servers.iter() {
        let server = server_resident.read();
        if server.address == server_url {
            debug!(
                address = %server_url,
                "Found saved server, attempting auto-login"
            );

            // TODO: We could skip to phase 2 with the saved credentials,
            // or even attempt auto-login with the saved token.
            // For now, we'll just fetch the banner and pre-fill the username.
            login_state.username = server.user.to_string();

            // Trigger connection to fetch banner
            login_state.loading = true;
            return;
        }
    }
}

/// Modal root; tracks which phase the form was built for so it can be rebuilt
/// when the login flow advances.
#[derive(Component)]
pub struct LoginRoot {
    pub phase: u8,
}

#[derive(Component)]
pub struct LoginServerInput;
#[derive(Component)]
pub struct LoginUserInput;
#[derive(Component)]
pub struct LoginPassInput;
#[derive(Component)]
pub struct LoginPassConfirmInput;
#[derive(Component)]
pub struct LoginOtpInput;
#[derive(Component)]
pub struct LoginErrorText;

fn phase_id(phase: &LoginPhase) -> u8 {
    match phase {
        LoginPhase::ServerAddress => 0,
        LoginPhase::Credentials { .. } => 1,
        LoginPhase::PasswordSetup { .. } => 2,
        LoginPhase::TotpEnroll { .. } => 3,
    }
}

/// Spawn/despawn the login modal, rebuilding it when the phase changes.
pub fn manage_login(
    mut commands: Commands,
    theme: Res<Theme>,
    state: Res<LoginDialogState>,
    root: Query<(Entity, &LoginRoot)>,
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

    let phase = phase_id(&state.phase);
    let needs_rebuild = match existing {
        Some((_, root)) => root.phase != phase,
        None => true,
    };
    if needs_rebuild {
        if let Some((entity, _)) = existing {
            commands.entity(entity).despawn();
        }
        spawn_login_modal(&mut commands, &theme, &state, phase);
    }
}

fn spawn_login_modal(commands: &mut Commands, theme: &Theme, state: &LoginDialogState, phase: u8) {
    commands
        .spawn((LoginRoot { phase }, modal_scrim()))
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
                    p.spawn(heading(theme, "Login to Server"));
                    match &state.phase {
                        LoginPhase::ServerAddress => {
                            p.spawn(muted(theme, "Server address", theme.metrics.font_sm));
                            p.spawn((LoginServerInput, text_input(theme)));
                        }
                        LoginPhase::Credentials { banner } => {
                            if let Some(message) = &banner.message {
                                p.spawn(text(
                                    theme,
                                    message.clone(),
                                    theme.metrics.font_md,
                                    Role::Text,
                                ));
                            }
                            if banner.maintenance {
                                p.spawn(text(
                                    theme,
                                    "Server is in maintenance mode",
                                    theme.metrics.font_sm,
                                    Role::Warn,
                                ));
                            }
                            p.spawn(muted(
                                theme,
                                format!("Server: {}", state.server_address),
                                theme.metrics.font_sm,
                            ));
                            p.spawn(muted(theme, "Username", theme.metrics.font_sm));
                            p.spawn((LoginUserInput, text_input(theme)));
                            p.spawn(muted(theme, "Password", theme.metrics.font_sm));
                            p.spawn((LoginPassInput, text_input(theme)));
                            if banner.mfa {
                                p.spawn(muted(theme, "One-time code", theme.metrics.font_sm));
                                p.spawn((LoginOtpInput, text_input(theme)));
                            }
                        }
                        LoginPhase::PasswordSetup { totp_required, .. } => {
                            p.spawn(text(
                                theme,
                                format!("Choose a password for {}", state.username),
                                theme.metrics.font_md,
                                Role::Text,
                            ));
                            p.spawn(muted(theme, "Password", theme.metrics.font_sm));
                            p.spawn((LoginPassInput, text_input(theme)));
                            p.spawn(muted(theme, "Confirm password", theme.metrics.font_sm));
                            p.spawn((LoginPassConfirmInput, text_input(theme)));
                            if *totp_required {
                                p.spawn(muted(
                                    theme,
                                    "Two-factor enrollment follows",
                                    theme.metrics.font_sm,
                                ));
                            }
                        }
                        LoginPhase::TotpEnroll { otpauth_url } => {
                            p.spawn(text(
                                theme,
                                "Add this secret to your authenticator app:",
                                theme.metrics.font_md,
                                Role::Text,
                            ));
                            p.spawn(text(
                                theme,
                                otpauth_url.clone(),
                                theme.metrics.font_sm,
                                Role::Text,
                            ));
                            p.spawn(muted(theme, "One-time code", theme.metrics.font_sm));
                            p.spawn((LoginOtpInput, text_input(theme)));
                        }
                    }
                    p.spawn((
                        LoginErrorText,
                        text(theme, String::new(), theme.metrics.font_sm, Role::Error),
                    ));
                    p.spawn(Node {
                        column_gap: Val::Px(8.0),
                        ..default()
                    })
                    .with_children(|row| {
                        let primary = match phase {
                            0 => "Connect",
                            2 => "Set Password",
                            3 => "Verify",
                            _ => "Login",
                        };
                        row.spawn(button(theme, primary)).observe(on_login_primary);
                        if phase == 1 {
                            row.spawn(button(theme, "Back")).observe(on_login_back);
                        }
                        row.spawn(button(theme, "Cancel")).observe(on_login_cancel);
                    });
                });
        });
}

/// Focus the first field when a form is (re)built.
pub fn focus_login_input(
    server: Query<Entity, Added<LoginServerInput>>,
    user: Query<Entity, Added<LoginUserInput>>,
    pass: Query<Entity, Added<LoginPassInput>>,
    otp: Query<Entity, Added<LoginOtpInput>>,
    mut focus: ResMut<InputFocus>,
) {
    if let Ok(entity) = server.single() {
        focus.set(entity, FocusCause::Navigated);
    } else if let Ok(entity) = user.single() {
        focus.set(entity, FocusCause::Navigated);
    } else if let Ok(entity) = pass.single() {
        focus.set(entity, FocusCause::Navigated);
    } else if let Ok(entity) = otp.single() {
        focus.set(entity, FocusCause::Navigated);
    }
}

/// Copy text-input contents into [`LoginDialogState`] for the login systems.
pub fn sync_login_inputs(
    mut state: ResMut<LoginDialogState>,
    server: Query<&EditableText, With<LoginServerInput>>,
    user: Query<&EditableText, With<LoginUserInput>>,
    pass: Query<&EditableText, With<LoginPassInput>>,
    confirm: Query<&EditableText, With<LoginPassConfirmInput>>,
    otp: Query<&EditableText, With<LoginOtpInput>>,
) {
    if let Ok(input) = confirm.single() {
        let value = input.value().to_string();
        if state.password_confirm != value {
            state.password_confirm = value;
        }
    }
    if let Ok(input) = server.single() {
        let value = input.value().to_string();
        if state.server_address != value {
            state.server_address = value;
        }
    }
    if let Ok(input) = user.single() {
        let value = input.value().to_string();
        if state.username != value {
            state.username = value;
        }
    }
    if let Ok(input) = pass.single() {
        let value = input.value().to_string();
        if state.password != value {
            state.password = value;
        }
    }
    if let Ok(input) = otp.single() {
        let value = input.value().to_string();
        if state.otp != value {
            state.otp = value;
        }
    }
}

/// Mirror the login error message into the form's error label.
pub fn update_login_error(
    state: Res<LoginDialogState>,
    mut label: Query<&mut Text, With<LoginErrorText>>,
) {
    if let Ok(mut text) = label.single_mut() {
        let message = state.error_message.clone().unwrap_or_default();
        if text.0 != message {
            text.0 = message;
        }
    }
}

fn on_login_primary(_activate: On<Activate>, mut state: ResMut<LoginDialogState>) {
    // Setting a password is the one submission worth stopping locally: the two
    // fields have to agree before the server ever sees it.
    if matches!(state.phase, LoginPhase::PasswordSetup { .. }) {
        if state.password.is_empty() {
            state.error_message = Some("Password cannot be empty".to_string());
            return;
        }
        if state.password != state.password_confirm {
            state.error_message = Some("Passwords do not match".to_string());
            return;
        }
    }
    state.loading = true;
    state.error_message = None;
}

fn on_login_back(_activate: On<Activate>, mut state: ResMut<LoginDialogState>) {
    state.phase = LoginPhase::ServerAddress;
    state.username.clear();
    state.password.clear();
    state.password_confirm.clear();
    state.otp.clear();
    state.error_message = None;
    state.loading = false;
}

fn on_login_cancel(_activate: On<Activate>, mut state: ResMut<LoginDialogState>) {
    state.show = false;
    state.phase = LoginPhase::ServerAddress;
    state.username.clear();
    state.password.clear();
    state.password_confirm.clear();
    state.otp.clear();
    state.error_message = None;
    state.loading = false;
}

/// Marker component for the rotating 3D logo shown behind the login dialog.
#[derive(Component)]
pub struct LoginLogo;

/// Marker component for the login logo's 3D camera and light.
#[derive(Component)]
pub struct LoginLogoCamera;

/// Spawn/despawn the rotating 3D logo with the login dialog. The realm
/// selection dialog shares the backdrop, so the handoff from one to the other
/// keeps the same scene behind it.
pub fn spawn_login_logo(
    mut commands: Commands,
    login_state: Res<LoginDialogState>,
    realm_state: Res<crate::gui::realm_select::RealmSelectState>,
    logo_query: Query<Entity, With<LoginLogo>>,
    camera_query: Query<Entity, With<LoginLogoCamera>>,
    asset_server: Res<AssetServer>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if !login_state.is_changed() && !realm_state.is_changed() {
        return;
    }

    // The realm dialog nearly fills a phone's narrow screen, so the logo
    // behind it would only bleed out around the edges there.
    let realm_shows_logo =
        cfg!(not(any(target_os = "android", target_os = "ios"))) && realm_state.show;
    if login_state.show || realm_shows_logo {
        if logo_query.is_empty() {
            // Load the logo mesh from the glTF primitive; we apply our own
            // material below, so only the mesh is needed.
            let mesh_handle: Handle<Mesh> = asset_server.load(
                GltfAssetLabel::Primitive {
                    mesh: 0,
                    primitive: 0,
                }
                .from_asset("mesh/v7.glb"),
            );

            commands.spawn((
                Mesh3d(mesh_handle),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: Color::srgb(0.784, 0.580, 0.216), // #c89437
                    metallic: 0.5,
                    perceptual_roughness: 0.3,
                    ..default()
                })),
                Transform::from_xyz(0.0, 2.0, 0.0).with_scale(Vec3::splat(0.01)),
                LoginLogo,
            ));

            if camera_query.is_empty() {
                commands.spawn((
                    Camera3d::default(),
                    Transform::from_xyz(0.0, 2.0, 5.0)
                        .looking_at(Vec3::new(0.0, 2.0, 0.0), Vec3::Y),
                    Camera {
                        order: 1, // Render after the main 2D camera
                        // Don't clear: overlay the logo on the world rather
                        // than erasing it (the dialog renders separately).
                        clear_color: bevy::camera::ClearColorConfig::None,
                        ..default()
                    },
                    LoginLogoCamera,
                ));

                commands.spawn((
                    PointLight {
                        intensity: 2_000_000.0,
                        shadow_maps_enabled: false,
                        ..default()
                    },
                    Transform::from_xyz(4.0, 8.0, 4.0),
                    LoginLogoCamera,
                ));
            }
        }
    } else {
        for entity in logo_query.iter() {
            commands.entity(entity).despawn();
        }
        for entity in camera_query.iter() {
            commands.entity(entity).despawn();
        }
    }
}

/// Rotate the logo slowly backwards while the dialog is up.
pub fn rotate_login_logo(time: Res<Time>, mut logo_query: Query<&mut Transform, With<LoginLogo>>) {
    for mut transform in logo_query.iter_mut() {
        transform.rotate_x(-0.5 * time.delta_secs());
    }
}
