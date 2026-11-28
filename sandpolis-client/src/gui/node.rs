//! Instance nodes: what Sandpolis adds to gesso's nodes.

use bevy::prelude::*;
use gesso::node::{NodeBundle, NodeId, node_svg};
use sandpolis_instance::{InstanceId, InstanceType};

/// Component that marks an entity as representing an instance node.
///
/// gesso keys its own systems on [`NodeId`], which says nothing about what a
/// node is. Every entity carrying this carries the matching `NodeId` too (see
/// [`node_id`]); this is the half that still knows it's an instance.
#[derive(Component)]
pub struct NodeEntity {
    pub instance_id: InstanceId,
}

/// The [`NodeId`] gesso knows an instance by.
///
/// The id's wire value, so nodes sort the same way as the instances they stand
/// for and [`instance_id`] can recover the instance.
pub fn node_id(instance_id: InstanceId) -> NodeId {
    NodeId(instance_id.to_wire())
}

/// The instance a [`NodeId`] stands for, if it was made by [`node_id`].
pub fn instance_id(node_id: NodeId) -> Option<InstanceId> {
    InstanceId::from_wire(node_id.0).ok()
}

#[derive(Bundle)]
pub struct Node {
    pub id: InstanceId,
    pub node_entity: NodeEntity,
    pub node: NodeBundle,
}

pub fn spawn_node(
    asset_server: &AssetServer,
    commands: &mut Commands,
    instance_id: InstanceId,
    _os_type: os_info::Type,
    _is_server: bool,
    position: Option<Vec3>,
) {
    // Start with a placeholder/default SVG that will be replaced by layer system
    // This ensures the correct SVG is loaded based on the current layer
    let svg_path = "network/agent.svg".to_string();

    // Spawn parent node with physics components
    let node_entity = commands
        .spawn((Node {
            id: instance_id,
            node_entity: NodeEntity { instance_id },
            node: NodeBundle::new(node_id(instance_id), position),
        },))
        .id();

    // Spawn SVG as a child entity
    let svg = node_svg(asset_server, svg_path);
    commands.entity(node_entity).with_children(|parent| {
        parent.spawn(svg);
    });
}

/// SVG asset path for an instance's type icon.
pub fn instance_icon_path(instance_type: InstanceType) -> &'static str {
    match instance_type {
        InstanceType::Agent => "network/agent.svg",
        InstanceType::Client => "network/client.svg",
        InstanceType::Server => "network/server.svg",
    }
}

pub fn get_os_image(os_type: os_info::Type) -> &'static str {
    match os_type {
        os_info::Type::Android => "os/Android.svg",
        os_info::Type::Macos => "os/macOS.svg",
        os_info::Type::Windows => "os/Windows.svg",
        os_info::Type::Arch => "os/Arch Linux.svg",
        os_info::Type::NixOS => "os/NixOS.svg",
        os_info::Type::SUSE => "os/SUSE Linux Enterprise Server.svg",
        os_info::Type::openSUSE => "os/SUSE Linux Enterprise Server.svg",
        _ => "os/Unknown.svg",
    }
}

#[cfg(test)]
mod test_node {
    use super::*;

    #[test]
    fn node_ids_round_trip_and_keep_instance_order() {
        let mut ids: Vec<InstanceId> = [
            InstanceType::Server,
            InstanceType::Agent,
            InstanceType::Client,
            InstanceType::Agent,
        ]
        .into_iter()
        .map(InstanceId::random)
        .collect();

        for id in &ids {
            assert_eq!(instance_id(node_id(*id)), Some(*id));
        }

        // Anything gesso orders by `NodeId` should come out the way the
        // instances themselves sort.
        let mut by_node_id = ids.clone();
        by_node_id.sort_by_key(|id| node_id(*id));
        ids.sort();
        assert_eq!(by_node_id, ids);
    }
}
