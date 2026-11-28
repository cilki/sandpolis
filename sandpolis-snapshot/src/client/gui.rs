//! GUI components for the Snapshot layer.
//!
//! The node panel lists an agent's partitions and stored snapshots with
//! per-row actions, and shows a progress bar while an operation runs. While
//! blocks are moving, the agent↔server link in the node graph is decorated
//! with an animated activity line.

use crate::client::{self, query_operations, query_snapshots};
use crate::streams::SnapshotMgmtRequest;
use crate::{SnapshotData, SnapshotDirection, SnapshotOperationData};
use bevy::prelude::*;
use gesso::ui::Activate;
use gesso::ui::bind::bind_text;
use gesso::ui::gauge::{GaugeValue, bind_gauge, gauge};
use gesso::ui::node_panel::{NodePanel, PanelCtx};
use gesso::ui::theme::{Role, Theme};
use gesso::ui::widgets::{button, column, heading, muted, row, text};
use sandpolis_client::gui::activity::{Activity, ActivityType, activity_line};
use sandpolis_client::gui::edges::{Edge, edge};
use sandpolis_client::gui::input::CurrentLayer;
use sandpolis_client::gui::node_panel::PanelTargetExt;
use sandpolis_client::gui::ui::layer::{LayerClientInfo, RegisterLayerClient};
use sandpolis_instance::network::NetworkManager;
use sandpolis_instance::{InstanceId, InstanceType, LayerName};
use sandpolis_inventory::hardware::disk::partition::PartitionData;
use std::hash::{DefaultHasher, Hash, Hasher};

/// The snapshot layer's node panel.
pub struct SnapshotPanel;

impl NodePanel for SnapshotPanel {
    fn build_summary(&self, ctx: &mut PanelCtx) {
        let Some(instance) = ctx.target.instance() else {
            return;
        };
        subscribe_panel(instance);

        let theme = ctx.theme;
        ctx.children(|p| {
            p.spawn((
                text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                bind_text(move || {
                    if let Some(op) = running_operation(instance) {
                        return operation_caption(&op);
                    }
                    let snapshots = query_snapshots(instance).unwrap_or_default();
                    match snapshots.len() {
                        0 => "No snapshots".into(),
                        1 => "1 snapshot".into(),
                        n => format!("{n} snapshots"),
                    }
                }),
            ));
        });
    }

    fn build_detail(&self, ctx: &mut PanelCtx) {
        let Some(instance) = ctx.target.instance() else {
            return;
        };
        subscribe_panel(instance);

        let theme = ctx.theme;
        ctx.children(|p| {
            // Progress of the running operation; reads as idle when none is.
            p.spawn((
                gauge(theme, "Operation", GaugeValue::default()),
                bind_gauge(move || match running_operation(instance) {
                    Some(op) => {
                        GaugeValue::new(op.progress, operation_caption(&op)).with_role(Role::Accent)
                    }
                    None => GaugeValue::new(0.0, "idle"),
                }),
            ));
            p.spawn((
                text(theme, "", theme.metrics.font_sm, Role::Error),
                bind_text(move || {
                    query_operations(instance)
                        .unwrap_or_default()
                        .iter()
                        .find_map(|op| op.error.clone())
                        .unwrap_or_default()
                }),
            ));

            p.spawn(heading(theme, "Partitions"));
            p.spawn((
                column(theme.metrics.space_xs),
                SnapshotPanelList {
                    instance,
                    rendered: 0,
                },
            ));

            p.spawn(text(
                theme,
                format!("Instance: {instance}"),
                theme.metrics.font_sm,
                Role::TextMuted,
            ));
        });
    }
}

/// Subscribe to everything the panel renders.
fn subscribe_panel(instance: InstanceId) {
    client::subscribe(instance);
    sandpolis_client::sync::subscribe(
        sandpolis_inventory::client::partition_model_id(),
        Some(instance),
    );
}

/// The operation currently running against an instance, if any.
fn running_operation(instance: InstanceId) -> Option<SnapshotOperationData> {
    query_operations(instance)
        .unwrap_or_default()
        .into_iter()
        .find(|op| op.state.active())
}

