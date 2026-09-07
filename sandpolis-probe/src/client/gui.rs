use bevy::asset::RenderAssetUsages;
use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::image::Image;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::text::EditableText;
use bevy_rapier2d::dynamics::{Damping, ExternalForce, RigidBody, Velocity};
use bevy_rapier2d::geometry::{Collider, Restitution};
use bevy_svg::prelude::{Origin, Svg2d};
use sandpolis_client::gui::node::{
    NeedsCredentials, NeedsScaling, NodeEntity, NodeHitbox, NodeIdentity, SubNode,
};
use sandpolis_client::gui::ui::Activate;
use sandpolis_client::gui::ui::bind::bind_text;
use sandpolis_client::gui::ui::layer::{LayerClientInfo, LayerRegistry, RegisterLayerClient};
use sandpolis_client::gui::ui::node_panel::{NodePanel, PanelCtx, PanelTarget};
use sandpolis_client::gui::ui::panel::modal_scrim;
use sandpolis_client::gui::ui::text_input::text_input;
use sandpolis_client::gui::ui::theme::{Role, Theme, ThemedBg, ThemedBorder, ThemedText};
use sandpolis_client::gui::ui::widgets::{button, heading, muted, row, text};
use sandpolis_instance::network::InstanceConnection;
use sandpolis_instance::network::stream::{StreamId, StreamMessage};
use sandpolis_instance::{InstanceId, LayerName};
use sandpolis_server::ServerUrl;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender, channel};

use crate::config::{DeviceConfig, RtspProbeConfig, WolProbeConfig};
use crate::rtsp::{
    RtspFrameRgba, RtspSessionStreamRequest, RtspSessionStreamRequester, RtspStreamEvent,
    RtspTransport,
};
use crate::{ProbeType, RegisteredDevice};

/// Marker component for device nodes (smaller nodes attached to gateways).
#[derive(Component)]
pub struct ProbeNode {
    // NOTE: fields are `pub` because `super::link` renders these nodes' links.
    /// The device ID.
    pub device_id: u64,
    /// The protocol used for the node's icon.
    pub icon: ProbeType,
    /// The gateway instance this device is attached to.
    pub gateway: InstanceId,
}

/// The visual diameter for device nodes (smaller than regular nodes).
pub const PROBE_NODE_VISUAL_DIAMETER: f32 = 50.0;

/// Bundle for spawning device nodes.
#[derive(Bundle)]
pub struct ProbeNodeBundle {
    pub probe_node: ProbeNode,
    pub node_entity: NodeEntity,
    /// Carries the device id, which is what lets the panel host tell a probe
    /// apart from the gateway server whose `InstanceId` it borrows.
    pub sub_node: SubNode,
    /// The device's display name, shown at the top of its node panel.
    pub identity: NodeIdentity,
    /// Opts these nodes into the generic selection and drag systems, which key
    /// on the hitbox rather than on `NodeEntity`.
    pub hitbox: NodeHitbox,
    pub collider: Collider,
    pub rigid_body: RigidBody,
    pub velocity: Velocity,
    pub external_force: ExternalForce,
    pub damping: Damping,
    pub restitution: Restitution,
    pub transform: Transform,
    pub visibility: Visibility,
}

/// Spawn a device node in the world view.
pub fn spawn_probe_node(
    asset_server: &AssetServer,
    commands: &mut Commands,
    device: &RegisteredDevice,
    parent_position: Vec3,
    visible: bool,
) {
    // Attach the probe to its owning server's node. Resolve the server URL to a
    // connected instance id; fall back to the recorded gateway if that server
    // isn't connected yet.
    let gateway = device
        .device
        .server
        .as_ref()
        .and_then(sandpolis_client::sync::instance_for)
        .unwrap_or(device.gateway);
    let instance_id = gateway;
    let icon = device.device.primary().unwrap_or(ProbeType::Http);

    // Position device nodes in an orbit around the parent, using the device id for
    // consistent golden-angle placement.
    let angle = (device.id.body() as f32 * 0.618_034 * std::f32::consts::TAU) % std::f32::consts::TAU;
    let orbit_radius = 120.0;
    let x = parent_position.x + orbit_radius * angle.cos();
    let y = parent_position.y + orbit_radius * angle.sin();

    let node_entity = commands
        .spawn(ProbeNodeBundle {
            probe_node: ProbeNode {
                device_id: device.id.body(),
                icon,
                gateway,
            },
            node_entity: NodeEntity { instance_id },
            sub_node: SubNode(device.id.body()),
            identity: NodeIdentity(device.display_name()),
            hitbox: NodeHitbox { radius: 25.0 },
            collider: Collider::ball(25.0),
            rigid_body: RigidBody::Dynamic,
            velocity: Velocity::zero(),
            external_force: ExternalForce::default(),
            damping: Damping {
                linear_damping: 0.0,
                angular_damping: 1.0,
            },
            restitution: Restitution::coefficient(0.7),
            transform: Transform::from_xyz(x, y, 0.0),
            visibility: if visible {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            },
        })
        .id();

    // Deliberately not tagged with NodeSvg so the layer visual systems don't
    // replace the icon or rescale it to regular node size.
    spawn_probe_icon(
        commands,
        asset_server,
        node_entity,
        ProbeNodeIcon::Svg(icon),
    );
}

/// Attach an icon child of the given kind to a probe node. The SVG variant keeps
/// the vector icon (scaled by `scale_probe_node_svgs`); the thumbnail variant
/// shows a captured stream frame sized to the node.
fn spawn_probe_icon(
    commands: &mut Commands,
    asset_server: &AssetServer,
    node: Entity,
    icon: ProbeNodeIcon,
) {
    let child = match &icon {
        ProbeNodeIcon::Svg(probe_type) => commands
            .spawn((
                Svg2d(asset_server.load(get_probe_svg(*probe_type))),
                Origin::Center,
                Transform::default(),
                NeedsScaling,
                ProbeNodeSvg,
            ))
            .id(),
        // Sprites are center-anchored and `custom_size` does the scaling, so this
        // needs none of the manual recentring the SVG path does.
        ProbeNodeIcon::Thumbnail(handle) => commands
            .spawn((
                Sprite {
                    image: handle.clone(),
                    custom_size: Some(Vec2::splat(PROBE_NODE_VISUAL_DIAMETER)),
                    ..default()
                },
                Transform::default(),
            ))
            .id(),
    };
    commands.entity(child).insert(icon);
    commands.entity(node).add_child(child);
}

/// Marker component for device node SVGs (for scaling to smaller size).
#[derive(Component)]
pub struct ProbeNodeSvg;

