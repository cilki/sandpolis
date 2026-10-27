//! GUI components for the Desktop layer.
//!
//! Provides the desktop-viewer node panel and the layer's client plugin.
//!
//! The panel wires the full client-side stream pipeline. As soon as it expands
//! it opens a relayed stream to the agent with [`sandpolis_client::sync`]'s
//! session (`open_stream_to`): the server routes requests to the
//! target agent and responses back. [`DesktopStreamRequester`] decodes incoming
//! frames into RGBA8, a Bevy system drains them and uploads them to the display
//! node's texture, and pointer / keyboard input over that node is mapped into
//! [`DesktopStreamInputEvent`]s and forwarded over the stream's outbound channel.

use bevy::asset::RenderAssetUsages;
use bevy::image::Image;
use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::ui::RelativeCursorPosition;
use sandpolis_client::gui::ui::Activate;
use sandpolis_client::gui::ui::bind::bind_text;
use sandpolis_client::gui::ui::layer::{LayerClientInfo, RegisterLayerClient};
use sandpolis_client::gui::ui::node_panel::{NodePanel, PanelCtx, PanelTarget};
use sandpolis_client::gui::ui::theme::Role;
use sandpolis_client::gui::ui::widgets::{button, heading, row, text};
use sandpolis_instance::network::stream::StreamMessage;
use sandpolis_instance::{InstanceId, InstanceType, LayerName};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver, channel};

use crate::screenshot::{DesktopScreenshotRequest, DesktopScreenshotRequester, DesktopScreenshotResult};
use crate::session::{
    DesktopFrame, DesktopStreamColorMode, DesktopStreamCompressionMode, DesktopStreamEvent,
    DesktopStreamInputEvent, DesktopStreamPointerButton, DesktopStreamRequest,
    DesktopStreamRequester,
};

/// Active client-side desktop sessions, keyed by the node they belong to.
///
/// The key is a [`PanelTarget`] rather than a bare `InstanceId` because a
/// probe node borrows its gateway server's id; only the sub-node device id tells
/// a VNC probe apart from the server it orbits.
#[derive(Resource, Default)]
struct DesktopStreams {
    streams: HashMap<PanelTarget, StreamSession>,
    screenshots: HashMap<PanelTarget, ScreenshotSession>,
    /// Targets the auto-start should leave alone: either the user stopped the
    /// stream by hand, or there's nothing here to stream. Without this the
    /// auto-start would reopen (or retry forever) on the very next frame.
    suppressed: HashSet<PanelTarget>,
}

/// A live stream the GUI is rendering.
struct StreamSession {
    /// Decoded frames/state pushed by the requester (registered on the
    /// connection; the requester holds the sending half).
    events: UnboundedReceiver<DesktopStreamEvent>,
    /// Outbound requests (input, Stop). A background task forwards these over
    /// the stream, translating to the VNC wire type for probe streams.
    outbound: Sender<DesktopStreamRequest>,
    /// The display node showing this stream.
    view: Entity,
    /// Remote display dimensions, once known.
    size: Option<(u32, u32)>,
    /// Last pointer position sent, to avoid flooding identical moves.
    last_pointer: Option<(i32, i32)>,
}

/// A pending one-shot screenshot.
struct ScreenshotSession {
    result: UnboundedReceiver<DesktopScreenshotResult>,
    view: Entity,
}

/// The display node showing a desktop stream/screenshot for `target`.
#[derive(Component)]
struct DesktopStreamView {
    target: PanelTarget,
}

/// A status label reflecting the stream state for `target`.
#[derive(Component)]
struct DesktopStatusText {
    target: PanelTarget,
}

/// The desktop layer's node panel (remote desktop viewer).
pub struct DesktopPanel;

impl NodePanel for DesktopPanel {
    fn build_summary(&self, ctx: &mut PanelCtx) {
        let target = ctx.target;
        let theme = ctx.theme;
        ctx.children(|p| {
            p.spawn((
                DesktopStatusText { target },
                text(
                    theme,
                    "Stream inactive",
                    theme.metrics.font_sm,
                    Role::TextMuted,
                ),
            ));
        });
    }

