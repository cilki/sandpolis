//! GUI for the Tunnel layer: a per-node table of tunnels, and decorated
//! world-view links between the endpoints of every active tunnel.
//!
//! The link decoration mirrors the probe layer's: a base line with two parallel
//! dotted lines travelling in opposite directions, their speed tracking
//! throughput, and a hover tooltip showing the current rate and cumulative
//! total. Throughput is derived by differencing the cumulative byte counters on
//! the replicated [`TunnelData`] rows, which the bridging server updates.

use crate::{TunnelData, TunnelMode};
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use gesso::drag::{cursor_world_position, is_visible};
use gesso::node::WorldView;
use gesso::ui::bind::bind_text;
use gesso::ui::gating::UiPointerState;
use gesso::ui::node_panel::{NodePanel, PanelCtx};
use gesso::ui::table::{TableData, TableRow, bind_table, table};
use gesso::ui::theme::{Role, Theme};
use gesso::ui::tooltip::WorldTooltip;
use gesso::ui::widgets::{heading, text};
use sandpolis_client::gui::input::CurrentLayer;
use sandpolis_client::gui::node::NodeEntity;
use sandpolis_client::gui::node_panel::PanelTargetExt;
use sandpolis_client::gui::ui::layer::{LayerClientInfo, RegisterLayerClient};
use sandpolis_instance::{InstanceId, InstanceType, LayerName};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const LAYER: &str = "Tunnel";

/// How far the cursor may sit from a link and still hover it, in world units.
const HOVER_RADIUS: f32 = 12.0;
/// Perpendicular distance of each directional line from the link itself.
const DASH_OFFSET: f32 = 4.0;
const DASH_LENGTH: f32 = 7.0;
const DASH_GAP: f32 = 7.0;
/// Dash travel for an open but idle tunnel, so a quiet tunnel still reads as up.
const DASH_BASE_SPEED: f32 = 25.0;
const DASH_MAX_SPEED: f32 = 220.0;
/// How often the cumulative byte counters are re-read.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
/// Weight given to the newest sample when smoothing the rate.
const SMOOTHING: f64 = 0.4;

// --- Node panel ------------------------------------------------------------

pub struct TunnelPanel;

impl NodePanel for TunnelPanel {
    fn build_summary(&self, ctx: &mut PanelCtx) {
        let Some(instance) = ctx.target.instance() else {
            return;
        };
        super::subscribe();
        let theme = ctx.theme;
        ctx.children(|p| {
            p.spawn((
                text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                bind_text(move || {
                    let tunnels = super::query_tunnels(instance).unwrap_or_default();
                    if tunnels.is_empty() {
                        return "No tunnels".into();
                    }
                    let active = tunnels.iter().filter(|t| t.state.active()).count();
                    format!("{} tunnels — {} active", tunnels.len(), active)
                }),
            ));
        });
    }

    fn build_detail(&self, ctx: &mut PanelCtx) {
        let Some(instance) = ctx.target.instance() else {
            return;
        };
        super::subscribe();
        let theme = ctx.theme;
        ctx.children(|p| {
            p.spawn(heading(theme, "Tunnels"));
            p.spawn((
                table(theme, None),
                bind_table(move || {
                    let mut tunnels = super::query_tunnels(instance).unwrap_or_default();
                    tunnels.sort_by(|a, b| a.name.cmp(&b.name));
                    let mut data = TableData::new([
                        "Name", "Endpoints", "Proto", "Mode", "State", "Conns", "↓", "↑",
                    ])
                    .with_placeholder("No tunnels");
                    for t in tunnels {
                        let endpoints = format!("{} → {}", t.listen_addr, t.target_addr);
                        let role = match t.state {
                            crate::TunnelState::Failed => Some(Role::Error),
                            crate::TunnelState::Pending => Some(Role::Warn),
                            crate::TunnelState::Active => None,
                        };
                        let row = TableRow::new([
                            t.name.clone(),
                            endpoints,
                            t.protocol.to_string(),
                            t.effective_mode.to_string(),
                            state_label(&t).to_string(),
                            t.active_connections.to_string(),
                            format_bytes(t.rx_bytes),
                            format_bytes(t.tx_bytes),
                        ]);
                        data.push_row(match role {
                            Some(role) => row.with_role(role),
                            None => row,
                        });
                    }
                    data
                }),
            ));
        });
    }
}

fn state_label(t: &TunnelData) -> &'static str {
    match t.state {
        crate::TunnelState::Pending => "pending",
        crate::TunnelState::Active => "active",
        crate::TunnelState::Failed => "failed",
    }
}

// --- Link decoration -------------------------------------------------------

