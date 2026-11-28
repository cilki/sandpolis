//! Links from probe nodes to their gateway server.
//!
//! Every probe orbits the server that reaches it, held there by
//! [`apply_probe_spring_forces`](super::gui::apply_probe_spring_forces); this
//! module makes that relationship visible as a drawn line. While a device has an
//! open stream the line grows two parallel dotted lines travelling in opposite
//! directions, and hovering it shows the stream and its throughput.
//!
//! The byte counts come from this client's connection to the gateway server —
//! the transport that actually carries the stream here — not from the
//! server↔device hop the line is drawn over. There is no visibility into the
//! latter from the client, and the two are the same data anyway.
//!
//! Drawing is immediate-mode via gizmos, matching
//! `gesso::link::render_links`. The generic `Edge` component is
//! deliberately not reused: it is keyed by `InstanceId` at both ends and feeds
//! the generic spring layout, so a probe→gateway edge would collapse into a
//! zero-length self-edge.

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use gesso::drag::{cursor_world_position, is_visible};
use gesso::node::WorldView;
use gesso::ui::gating::UiPointerState;
use gesso::ui::theme::{Role, Theme};
use gesso::ui::tooltip::WorldTooltip;
use sandpolis_client::gui::input::CurrentLayer;
use sandpolis_client::gui::node::NodeEntity;
use sandpolis_client::gui::ui::layer::LayerRegistry;
use sandpolis_instance::InstanceId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::gui::{ProbeNode, ProbeStreams, device_by_id};

/// How far the cursor may sit from a link and still hover it, in world units.
const HOVER_RADIUS: f32 = 12.0;

/// Perpendicular distance of each directional line from the link itself.
const DASH_OFFSET: f32 = 4.0;

const DASH_LENGTH: f32 = 7.0;
const DASH_GAP: f32 = 7.0;

/// Dash travel for an open but idle stream, in world units per second. Kept
/// non-zero so a stalled stream still reads as connected rather than dead.
const DASH_BASE_SPEED: f32 = 25.0;
const DASH_MAX_SPEED: f32 = 220.0;

/// How often the cumulative byte counters are re-read.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// Weight given to the newest sample when smoothing the rate.
const SMOOTHING: f64 = 0.4;

/// Per-device stream throughput, refreshed from the connection's byte counters.
#[derive(Resource, Default)]
pub(crate) struct ProbeLinkTraffic(HashMap<u64, LinkTraffic>);

pub(crate) struct LinkTraffic {
    /// What the stream is, for the tooltip. A device could eventually run more
    /// than one kind of stream, so this isn't hardcoded at the display site.
    label: &'static str,
    /// Smoothed bytes/second arriving from the device.
    rx_bps: f64,
    /// Smoothed bytes/second travelling to the device.
    tx_bps: f64,
    /// Cumulative `(rx, tx)` at the previous sample.
    last: Option<(u64, u64)>,
    sampled_at: Instant,
}

impl Default for LinkTraffic {
    fn default() -> Self {
        Self {
            label: "RTSP session",
            rx_bps: 0.0,
            tx_bps: 0.0,
            last: None,
            sampled_at: Instant::now(),
        }
    }
}

/// Refresh throughput for every device with a live stream, and forget devices
/// whose stream has ended.
pub(crate) fn sample_link_traffic(
    streams: Res<ProbeStreams>,
    mut traffic: ResMut<ProbeLinkTraffic>,
) {
    traffic
        .0
        .retain(|device_id, _| streams.streams.contains_key(device_id));

    for (device_id, session) in streams.streams.iter() {
        let entry = traffic.0.entry(*device_id).or_default();

        // Counters only exist once the stream is actually open; until then the
        // entry still exists so the link is decorated while connecting.
        let Some((conn, stream_id)) = session.conn.as_ref().zip(session.stream_id) else {
            continue;
        };
        let Some(totals) = conn.streams.traffic(stream_id) else {
            continue;
        };

        let elapsed = entry.sampled_at.elapsed();
        if elapsed < SAMPLE_INTERVAL {
            continue;
        }

        if let Some((last_rx, last_tx)) = entry.last {
            let seconds = elapsed.as_secs_f64();
            let rx = totals.0.saturating_sub(last_rx) as f64 / seconds;
            let tx = totals.1.saturating_sub(last_tx) as f64 / seconds;
            entry.rx_bps = entry.rx_bps * (1.0 - SMOOTHING) + rx * SMOOTHING;
            entry.tx_bps = entry.tx_bps * (1.0 - SMOOTHING) + tx * SMOOTHING;
        }
        entry.last = Some(totals);
        entry.sampled_at = Instant::now();
    }
}

/// Draw each probe's link to its gateway, decorated while a stream runs.
pub(crate) fn render_probe_links(
    mut gizmos: Gizmos,
    time: Res<Time>,
    theme: Res<Theme>,
    registry: Res<LayerRegistry>,
    current_layer: Res<CurrentLayer>,
    traffic: Res<ProbeLinkTraffic>,
    probes: Query<(&Transform, &ProbeNode, Option<&Visibility>)>,
    gateways: Query<(&Transform, &NodeEntity, Option<&Visibility>), Without<ProbeNode>>,
) {
    if !registry.show_probe_nodes(&current_layer) {
        return;
    }

    let gateway_positions = gateway_positions(&gateways);
    let elapsed = time.elapsed_secs();

    for (from, to, device_id) in links(&probes, &gateway_positions) {
        gizmos.line_2d(from, to, theme.color(Role::Border));

        let Some(link) = traffic.0.get(&device_id) else {
            continue;
        };

        let Some(direction) = (to - from).try_normalize() else {
            continue;
        };
        let normal = direction.perp() * DASH_OFFSET;
        let color = theme.color(Role::Accent);

        // Data coming off the device runs probe -> gateway; requests going to
        // it run the other way, on the opposite side of the link.
        draw_dashes(
            &mut gizmos,
            from + normal,
            to + normal,
            elapsed * dash_speed(link.rx_bps),
            color,
        );
        draw_dashes(
            &mut gizmos,
            to - normal,
            from - normal,
            elapsed * dash_speed(link.tx_bps),
            color,
        );
    }
}

