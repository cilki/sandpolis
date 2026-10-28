//! The Agent layer's client: its toolbar button and the deploy dialog behind it.
//!
//! Deployment is driven from here but performed by the server (see
//! [`crate::deploy`]), so this module's job is to collect connection details and
//! render what comes back. The details come from the operator's own SSH setup —
//! anything left blank is filled in from `~/.ssh/config` and their default key,
//! so the common case is "type a host, press Deploy".
//!
//! The dialog has two faces: the form, and the progress list it switches to once
//! a deployment is running. The switch is a despawn/respawn, which is why
//! [`manage_deploy_dialog`] tracks what it last built.

use crate::client::deploy_form::{self, DeployDefaults, DeployForm};
use crate::deploy::client::DeployStreamRequester;
use crate::deploy::{DeployStep, DeployStreamRequest, DeployStreamResponse, DeployTarget};
use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::prelude::*;
use bevy::text::EditableText;
use sandpolis_client::gui::ui::Activate;
use sandpolis_client::gui::ui::layer::{LayerClientInfo, RegisterLayerClient};
use sandpolis_client::gui::ui::panel::modal_scrim;
use sandpolis_client::gui::ui::text_input::text_input;
use sandpolis_client::gui::ui::theme::{Role, Theme, ThemedBg, ThemedBorder};
use sandpolis_client::gui::ui::widgets::{button, heading, muted, text};
use sandpolis_instance::{InstanceType, LayerName};
use sandpolis_instance::network::stream::StreamMessage;
use sandpolis_instance::notification::{Notification, notify};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::warn;

/// Registers the Agent layer's client and the deploy dialog.
pub struct AgentClientPlugin;

impl Plugin for AgentClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DeployDialogState>()
            .add_systems(
                Update,
                (
                    manage_deploy_dialog,
                    focus_deploy_input,
                    sync_deploy_inputs,
                    poll_deploy_events,
                    update_deploy_steps,
                    animate_deploy_progress,
                ),
            )
            .register_layer_client(
                LayerClientInfo::new(
                    LayerName::from("Agent"),
                    "Managed instances running the agent",
                )
                .with_visible_instance_types(&[InstanceType::Agent])
                .with_toolbar_action("Deploy agent", "toolbar/add_agent.svg", |commands| {
                    commands.queue(|world: &mut World| {
                        if let Some(mut state) = world.get_resource_mut::<DeployDialogState>() {
                            state.open();
                        }
                    });
                }),
            );
    }
}

/// Which face of the dialog is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DeployPhase {
    /// Collecting connection details.
    #[default]
    Form,
    /// A deployment is running (or has ended, with its outcome on screen).
    Progress,
}

/// How one step is getting on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum StepState {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
}

impl StepState {
    /// Leading glyph in the progress list.
    fn glyph(&self) -> &'static str {
        match self {
            Self::Pending => "·",
            Self::Running => "▶",
            Self::Done => "✓",
            Self::Failed => "✗",
        }
    }

    fn role(&self) -> Role {
        match self {
            Self::Pending => Role::TextMuted,
            Self::Running => Role::Accent,
            Self::Done => Role::Text,
            Self::Failed => Role::Error,
        }
    }
}

/// State of the deploy dialog.
#[derive(Resource, Default)]
pub struct DeployDialogState {
    pub show: bool,
    phase: DeployPhase,
    /// Host or `~/.ssh/config` alias.
    host: String,
    username: String,
    port: String,
    key_path: String,
    password: String,
    defaults: DeployDefaults,
    /// Per-step state, indexed the same as [`DeployStep::ALL`].
    steps: [StepState; DeployStep::ALL.len()],
    /// The running step's description, or the outcome once it's over.
    message: Option<String>,
    /// Progress from the server. Present only while a deployment is running.
    events: Option<UnboundedReceiver<DeployStreamResponse>>,
    /// The open stream, kept so the deployment can be called off.
    stream: Option<DeployStream>,
}