    /// Expanding the panel starts the stream: the viewer spawned here carries no
    /// "Start" button, because wanting to look at the desktop is what expanding
    /// the panel means. [`start_desktop_streams`] picks the view up as it lands.
    fn build_detail(&self, ctx: &mut PanelCtx) {
        let target = ctx.target;
        let theme = ctx.theme;

        ctx.children(|p| {
            p.spawn(heading(theme, "Desktop Stream"));

            // Live stream display: an `ImageNode` (transparent until frames
            // arrive, so the dark background shows through) over a dark
            // background. `RelativeCursorPosition` maps the pointer into the
            // remote display's coordinate space for input forwarding.
            p.spawn((
                DesktopStreamView { target },
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(280.0),
                    ..default()
                },
                BackgroundColor(Color::srgb_u8(30, 30, 30)),
                ImageNode {
                    color: Color::NONE,
                    ..default()
                },
                Interaction::default(),
                RelativeCursorPosition::default(),
            ));

            // Controls.
            p.spawn(row(theme.metrics.space_sm))
                .with_children(|controls| {
                    controls.spawn(button(theme, "Stop")).observe(
                        move |_: On<Activate>,
                              mut streams: ResMut<DesktopStreams>,
                              mut nodes: Query<&mut ImageNode>| {
                            stop_stream(target, &mut streams, &mut nodes);
                            // Remember the stop, so the auto-start doesn't
                            // immediately undo it.
                            streams.suppressed.insert(target);
                        },
                    );

                    // Screenshots are a one-shot agent responder; a probe has no
                    // equivalent, so the button only exists for agents.
                    if let (Some(instance), None) = (target.instance, target.sub) {
                        controls.spawn(button(theme, "Screenshot")).observe(
                            move |_: On<Activate>,
                                  mut streams: ResMut<DesktopStreams>,
                                  views: Query<(Entity, &DesktopStreamView)>| {
                                let Some((view, _)) =
                                    views.iter().find(|(_, v)| v.target == target)
                                else {
                                    return;
                                };
                                let (requester, result) = DesktopScreenshotRequester::channel();
                                // One-shot: open a relayed stream to the agent with
                                // the screenshot request; the response returns via
                                // `result`. A session is only recorded if the
                                // request actually went out, so a press with no
                                // connection doesn't leave one waiting forever.
                                if spawn_screenshot(
                                    instance,
                                    requester,
                                    DesktopScreenshotRequest {
                                        desktop_uuid: String::new(),
                                    },
                                ) {
                                    streams
                                        .screenshots
                                        .insert(target, ScreenshotSession { result, view });
                                    info!("Screenshot requested for {}", instance);
                                }
                            },
                        );
                    }
                });

            // Stream status line.
            p.spawn((
                DesktopStatusText { target },
                text(
                    theme,
                    "Stream inactive",
                    theme.metrics.font_sm,
                    Role::TextMuted,
                ),
            ));

            // Node information. A probe isn't an instance, so it has no instance
            // metadata to report — its registration is all there is to say.
            p.spawn((
                text(theme, "", theme.metrics.font_md, Role::Text),
                bind_text(move || describe_target(target)),
            ));
        });
    }

    /// A desktop stream is pure bandwidth with no state worth keeping, so a
    /// collapsed panel stops it rather than leaving it running unwatched.
    fn on_collapse(&self, commands: &mut Commands, target: PanelTarget) {
        commands.queue(move |world: &mut World| {
            let Some(mut streams) = world.get_resource_mut::<DesktopStreams>() else {
                return;
            };
            // A manual stop only holds for as long as the panel is open; the next
            // time it expands, the user asked to watch again.
            streams.suppressed.remove(&target);
            streams.screenshots.remove(&target);
            if let Some(session) = streams.streams.remove(&target) {
                let _ = session.outbound.try_send(DesktopStreamRequest::Stop);
            }
        });
    }
}

/// Stop `target`'s stream and blank its view.
fn stop_stream(
    target: PanelTarget,
    streams: &mut DesktopStreams,
    nodes: &mut Query<&mut ImageNode>,
) {
    let Some(session) = streams.streams.remove(&target) else {
        return;
    };
    let _ = session.outbound.try_send(DesktopStreamRequest::Stop);
    if let Ok(mut node) = nodes.get_mut(session.view) {
        node.color = Color::NONE;
    }
    info!("Desktop stream stopped for {:?}", target);
}

