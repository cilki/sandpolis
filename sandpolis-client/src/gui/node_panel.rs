//! What Sandpolis puts in node panels.
//!
//! The panel host — the chrome, the anchoring, collapsing and expanding — is
//! `gesso::node_panel`. It builds every panel's body from whatever
//! [`ActiveNodePanel`] holds, and this module keeps that pointed at the
//! current layer's [`NodePanel`](gesso::ui::node_panel::NodePanel), names
//! instance nodes, and picks their icons.

use crate::gui::input::CurrentLayer;
use crate::gui::layer_ui::layer_icon_path;
use crate::gui::node::{NodeEntity, instance_icon_path, instance_id};
use crate::gui::queries;
use crate::gui::ui::layer::LayerRegistry;
use bevy::prelude::*;
use gesso::node::{NodeIdentity, SubNode};
use gesso::node_panel::{ActiveNodePanel, manage_node_panels, update_panel_verbosity};
use gesso::ui::node_panel::PanelTarget;
use sandpolis_instance::{InstanceId, LayerName};
use std::sync::Arc;

/// Reads the instance out of a [`PanelTarget`].
pub trait PanelTargetExt {
    /// The instance this node stands for, when it is one. A probe device
    /// borrows its gateway server's `InstanceId` (and adds a `sub`), while an
    /// account node has no instance at all.
    fn instance(&self) -> Option<InstanceId>;
}

impl PanelTargetExt for PanelTarget {
    fn instance(&self) -> Option<InstanceId> {
        self.node.and_then(instance_id)
    }
}

/// Installs the layer-driven half of the node panels.
pub struct LayerPanelPlugin;

impl Plugin for LayerPanelPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (update_instance_node_identity, sync_active_node_panel)
                .after(update_panel_verbosity)
                .before(manage_node_panels),
        );
    }
}

/// The identity to show for an instance node.
pub fn node_identity(instance_id: InstanceId) -> String {
    queries::query_instance_metadata(instance_id)
        .ok()
        .and_then(|m| m.hostname)
        .unwrap_or_else(|| format!("Node {}", instance_id))
}

/// Name instance nodes from their metadata.
///
/// Only newly spawned nodes are named: resolving metadata is not cheap enough to
/// redo every frame for every node, and a host's name doesn't change under it.
/// Nodes that stand for something else — probe devices, accounts — are named by
/// the layer that owns them, so they're skipped here.
pub fn update_instance_node_identity(
    mut commands: Commands,
    nodes: Query<(Entity, &NodeEntity), (Added<NodeEntity>, Without<SubNode>)>,
) {
    for (entity, node) in &nodes {
        commands
            .entity(entity)
            .insert(NodeIdentity(node_identity(node.instance_id)));
    }
}

/// Point the panel host at the current layer: its panel builds the bodies, and
/// its icon is the fallback for nodes that aren't instances.
///
/// Keyed on the layer's name, so the host rebuilds every panel when the layer
/// changes and at no other time.
pub fn sync_active_node_panel(
    current_layer: Res<CurrentLayer>,
    registry: Res<LayerRegistry>,
    mut active: ResMut<ActiveNodePanel>,
) {
    if !current_layer.is_changed() && !registry.is_changed() {
        return;
    }

    let layer = (**current_layer).clone();
    *active = ActiveNodePanel {
        key: layer.name().to_string(),
        panel: registry.get(&layer).and_then(|info| info.panel.clone()),
        fallback_icon: Some(Arc::new(move |target| {
            fallback_icon_path(target, &layer).to_string()
        })),
    };
}

/// The icon a node's panel shows when the node doesn't override it with
/// [`PanelIcon`](gesso::node::PanelIcon).
///
/// An instance is worth naming by *what it is* — a server, an agent, a client —
/// which tells the user more than the active layer's icon, already shown by the
/// layer indicator. Nodes standing in for something finer-grained than an
/// instance (a probe device, an account) have no instance type of their own, so
/// they keep the layer icon. A probe device borrows its gateway's `InstanceId`,
/// which is why the `sub` check has to come before the id is consulted.
fn fallback_icon_path(target: PanelTarget, layer: &LayerName) -> &'static str {
    match target
        .instance()
        .filter(|_| target.sub.is_none())
        .map(|instance| instance.instance_type())
    {
        Some(instance_type) => instance_icon_path(instance_type),
        None => layer_icon_path(layer),
    }
}

#[cfg(test)]
mod test_node_panel {
    use super::*;
    use crate::gui::node::node_id;
    use sandpolis_instance::InstanceType;

    /// A target standing in for an instance of the given type.
    fn instance_target(instance_type: InstanceType) -> PanelTarget {
        PanelTarget::node(node_id(InstanceId::random(instance_type)))
    }

    #[test]
    fn instances_are_iconed_by_type_whatever_the_layer() {
        for layer in ["Network", "Shell", "Filesystem"] {
            let layer = LayerName(layer.to_string());
            for (instance_type, want) in [
                (InstanceType::Agent, "network/agent.svg"),
                (InstanceType::Client, "network/client.svg"),
                (InstanceType::Server, "network/server.svg"),
            ] {
                assert_eq!(
                    fallback_icon_path(instance_target(instance_type), &layer),
                    want
                );
            }
        }
    }

    #[test]
    fn sub_nodes_keep_the_layer_icon() {
        let layer = LayerName("Probe".to_string());

        // A probe device carries its gateway's `InstanceId`, so keying on the id
        // alone would give every device on a server the server's icon.
        assert_eq!(
            fallback_icon_path(
                PanelTarget {
                    sub: Some(7),
                    ..instance_target(InstanceType::Server)
                },
                &layer
            ),
            "layer/Probe.svg"
        );

        // An account node has no instance at all.
        assert_eq!(
            fallback_icon_path(
                PanelTarget {
                    node: None,
                    sub: Some(7)
                },
                &LayerName("Account".to_string())
            ),
            "layer/Account.svg"
        );
    }
}