/// Offer a tooltip for whichever link the cursor is closest to.
pub(crate) fn hover_probe_links(
    ui_pointer: Res<UiPointerState>,
    registry: Res<LayerRegistry>,
    current_layer: Res<CurrentLayer>,
    traffic: Res<ProbeLinkTraffic>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform), With<WorldView>>,
    probes: Query<(&Transform, &ProbeNode, Option<&Visibility>)>,
    gateways: Query<(&Transform, &NodeEntity, Option<&Visibility>), Without<ProbeNode>>,
    mut tooltip: ResMut<WorldTooltip>,
    // Whether the tooltip currently showing is ours, so ending a hover can't
    // clear one another layer put there.
    mut showing: Local<bool>,
) {
    let hovered = if ui_pointer.over_ui_blocking || !registry.show_probe_nodes(&current_layer) {
        None
    } else {
        cursor_world_position(&windows, &cameras).and_then(|cursor| {
            let gateway_positions = gateway_positions(&gateways);
            links(&probes, &gateway_positions)
                .filter_map(|(from, to, device_id)| {
                    let distance = distance_to_segment(cursor, from, to);
                    (distance <= HOVER_RADIUS).then_some((device_id, distance))
                })
                .min_by(|(_, a), (_, b)| a.total_cmp(b))
                .map(|(device_id, _)| device_id)
        })
    };

    match hovered {
        Some(device_id) => {
            tooltip.0 = Some(describe_link(device_id, traffic.0.get(&device_id)));
            *showing = true;
        }
        None if *showing => {
            tooltip.0 = None;
            *showing = false;
        }
        None => {}
    }
}

/// Where every *visible* non-probe node sits, keyed by the instance it
/// represents.
///
/// Hidden gateways are dropped so their probes' links vanish with them: most
/// layers showing probe nodes hide server nodes, and a link drawn to one would
/// run off into empty canvas. In practice that leaves these links to the Network
/// layer, the only one showing servers and probes together.
fn gateway_positions(
    gateways: &Query<(&Transform, &NodeEntity, Option<&Visibility>), Without<ProbeNode>>,
) -> HashMap<InstanceId, Vec2> {
    gateways
        .iter()
        .filter(|(_, _, visibility)| is_visible(*visibility))
        .map(|(transform, node, _)| (node.instance_id, transform.translation.truncate()))
        .collect()
}

/// Every drawable link as `(probe position, gateway position, device id)`.
fn links<'a>(
    probes: &'a Query<(&Transform, &ProbeNode, Option<&Visibility>)>,
    gateway_positions: &'a HashMap<InstanceId, Vec2>,
) -> impl Iterator<Item = (Vec2, Vec2, u64)> + 'a {
    probes
        .iter()
        .filter(|(_, _, visibility)| is_visible(*visibility))
        .filter_map(|(transform, probe, _)| {
            gateway_positions
                .get(&probe.gateway)
                .map(|gateway| (transform.translation.truncate(), *gateway, probe.device_id))
        })
}

/// Draw a dashed line from `from` to `to`, with the dash pattern shifted by
/// `phase` world units toward `to`.
fn draw_dashes(gizmos: &mut Gizmos, from: Vec2, to: Vec2, phase: f32, color: Color) {
    let delta = to - from;
    let length = delta.length();
    let Some(direction) = delta.try_normalize() else {
        return;
    };

    let period = DASH_LENGTH + DASH_GAP;
    // Start a period early so the dash partway into the segment is clipped
    // rather than popping into existence at the endpoint.
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

/// How fast dashes travel for a given throughput.
fn dash_speed(bytes_per_second: f64) -> f32 {
    (DASH_BASE_SPEED + (bytes_per_second / 4096.0) as f32).min(DASH_MAX_SPEED)
}

/// Shortest distance from `point` to the segment `from`..`to`.
fn distance_to_segment(point: Vec2, from: Vec2, to: Vec2) -> f32 {
    let delta = to - from;
    let length_squared = delta.length_squared();
    if length_squared <= f32::EPSILON {
        return point.distance(from);
    }
    let t = ((point - from).dot(delta) / length_squared).clamp(0.0, 1.0);
    point.distance(from + delta * t)
}

/// The tooltip body for one link.
fn describe_link(device_id: u64, link: Option<&LinkTraffic>) -> String {
    let name = device_by_id(device_id)
        .map(|device| device.display_name())
        .unwrap_or_else(|| format!("Device {device_id}"));

    match link {
        Some(link) => format!(
            "{name}\n{}\n  ↓ {} from device\n  ↑ {} to device",
            link.label,
            format_rate(link.rx_bps),
            format_rate(link.tx_bps),
        ),
        None => format!("{name}\nNo active stream"),
    }
}

/// A byte rate at human scale.
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
