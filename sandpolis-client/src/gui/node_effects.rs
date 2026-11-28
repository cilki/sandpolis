//! Which nodes are offline.
//!
//! The node effects themselves (the selection ring, the offline scrim, the
//! credential warning) are drawn by `gesso::node_effects` from marker
//! components. This module is what decides an instance node gets the
//! [`Offline`] marker.

use crate::gui::node::NodeEntity;
use bevy::prelude::*;
use gesso::node::Offline;
use sandpolis_instance::InstanceManager;
use sandpolis_instance::network::{NetworkManager, liveness};

/// Mark nodes the client currently can't reach.
///
/// The servers this client dials are the part it knows first-hand; everything
/// else — agents, and servers it has no connection to — comes from the liveness
/// rows those servers replicate, resolved outward from the direct connections by
/// [`liveness::reachable`].
pub fn update_offline_markers(
    mut commands: Commands,
    instance_manager: Res<InstanceManager>,
    network: Res<NetworkManager>,
    nodes: Query<(Entity, &NodeEntity, Has<Offline>)>,
) {
    let online = liveness::reachable(
        network.liveness.iter().map(|row| row.read().clone()),
        crate::sync::connected_instances(),
    );

    for (entity, node, marked) in nodes.iter() {
        // We're never out of touch with ourselves.
        let offline =
            node.instance_id != instance_manager.instance_id && !online.contains(&node.instance_id);

        if offline && !marked {
            commands.entity(entity).insert(Offline);
        } else if !offline && marked {
            commands.entity(entity).remove::<Offline>();
        }
    }
}
