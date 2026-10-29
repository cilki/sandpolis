//! GUI components for the Shell layer.
//!
//! Renders remote shell sessions through [`alacritty_terminal`], a headless VT
//! emulator. Expanding a node's panel opens a [`ShellSessionStreamRequester`] to
//! the agent via [`InstanceConnection::open_stream_to`](sandpolis_instance); the
//! agent runs the shell on a real PTY. Output bytes are fed to an
//! [`alacritty_terminal::Term`] grid and rendered as a fixed grid of styled
//! text rows; keystrokes are translated to terminal input and sent back as
//! `Stdin`.

mod keys;
mod render;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::vte::ansi::Processor;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::prelude::*;
use sandpolis_client::gui::ui::gating::WantsKeyboard;
use sandpolis_client::gui::ui::layer::{LayerClientInfo, RegisterLayerClient};
use sandpolis_client::gui::ui::node_panel::{NodePanel, PanelCtx, PanelTarget};
use sandpolis_client::gui::ui::theme::Role;
use sandpolis_client::gui::ui::widgets::{row, text};
use sandpolis_instance::network::stream::StreamMessage;
use sandpolis_instance::{InstanceType, LayerName};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver, channel};

use crate::session::{
    ShellEvent, ShellSessionStreamRequest, ShellSessionStreamRequester, spawn_shell_stream,
};

/// Terminal font size, in logical pixels.
const FONT_SIZE: f32 = 13.0;
/// Advance width of one cell. JetBrains Mono advances 0.6em per glyph.
const CELL_W: f32 = FONT_SIZE * 0.6;
/// Height of one cell (line height).
const CELL_H: f32 = FONT_SIZE * 1.2;
/// Default terminal background.
const TERM_BG: Color = Color::srgb(0.08, 0.08, 0.10);
/// Height the terminal grid claims inside a panel, so a session opens with a
/// usable number of rows instead of whatever a zero-height flex box leaves.
const TERMINAL_MIN_HEIGHT: f32 = 260.0;

/// Handle to the embedded monospace font the terminal renders with.
#[derive(Resource)]
struct TerminalFont(Handle<Font>);

/// Active client-side shell sessions, keyed by the node they belong to.
///
/// The key is a [`PanelTarget`] rather than a bare `InstanceId` because a
/// probe node borrows its gateway server's id; only the sub-node device id tells
/// an SSH probe apart from the server it orbits.
#[derive(Resource, Default)]
struct ShellStreams {
    sessions: HashMap<PanelTarget, ShellStreamSession>,
}

/// A live shell session the GUI is rendering.
struct ShellStreamSession {
    /// Output chunks pushed by the requester (registered on the connection).
    output: UnboundedReceiver<ShellEvent>,
    /// Outbound requests (stdin, resize). A background task forwards these over
    /// the stream, translating to the SSH wire type for probe sessions.
    outbound: Sender<ShellSessionStreamRequest>,
    /// Headless terminal emulator fed by `output`.
    term: Term<EventProxy>,
    /// ANSI byte parser driving `term`.
    processor: Processor,
    /// The grid container entity this session renders into, if a node panel is
    /// currently expanded on it. `None` while the panel is collapsed.
    grid: Option<Entity>,
    rows: u16,
    cols: u16,
    /// The grid changed and needs to be redrawn.
    dirty: bool,
    /// The stream ended (child exited or connection dropped).
    ended: bool,
}

/// Forwards terminal replies (cursor/device/color queries) back to the PTY.
#[derive(Clone)]
struct EventProxy(Sender<ShellSessionStreamRequest>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            let _ = self
                .0
                .try_send(ShellSessionStreamRequest::Stdin {
                    data: text.into_bytes(),
                });
        }
    }
}

/// Grid dimensions passed to [`Term::new`] / [`Term::resize`].
#[derive(Clone, Copy)]
struct TermDimensions {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

/// Marks the grid container of a terminal panel.
#[derive(Component)]
struct TerminalGrid {
    target: PanelTarget,
}

/// Marks a grid that should open a session once its size is known.
#[derive(Component)]
struct TerminalPendingStart;

/// The shell layer's node panel (VT terminal).
pub struct ShellPanel;

impl NodePanel for ShellPanel {
    fn build_summary(&self, ctx: &mut PanelCtx) {
        let target = ctx.target;
        let theme = ctx.theme;
        ctx.children(|p| {
            p.spawn((
                // Filled in by `update_session_summaries`, which reads the
                // session map rather than captured state — so a session started
                // from an expanded panel shows up here the moment that panel
                // collapses.
                text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                SessionSummary { target },
            ));
        });
    }