/// A deploy stream, and what it takes to close it.
///
/// Closing matters: the server's responder cancels the deployment when it's
/// dropped, and that only happens when the stream goes away. Dropping the
/// receiving end here would leave the deployment running with nobody watching.
struct DeployStream {
    connection: std::sync::Arc<sandpolis_instance::network::InstanceConnection>,
    id: sandpolis_instance::network::stream::StreamId,
}

impl DeployStream {
    fn close(self) {
        self.connection.close_stream(self.id);
    }
}

impl DeployDialogState {
    /// Open the dialog on a blank form.
    fn open(&mut self) {
        self.end();
        self.show = true;
        self.phase = DeployPhase::Form;
        self.host.clear();
        self.username.clear();
        self.port.clear();
        self.key_path.clear();
        self.password.clear();
        self.message = None;
        self.steps = Default::default();
        self.defaults = DeployDefaults::detect();
    }

    fn close(&mut self) {
        self.show = false;
        self.end();
    }

    /// Stop watching, and stop whatever the server is still doing for us.
    fn end(&mut self) {
        self.events = None;
        if let Some(stream) = self.stream.take() {
            stream.close();
        }
    }

    fn step_mut(&mut self, step: DeployStep) -> &mut StepState {
        let index = DeployStep::ALL
            .iter()
            .position(|candidate| *candidate == step)
            .unwrap_or(0);
        &mut self.steps[index]
    }

    fn step(&self, step: DeployStep) -> StepState {
        DeployStep::ALL
            .iter()
            .position(|candidate| *candidate == step)
            .map(|index| self.steps[index])
            .unwrap_or_default()
    }

    /// Whether a deployment is still in flight.
    fn running(&self) -> bool {
        self.events.is_some()
    }
}

#[derive(Component)]
struct DeployRoot;

/// What the dialog's body was last built for, so a phase change rebuilds it.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
struct DeployBody(DeployPhase);

#[derive(Component)]
struct HostInput;
#[derive(Component)]
struct UserInput;
#[derive(Component)]
struct PortInput;
#[derive(Component)]
struct KeyInput;
#[derive(Component)]
struct PasswordInput;

/// One row of the progress list.
#[derive(Component)]
struct DeployStepRow(DeployStep);

/// The sweeping fill of the indeterminate progress bar.
#[derive(Component)]
struct DeployProgressBar;

/// The line under the step list carrying the current message or the outcome.
#[derive(Component)]
struct DeployMessageLine;

/// Spawn, rebuild, and despawn the deploy modal.
fn manage_deploy_dialog(
    mut commands: Commands,
    theme: Res<Theme>,
    state: Res<DeployDialogState>,
    root: Query<Entity, With<DeployRoot>>,
    body: Query<&DeployBody>,
    mut focus: ResMut<InputFocus>,
) {
    let existing = root.single().ok();

    if !state.show {
        if let Some(entity) = existing {
            commands.entity(entity).despawn();
            focus.clear();
        }
        return;
    }

    // Already showing the right face.
    if existing.is_some() && body.single().is_ok_and(|built| built.0 == state.phase) {
        return;
    }

    if let Some(entity) = existing {
        commands.entity(entity).despawn();
        focus.clear();
    }

    let mut root = commands.spawn((DeployRoot, modal_scrim()));
    root.with_children(|scrim| {
        let mut panel = scrim.spawn((
            DeployBody(state.phase),
            Node {
                flex_direction: FlexDirection::Column,
                width: Val::Px(420.0),
                max_height: Val::Percent(90.0),
                overflow: Overflow::scroll_y(),
                padding: UiRect::all(Val::Px(16.0)),
                row_gap: Val::Px(6.0),
                border: UiRect::all(Val::Px(1.0)),
                ..default()
            },
            BackgroundColor(theme.color(Role::Panel)),
            ThemedBg(Role::Panel),
            BorderColor::all(theme.color(Role::Border)),
            ThemedBorder(Role::Border),
        ));

        panel.with_children(|body| match state.phase {
            DeployPhase::Form => build_form(body, &theme, &state),
            DeployPhase::Progress => build_progress(body, &theme, &state),
        });
    });
}