fn operation_caption(op: &SnapshotOperationData) -> String {
    let verb = match op.direction {
        SnapshotDirection::Create => "Capturing",
        SnapshotDirection::Apply => "Restoring",
    };
    format!(
        "{verb} — {:.0}% — {} / {}",
        (op.progress * 100.0).clamp(0.0, 100.0),
        format_bytes(op.bytes_transferred),
        format_bytes(op.total_bytes),
    )
}

/// The panel's partition/snapshot rows, rebuilt when the underlying data
/// changes. Rows carry action buttons, which the frame-driven table widget
/// can't, so this list drives its own children.
#[derive(Component)]
struct SnapshotPanelList {
    instance: InstanceId,
    /// Hash of the content currently spawned, so unchanged data isn't rebuilt.
    rendered: u64,
}

fn drive_snapshot_lists(
    mut commands: Commands,
    theme: Res<Theme>,
    mut lists: Query<(Entity, &mut SnapshotPanelList)>,
) {
    for (entity, mut list) in lists.iter_mut() {
        let instance = list.instance;
        let partitions =
            sandpolis_inventory::client::query_partitions(instance).unwrap_or_default();
        let snapshots = query_snapshots(instance).unwrap_or_default();
        let busy = running_operation(instance).is_some();

        let mut hasher = DefaultHasher::new();
        busy.hash(&mut hasher);
        for p in &partitions {
            (&p.name, &p.uuid, p.size, &p.mount).hash(&mut hasher);
        }
        for s in &snapshots {
            (&s.uuid, &s.partition_uuid, s.stored_size, &s.label).hash(&mut hasher);
        }
        let hash = hasher.finish();
        if hash == list.rendered {
            continue;
        }
        list.rendered = hash;

        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|p| {
            if partitions.is_empty() {
                p.spawn(muted(&theme, "No partition data", theme.metrics.font_md));
            }
            for partition in &partitions {
                spawn_partition_row(p, &theme, instance, partition, &snapshots, busy);
            }
        });
    }
}

fn spawn_partition_row(
    parent: &mut ChildSpawnerCommands,
    theme: &Theme,
    instance: InstanceId,
    partition: &PartitionData,
    snapshots: &[SnapshotData],
    busy: bool,
) {
    let label = format!(
        "{} — {}{}",
        partition.name,
        format_bytes(partition.size),
        if partition.mount.is_empty() {
            String::new()
        } else {
            format!(" on {}", partition.mount)
        },
    );

    parent
        .spawn(row(theme.metrics.space_sm))
        .with_children(|r| {
            r.spawn(text(theme, label, theme.metrics.font_md, Role::Text));
            if partition.uuid.is_empty() {
                r.spawn(muted(theme, "(no partition uuid)", theme.metrics.font_sm));
            } else if !busy {
                let agent = instance;
                let uuid = partition.uuid.clone();
                r.spawn(button(theme, "Capture"))
                    .observe(move |_: On<Activate>| {
                        let _ = client::request(SnapshotMgmtRequest::Create {
                            agent,
                            partition_uuid: uuid.clone(),
                            label: None,
                        });
                    });
            }
        });

    for snapshot in snapshots
        .iter()
        .filter(|s| s.partition_uuid == partition.uuid && !partition.uuid.is_empty())
    {
        spawn_snapshot_row(parent, theme, instance, snapshot, snapshots, busy);
    }
}