/// A tunnel's throughput sample, plus the geometry the renderer needs, kept in
/// a resource so the per-frame render/hover systems never touch the database.
#[derive(Resource, Default)]
struct TunnelLinks {
    samples: HashMap<String, LinkSample>,
    views: Vec<TunnelView>,
}

struct LinkSample {
    rx_bps: f64,
    tx_bps: f64,
    last: Option<(u64, u64)>,
    sampled_at: Instant,
}

impl Default for LinkSample {
    fn default() -> Self {
        Self {
            rx_bps: 0.0,
            tx_bps: 0.0,
            last: None,
            sampled_at: Instant::now(),
        }
    }
}

/// One active tunnel's drawable state.
struct TunnelView {
    name: String,
    listener_id: Option<InstanceId>,
    terminator_id: Option<InstanceId>,
    mode: TunnelMode,
    rx_bps: f64,
    tx_bps: f64,
    rx_total: u64,
    tx_total: u64,
}

/// Refresh throughput for every active tunnel, forgetting ones that ended.
fn sample_tunnel_traffic(mut links: ResMut<TunnelLinks>, mut last_scan: Local<Option<Instant>>) {
    let now = Instant::now();
    if last_scan.is_some_and(|last| now.duration_since(last) < SAMPLE_INTERVAL) {
        return;
    }
    *last_scan = Some(now);

    let tunnels = super::active_tunnels().unwrap_or_default();
    let names: HashSet<String> = tunnels.iter().map(|t| t.name.clone()).collect();
    links.samples.retain(|name, _| names.contains(name));

    let mut views = Vec::with_capacity(tunnels.len());
    for t in &tunnels {
        let entry = links.samples.entry(t.name.clone()).or_default();
        let totals = (t.rx_bytes, t.tx_bytes);
        let elapsed = entry.sampled_at.elapsed().as_secs_f64();
        if let Some((last_rx, last_tx)) = entry.last {
            if elapsed > 0.0 {
                let rx = totals.0.saturating_sub(last_rx) as f64 / elapsed;
                let tx = totals.1.saturating_sub(last_tx) as f64 / elapsed;
                entry.rx_bps = entry.rx_bps * (1.0 - SMOOTHING) + rx * SMOOTHING;
                entry.tx_bps = entry.tx_bps * (1.0 - SMOOTHING) + tx * SMOOTHING;
            }
        }
        entry.last = Some(totals);
        entry.sampled_at = now;

        views.push(TunnelView {
            name: t.name.clone(),
            listener_id: t.listener_id,
            terminator_id: t.terminator_id,
            mode: t.effective_mode,
            rx_bps: entry.rx_bps,
            tx_bps: entry.tx_bps,
            rx_total: t.rx_bytes,
            tx_total: t.tx_bytes,
        });
    }
    links.views = views;
}

/// Positions of every visible node, keyed by the instance it represents.
fn node_positions(
    nodes: &Query<(&Transform, &NodeEntity, Option<&Visibility>)>,
) -> HashMap<InstanceId, Vec2> {
    nodes
        .iter()
        .filter(|(_, _, visibility)| is_visible(*visibility))
        .map(|(transform, node, _)| (node.instance_id, transform.translation.truncate()))
        .collect()
}

/// The endpoints of every drawable tunnel as `(listener pos, terminator pos, index)`.
fn link_segments<'a>(
    links: &'a TunnelLinks,
    positions: &'a HashMap<InstanceId, Vec2>,
) -> impl Iterator<Item = (Vec2, Vec2, usize)> + 'a {
    links.views.iter().enumerate().filter_map(move |(i, view)| {
        let from = positions.get(&view.listener_id?)?;
        let to = positions.get(&view.terminator_id?)?;
        Some((*from, *to, i))
    })
}

fn dash_color(theme: &Theme, mode: TunnelMode) -> Color {
    theme.color(match mode {
        // A direct (hole-punched) tunnel reads distinctly from the indirect
        // default so its P2P path is visible at a glance.
        TunnelMode::Direct => Role::Warn,
        TunnelMode::Indirect => Role::Accent,
    })
}