    /// Terminal panels open their session immediately: expanding the panel is
    /// the request for a shell, and a button in front of it was only ever an
    /// extra click. Collapsing and reopening the panel is how a session that
    /// ended gets restarted.
    fn build_detail(&self, ctx: &mut PanelCtx) {
        let target = ctx.target;
        let theme = ctx.theme;

        // Grid container: captures keyboard focus and renders the terminal.
        let grid = ctx
            .commands
            .spawn((
                TerminalGrid { target },
                TerminalPendingStart,
                WantsKeyboard,
                TabIndex(0),
                Interaction::default(),
                Node {
                    flex_direction: FlexDirection::Column,
                    flex_grow: 1.0,
                    min_height: Val::Px(TERMINAL_MIN_HEIGHT),
                    padding: UiRect::all(Val::Px(theme.metrics.space_xs)),
                    overflow: Overflow::clip(),
                    ..default()
                },
                BackgroundColor(TERM_BG),
            ))
            .id();

        // Clicking the grid focuses it so keystrokes route to the terminal.
        ctx.commands.entity(grid).observe(
            move |_: On<PointerClick>, mut focus: ResMut<InputFocus>| {
                focus.set(grid, FocusCause::Navigated);
            },
        );

        let header = ctx
            .commands
            .spawn(row(theme.metrics.space_sm))
            .with_children(|h| {
                h.spawn((
                    text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                    SessionSummary { target },
                ));
            })
            .id();

        ctx.commands
            .entity(ctx.body)
            .add_children(&[header, grid]);
    }
}

/// Marks a label describing a target's session state, filled in by
/// [`update_session_summaries`].
#[derive(Component)]
struct SessionSummary {
    target: PanelTarget,
}

/// Describe each target's session state in its summary labels.
fn update_session_summaries(
    streams: Res<ShellStreams>,
    mut labels: Query<(&SessionSummary, &mut Text)>,
) {
    // The session comes up asynchronously after startup, so a panel expanded
    // early has nothing to open a session over yet. Saying so beats "no
    // session", which reads as "and nothing is trying to change that".
    let connected = sandpolis_client::sync::connection().is_some();

    for (summary, mut label) in &mut labels {
        let value = match streams.sessions.get(&summary.target) {
            Some(session) if session.ended => "Session ended".to_string(),
            Some(session) => format!("Shell {}×{}", session.cols, session.rows),
            None if !connected => "Waiting for server…".to_string(),
            None => "No session".to_string(),
        };
        if label.0 != value {
            label.0 = value;
        }
    }
}

/// Load the embedded monospace font.
fn load_terminal_font(mut commands: Commands, asset_server: Res<AssetServer>) {
    commands.insert_resource(TerminalFont(
        asset_server.load("fonts/JetBrainsMono-Regular.ttf"),
    ));
}

/// Reattach a live session to a freshly (re)built grid entity, e.g. when the
/// node panel is expanded again.
///
/// This runs before [`start_pending_sessions`], which is what makes a panel that
/// reopens on a live session reattach to it rather than start a second one. A
/// session whose shell already exited is dropped instead, so reopening the panel
/// is what restarts it — there's no "restart" button, and reattaching to a dead
/// terminal would leave one permanently stuck.
fn attach_terminal_grids(
    mut commands: Commands,
    grids: Query<(Entity, &TerminalGrid), Added<TerminalGrid>>,
    mut streams: ResMut<ShellStreams>,
) {
    for (entity, grid) in grids.iter() {
        let Some(session) = streams.sessions.get_mut(&grid.target) else {
            continue;
        };
        if session.ended {
            streams.sessions.remove(&grid.target);
            continue;
        }
        session.grid = Some(entity);
        session.dirty = true;
        commands.entity(entity).remove::<TerminalPendingStart>();
    }
}

/// Open sessions for grids marked pending, once their laid-out size is known.
fn start_pending_sessions(
    mut commands: Commands,
    pending: Query<(Entity, &TerminalGrid, &ComputedNode), With<TerminalPendingStart>>,
    mut streams: ResMut<ShellStreams>,
) {
    for (entity, grid, node) in pending.iter() {
        // Reattachment already claimed this grid.
        if streams.sessions.contains_key(&grid.target) {
            commands.entity(entity).remove::<TerminalPendingStart>();
            continue;
        }
        let logical = node.size() * node.inverse_scale_factor();
        if logical.x < CELL_W || logical.y < CELL_H {
            continue; // not laid out yet
        }
        let cols = ((logical.x / CELL_W).floor() as u16).max(2);
        let rows = ((logical.y / CELL_H).floor() as u16).max(2);

        let (outbound, outbound_rx) = channel(64);

        let output = match open_session_stream(grid.target, rows, cols, outbound_rx) {
            SessionStart::Opened(output) => output,
            // Keep the marker and come back next frame. Recording a session here
            // would be worse than waiting: the terminal would look live, take
            // keystrokes nothing reads, and block the retry that would have
            // worked once the session came up.
            SessionStart::NotReady => continue,
            SessionStart::Unsupported => {
                // Nothing this layer can drive on the other end (an account
                // node, or a probe with no SSH). Stop asking.
                commands.entity(entity).remove::<TerminalPendingStart>();
                continue;
            }
        };

        let dims = TermDimensions {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        let term = Term::new(
            alacritty_terminal::term::Config::default(),
            &dims,
            EventProxy(outbound.clone()),
        );

        streams.sessions.insert(
            grid.target,
            ShellStreamSession {
                output,
                outbound,
                term,
                processor: Processor::new(),
                grid: Some(entity),
                rows,
                cols,
                dirty: true,
                ended: false,
            },
        );
        commands.entity(entity).remove::<TerminalPendingStart>();
        info!("Shell session opened for {:?} ({}x{})", grid.target, cols, rows);
    }
}

/// The SSH probe device behind `target`, if there is one.
///
/// Reads the probe subsystem's device registry directly rather than going through
/// `sandpolis_probe::client::gui`, whose helpers are behind that crate's `client`
/// feature — depending on them would drag its whole GUI stack in here.
#[cfg(feature = "probe")]
fn ssh_probe_device(target: PanelTarget) -> Option<sandpolis_probe::RegisteredDevice> {
    let device_id = target.sub?;
    let device = sandpolis_probe::REGISTERED_DEVICES
        .read()
        .ok()?
        .iter()
        .find(|device| device.id.body() == device_id)?
        .clone();
    device.device.ssh.as_ref()?;
    Some(device)
}

/// Open the stream backing a new session and hand back the channel its output
/// arrives on.
///
/// The outcome of trying to open a session's stream.
enum SessionStart {
    /// The stream is open; here is where its output will arrive.
    Opened(UnboundedReceiver<ShellEvent>),
    /// There's no server connection yet. Worth trying again shortly — the
    /// session comes up asynchronously after startup, so a panel expanded in
    /// the first second of a run lands here.
    NotReady,
    /// Nothing this layer can open a shell to, now or later.
    Unsupported,
}

/// A probe target gets an SSH session run by the device's owning server; anything
/// else gets a PTY on the agent itself. Both produce [`ShellEvent`], so the
/// terminal above this doesn't care which it got.
///
/// The connection is resolved here rather than inside the spawn helpers so that
/// "no connection yet" is reported instead of leaving the caller holding a
/// session whose stream was never opened.
fn open_session_stream(
    target: PanelTarget,
    rows: u16,
    cols: u16,
    outbound_rx: Receiver<ShellSessionStreamRequest>,
) -> SessionStart {
    #[cfg(feature = "probe")]
    if let Some(device) = ssh_probe_device(target) {
        // Probes are reachable only from servers, so this routes to the server
        // that owns the device, falling back to the primary.
        let Some(conn) = device
            .device
            .server
            .as_ref()
            .and_then(sandpolis_client::sync::connection_for)
            .or_else(sandpolis_client::sync::connection)
        else {
            return SessionStart::NotReady;
        };
        let (requester, output) = crate::ssh::SshSessionStreamRequester::channel();
        let initial = crate::ssh::SshSessionStreamRequest::Start {
            device_id: device.id.body(),
            rows: rows as u32,
            cols: cols as u32,
        };
        spawn_ssh_stream(conn, requester, initial, outbound_rx);
        return SessionStart::Opened(output);
    }

    // A sub-node that isn't an SSH probe borrows its gateway's instance id, so
    // falling through to the agent path would open a shell on the wrong host.
    let instance = match (target.instance, target.sub) {
        (Some(instance), None) => instance,
        _ => return SessionStart::Unsupported,
    };
    let Some(conn) = sandpolis_client::sync::connection() else {
        return SessionStart::NotReady;
    };

    let (requester, output) = ShellSessionStreamRequester::channel();
    let initial = ShellSessionStreamRequest::Start {
        path: PathBuf::from("/bin/sh"),
        environment: HashMap::new(),
        rows: rows as u32,
        cols: cols as u32,
    };
    spawn_shell_stream(conn, instance, requester, initial, outbound_rx);
    SessionStart::Opened(output)
}

/// Open an SSH session to a probe device over `conn` and forward outbound
/// requests over it until the channel closes.
///
/// `conn` is the owning server's connection (not a relayed one to an agent),
/// resolved by the caller; this translates the terminal's agent-shaped requests
/// into the SSH wire type on the way out.
#[cfg(feature = "probe")]
fn spawn_ssh_stream(
    conn: std::sync::Arc<sandpolis_instance::network::InstanceConnection>,
    requester: crate::ssh::SshSessionStreamRequester,
    initial: crate::ssh::SshSessionStreamRequest,
    mut outbound_rx: Receiver<ShellSessionStreamRequest>,
) {
    sandpolis_client::sync::spawn(async move {
        let (id, msg_tx) = match conn.open_stream(requester, initial).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "Failed to open SSH session");
                return;
            }
        };
        while let Some(req) = outbound_rx.recv().await {
            let req = match req {
                ShellSessionStreamRequest::Stdin { data } => {
                    crate::ssh::SshSessionStreamRequest::Stdin { data }
                }
                ShellSessionStreamRequest::Resize { rows, cols } => {
                    crate::ssh::SshSessionStreamRequest::Resize { rows, cols }
                }
                // Sent as the stream's initial request, never through here.
                ShellSessionStreamRequest::Start { .. } => continue,
            };
            let payload = match serde_cbor::to_vec(&req) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if msg_tx.send(StreamMessage::local(id, payload)).await.is_err() {
                break;
            }
        }
        conn.close_stream(id);
    });
}