fn spawn_snapshot_row(
    parent: &mut ChildSpawnerCommands,
    theme: &Theme,
    instance: InstanceId,
    snapshot: &SnapshotData,
    snapshots: &[SnapshotData],
    busy: bool,
) {
    let short_uuid: String = snapshot.uuid.chars().take(8).collect();
    let label = format!(
        "  {} {} — {} stored",
        snapshot._creation.timestamp().format("%Y-%m-%d %H:%M"),
        snapshot.label.as_deref().unwrap_or(&short_uuid),
        format_bytes(snapshot.stored_size),
    );
    let leaf = !snapshots
        .iter()
        .any(|other| other.parent.as_deref() == Some(&snapshot.uuid));

    parent
        .spawn(row(theme.metrics.space_sm))
        .with_children(|r| {
            r.spawn(text(theme, label, theme.metrics.font_sm, Role::TextMuted));
            if busy {
                return;
            }
            let agent = instance;
            let partition_uuid = snapshot.partition_uuid.clone();
            let snapshot_uuid = snapshot.uuid.clone();
            {
                let partition_uuid = partition_uuid.clone();
                let snapshot_uuid = snapshot_uuid.clone();
                r.spawn(button(theme, "Apply"))
                    .observe(move |_: On<Activate>| {
                        let _ = client::request(SnapshotMgmtRequest::Apply {
                            agent,
                            partition_uuid: partition_uuid.clone(),
                            snapshot_uuid: snapshot_uuid.clone(),
                        });
                    });
            }
            if leaf {
                r.spawn(button(theme, "Delete"))
                    .observe(move |_: On<Activate>| {
                        let _ = client::request(SnapshotMgmtRequest::Delete {
                            agent,
                            partition_uuid: partition_uuid.clone(),
                            snapshot_uuid: snapshot_uuid.clone(),
                        });
                    });
            }
        });
}

/// Decorate the agent↔server link with an edge and an animated activity line
/// while a snapshot operation runs. Blocks flow toward the server on a create
/// and toward the agent on an apply, and the line moves the same way.
fn update_snapshot_activity(
    mut commands: Commands,
    current_layer: Res<CurrentLayer>,
    network: Res<NetworkManager>,
    edges: Query<(Entity, &Edge)>,
    activities: Query<(Entity, &Activity)>,
    mut subscribed: Local<bool>,
) {
    // Decorations belong to this layer; the shared cleanup systems cull them
    // when the picker moves elsewhere.
    if **current_layer != "Snapshot" {
        return;
    }

    // Operations for agents whose panels were never opened still need to
    // replicate down for their links to light up.
    if !*subscribed {
        *subscribed = true;
        sandpolis_client::sync::subscribe(client::operation_model_id(), None);
    }

    let operations = client::active_operations().unwrap_or_default();
    let topology =
        sandpolis_client::gui::queries::query_network_topology(&network).unwrap_or_default();

    let mut desired: Vec<(InstanceId, InstanceId)> = Vec::new();
    for op in &operations {
        let agent = op._instance_id;
        let Some(server) = topology.iter().find_map(|edge| {
            if edge.from == agent {
                Some(edge.to)
            } else if edge.to == agent {
                Some(edge.from)
            } else {
                None
            }
        }) else {
            continue;
        };
        desired.push(match op.direction {
            SnapshotDirection::Create => (agent, server),
            SnapshotDirection::Apply => (server, agent),
        });
    }

    for &(from, to) in &desired {
        if !edges
            .iter()
            .any(|(_, e)| e.layer.name() == "Snapshot" && e.from == from && e.to == to)
        {
            commands.spawn(edge(from, to, LayerName::from("Snapshot"), &current_layer));
        }
        if !activities
            .iter()
            .any(|(_, a)| a.activity_type == ActivityType::Snapshot && a.from == from && a.to == to)
        {
            commands.spawn(activity_line(from, to, 0.4, ActivityType::Snapshot));
        }
    }

    for (entity, edge) in edges.iter() {
        if edge.layer.name() == "Snapshot" && !desired.contains(&(edge.from, edge.to)) {
            commands.entity(entity).despawn();
        }
    }
    for (entity, activity) in activities.iter() {
        if activity.activity_type == ActivityType::Snapshot
            && !desired.contains(&(activity.from, activity.to))
        {
            commands.entity(entity).despawn();
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The snapshot layer's client plugin.
pub struct SnapshotClientPlugin;

impl Plugin for SnapshotClientPlugin {
    fn build(&self, app: &mut App) {
        app.register_layer_client(
            LayerClientInfo::new(LayerName::from("Snapshot"), "Cold partition snapshots")
                .with_panel(SnapshotPanel)
                .with_visible_instance_types(&[InstanceType::Agent, InstanceType::Server]),
        )
        .add_systems(Update, (drive_snapshot_lists, update_snapshot_activity));
    }
}
