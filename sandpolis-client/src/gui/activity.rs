use crate::gui::edges::Edge;
use crate::gui::input::CurrentLayer;
use crate::gui::node::node_id;
use bevy::prelude::*;
use gesso::activity::ActivityLine;
use sandpolis_instance::InstanceId;

/// What an activity line stands for: which instances it runs between, and what
/// kind of activity it shows.
///
/// gesso moves the [`ActivityLine`] spawned alongside it, so always spawn one
/// with [`activity_line`].
#[derive(Component)]
pub struct Activity {
    /// Source instance ID
    pub from: InstanceId,
    /// Destination instance ID
    pub to: InstanceId,
    /// Activity type identifier (for different colors/sprites)
    pub activity_type: ActivityType,
}

/// Types of activities that can be visualized
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ActivityType {
    FileTransfer,
    NetworkTraffic,
    ShellCommand,
    DesktopStream,
    /// Snapshot blocks moving between an agent and its server
    Snapshot,
}

impl ActivityType {
    /// Get color for this activity type
    pub fn color(&self) -> Color {
        match self {
            ActivityType::FileTransfer => Color::srgb(0.3, 0.8, 0.3), // Green
            ActivityType::NetworkTraffic => Color::srgb(0.3, 0.6, 1.0), // Blue
            ActivityType::ShellCommand => Color::srgb(1.0, 0.8, 0.2), // Yellow
            ActivityType::DesktopStream => Color::srgb(0.9, 0.3, 0.9), // Purple
            ActivityType::Snapshot => Color::srgb(0.3, 0.9, 0.8),     // Teal
        }
    }

    /// Get size for this activity type
    pub fn size(&self) -> f32 {
        match self {
            ActivityType::FileTransfer => 8.0,
            ActivityType::NetworkTraffic => 5.0,
            ActivityType::ShellCommand => 6.0,
            ActivityType::DesktopStream => 7.0,
            ActivityType::Snapshot => 7.0,
        }
    }
}

/// Everything an activity line needs: the [`Activity`] itself, the
/// [`ActivityLine`] gesso moves from one node to the other at `speed` (the
/// fraction of the way covered per second), and the sprite it's drawn with.
pub fn activity_line(
    from: InstanceId,
    to: InstanceId,
    speed: f32,
    activity_type: ActivityType,
) -> impl Bundle {
    (
        Activity {
            from,
            to,
            activity_type,
        },
        ActivityLine {
            from: node_id(from),
            to: node_id(to),
            progress: 0.0,
            speed,
        },
        Sprite {
            color: activity_type.color(),
            custom_size: Some(Vec2::splat(activity_type.size())),
            ..default()
        },
        Transform::default(),
    )
}

/// Spawn activity lines for active file transfers
pub fn spawn_transfer_activity_lines(
    mut commands: Commands,
    existing_activities: Query<&Activity>,
) {
    // Query database for active transfers
    let Ok(transfers) = super::queries::query_active_transfers() else {
        return;
    };

    // Check which transfers already have activity lines
    for transfer in transfers {
        // Skip if activity line already exists for this transfer
        let exists = existing_activities.iter().any(|activity| {
            activity.from == transfer.from
                && activity.to == transfer.to
                && activity.activity_type == ActivityType::FileTransfer
        });

        if exists {
            continue;
        }

        // Spawn new activity line: 30% of edge per second
        commands.spawn(activity_line(
            transfer.from,
            transfer.to,
            0.3,
            ActivityType::FileTransfer,
        ));
    }
}

/// Despawn activity lines for completed transfers
pub fn despawn_completed_activity_lines(
    mut commands: Commands,
    activity_query: Query<(Entity, &Activity)>,
) {
    // Get list of active transfers
    let Ok(active_transfers) = super::queries::query_active_transfers() else {
        return;
    };

    // Check each activity line
    for (entity, activity) in activity_query.iter() {
        if activity.activity_type != ActivityType::FileTransfer {
            continue;
        }

        // Check if transfer still exists
        let transfer_exists = active_transfers
            .iter()
            .any(|transfer| transfer.from == activity.from && transfer.to == activity.to);

        // Despawn if transfer is complete
        if !transfer_exists {
            commands.entity(entity).despawn();
        }
    }
}

/// Spawn network traffic activity lines (for demonstration)
pub fn spawn_network_activity_lines(
    mut commands: Commands,
    edges: Query<&Edge>,
    existing_activities: Query<&Activity>,
    current_layer: Res<CurrentLayer>,
) {
    // Only spawn on Network layer
    if **current_layer != "Network" {
        return;
    }

    // Spawn activity line for each edge (if not already exists)
    for edge in edges.iter() {
        // Skip if activity line already exists
        let exists = existing_activities.iter().any(|activity| {
            activity.from == edge.from
                && activity.to == edge.to
                && activity.activity_type == ActivityType::NetworkTraffic
        });

        if exists {
            continue;
        }

        // Spawn network traffic indicator, faster than file transfers
        commands.spawn(activity_line(
            edge.from,
            edge.to,
            0.5,
            ActivityType::NetworkTraffic,
        ));
    }
}

/// Despawn activity lines when switching away from their layer
pub fn cleanup_layer_activity_lines(
    mut commands: Commands,
    activity_query: Query<(Entity, &Activity)>,
    current_layer: Res<CurrentLayer>,
) {
    // Only keep activity lines relevant to current layer. A line belongs to the
    // layer that raises it; if that layer isn't in this build nothing spawns the
    // line in the first place, so there's nothing to gate on here.
    for (entity, activity) in activity_query.iter() {
        let should_keep = match activity.activity_type {
            ActivityType::FileTransfer => **current_layer == "Filesystem",
            ActivityType::NetworkTraffic => **current_layer == "Network",
            ActivityType::ShellCommand => **current_layer == "Shell",
            ActivityType::DesktopStream => **current_layer == "Desktop",
            ActivityType::Snapshot => **current_layer == "Snapshot",
        };

        if !should_keep {
            commands.entity(entity).despawn();
        }
    }
}