/// Open a stream for every viewer that doesn't have one yet.
///
/// Driven off the view entity rather than a button, so a panel that expands —
/// however it was expanded — starts streaming on its own. Checked every frame
/// rather than on `Added`, which is also what makes "Restart" work: it drops the
/// session and this picks the view back up on the next pass.
fn start_desktop_streams(
    mut streams: ResMut<DesktopStreams>,
    views: Query<(Entity, &DesktopStreamView)>,
) {
    for (view, stream_view) in &views {
        let target = stream_view.target;
        if streams.streams.contains_key(&target) || streams.suppressed.contains(&target) {
            continue;
        }
        let (outbound, outbound_rx) = channel(64);
        let events = match open_stream(target, outbound_rx) {
            StreamStart::Opened(events) => events,
            // Leave the target alone and come back next frame. Recording a
            // session here would be worse than doing nothing: it would look
            // open, produce no frames, and block the retry that would have
            // worked once the session came up.
            StreamStart::NotReady => continue,
            StreamStart::Unsupported => {
                streams.suppressed.insert(target);
                continue;
            }
        };
        streams.streams.insert(
            target,
            StreamSession {
                events,
                outbound,
                view,
                size: None,
                last_pointer: None,
            },
        );
        info!("Desktop stream opened for {:?}", target);
    }
}

/// One line describing what the controller is pointed at.
fn describe_target(target: PanelTarget) -> String {
    #[cfg(feature = "probe")]
    if let Some(device) = probe_device(target) {
        return format!("{} — {}", device.display_name(), device.device.ip);
    }
    let Some(instance) = target.instance else {
        return "Not an instance".into();
    };
    match sandpolis_client::gui::queries::query_instance_metadata(instance) {
        Ok(m) => {
            let host = m.hostname.unwrap_or_else(|| "unknown".into());
            format!("{} — OS: {}", host, m.os_type)
        }
        Err(_) => "OS: unknown".into(),
    }
}

/// The probe device behind `target`, if there is one.
///
/// Reads the probe subsystem's device registry directly rather than going through
/// `sandpolis_probe::client::gui`, whose helpers are behind that crate's `client`
/// feature — depending on them would drag its whole GUI stack in here.
#[cfg(feature = "probe")]
fn probe_device(target: PanelTarget) -> Option<sandpolis_probe::RegisteredDevice> {
    let device_id = target.sub?;
    let device = sandpolis_probe::REGISTERED_DEVICES
        .read()
        .ok()?
        .iter()
        .find(|device| device.id.body() == device_id)?
        .clone();
    Some(device)
}

/// The outcome of trying to open a viewer's stream.
enum StreamStart {
    /// The stream is open; here is where its events will arrive.
    Opened(UnboundedReceiver<DesktopStreamEvent>),
    /// There's no server connection yet. Worth trying again shortly — the
    /// session comes up asynchronously after startup, so a panel expanded in
    /// the first second of a run lands here.
    NotReady,
    /// Nothing this layer can stream, now or later.
    Unsupported,
}