/// Draw each active tunnel's link with animated dashes tracking throughput.
fn render_tunnel_links(
    mut gizmos: Gizmos,
    time: Res<Time>,
    theme: Res<Theme>,
    current_layer: Res<CurrentLayer>,
    links: Res<TunnelLinks>,
    nodes: Query<(&Transform, &NodeEntity, Option<&Visibility>)>,
) {
    if current_layer.0.name() != LAYER {
        return;
    }
    let positions = node_positions(&nodes);
    let elapsed = time.elapsed_secs();

    for (from, to, index) in link_segments(&links, &positions) {
        gizmos.line_2d(from, to, theme.color(Role::Border));
        let view = &links.views[index];
        let Some(direction) = (to - from).try_normalize() else {
            continue;
        };
        let normal = direction.perp() * DASH_OFFSET;
        let color = dash_color(&theme, view.mode);

        // Upload (listener -> target) runs one way; download the other, on the
        // opposite side of the link.
        draw_dashes(
            &mut gizmos,
            from + normal,
            to + normal,
            elapsed * dash_speed(view.tx_bps),
            color,
        );
        draw_dashes(
            &mut gizmos,
            to - normal,
            from - normal,
            elapsed * dash_speed(view.rx_bps),
            color,
        );
    }
}

/// Offer a tooltip for whichever tunnel link the cursor is closest to.
fn hover_tunnel_links(
    ui_pointer: Res<UiPointerState>,
    current_layer: Res<CurrentLayer>,
    links: Res<TunnelLinks>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform), With<WorldView>>,
    nodes: Query<(&Transform, &NodeEntity, Option<&Visibility>)>,
    mut tooltip: ResMut<WorldTooltip>,
    // Whether the tooltip currently showing is ours, so ending a hover can't
    // clear one another layer put there.
    mut showing: Local<bool>,
) {
    let hovered = if ui_pointer.over_ui_blocking || current_layer.0.name() != LAYER {
        None
    } else {
        cursor_world_position(&windows, &cameras).and_then(|cursor| {
            let positions = node_positions(&nodes);
            link_segments(&links, &positions)
                .filter_map(|(from, to, index)| {
                    let distance = distance_to_segment(cursor, from, to);
                    (distance <= HOVER_RADIUS).then_some((index, distance))
                })
                .min_by(|(_, a), (_, b)| a.total_cmp(b))
                .map(|(index, _)| index)
        })
    };

    match hovered {
        Some(index) => {
            tooltip.0 = Some(describe_link(&links.views[index]));
            *showing = true;
        }
        None if *showing => {
            tooltip.0 = None;
            *showing = false;
        }
        None => {}
    }
}

/// Draw a dashed line from `from` to `to`, shifted by `phase` world units.
fn draw_dashes(gizmos: &mut Gizmos, from: Vec2, to: Vec2, phase: f32, color: Color) {
    let delta = to - from;
    let length = delta.length();
    let Some(direction) = delta.try_normalize() else {
        return;
    };
    let period = DASH_LENGTH + DASH_GAP;
    let mut start = phase.rem_euclid(period) - period;
    while start < length {
        let head = start.max(0.0);
        let tail = (start + DASH_LENGTH).min(length);
        if tail > head {
            gizmos.line_2d(from + direction * head, from + direction * tail, color);
        }
        start += period;
    }
}

fn dash_speed(bytes_per_second: f64) -> f32 {
    (DASH_BASE_SPEED + (bytes_per_second / 4096.0) as f32).min(DASH_MAX_SPEED)
}

fn distance_to_segment(point: Vec2, from: Vec2, to: Vec2) -> f32 {
    let delta = to - from;
    let length_squared = delta.length_squared();
    if length_squared <= f32::EPSILON {
        return point.distance(from);
    }
    let t = ((point - from).dot(delta) / length_squared).clamp(0.0, 1.0);
    point.distance(from + delta * t)
}

fn describe_link(view: &TunnelView) -> String {
    format!(
        "{} ({})\n  ↓ {} ({} total)\n  ↑ {} ({} total)",
        view.name,
        view.mode,
        format_rate(view.rx_bps),
        format_bytes(view.rx_total),
        format_rate(view.tx_bps),
        format_bytes(view.tx_total),
    )
}

fn format_rate(bytes_per_second: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    if bytes_per_second >= MB {
        format!("{:.1} MB/s", bytes_per_second / MB)
    } else if bytes_per_second >= KB {
        format!("{:.1} KB/s", bytes_per_second / KB)
    } else {
        format!("{:.0} B/s", bytes_per_second)
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{bytes:.0} B")
    }
}

// --- Plugin ----------------------------------------------------------------

pub struct TunnelClientPlugin;

impl Plugin for TunnelClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TunnelLinks>();
        app.add_systems(Update, (sample_tunnel_traffic, hover_tunnel_links));
        app.add_systems(PostUpdate, render_tunnel_links);
        app.register_layer_client(
            LayerClientInfo::new(LayerName::from(LAYER), "Application-level tunnels")
                .with_panel(TunnelPanel)
                .with_visible_instance_types(&[
                    InstanceType::Agent,
                    InstanceType::Server,
                    InstanceType::Client,
                ]),
        );
    }
}