/// System to scale device SVGs to a smaller uniform size once loaded.
pub fn scale_probe_node_svgs(
    mut commands: Commands,
    svg_assets: Res<Assets<bevy_svg::prelude::Svg>>,
    mut nodes_needing_scale: Query<
        (Entity, &Svg2d, &mut Transform),
        (With<NeedsScaling>, With<ProbeNodeSvg>),
    >,
) {
    for (entity, svg_handle, mut transform) in nodes_needing_scale.iter_mut() {
        if let Some(svg) = svg_assets.get(&svg_handle.0) {
            let svg_size = svg.size;
            let max_dimension = svg_size.x.max(svg_size.y);

            if max_dimension > 0.0 {
                let scale = PROBE_NODE_VISUAL_DIAMETER / max_dimension;
                transform.scale = Vec3::splat(scale);

                let scaled_size = svg_size * scale;
                transform.translation.x = -scaled_size.x / 2.0;
                transform.translation.y = scaled_size.y / 2.0;

                commands.entity(entity).remove::<NeedsScaling>();
            }
        }
    }
}

/// Spawn/despawn device nodes to match the registered device list.
pub fn update_probe_nodes(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    current_layer: Res<sandpolis_client::gui::input::CurrentLayer>,
    registry: Res<LayerRegistry>,
    existing_probes: Query<(Entity, &ProbeNode)>,
    parent_nodes: Query<(&Transform, &NodeEntity), Without<ProbeNode>>,
) {
    let gateway_positions: HashMap<InstanceId, Vec3> = parent_nodes
        .iter()
        .map(|(transform, node)| (node.instance_id, transform.translation))
        .collect();

    let mut all_devices = Vec::new();
    for (_, node) in parent_nodes.iter() {
        all_devices.extend(query_devices(node.instance_id));
    }

    let existing_ids: std::collections::HashSet<u64> = existing_probes
        .iter()
        .map(|(_, probe)| probe.device_id)
        .collect();

    for device in &all_devices {
        if !existing_ids.contains(&device.id.body())
            && let Some(&parent_pos) = gateway_positions.get(&device.gateway)
        {
            let visible = device_visible(&registry, &current_layer, &device.device);
            spawn_probe_node(&asset_server, &mut commands, device, parent_pos, visible);
        }
    }

    let db_ids: std::collections::HashSet<u64> = all_devices.iter().map(|d| d.id.body()).collect();
    for (entity, probe) in existing_probes.iter() {
        if !db_ids.contains(&probe.device_id) {
            commands.entity(entity).despawn();
        }
    }
}

/// Toggle the [`NeedsCredentials`] marker on each device node to match whether
/// its device still lacks credentials it needs. The generic
/// `update_credential_visuals` effect renders the marker.
pub fn update_credential_markers(
    mut commands: Commands,
    probe_nodes: Query<(Entity, &ProbeNode, Has<NeedsCredentials>)>,
) {
    let devices = crate::REGISTERED_DEVICES.read().unwrap();
    for (entity, probe, marked) in probe_nodes.iter() {
        let needs = devices
            .iter()
            .find(|d| d.id.body() == probe.device_id)
            .is_some_and(|d| d.device.needs_credentials());

        if needs && !marked {
            commands.entity(entity).insert(NeedsCredentials);
        } else if !needs && marked {
            commands.entity(entity).remove::<NeedsCredentials>();
        }
    }
}

/// Keep device nodes orbiting near their parent gateways.
pub fn apply_probe_spring_forces(
    mut probe_query: Query<(&Transform, &mut ExternalForce, &ProbeNode)>,
    parent_query: Query<(&Transform, &NodeEntity), Without<ProbeNode>>,
) {
    let gateway_positions: HashMap<InstanceId, Vec3> = parent_query
        .iter()
        .map(|(transform, node)| (node.instance_id, transform.translation))
        .collect();

    let spring_strength = 0.05;
    let rest_length = 120.0;
    let max_force = 500.0;

    for (transform, mut force, probe) in probe_query.iter_mut() {
        if let Some(&gateway_pos) = gateway_positions.get(&probe.gateway) {
            let delta = gateway_pos - transform.translation;
            let distance = delta.length().max(1.0);
            let displacement = distance - rest_length;
            let force_magnitude = (spring_strength * displacement).clamp(-max_force, max_force);
            let force_direction = delta.normalize_or_zero();
            force.force += (force_direction * force_magnitude).truncate();
        }
    }
}