/// Open the stream backing a new session and hand back the channel its events
/// arrive on.
///
/// A probe target gets a VNC session run by the device's owning server; anything
/// else gets a capture stream relayed to the agent. Both produce
/// [`DesktopStreamEvent`], so the viewer above this doesn't care which it got.
///
/// The connection is resolved here rather than inside the spawn helpers so that
/// "no connection yet" is reported instead of leaving the caller holding a
/// session whose stream was never opened.
fn open_stream(target: PanelTarget, outbound_rx: Receiver<DesktopStreamRequest>) -> StreamStart {
    #[cfg(feature = "probe")]
    if let Some(device) = probe_device(target)
        && device.device.vnc.is_some()
    {
        // Probes are reachable only from servers, so this routes to the server
        // that owns the device, falling back to the primary.
        let Some(conn) = device
            .device
            .server
            .as_ref()
            .and_then(sandpolis_client::sync::connection_for)
            .or_else(sandpolis_client::sync::connection)
        else {
            return StreamStart::NotReady;
        };
        let (requester, events) = crate::vnc::VncStreamRequester::channel();
        let initial = crate::vnc::VncStreamRequest::Start {
            device_id: device.id.body(),
        };
        spawn_vnc_stream(conn, requester, initial, outbound_rx);
        return StreamStart::Opened(events);
    }

    // RDP probes stream the same way, run by the device's owning server. VNC is
    // preferred above, so this is reached for RDP-only devices.
    #[cfg(feature = "probe")]
    if let Some(device) = probe_device(target)
        && device.device.rdp.is_some()
    {
        let Some(conn) = device
            .device
            .server
            .as_ref()
            .and_then(sandpolis_client::sync::connection_for)
            .or_else(sandpolis_client::sync::connection)
        else {
            return StreamStart::NotReady;
        };
        let (requester, events) = crate::rdp::RdpStreamRequester::channel();
        let initial = crate::rdp::RdpStreamRequest::Start {
            device_id: device.id.body(),
        };
        spawn_rdp_stream(conn, requester, initial, outbound_rx);
        return StreamStart::Opened(events);
    }

    // A sub-node that isn't a VNC probe borrows its gateway's instance id, so
    // falling through to the agent path would capture the wrong host's screen.
    let instance = match (target.instance, target.sub) {
        (Some(instance), None) => instance,
        _ => return StreamStart::Unsupported,
    };
    let Some(conn) = sandpolis_client::sync::connection() else {
        return StreamStart::NotReady;
    };

    let (requester, events) = DesktopStreamRequester::channel();
    let initial = DesktopStreamRequest::Start {
        desktop_uuid: String::new(),
        color_mode: DesktopStreamColorMode::Rgb888,
        compression_mode: DesktopStreamCompressionMode::Zstd,
    };
    spawn_stream(conn, instance, requester, initial, outbound_rx);
    StreamStart::Opened(events)
}