/// The connection form.
fn build_form(p: &mut ChildSpawnerCommands, theme: &Theme, state: &DeployDialogState) {
    let default_key = state
        .defaults
        .identity_file
        .clone()
        .unwrap_or_else(|| "none found".to_string());
    let default_user = state.defaults.username.clone();

    p.spawn(heading(theme, "Deploy Agent"));
    p.spawn(muted(
        theme,
        "Blank fields are filled in from ~/.ssh/config.",
        theme.metrics.font_sm,
    ));

    p.spawn(muted(theme, "Host or SSH alias", theme.metrics.font_sm));
    p.spawn((HostInput, text_input(theme)));

    p.spawn(muted(
        theme,
        format!("User (default: {default_user})"),
        theme.metrics.font_sm,
    ));
    p.spawn((UserInput, text_input(theme)));

    p.spawn(muted(theme, "Port (default: 22)", theme.metrics.font_sm));
    p.spawn((PortInput, text_input(theme)));

    p.spawn(muted(
        theme,
        format!("Private key (default: {default_key})"),
        theme.metrics.font_sm,
    ));
    p.spawn((KeyInput, text_input(theme)));

    p.spawn(muted(
        theme,
        "Password (used only when no key is given)",
        theme.metrics.font_sm,
    ));
    p.spawn((PasswordInput, text_input(theme)));

    // Always present, even when empty: a submit that fails validation updates
    // this line without rebuilding the form, so there has to be one to update.
    p.spawn((
        DeployMessageLine,
        text(
            theme,
            state.message.clone().unwrap_or_default(),
            theme.metrics.font_sm,
            Role::Error,
        ),
    ));

    p.spawn(Node {
        column_gap: Val::Px(8.0),
        ..default()
    })
    .with_children(|row| {
        row.spawn(button(theme, "Deploy")).observe(on_deploy_submit);
        row.spawn(button(theme, "Cancel")).observe(on_deploy_close);
    });
}

/// The progress list.
fn build_progress(p: &mut ChildSpawnerCommands, theme: &Theme, state: &DeployDialogState) {
    p.spawn(heading(theme, "Deploy Agent"));
    p.spawn(muted(
        theme,
        format!("Target: {}", state.host),
        theme.metrics.font_sm,
    ));

    for step in DeployStep::ALL {
        let status = state.step(step);
        p.spawn((
            DeployStepRow(step),
            text(
                theme,
                format!("{}  {}", status.glyph(), step.label()),
                theme.metrics.font_md,
                status.role(),
            ),
        ));
    }

    // Indeterminate bar: the server reports steps, not bytes, so there is no
    // honest percentage to show — only that something is still happening.
    p.spawn((
        Node {
            width: Val::Percent(100.0),
            height: Val::Px(4.0),
            margin: UiRect::vertical(Val::Px(6.0)),
            overflow: Overflow::clip(),
            ..default()
        },
        BackgroundColor(theme.color(Role::Surface)),
        ThemedBg(Role::Surface),
    ))
    .with_children(|track| {
        track.spawn((
            DeployProgressBar,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Percent(0.0),
                width: Val::Percent(30.0),
                height: Val::Px(4.0),
                ..default()
            },
            BackgroundColor(theme.color(Role::Accent)),
        ));
    });

    p.spawn((
        DeployMessageLine,
        text(
            theme,
            state.message.clone().unwrap_or_default(),
            theme.metrics.font_sm,
            Role::TextMuted,
        ),
    ));

    p.spawn(Node {
        column_gap: Val::Px(8.0),
        ..default()
    })
    .with_children(|row| {
        row.spawn(button(theme, "Close")).observe(on_deploy_close);
    });
}

/// Focus the host field when the form opens.
fn focus_deploy_input(inputs: Query<Entity, Added<HostInput>>, mut focus: ResMut<InputFocus>) {
    if let Ok(entity) = inputs.single() {
        focus.set(entity, FocusCause::Navigated);
    }
}