/// Update device node visibility based on the current layer.
pub fn update_probe_node_visibility(
    current_layer: Res<sandpolis_client::gui::input::CurrentLayer>,
    registry: Res<LayerRegistry>,
    mut probe_query: Query<(&ProbeNode, &mut Visibility)>,
) {
    if !current_layer.is_changed() {
        return;
    }
    for (probe, mut visibility) in probe_query.iter_mut() {
        let visible = device_by_id(probe.device_id)
            .is_some_and(|device| device_visible(&registry, &current_layer, &device.device));
        *visibility = if visible {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

/// Whether `device`'s node should be visible while `layer` is active.
///
/// Layers other than Probe show a filtered subset: the Shell layer only wants
/// devices it can open a terminal to, the Desktop layer only ones it can stream.
/// The allowlist is matched against [`ProbeType::display_name`] because
/// `sandpolis-client`, where it's declared, can't reference [`ProbeType`].
fn device_visible(registry: &LayerRegistry, layer: &LayerName, device: &DeviceConfig) -> bool {
    if !registry.show_probe_nodes(layer) {
        return false;
    }
    let allowed = registry.probe_protocols(layer);
    allowed.is_empty()
        || device
            .protocols()
            .iter()
            .any(|proto| allowed.contains(&proto.display_name()))
}

/// Query registered devices for a gateway.
pub fn query_devices(gateway: InstanceId) -> Vec<RegisteredDevice> {
    crate::REGISTERED_DEVICES
        .read()
        .unwrap()
        .iter()
        .filter(|device| device.gateway == gateway)
        .cloned()
        .collect()
}

/// Look up a single device by its sub-node key (the id's body).
pub(crate) fn device_by_id(id: u64) -> Option<RegisteredDevice> {
    crate::REGISTERED_DEVICES
        .read()
        .unwrap()
        .iter()
        .find(|d| d.id.body() == id)
        .cloned()
}

/// Ask the owning server to send a Wake-on-LAN magic packet. Probes are accessed
/// only from servers, so the packet is sent server-side.
fn send_wake(wol: &WolProbeConfig, server: Option<&ServerUrl>) {
    let mac_address = match wol.mac_address.parse::<macaddr::MacAddr6>() {
        Ok(mac) => mac,
        Err(e) => {
            warn!("Invalid MAC address: {}", e);
            return;
        }
    };
    let request = crate::wol::WolPacketRequest {
        mac_address,
        broadcast_address: wol.broadcast_address.clone(),
        port: wol.port,
    };
    let conn = server
        .and_then(sandpolis_client::sync::connection_for)
        .or_else(sandpolis_client::sync::connection);
    if let Some(conn) = conn {
        crate::wol::send_wake(conn, request);
    } else {
        warn!("No server connection; cannot send Wake-on-LAN packet");
    }
}

/// Get the SVG asset path for a probe type.
pub fn get_probe_svg(probe_type: ProbeType) -> &'static str {
    match probe_type {
        ProbeType::Rdp => "probe/rdp.svg",
        ProbeType::Ssh => "probe/ssh.svg",
        ProbeType::Ups => "probe/ups.svg",
        ProbeType::Vnc => "probe/vnc.svg",
        ProbeType::Wol => "probe/wol.svg",
        ProbeType::Http => "probe/http.svg",
        ProbeType::Ipmi => "probe/ipmi.svg",
        ProbeType::Rtsp => "probe/rtsp.svg",
        ProbeType::Snmp => "probe/snmp.svg",
        ProbeType::Onvif => "probe/onvif.svg",
        ProbeType::Docker => "probe/docker.svg",
        ProbeType::Libvirt => "probe/libvirt.svg",
        ProbeType::Nfs => "probe/nfs.svg",
        ProbeType::Smb => "probe/smb.svg",
    }
}

/// Build the RTSP URL from the device's IP and the probe's port/path.
///
/// Credentials are deliberately *not* embedded here — they travel in the start
/// request and are applied as digest auth server-side, because `retina` refuses
/// outright to open a URL that carries userinfo.
fn build_rtsp_url(ip: std::net::IpAddr, cfg: &RtspProbeConfig) -> String {
    let port = cfg.port.unwrap_or(554);
    let path = cfg.path.trim_start_matches('/');
    format!("rtsp://{}:{}/{}", ip, port, path)
}

fn rtsp_transport(cfg: &RtspProbeConfig) -> RtspTransport {
    match cfg.transport.as_deref().map(|s| s.to_ascii_lowercase()) {
        Some(ref s) if s == "udp" => RtspTransport::Udp,
        _ => RtspTransport::Tcp,
    }
}

/// Active client-side RTSP sessions, keyed by device id.
#[derive(Resource, Default)]
pub(crate) struct ProbeStreams {
    pub(crate) streams: HashMap<u64, StreamSession>,
    /// Why the last stream for a device ended, shown once its session is gone.
    /// Also carries the reason a start never got off the ground at all.
    last_status: HashMap<u64, StreamStatus>,
    /// Devices whose stream the user stopped by hand. Without this the
    /// auto-start would reopen the stream on the very next frame.
    stopped: HashSet<u64>,
}

/// What a device's stream is currently doing, mirrored into its status label.
#[derive(Clone)]
pub(crate) enum StreamStatus {
    /// Waiting for the owning server's connection to come up.
    Pending,
    /// The stream is open; no frames decoded yet.
    Connecting,
    Streaming(u32, u32),
    /// The remote end closed the stream normally.
    Ended(String),
    Failed(String),
}

pub(crate) struct StreamSession {
    events: UnboundedReceiver<RtspStreamEvent>,
    outbound: Sender<RtspSessionStreamRequest>,
    pub(crate) status: StreamStatus,
    /// Launch parameters held until a server connection is available. Probe nodes
    /// are spawned from config before the sync websocket is up, so a stream can be
    /// requested too early; `launch_pending_streams` takes this once sync is ready.
    pending: Option<PendingLaunch>,
    /// The connection carrying this stream, and the stream's id on it. Both are
    /// only known once the stream is actually open; together they're what the
    /// link renderer reads byte counters from.
    pub(crate) conn: Option<Arc<InstanceConnection>>,
    pub(crate) stream_id: Option<StreamId>,
}

/// How long to wait for the owning server's connection before giving up on a
/// deferred stream start.
const PENDING_LAUNCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A stream start deferred until the owning server's connection is ready.
struct PendingLaunch {
    /// The server that reaches this device; `None` uses the primary connection.
    server: Option<ServerUrl>,
    requester: RtspSessionStreamRequester,
    events: UnboundedSender<RtspStreamEvent>,
    initial: RtspSessionStreamRequest,
    outbound_rx: Receiver<RtspSessionStreamRequest>,
    /// When the start was requested, so a connection that never arrives is
    /// reported instead of retried silently forever.
    since: std::time::Instant,
}

/// Latest captured frame per device id, used as the node icon thumbnail. Populated
/// passively while a stream runs; reusable by any probe type that decodes frames.
#[derive(Resource, Default)]
struct ProbeThumbnails(HashMap<u64, Handle<Image>>);

/// A probe node's icon child kind. The variants can't share an entity: `Svg2d`
/// requires `Mesh2d` and inserts its own material, so switching to a `Sprite` means
/// despawning and respawning the child.
#[derive(Component, Clone, PartialEq)]
enum ProbeNodeIcon {
    Svg(ProbeType),
    Thumbnail(Handle<Image>),
}

/// The display node showing a device's RTSP stream.
#[derive(Component)]
struct RtspStreamView {
    device_id: u64,
}

/// A status label reflecting the stream state for a device.
#[derive(Component)]
struct RtspStatusText {
    device_id: u64,
}

/// The exports/shares label in a filesystem protocol tab.
#[derive(Component)]
struct ShareList {
    device_id: u64,
    protocol: ProbeType,
}

/// Devices whose exports/shares have already been asked for, so the auto-query
/// below doesn't fire again every frame.
#[derive(Resource, Default)]
struct QueriedShares(HashSet<(u64, ProbeType)>);

/// Ask for the exports/shares behind any filesystem tab that hasn't been asked
/// yet.
///
/// Checked every frame rather than on `Added` so a query that couldn't happen yet
/// (no connection) is retried until it can.
fn start_pending_share_queries(
    mut queried: ResMut<QueriedShares>,
    lists: Query<&ShareList>,
    mut open: Local<HashSet<(u64, ProbeType)>>,
) {
    open.clear();
    for list in &lists {
        let key = (list.device_id, list.protocol);
        open.insert(key);
        if queried.0.contains(&key) {
            continue;
        }
        if crate::filesystem::client::connection_for(list.device_id).is_none() {
            continue;
        }
        crate::filesystem::client::enumerate(list.device_id, list.protocol);
        queried.0.insert(key);
    }
    // Forget tabs that have closed, so reopening the panel re-queries — which is
    // how a file server that was unreachable gets retried.
    queried.0.retain(|key| open.contains(key));
}

/// The set of currently selected device nodes (by id).
///
/// Derived from the generic [`SelectionSet`](sandpolis_client::gui::drag::SelectionSet)
/// by [`sync_device_selection`] rather than maintained by its own click handler —
/// device nodes select like any other node.
#[derive(Resource, Default)]
pub struct DeviceSelectionSet {
    pub selected: Vec<u64>,
}

/// A device panel's tab bar; `active` is the visible tab index.
#[derive(Component)]
struct DeviceTabBar {
    device_id: u64,
    active: usize,
}

/// One tab's content panel within a device panel.
#[derive(Component)]
struct DeviceTabContent {
    device_id: u64,
    index: usize,
}

/// The probe layer's node panel.
///
/// Expanded from a device node it shows just that device; expanded from the
/// gateway server the devices orbit, it lists all of them.
pub struct ProbePanel;

impl NodePanel for ProbePanel {
    fn build_summary(&self, ctx: &mut PanelCtx) {
        let target = ctx.target;
        let theme = ctx.theme;

        if let Some(device_id) = target.sub {
            ctx.children(|p| {
                p.spawn((
                    text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                    bind_text(move || match device_by_id(device_id) {
                        Some(device) => device
                            .device
                            .protocols()
                            .iter()
                            .map(|protocol| protocol.display_name())
                            .collect::<Vec<_>>()
                            .join(", "),
                        None => "Unregistered".to_string(),
                    }),
                ));
            });
            return;
        }

        let Some(instance) = target.instance else {
            return;
        };
        ctx.children(|p| {
            p.spawn((
                text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                bind_text(move || format!("{} device(s)", query_devices(instance).len())),
            ));
        });
    }

    fn build_detail(&self, ctx: &mut PanelCtx) {
        let target = ctx.target;
        let theme = ctx.theme;

        if let Some(device_id) = target.sub {
            ctx.children(|p| match device_by_id(device_id) {
                Some(device) => build_device_section(p, theme, &device),
                None => {
                    p.spawn(muted(
                        theme,
                        "This device is no longer registered.",
                        theme.metrics.font_md,
                    ));
                }
            });
            return;
        }

        let Some(instance) = target.instance else {
            return;
        };
        let devices = query_devices(instance);

        ctx.children(|p| {
            p.spawn((
                heading(theme, "Devices"),
                bind_text(move || format!("Devices ({})", query_devices(instance).len())),
            ));

            if devices.is_empty() {
                p.spawn(muted(
                    theme,
                    "No devices registered on this node.",
                    theme.metrics.font_md,
                ));
            }

            for device in &devices {
                build_device_section(p, theme, device);
            }
        });
    }

    /// A camera feed is worth nothing once nobody is looking at it, so a
    /// collapsing panel takes its stream down with it.
    fn on_collapse(&self, commands: &mut Commands, target: PanelTarget) {
        let Some(device_id) = target.sub else {
            return;
        };
        commands.queue(move |world: &mut World| {
            let Some(mut streams) = world.get_resource_mut::<ProbeStreams>() else {
                return;
            };
            // Clearing both is what makes reopening the panel the way to
            // restart a stream that was stopped or that failed.
            streams.stopped.remove(&device_id);
            streams.last_status.remove(&device_id);
            if let Some(session) = streams.streams.remove(&device_id) {
                let _ = session.outbound.try_send(RtspSessionStreamRequest::Stop);
            }
            if let Some(mut thumbnails) = world.get_resource_mut::<ProbeThumbnails>() {
                thumbnails.0.remove(&device_id);
            }
        });
    }
}

/// Mirror the generic node selection into [`DeviceSelectionSet`], which the
/// layer's "Delete probe" action is gated on.
fn sync_device_selection(
    selection: Res<sandpolis_client::gui::drag::SelectionSet>,
    probes: Query<&ProbeNode>,
    mut devices: ResMut<DeviceSelectionSet>,
) {
    if !selection.is_changed() {
        return;
    }
    let selected: Vec<u64> = selection
        .selected_nodes
        .iter()
        .filter_map(|entity| probes.get(*entity).ok())
        .map(|probe| probe.device_id)
        .collect();
    if devices.selected != selected {
        devices.selected = selected;
    }
}

/// Build one device's section: header + protocol tab bar + tab contents.
fn build_device_section(
    parent: &mut ChildSpawnerCommands,
    theme: &Theme,
    device: &RegisteredDevice,
) {
    let device_id = device.id.body();
    let protocols = device.device.protocols();

    parent
        .spawn((
            Node {
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(theme.metrics.space_sm),
                margin: UiRect::bottom(Val::Px(theme.metrics.space_md)),
                padding: UiRect::all(Val::Px(theme.metrics.space_sm)),
                border: UiRect::all(Val::Px(1.0)),
                ..default()
            },
            BorderColor::all(theme.color(Role::Border)),
            ThemedBorder(Role::Border),
        ))
        .with_children(|section| {
            section.spawn(heading(theme, device.display_name()));

            // Tab bar.
            section
                .spawn((
                    DeviceTabBar {
                        device_id,
                        active: 0,
                    },
                    row(theme.metrics.space_sm),
                ))
                .with_children(|bar| {
                    for (index, proto) in protocols.iter().enumerate() {
                        let proto = *proto;
                        bar.spawn(button(theme, proto.display_name())).observe(
                            move |_: On<Activate>, mut bars: Query<&mut DeviceTabBar>| {
                                for mut b in &mut bars {
                                    if b.device_id == device_id {
                                        b.active = index;
                                    }
                                }
                            },
                        );
                    }
                });

            // Tab contents (only the active one is visible).
            for (index, proto) in protocols.iter().enumerate() {
                section
                    .spawn((
                        DeviceTabContent { device_id, index },
                        Node {
                            flex_direction: FlexDirection::Column,
                            row_gap: Val::Px(theme.metrics.space_sm),
                            ..default()
                        },
                        if index == 0 {
                            Visibility::Inherited
                        } else {
                            Visibility::Hidden
                        },
                    ))
                    .with_children(|content| {
                        build_tab_content(content, theme, device, *proto);
                    });
            }
        });
}

/// Build the content for one protocol tab.
fn build_tab_content(
    content: &mut ChildSpawnerCommands,
    theme: &Theme,
    device: &RegisteredDevice,
    proto: ProbeType,
) {
    let device_id = device.id.body();
    match proto {
        ProbeType::Rtsp => {
            // Live video display (transparent until frames arrive).
            content.spawn((
                RtspStreamView { device_id },
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(240.0),
                    ..default()
                },
                BackgroundColor(Color::srgb_u8(30, 30, 30)),
                ImageNode {
                    color: Color::NONE,
                    ..default()
                },
            ));
            content.spawn((
                RtspStatusText { device_id },
                text(theme, "", theme.metrics.font_sm, Role::TextMuted),
            ));
            // No "Start": the stream opens as soon as the view is spawned (see
            // `start_pending_rtsp_streams`), because opening the panel on a
            // camera is the request to see what it sees.
            content
                .spawn(row(theme.metrics.space_sm))
                .with_children(|controls| {
                    controls.spawn(button(theme, "Stop")).observe(
                        move |_: On<Activate>,
                              mut streams: ResMut<ProbeStreams>,
                              mut thumbnails: ResMut<ProbeThumbnails>,
                              mut views: Query<(&RtspStreamView, &mut ImageNode)>| {
                            stop_rtsp_stream(device_id, &mut streams, &mut thumbnails, &mut views);
                            streams.stopped.insert(device_id);
                        },
                    );
                });
        }
        ProbeType::Wol => {
            if let Some(wol) = device.device.wol.clone() {
                let server = device.device.server.clone();
                content
                    .spawn(button(theme, "Wake"))
                    .observe(move |_: On<Activate>| {
                        send_wake(&wol, server.as_ref());
                    });
            }
        }
        ProbeType::Nfs | ProbeType::Smb => build_filesystem_tab(content, theme, device, proto),
        // Probe tabs stay descriptive; containers and virtual machines are
        // controlled from the Health layer.
        ProbeType::Docker => {
            if let Some(docker) = &device.device.docker {
                content.spawn(text(
                    theme,
                    docker.host.clone(),
                    theme.metrics.font_sm,
                    Role::Text,
                ));
            }
            content.spawn(muted(
                theme,
                "Manage containers in the Health layer.",
                theme.metrics.font_sm,
            ));
        }
        ProbeType::Libvirt => {
            if let Some(libvirt) = &device.device.libvirt {
                content.spawn(text(
                    theme,
                    libvirt.uri.clone(),
                    theme.metrics.font_sm,
                    Role::Text,
                ));
            }
            content.spawn(muted(
                theme,
                "Manage virtual machines in the Health layer.",
                theme.metrics.font_sm,
            ));
        }
        other => {
            content.spawn(muted(
                theme,
                format!(
                    "{} integration is not implemented yet.",
                    other.display_name()
                ),
                theme.metrics.font_sm,
            ));
        }
    }
}

/// Build the tab for a filesystem protocol.
///
/// The probe layer's job here is only to report what the device serves — its
/// exports or shares. Browsing them is the filesystem layer's job, which reaches
/// the same device through [`crate::filesystem`].
fn build_filesystem_tab(
    content: &mut ChildSpawnerCommands,
    theme: &Theme,
    device: &RegisteredDevice,
    proto: ProbeType,
) {
    let device_id = device.id.body();

    // What's configured, so a misconfigured device is obvious next to what the
    // server actually reports.
    let configured = match proto {
        ProbeType::Nfs => device
            .device
            .nfs
            .as_ref()
            .map(|nfs| format!("{}:{}", device.device.ip, nfs.export)),
        _ => device
            .device
            .smb
            .as_ref()
            .map(|smb| format!("\\\\{}\\{}", device.device.ip, smb.share)),
    };
    if let Some(configured) = configured {
        content.spawn(text(theme, configured, theme.metrics.font_sm, Role::Text));
    }

    let label = if proto == ProbeType::Nfs {
        "Exports"
    } else {
        "Shares"
    };
    content.spawn(muted(theme, label, theme.metrics.font_sm));
    content.spawn((
        // No "Query" button for the first load: opening a file server's panel is
        // the request to see what it serves (see `start_pending_share_queries`).
        ShareList {
            device_id,
            protocol: proto,
        },
        text(theme, "", theme.metrics.font_sm, Role::TextMuted),
        bind_text(move || match crate::filesystem::client::view(device_id) {
            Some(view) => {
                if let Some(error) = view.error {
                    error
                } else if let Some(shares) = view.shares {
                    if shares.is_empty() {
                        "None reported".to_string()
                    } else {
                        shares
                            .iter()
                            .map(|share| match &share.comment {
                                Some(comment) => format!("{}  ({comment})", share.name),
                                None => share.name.clone(),
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                } else if view.busy {
                    "Querying…".to_string()
                } else {
                    "Not queried yet".to_string()
                }
            }
            None => "Not queried yet".to_string(),
        }),
    ));

    content
        .spawn(row(theme.metrics.space_sm))
        .with_children(|controls| {
            controls
                .spawn(button(theme, "Refresh"))
                .observe(move |_: On<Activate>| {
                    crate::filesystem::client::enumerate(device_id, proto);
                });
        });

    content.spawn(muted(
        theme,
        "Browse files from the Filesystem layer.",
        theme.metrics.font_sm,
    ));
}

/// Stop `device_id`'s stream, blank its view, and drop its node thumbnail.
fn stop_rtsp_stream(
    device_id: u64,
    streams: &mut ProbeStreams,
    thumbnails: &mut ProbeThumbnails,
    views: &mut Query<(&RtspStreamView, &mut ImageNode)>,
) {
    let Some(session) = streams.streams.remove(&device_id) else {
        return;
    };
    let _ = session.outbound.try_send(RtspSessionStreamRequest::Stop);
    streams
        .last_status
        .insert(device_id, StreamStatus::Ended("Stopped".into()));
    clear_stream_view(device_id, thumbnails, views);
}

/// Open a stream for every RTSP view that doesn't have one yet.
///
/// Checked every frame rather than on `Added` so a start that couldn't happen
/// yet (no connection) is retried until it can. A device that already reported
/// how its last stream ended is left alone, so a camera that can't be reached is
/// retried when the panel is reopened rather than sixty times a second.
fn start_pending_rtsp_streams(mut streams: ResMut<ProbeStreams>, views: Query<&RtspStreamView>) {
    for view in &views {
        let device_id = view.device_id;
        if streams.streams.contains_key(&device_id)
            || streams.stopped.contains(&device_id)
            || streams.last_status.contains_key(&device_id)
        {
            continue;
        }
        // Wait for a connection before starting at all. `start_rtsp_stream`
        // would happily defer the launch, but its deferral window is finite —
        // spending it on the seconds before the websocket even exists means the
        // stream gives up just as the connection becomes usable.
        if device_by_id(device_id)
            .as_ref()
            .and_then(connection_for_device)
            .is_none()
        {
            continue;
        }
        start_rtsp_stream(device_id, &mut streams);
    }
}

/// The connection that reaches `device`: the server that owns it, else the
/// primary. `None` while neither is up yet.
fn connection_for_device(device: &RegisteredDevice) -> Option<Arc<InstanceConnection>> {
    device
        .device
        .server
        .as_ref()
        .and_then(sandpolis_client::sync::connection_for)
        .or_else(sandpolis_client::sync::connection)
}

/// Open an RTSP stream for `device_id` if one isn't already running. Every way
/// this can decline to start records a reason, so the tab's status label can say
/// what happened instead of nothing appearing to happen at all.
fn start_rtsp_stream(device_id: u64, streams: &mut ProbeStreams) {
    if streams.streams.contains_key(&device_id) {
        return;
    }
    let Some(device) = device_by_id(device_id) else {
        streams.last_status.insert(
            device_id,
            StreamStatus::Failed("Device is no longer registered".into()),
        );
        return;
    };
    let Some(rtsp) = device.device.rtsp.clone() else {
        streams.last_status.insert(
            device_id,
            StreamStatus::Failed("Device has no RTSP configuration".into()),
        );
        return;
    };

    let (requester, events_tx, events) = RtspSessionStreamRequester::channel();
    let (outbound, outbound_rx) = channel(16);
    let initial = RtspSessionStreamRequest::Start {
        url: build_rtsp_url(device.device.ip, &rtsp),
        transport: rtsp_transport(&rtsp),
        username: rtsp.username.clone(),
        password: rtsp.password.clone(),
    };

    streams.last_status.remove(&device_id);
    streams.streams.insert(
        device_id,
        StreamSession {
            events,
            outbound,
            status: StreamStatus::Pending,
            conn: None,
            stream_id: None,
            pending: Some(PendingLaunch {
                server: device.device.server.clone(),
                requester,
                events: events_tx,
                initial,
                outbound_rx,
                since: std::time::Instant::now(),
            }),
        },
    );
    info!("RTSP stream requested for device {}", device_id);
}

/// Launch any deferred stream starts once the owning server's connection is up.
fn launch_pending_streams(mut streams: ResMut<ProbeStreams>) {
    let mut timed_out = Vec::new();

    for (device_id, session) in streams.streams.iter_mut() {
        if session.pending.is_none() {
            continue;
        }
        // Route to the server that owns the device; fall back to the primary.
        // Same resolution as `connection_for_device`, but from the launch's
        // recorded server rather than the (possibly deleted) device.
        let conn = session
            .pending
            .as_ref()
            .and_then(|p| p.server.as_ref())
            .and_then(sandpolis_client::sync::connection_for)
            .or_else(sandpolis_client::sync::connection);
        let Some(conn) = conn else {
            // A connection that never arrives used to leave this spinning
            // silently forever; give up and say why.
            if session
                .pending
                .as_ref()
                .is_some_and(|p| p.since.elapsed() > PENDING_LAUNCH_TIMEOUT)
            {
                timed_out.push(*device_id);
            }
            continue;
        };
        let launch = session.pending.take().unwrap();
        session.status = StreamStatus::Connecting;
        session.conn = Some(conn.clone());
        spawn_stream(
            conn,
            launch.requester,
            launch.events,
            launch.initial,
            launch.outbound_rx,
        );
    }

    for device_id in timed_out {
        streams.streams.remove(&device_id);
        streams.last_status.insert(
            device_id,
            StreamStatus::Failed("No connection to the owning server".into()),
        );
        warn!("No server connection; cannot open RTSP stream for device {device_id}");
    }
}

/// Open an RTSP stream handled directly by the owning server and forward outbound
/// requests (Stop) over it until the channel closes.
fn spawn_stream(
    conn: Arc<InstanceConnection>,
    requester: RtspSessionStreamRequester,
    events: UnboundedSender<RtspStreamEvent>,
    initial: RtspSessionStreamRequest,
    mut outbound_rx: Receiver<RtspSessionStreamRequest>,
) {
    sandpolis_client::sync::spawn(async move {
        let (id, msg_tx) = match conn.open_stream(requester, initial).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "Failed to open RTSP stream");
                let _ = events.send(RtspStreamEvent::Failed(format!(
                    "Failed to open stream: {e}"
                )));
                return;
            }
        };
        // Hand the id back so the link renderer can read this stream's counters.
        let _ = events.send(RtspStreamEvent::Opened(id));
        while let Some(req) = outbound_rx.recv().await {
            let payload = match serde_cbor::to_vec(&req) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if msg_tx
                .send(StreamMessage::local(id, payload))
                .await
                .is_err()
            {
                break;
            }
        }
        conn.close_stream(id);
    });
}

/// Build an RGBA8 [`Image`] from a decoded frame.
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

/// Blank a device's stream view and drop its node thumbnail.
fn clear_stream_view(
    device_id: u64,
    thumbnails: &mut ProbeThumbnails,
    views: &mut Query<(&RtspStreamView, &mut ImageNode)>,
) {
    thumbnails.0.remove(&device_id);
    for (view, mut node) in views.iter_mut() {
        if view.device_id == device_id {
            node.image = Handle::default();
            node.color = Color::NONE;
        }
    }
}

/// Drain decoded RTSP frames and upload the latest to each display texture.
fn drive_probe_streams(
    mut streams: ResMut<ProbeStreams>,
    mut images: ResMut<Assets<Image>>,
    mut views: Query<(&RtspStreamView, &mut ImageNode)>,
    mut thumbnails: ResMut<ProbeThumbnails>,
) {
    // Sessions that ended this tick, with the status to leave behind.
    let mut finished: Vec<(u64, StreamStatus)> = Vec::new();

    for (device_id, session) in streams.streams.iter_mut() {
        let mut latest: Option<RtspFrameRgba> = None;
        while let Ok(event) = session.events.try_recv() {
            match event {
                RtspStreamEvent::Opened(id) => session.stream_id = Some(id),
                RtspStreamEvent::Started { width, height } => {
                    session.status = StreamStatus::Streaming(width, height);
                }
                RtspStreamEvent::Frame(frame) => latest = Some(frame),
                RtspStreamEvent::Stopped { reason } => {
                    finished.push((*device_id, StreamStatus::Ended(reason)));
                }
                RtspStreamEvent::Failed(reason) => {
                    warn!(device_id, %reason, "RTSP stream failed");
                    finished.push((*device_id, StreamStatus::Failed(reason)));
                }
            }
        }

        if let Some(frame) = latest {
            if frame.width == 0 || frame.height == 0 {
                continue;
            }
            session.status = StreamStatus::Streaming(frame.width, frame.height);
            let handle = images.add(image_from_rgba(frame.width, frame.height, frame.rgba));
            // Passively capture the latest frame as the node's thumbnail.
            thumbnails.0.insert(*device_id, handle.clone());
            // Resolved by device id rather than a cached entity: the node
            // node panel is rebuilt when reopened, so any entity captured
            // at start time goes stale.
            for (view, mut node) in views.iter_mut() {
                if view.device_id == *device_id {
                    node.image = handle.clone();
                    node.color = Color::WHITE;
                }
            }
        }
    }

    // Drop ended sessions so the next "Start stream" isn't a no-op.
    for (device_id, status) in finished {
        streams.streams.remove(&device_id);
        streams.last_status.insert(device_id, status);
        clear_stream_view(device_id, &mut thumbnails, &mut views);
    }
}

/// Reflect each device's stream state in its status label.
fn update_rtsp_status(
    streams: Res<ProbeStreams>,
    theme: Res<Theme>,
    mut labels: Query<(&RtspStatusText, &mut Text, &mut TextColor, &mut ThemedText)>,
) {
    for (status, mut label, mut color, mut themed) in &mut labels {
        let state = streams
            .streams
            .get(&status.device_id)
            .map(|session| session.status.clone())
            .or_else(|| streams.last_status.get(&status.device_id).cloned());

        let (value, role) = match state {
            Some(StreamStatus::Pending) => (
                "Waiting for server connection…".to_string(),
                Role::TextMuted,
            ),
            Some(StreamStatus::Connecting) => ("Connecting…".to_string(), Role::TextMuted),
            Some(StreamStatus::Streaming(w, h)) => (format!("Streaming {w}×{h}"), Role::TextMuted),
            Some(StreamStatus::Ended(reason)) => {
                (format!("Stream ended: {reason}"), Role::TextMuted)
            }
            Some(StreamStatus::Failed(reason)) => (reason, Role::Error),
            None => ("Stream inactive".to_string(), Role::TextMuted),
        };

        if label.0 != value {
            label.0 = value;
        }
        // Both are set: `ThemedText` so a theme switch repaints with the right
        // role, `TextColor` because the theme system only repaints on change.
        if themed.0 != role {
            themed.0 = role;
        }
        let want = theme.color(role);
        if color.0 != want {
            color.0 = want;
        }
    }
}

/// Swap a probe node's icon between its protocol SVG and a captured stream
/// thumbnail as frames arrive (and back if the thumbnail is cleared).
fn update_probe_node_icons(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    thumbnails: Res<ProbeThumbnails>,
    nodes: Query<(Entity, &ProbeNode, Option<&Children>)>,
    mut icons: Query<&mut ProbeNodeIcon>,
    mut sprites: Query<&mut Sprite>,
) {
    if !thumbnails.is_changed() {
        return;
    }

    for (entity, probe, children) in nodes.iter() {
        let desired = match thumbnails.0.get(&probe.device_id) {
            Some(handle) => ProbeNodeIcon::Thumbnail(handle.clone()),
            None => ProbeNodeIcon::Svg(probe.icon),
        };

        let current = children
            .into_iter()
            .flatten()
            .find_map(|child| icons.get(*child).ok().map(|_| *child));

        let Some(child) = current else {
            spawn_probe_icon(&mut commands, &asset_server, entity, desired);
            continue;
        };

        let icon = icons.get(child).unwrap().clone();
        if icon == desired {
            continue;
        }

        // Thumbnail handle changed (new frame): update the sprite in place rather
        // than despawning a child every frame.
        if let (ProbeNodeIcon::Thumbnail(_), ProbeNodeIcon::Thumbnail(new_handle)) =
            (&icon, &desired)
        {
            if let Ok(mut sprite) = sprites.get_mut(child) {
                sprite.image = new_handle.clone();
            }
            *icons.get_mut(child).unwrap() = desired;
            continue;
        }

        // Variant change (SVG <-> thumbnail): despawn and respawn the child.
        commands.entity(child).despawn();
        spawn_probe_icon(&mut commands, &asset_server, entity, desired);
    }
}

/// Switch which tab content is visible to match its tab bar's active index.
fn update_device_tabs(
    bars: Query<&DeviceTabBar>,
    mut contents: Query<(&DeviceTabContent, &mut Visibility)>,
) {
    for (content, mut vis) in &mut contents {
        if let Some(bar) = bars.iter().find(|b| b.device_id == content.device_id) {
            let want = if bar.active == content.index {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            };
            if *vis != want {
                *vis = want;
            }
        }
    }
}

/// Delete the currently selected devices over the management stream.
fn delete_selected_devices(commands: &mut Commands) {
    commands.queue(|world: &mut World| {
        let ids: Vec<u64> = world
            .get_resource::<DeviceSelectionSet>()
            .map(|s| s.selected.clone())
            .unwrap_or_default();
        if ids.is_empty() {
            return;
        }
        if let Some(conn) = sandpolis_client::sync::connection() {
            for id in ids {
                if let Some(device) = device_by_id(id) {
                    crate::management::delete_device(conn.clone(), device.id);
                }
            }
        }
        if let Some(mut sel) = world.get_resource_mut::<DeviceSelectionSet>() {
            sel.selected.clear();
        }
    });
}

/// State of the "register device" dialog.
#[derive(Resource, Default)]
pub struct RegisterProbeDialogState {
    pub show: bool,
    pub name: String,
    /// Server URL to associate the probe with; blank means the primary server.
    pub server: String,
    pub ip: String,
    pub rtsp_path: String,
    pub rtsp_port: String,
    pub rtsp_user: String,
    pub rtsp_pass: String,
    pub wol_mac: String,
}

#[derive(Component)]
pub struct RegisterProbeRoot;
#[derive(Component)]
struct NameInput;
#[derive(Component)]
struct ServerInput;
#[derive(Component)]
struct IpInput;
#[derive(Component)]
struct RtspPathInput;
#[derive(Component)]
struct RtspPortInput;
#[derive(Component)]
struct RtspUserInput;
#[derive(Component)]
struct RtspPassInput;
#[derive(Component)]
struct WolMacInput;

/// Spawn/despawn the register-device modal.
pub fn manage_register_probe(
    mut commands: Commands,
    theme: Res<Theme>,
    state: Res<RegisterProbeDialogState>,
    root: Query<Entity, With<RegisterProbeRoot>>,
    mut focus: ResMut<InputFocus>,
) {
    let exists = !root.is_empty();
    if state.show && !exists {
        commands
            .spawn((RegisterProbeRoot, modal_scrim()))
            .with_children(|scrim| {
                scrim
                    .spawn((
                        Node {
                            flex_direction: FlexDirection::Column,
                            width: Val::Px(380.0),
                            // The form is tall (many fields); cap it to the viewport
                            // and let it scroll so no field is pushed off-screen.
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
                    ))
                    .with_children(|p| {
                        p.spawn(heading(&theme, "Register Device"));
                        p.spawn(muted(&theme, "Name", theme.metrics.font_sm));
                        p.spawn((NameInput, text_input(&theme)));
                        p.spawn(muted(
                            &theme,
                            "Server URL (blank = default)",
                            theme.metrics.font_sm,
                        ));
                        p.spawn((ServerInput, text_input(&theme)));
                        p.spawn(muted(&theme, "IP address", theme.metrics.font_sm));
                        p.spawn((IpInput, text_input(&theme)));

                        p.spawn(muted(&theme, "RTSP path", theme.metrics.font_sm));
                        p.spawn((RtspPathInput, text_input(&theme)));
                        p.spawn(muted(&theme, "RTSP port", theme.metrics.font_sm));
                        p.spawn((RtspPortInput, text_input(&theme)));
                        p.spawn(muted(&theme, "RTSP username", theme.metrics.font_sm));
                        p.spawn((RtspUserInput, text_input(&theme)));
                        p.spawn(muted(&theme, "RTSP password", theme.metrics.font_sm));
                        p.spawn((RtspPassInput, text_input(&theme)));

                        p.spawn(muted(&theme, "Wake-on-LAN MAC", theme.metrics.font_sm));
                        p.spawn((WolMacInput, text_input(&theme)));

                        p.spawn(Node {
                            column_gap: Val::Px(8.0),
                            ..default()
                        })
                        .with_children(|row| {
                            row.spawn(button(&theme, "Register"))
                                .observe(on_register_submit);
                            row.spawn(button(&theme, "Cancel"))
                                .observe(on_register_cancel);
                        });
                    });
            });
    } else if !state.show && exists {
        for entity in &root {
            commands.entity(entity).despawn();
        }
        focus.clear();
    }
}

/// Focus the name field when the dialog opens.
pub fn focus_register_probe_input(
    inputs: Query<Entity, Added<NameInput>>,
    mut focus: ResMut<InputFocus>,
) {
    if let Ok(entity) = inputs.single() {
        focus.set(entity, FocusCause::Navigated);
    }
}

/// Copy dialog input contents into [`RegisterProbeDialogState`].
pub fn sync_register_probe_inputs(
    mut state: ResMut<RegisterProbeDialogState>,
    name: Query<&EditableText, With<NameInput>>,
    server: Query<&EditableText, With<ServerInput>>,
    ip: Query<&EditableText, With<IpInput>>,
    path: Query<&EditableText, With<RtspPathInput>>,
    port: Query<&EditableText, With<RtspPortInput>>,
    user: Query<&EditableText, With<RtspUserInput>>,
    pass: Query<&EditableText, With<RtspPassInput>>,
    mac: Query<&EditableText, With<WolMacInput>>,
) {
    if let Ok(i) = name.single() {
        let value = i.value().to_string();
        if state.name != value {
            state.name = value;
        }
    }
    if let Ok(i) = server.single() {
        let value = i.value().to_string();
        if state.server != value {
            state.server = value;
        }
    }
    if let Ok(i) = ip.single() {
        let value = i.value().to_string();
        if state.ip != value {
            state.ip = value;
        }
    }
    if let Ok(i) = path.single() {
        let value = i.value().to_string();
        if state.rtsp_path != value {
            state.rtsp_path = value;
        }
    }
    if let Ok(i) = port.single() {
        let value = i.value().to_string();
        if state.rtsp_port != value {
            state.rtsp_port = value;
        }
    }
    if let Ok(i) = user.single() {
        let value = i.value().to_string();
        if state.rtsp_user != value {
            state.rtsp_user = value;
        }
    }
    if let Ok(i) = pass.single() {
        let value = i.value().to_string();
        if state.rtsp_pass != value {
            state.rtsp_pass = value;
        }
    }
    if let Ok(i) = mac.single() {
        let value = i.value().to_string();
        if state.wol_mac != value {
            state.wol_mac = value;
        }
    }
}

fn on_register_submit(_activate: On<Activate>, mut state: ResMut<RegisterProbeDialogState>) {
    let ip = match state.ip.trim().parse::<std::net::IpAddr>() {
        Ok(ip) => ip,
        Err(_) => {
            warn!("Register device: invalid IP address {:?}", state.ip);
            return;
        }
    };

    let mut device = DeviceConfig {
        name: (!state.name.is_empty()).then(|| state.name.clone()),
        ip,
        ..Default::default()
    };
    if !state.rtsp_path.is_empty() {
        device.rtsp = Some(RtspProbeConfig {
            port: state.rtsp_port.trim().parse::<u16>().ok(),
            path: state.rtsp_path.clone(),
            username: (!state.rtsp_user.is_empty()).then(|| state.rtsp_user.clone()),
            password: (!state.rtsp_pass.is_empty()).then(|| state.rtsp_pass.clone()),
            transport: None,
        });
    }
    if !state.wol_mac.is_empty() {
        device.wol = Some(WolProbeConfig {
            mac_address: state.wol_mac.clone(),
            ..Default::default()
        });
    }

    if device.protocols().is_empty() {
        warn!("Register device: no protocols specified");
        return;
    }

    // Resolve the associated server: an explicit URL if given, else the primary.
    let server = if state.server.trim().is_empty() {
        match sandpolis_client::sync::primary_server_url() {
            Some(url) => url,
            None => {
                warn!("No server connection; cannot register device");
                return;
            }
        }
    } else {
        match state.server.trim().parse() {
            Ok(url) => url,
            Err(e) => {
                warn!("Register device: invalid server URL: {}", e);
                return;
            }
        }
    };

    // Registration always targets the authoritative server (primary/GS), which
    // records the association and persists the device list.
    if let Some(conn) = sandpolis_client::sync::connection() {
        crate::management::register_device(conn, server, device);
    } else {
        warn!("No server connection; cannot register device");
    }

    *state = RegisterProbeDialogState::default();
}

fn on_register_cancel(_activate: On<Activate>, mut state: ResMut<RegisterProbeDialogState>) {
    *state = RegisterProbeDialogState::default();
}

/// Open the device-management subscription once a connection is available.
fn open_device_subscription(mut done: Local<bool>) {
    if *done {
        return;
    }
    if let Some(conn) = sandpolis_client::sync::connection() {
        crate::management::subscribe(conn);
        *done = true;
    }
}

/// The probe layer's client plugin.
pub struct ProbeClientPlugin;

impl Plugin for ProbeClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RegisterProbeDialogState>();
        app.init_resource::<ProbeStreams>();
        app.init_resource::<ProbeThumbnails>();
        app.init_resource::<DeviceSelectionSet>();
        app.init_resource::<QueriedShares>();
        app.init_resource::<super::link::ProbeLinkTraffic>();
        app.add_systems(
            Update,
            (
                scale_probe_node_svgs,
                update_probe_nodes,
                update_credential_markers,
                apply_probe_spring_forces,
                manage_register_probe,
                focus_register_probe_input,
                sync_register_probe_inputs,
                start_pending_rtsp_streams,
                start_pending_share_queries,
                launch_pending_streams,
                drive_probe_streams,
                update_rtsp_status.after(drive_probe_streams),
                update_probe_node_icons,
                update_device_tabs,
                sync_device_selection,
                open_device_subscription,
                super::link::sample_link_traffic.after(drive_probe_streams),
                super::link::hover_probe_links,
            ),
        );
        // Must run after the generic node visibility system (which also matches
        // device nodes) so probe-specific visibility wins.
        app.add_systems(
            PostUpdate,
            (
                update_probe_node_visibility
                    .after(sandpolis_client::gui::layer_visuals::update_node_visibility_for_layer),
                super::link::render_probe_links,
            ),
        );
        app.register_layer_client(
            LayerClientInfo::new(LayerName::from("Probe"), "Device monitoring probes")
                .with_panel(ProbePanel)
                // Devices get the whole canvas: this is the layer for surveying
                // what's registered, and the gateway servers they hang off are
                // the Network layer's business.
                .with_visible_instance_types(&[])
                .showing_probe_nodes()
                .with_toolbar_action("Register probe", "toolbar/register_probe.svg", |commands| {
                    commands.queue(|world: &mut World| {
                        if let Some(mut state) =
                            world.get_resource_mut::<RegisterProbeDialogState>()
                        {
                            state.show = true;
                        }
                    });
                })
                .with_toolbar_action_gated(
                    "Delete probe",
                    "toolbar/delete_probe.svg",
                    delete_selected_devices,
                    |world: &World| {
                        world
                            .get_resource::<DeviceSelectionSet>()
                            .is_some_and(|s| !s.selected.is_empty())
                    },
                ),
        );
    }
}