/// Open a VNC stream against a probe device over `conn` and forward outbound
/// requests over it until the channel closes.
///
/// `conn` is the owning server's connection (not a relayed one to an agent),
/// resolved by the caller; this translates the viewer's agent-shaped requests
/// into the VNC wire type on the way out.
#[cfg(feature = "probe")]
fn spawn_vnc_stream(
    conn: std::sync::Arc<sandpolis_instance::network::InstanceConnection>,
    requester: crate::vnc::VncStreamRequester,
    initial: crate::vnc::VncStreamRequest,
    mut outbound_rx: Receiver<DesktopStreamRequest>,
) {
    sandpolis_client::sync::spawn(async move {
        let (id, msg_tx) = match conn.open_stream(requester, initial).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "Failed to open VNC stream");
                return;
            }
        };
        while let Some(req) = outbound_rx.recv().await {
            let req = match req {
                DesktopStreamRequest::Input(event) => crate::vnc::VncStreamRequest::Input(event),
                DesktopStreamRequest::Stop => crate::vnc::VncStreamRequest::Stop,
                // Sent as the stream's initial request, never through here.
                DesktopStreamRequest::Start { .. } => continue,
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

/// Open an RDP stream against a probe device over `conn` and forward outbound
/// requests over it until the channel closes. The shape mirrors
/// [`spawn_vnc_stream`]; only the wire request type differs.
#[cfg(feature = "probe")]
fn spawn_rdp_stream(
    conn: std::sync::Arc<sandpolis_instance::network::InstanceConnection>,
    requester: crate::rdp::RdpStreamRequester,
    initial: crate::rdp::RdpStreamRequest,
    mut outbound_rx: Receiver<DesktopStreamRequest>,
) {
    sandpolis_client::sync::spawn(async move {
        let (id, msg_tx) = match conn.open_stream(requester, initial).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "Failed to open RDP stream");
                return;
            }
        };
        while let Some(req) = outbound_rx.recv().await {
            let req = match req {
                DesktopStreamRequest::Input(event) => crate::rdp::RdpStreamRequest::Input(event),
                DesktopStreamRequest::Stop => crate::rdp::RdpStreamRequest::Stop,
                // Sent as the stream's initial request, never through here.
                DesktopStreamRequest::Start { .. } => continue,
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

/// Open a relayed desktop stream to `instance` over `conn` and forward outbound
/// requests (input, Stop) over it until the channel closes.
fn spawn_stream(
    conn: std::sync::Arc<sandpolis_instance::network::InstanceConnection>,
    instance: InstanceId,
    requester: DesktopStreamRequester,
    initial: DesktopStreamRequest,
    mut outbound_rx: Receiver<DesktopStreamRequest>,
) {
    sandpolis_client::sync::spawn(async move {
        let (id, msg_tx) = match conn.open_stream_to(instance, requester, initial).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "Failed to open desktop stream");
                return;
            }
        };
        while let Some(req) = outbound_rx.recv().await {
            let payload = match serde_cbor::to_vec(&req) {
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
    });
}

/// Open a one-shot relayed screenshot stream to `instance`. The response returns
/// over the requester's channel. Returns whether the request was sent at all.
fn spawn_screenshot(
    instance: InstanceId,
    requester: DesktopScreenshotRequester,
    initial: DesktopScreenshotRequest,
) -> bool {
    let Some(conn) = sandpolis_client::sync::connection() else {
        warn!("No server connection; cannot request screenshot");
        return false;
    };
    sandpolis_client::sync::spawn(async move {
        if let Err(e) = conn.open_stream_to(instance, requester, initial).await {
            warn!(error = %e, "Failed to request screenshot");
        }
    });
    true
}

/// Build an RGBA8 [`Image`] for a decoded frame.
fn image_from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Image {
    Image::new(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    )
}

/// Drain decoded stream frames and upload the latest to the display texture.
fn drive_desktop_streams(
    mut streams: ResMut<DesktopStreams>,
    mut images: ResMut<Assets<Image>>,
    mut nodes: Query<&mut ImageNode>,
) {
    for session in streams.streams.values_mut() {
        // Coalesce to the most recent frame so we only upload one texture/tick.
        let mut latest: Option<DesktopFrame> = None;
        while let Ok(event) = session.events.try_recv() {
            match event {
                DesktopStreamEvent::Started { width, height } => {
                    session.size = Some((width, height));
                }
                DesktopStreamEvent::Frame(frame) => latest = Some(frame),
                DesktopStreamEvent::Stopped => {}
            }
        }

        if let Some(frame) = latest {
            if frame.width == 0 || frame.height == 0 {
                continue;
            }
            session.size = Some((frame.width, frame.height));
            let handle = images.add(image_from_rgba(frame.width, frame.height, frame.rgba));
            if let Ok(mut node) = nodes.get_mut(session.view) {
                node.image = handle;
                node.color = Color::WHITE;
            }
        }
    }
}

/// Drain pending screenshots and display the returned image.
fn drive_desktop_screenshots(
    mut streams: ResMut<DesktopStreams>,
    mut images: ResMut<Assets<Image>>,
    mut nodes: Query<&mut ImageNode>,
) {
    let mut finished = Vec::new();
    for (target, session) in streams.screenshots.iter_mut() {
        let target = *target;
        while let Ok(result) = session.result.try_recv() {
            match result {
                DesktopScreenshotResult::Ok(png) => match crate::screenshot::decode_png(&png) {
                    Ok(frame) if frame.width > 0 && frame.height > 0 => {
                        let handle =
                            images.add(image_from_rgba(frame.width, frame.height, frame.rgba));
                        if let Ok(mut node) = nodes.get_mut(session.view) {
                            node.image = handle;
                            node.color = Color::WHITE;
                        }
                        info!("Screenshot received for {:?}", target);
                    }
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "Failed to decode screenshot for {:?}", target),
                },
                DesktopScreenshotResult::Failed => {
                    warn!("Screenshot failed for {:?}", target);
                }
            }
            finished.push(target);
        }
    }
    for target in finished {
        streams.screenshots.remove(&target);
    }
}

/// Reflect each instance's stream state in its status label.
fn update_desktop_status(
    streams: Res<DesktopStreams>,
    mut labels: Query<(&DesktopStatusText, &mut Text)>,
) {
    // The session comes up asynchronously after startup, so a panel expanded
    // early has nothing to open a stream over yet. Saying so beats "inactive",
    // which reads as "and it's staying that way".
    let connected = sandpolis_client::sync::connection().is_some();

    for (status, mut label) in &mut labels {
        let value = match streams.streams.get(&status.target) {
            Some(session) => match session.size {
                Some((w, h)) => format!("Streaming {}×{}", w, h),
                None => "Connecting…".to_string(),
            },
            None if streams.suppressed.contains(&status.target) => "Stream stopped".to_string(),
            None if !connected => "Waiting for server…".to_string(),
            None => "Stream inactive".to_string(),
        };
        if label.0 != value {
            label.0 = value;
        }
    }
}

/// Map a just-pressed/just-released mouse button to a stream pointer button.
fn pointer_button(
    mouse: &ButtonInput<MouseButton>,
    pressed: bool,
) -> Option<DesktopStreamPointerButton> {
    let hit = |b: MouseButton| {
        if pressed {
            mouse.just_pressed(b)
        } else {
            mouse.just_released(b)
        }
    };
    if hit(MouseButton::Left) {
        Some(DesktopStreamPointerButton::Primary)
    } else if hit(MouseButton::Right) {
        Some(DesktopStreamPointerButton::Secondary)
    } else if hit(MouseButton::Middle) {
        Some(DesktopStreamPointerButton::Middle)
    } else {
        None
    }
}

/// Extract a typed character from a logical key, if any.
fn logical_char(key: &Key) -> Option<char> {
    match key {
        Key::Character(s) => s.chars().next(),
        Key::Space => Some(' '),
        _ => None,
    }
}

/// An input event with no fields set.
fn empty_input() -> DesktopStreamInputEvent {
    DesktopStreamInputEvent {
        key_pressed: None,
        key_released: None,
        key_typed: None,
        pointer_pressed: None,
        pointer_released: None,
        pointer_x: None,
        pointer_y: None,
        clipboard: None,
    }
}

/// Forward pointer and keyboard input over the hovered active stream.
fn forward_desktop_input(
    mut streams: ResMut<DesktopStreams>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut keys: MessageReader<KeyboardInput>,
    views: Query<(&DesktopStreamView, &RelativeCursorPosition)>,
) {
    // Keyboard events are global; route them to whichever view is hovered.
    let key_events: Vec<(Option<char>, ButtonState)> = keys
        .read()
        .map(|ev| (logical_char(&ev.logical_key), ev.state))
        .collect();

    for (view, rel) in &views {
        let Some(session) = streams.streams.get_mut(&view.target) else {
            continue;
        };
        let Some((w, h)) = session.size else {
            continue;
        };
        let Some(norm) = rel.normalized else {
            // Pointer left the view; reset so re-entry re-sends a position.
            session.last_pointer = None;
            continue;
        };

        let x = (norm.x.clamp(0.0, 1.0) * w as f32).round() as i32;
        let y = (norm.y.clamp(0.0, 1.0) * h as f32).round() as i32;
        let pressed = pointer_button(&mouse, true);
        let released = pointer_button(&mouse, false);
        let moved = session.last_pointer != Some((x, y));

        if moved || pressed.is_some() || released.is_some() {
            let mut event = empty_input();
            event.pointer_x = Some(x);
            event.pointer_y = Some(y);
            event.pointer_pressed = pressed;
            event.pointer_released = released;
            let _ = session.outbound.try_send(DesktopStreamRequest::Input(event));
            session.last_pointer = Some((x, y));
        }

        for (character, state) in &key_events {
            let Some(c) = character else {
                continue;
            };
            let mut event = empty_input();
            match state {
                ButtonState::Pressed => {
                    event.key_pressed = Some(*c);
                    event.key_typed = Some(*c);
                }
                ButtonState::Released => {
                    event.key_released = Some(*c);
                }
            }
            let _ = session.outbound.try_send(DesktopStreamRequest::Input(event));
        }
    }
}

/// The desktop layer's client plugin.
pub struct DesktopClientPlugin;

impl Plugin for DesktopClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DesktopStreams>()
            .add_systems(
                Update,
                (
                    start_desktop_streams,
                    drive_desktop_streams,
                    drive_desktop_screenshots,
                    update_desktop_status,
                    forward_desktop_input,
                )
                    .chain(),
            )
            .register_layer_client(
                LayerClientInfo::new(
                    LayerName::from("Desktop"),
                    "Remote desktop viewing and control",
                )
                .with_panel(DesktopPanel)
                .with_visible_instance_types(&[InstanceType::Agent])
                // TODO: pick per desktop environment once agents report one
                .with_node_icon(|_| "desktop/generic.svg")
                // VNC probes stream just like agents do; RDP ones are shown but
                // open a placeholder until there's a backend for them.
                .showing_probe_nodes_for(&["VNC", "RDP"]),
            );
    }
}