/// Copy the form's inputs into [`DeployDialogState`].
fn sync_deploy_inputs(
    mut state: ResMut<DeployDialogState>,
    host: Query<&EditableText, With<HostInput>>,
    user: Query<&EditableText, With<UserInput>>,
    port: Query<&EditableText, With<PortInput>>,
    key: Query<&EditableText, With<KeyInput>>,
    password: Query<&EditableText, With<PasswordInput>>,
) {
    /// Copy a field, leaving the state untouched when it hasn't changed so the
    /// resource isn't marked dirty every frame.
    fn sync(field: &mut String, value: Option<String>) {
        if let Some(value) = value
            && *field != value
        {
            *field = value;
        }
    }

    fn read<M: Component>(query: &Query<&EditableText, With<M>>) -> Option<String> {
        query.single().ok().map(|input| input.value().to_string())
    }

    let (host, user, port, key, password) = (
        read(&host),
        read(&user),
        read(&port),
        read(&key),
        read(&password),
    );
    let state = state.bypass_change_detection();
    sync(&mut state.host, host);
    sync(&mut state.username, user);
    sync(&mut state.port, port);
    sync(&mut state.key_path, key);
    sync(&mut state.password, password);
}

/// Resolve the form against `~/.ssh/config` and start the deployment.
fn on_deploy_submit(_: On<Activate>, mut state: ResMut<DeployDialogState>) {
    match start_deploy(&mut state) {
        Ok((events, stream)) => {
            state.events = Some(events);
            state.stream = Some(stream);
            state.steps = Default::default();
            state.message = Some("Starting".to_string());
            state.phase = DeployPhase::Progress;
        }
        Err(e) => {
            // Stay on the form: everything that fails here is something the
            // operator can fix in the fields in front of them.
            state.message = Some(format!("{e:#}"));
        }
    }
}

fn on_deploy_close(_: On<Activate>, mut state: ResMut<DeployDialogState>) {
    state.close();
}

/// Build the request from the form plus the operator's SSH config, and open the
/// stream. Returns the channel the server's progress arrives on, and the stream
/// itself so it can be closed.
fn start_deploy(
    state: &mut DeployDialogState,
) -> anyhow::Result<(UnboundedReceiver<DeployStreamResponse>, DeployStream)> {
    use anyhow::{Context, bail};

    let password = state.password.trim().to_string();
    let port = match state.port.trim() {
        "" => None,
        given => Some(
            given
                .parse()
                .with_context(|| format!("{given:?} is not a port number"))?,
        ),
    };

    let resolved = deploy_form::resolve_with_ssh_config(
        &DeployForm {
            host: state.host.clone(),
            username: state.username.clone(),
            port,
            key_path: state.key_path.clone(),
            have_password: !password.is_empty(),
            fingerprint: None,
        },
        &state.defaults,
    )?;
    let auth = deploy_form::read_auth(resolved.key_path.as_deref(), &password)?;

    let connection =
        sandpolis_client::sync::connection().context("Not connected to a server yet")?;
    let server = sandpolis_client::sync::primary_server_url()
        .context("The server connection has no URL yet")?;

    let (requester, events) = DeployStreamRequester::channel();
    let request = DeployStreamRequest::Start {
        target: DeployTarget {
            host: resolved.host,
            port: resolved.port,
            username: resolved.username,
            auth,
            fingerprint: resolved.fingerprint,
        },
        server,
        dry_run: false,
    };

    // Registered here rather than inside the task so the caller learns the
    // stream's id and can close it — closing is what calls a deployment off.
    let payload = serde_cbor::to_vec(&request)?;
    let (id, tx) = connection.register_stream(requester);
    let stream = DeployStream {
        connection: connection.clone(),
        id,
    };

    let started = sandpolis_client::sync::spawn(async move {
        // The only message this side sends. The stream stays registered after
        // the sender is dropped, which is what keeps progress flowing back.
        if let Err(e) = tx.send(StreamMessage::local(id, payload)).await {
            warn!(error = %e, "Failed to send the deploy request");
        }
    });
    if !started {
        stream.close();
        bail!("The client's connection isn't ready yet");
    }

    Ok((events, stream))
}