/// Drain shell output into each session's terminal emulator.
fn drive_shell_streams(mut streams: ResMut<ShellStreams>) {
    use tokio::sync::mpsc::error::TryRecvError;

    for session in streams.sessions.values_mut() {
        let mut chunk = Vec::new();
        loop {
            match session.output.try_recv() {
                Ok(ShellEvent::Output(output)) => {
                    chunk.extend_from_slice(&output.stdout);
                    chunk.extend_from_slice(&output.stderr);
                }
                Ok(ShellEvent::Exited(_)) => {
                    session.ended = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    session.ended = true;
                    break;
                }
            }
        }
        if !chunk.is_empty() {
            session.processor.advance(&mut session.term, &chunk);
            session.dirty = true;
        }
    }
}

/// Recompute grid size from the laid-out container and resize the terminal.
fn handle_terminal_resize(
    grids: Query<(&TerminalGrid, &ComputedNode), Changed<ComputedNode>>,
    mut streams: ResMut<ShellStreams>,
) {
    for (grid, node) in grids.iter() {
        let Some(session) = streams.sessions.get_mut(&grid.target) else {
            continue;
        };
        let logical = node.size() * node.inverse_scale_factor();
        if logical.x < CELL_W || logical.y < CELL_H {
            continue;
        }
        let cols = ((logical.x / CELL_W).floor() as u16).max(2);
        let rows = ((logical.y / CELL_H).floor() as u16).max(2);
        if cols == session.cols && rows == session.rows {
            continue;
        }
        session.cols = cols;
        session.rows = rows;
        session.term.resize(TermDimensions {
            columns: cols as usize,
            screen_lines: rows as usize,
        });
        let _ = session.outbound.try_send(ShellSessionStreamRequest::Resize {
            rows: rows as u32,
            cols: cols as u32,
        });
        session.dirty = true;
    }
}

/// The shell layer's client plugin.
pub struct ShellClientPlugin;

impl Plugin for ShellClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ShellStreams>()
            .add_systems(Startup, load_terminal_font)
            .add_systems(
                Update,
                (
                    attach_terminal_grids,
                    start_pending_sessions,
                    keys::terminal_keyboard_input,
                    drive_shell_streams,
                    handle_terminal_resize,
                    update_session_summaries,
                    render::render_terminals,
                )
                    .chain(),
            )
            .register_layer_client(
                LayerClientInfo::new(
                    LayerName::from("Shell"),
                    "Remote shell access and command execution",
                )
                .with_panel(ShellPanel)
                .with_visible_instance_types(&[InstanceType::Agent])
                .with_node_icon(|_| "shell/terminal.svg")
                // SSH probes get a terminal just like agents do. Devices that
                // expose nothing this layer can drive stay hidden.
                .showing_probe_nodes_for(&["SSH"]),
            );
    }
}