/// Drain progress from the server into the dialog's state.
fn poll_deploy_events(mut state: ResMut<DeployDialogState>) {
    if !state.running() {
        return;
    }

    // Take the receiver so the rest of the state can be mutated while draining.
    let Some(mut events) = state.events.take() else {
        return;
    };
    let mut finished = false;

    while let Ok(response) = events.try_recv() {
        match response {
            DeployStreamResponse::Step { step, message } => {
                *state.step_mut(step) = StepState::Running;
                state.message = Some(message);
            }
            DeployStreamResponse::Done { step } => {
                *state.step_mut(step) = StepState::Done;
            }
            DeployStreamResponse::Finished { reconfigured } => {
                let message = if reconfigured {
                    format!(
                        "{} already had an agent; its realm cert was rewritten.",
                        state.host
                    )
                } else {
                    format!("The agent is installed and running on {}.", state.host)
                };
                notify(Notification::info("Agent", "Deployment finished").body(&message));
                state.message = Some(message);
                finished = true;
            }
            DeployStreamResponse::Failed { step, message } => {
                *state.step_mut(step) = StepState::Failed;
                notify(
                    Notification::error("Agent", "Deployment failed")
                        .body(format!("{}: {message}", step.label())),
                );
                state.message = Some(message);
                finished = true;
            }
            // The dialog never asks for a dry run, but a verdict is a verdict.
            DeployStreamResponse::Planned { blocker, .. } => {
                state.message = Some(match blocker {
                    Some(blocker) => format!("Dry run: {blocker}"),
                    None => "Dry run finished; nothing was changed.".to_string(),
                });
                finished = true;
            }
        }
    }

    // A closed channel means the connection dropped without a verdict.
    if !finished && events.is_closed() && events.is_empty() {
        state.message = Some("The connection to the server was lost.".to_string());
        finished = true;
    }

    if finished {
        // The outcome stays on screen until the operator closes the dialog, but
        // the stream has nothing left to carry.
        if let Some(stream) = state.stream.take() {
            stream.close();
        }
    } else {
        state.events = Some(events);
    }
}

/// Repaint the step rows and the message line from the dialog's state.
fn update_deploy_steps(
    state: Res<DeployDialogState>,
    theme: Res<Theme>,
    mut rows: Query<(&DeployStepRow, &mut Text, &mut TextColor), Without<DeployMessageLine>>,
    mut message: Query<&mut Text, With<DeployMessageLine>>,
) {
    // The rows carry colors we compute rather than a `ThemedText` role, so a
    // theme change has to come back through here.
    if !(state.is_changed() || theme.is_changed()) {
        return;
    }

    for (row, mut text, mut color) in &mut rows {
        let status = state.step(row.0);
        let label = format!("{}  {}", status.glyph(), row.0.label());
        if text.0 != label {
            text.0 = label;
        }
        color.0 = theme.color(status.role());
    }

    if let Ok(mut text) = message.single_mut() {
        let value = state.message.clone().unwrap_or_default();
        if text.0 != value {
            text.0 = value;
        }
    }
}

/// Sweep the indeterminate bar while a deployment is running, and park it at
/// the far left once it isn't.
fn animate_deploy_progress(
    time: Res<Time>,
    state: Res<DeployDialogState>,
    mut bars: Query<&mut Node, With<DeployProgressBar>>,
) {
    /// Seconds for one sweep across the track and back.
    const PERIOD: f32 = 2.4;
    /// The fill's width, as a percentage of the track.
    const WIDTH: f32 = 30.0;

    for mut node in &mut bars {
        let left = if state.running() {
            // Triangle wave: across, then back, with no jump at either end.
            let phase = (time.elapsed_secs() % PERIOD) / PERIOD;
            let sweep = if phase < 0.5 { phase * 2.0 } else { (1.0 - phase) * 2.0 };
            sweep * (100.0 - WIDTH)
        } else {
            0.0
        };
        node.left = Val::Percent(left);
    }
}
