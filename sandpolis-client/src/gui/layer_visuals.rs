use crate::gui::input::CurrentLayer;
use crate::gui::node::NodeEntity;
use crate::gui::queries;
#[allow(unused_imports)] // referenced from doc links below
use crate::gui::ui::layer::LayerClientInfo;
use crate::gui::ui::layer::LayerRegistry;
use bevy::prelude::*;
use bevy_svg::prelude::{Origin, Svg2d};
use gesso::node::{NeedsScaling, NodeSvg};
use sandpolis_instance::{InstanceType, LayerName};

/// Update node SVG images when layer changes or for newly spawned nodes
pub fn update_node_svgs_for_layer(
    mut commands: Commands,
    current_layer: Res<CurrentLayer>,
    asset_server: Res<AssetServer>,
    instance_manager: Res<sandpolis_instance::InstanceManager>,
    registry: Res<LayerRegistry>,
    node_query: Query<(Entity, &NodeEntity)>,
    children_query: Query<&Children>,
    svg_query: Query<Entity, With<NodeSvg>>,
    mut has_run: Local<bool>,
) {
    // Run when layer changes OR on first execution (to initialize newly spawned nodes)
    let should_run = current_layer.is_changed() || !*has_run;

    if !should_run {
        return;
    }

    // Mark that we've run at least once
    *has_run = true;

    // Update each node's SVG based on current layer
    for (entity, node_entity) in node_query.iter() {
        let svg_path = get_layer_svg_path(
            &registry,
            &current_layer,
            &instance_manager,
            node_entity.instance_id,
        );

        // Find the SVG child entity
        if let Ok(children) = children_query.get(entity) {
            for &child in children {
                if svg_query.contains(child) {
                    // Update the SVG child
                    commands.entity(child).insert((
                        Svg2d(asset_server.load(svg_path)),
                        Origin::Center,
                        NeedsScaling,
                    ));
                    break;
                }
            }
        }
    }
}

/// Get the appropriate SVG path for a node based on the current layer.
///
/// A layer picks its own icon by registering [`LayerClientInfo::with_node_icon`];
/// this crate can't do it per-layer itself, because the layer crates depend on
/// it rather than the other way around.
fn get_layer_svg_path(
    registry: &LayerRegistry,
    layer: &LayerName,
    instance_manager: &sandpolis_instance::InstanceManager,
    instance_id: sandpolis_instance::InstanceId,
) -> &'static str {
    if let Some(icon) = registry.get(layer).and_then(|info| info.node_icon.as_ref()) {
        return icon(instance_id);
    }

    match layer.name() {
        "Network" => {
            // Distinguish between servers and agents
            // TODO: Query instance type from database
            // For now, use the local instance as reference
            if instance_id == instance_manager.instance_id {
                "network/agent.svg"
            } else {
                "network/server.svg"
            }
        }

        _ => {
            // Default to OS icons for other layers
            if let Ok(metadata) = queries::query_instance_metadata(instance_id) {
                super::node::get_os_image(metadata.os_type)
            } else {
                "os/Unknown.svg"
            }
        }
    }
}

/// Update node colors based on specific states
pub fn update_node_colors_for_layer(
    current_layer: Res<CurrentLayer>,
    network_manager: Res<sandpolis_instance::network::NetworkManager>,
    registry: Res<LayerRegistry>,
    mut node_query: Query<(&NodeEntity, &mut Sprite)>,
) {
    // Only update when layer changes
    if !current_layer.is_changed() {
        return;
    }

    for (node_entity, mut sprite) in node_query.iter_mut() {
        // Get specific color tint
        let color = get_layer_color_tint(
            &registry,
            &current_layer,
            &network_manager,
            node_entity.instance_id,
        );
        sprite.color = color;
    }
}

/// Get color tint for a node based on layer and state.
///
/// A layer tints its own nodes by registering
/// [`LayerClientInfo::with_node_tint`]; see [`get_layer_svg_path`] for why this
/// can't live here.
fn get_layer_color_tint(
    registry: &LayerRegistry,
    layer: &LayerName,
    network_manager: &sandpolis_instance::network::NetworkManager,
    instance_id: sandpolis_instance::InstanceId,
) -> Color {
    if let Some(tint) = registry.get(layer).and_then(|info| info.node_tint.as_ref()) {
        return tint(instance_id);
    }

    match layer.name() {
        "Network" => {
            // Color based on connection latency
            if let Ok(stats) = queries::query_network_stats(network_manager, instance_id) {
                if let Some(latency) = stats.latency_ms {
                    if latency < 50 {
                        Color::srgb(0.7, 1.0, 0.7) // Green - good connection
                    } else if latency < 150 {
                        Color::srgb(1.0, 1.0, 0.7) // Yellow - moderate
                    } else {
                        Color::srgb(1.0, 0.7, 0.7) // Red - poor connection
                    }
                } else {
                    Color::srgb(0.7, 0.7, 0.7) // Gray - no connection
                }
            } else {
                Color::WHITE
            }
        }

        _ => Color::WHITE, // No tint for other layers
    }
}

/// Map a 0-100 utilization percentage onto the shared green / yellow / red
/// gradient that every layer's node tint uses.
pub fn utilization_tint(used: u64, total: u64) -> Color {
    let percent = if total > 0 {
        (used as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    if percent < 70.0 {
        Color::srgb(0.7, 1.0, 0.7) // Green
    } else if percent < 90.0 {
        Color::srgb(1.0, 1.0, 0.7) // Yellow
    } else {
        Color::srgb(1.0, 0.7, 0.7) // Red
    }
}

/// Update node visibility based on the current layer's visible instance types.
///
/// Each layer can specify which instance types (Server, Agent, Client) should be
/// visible when that layer is active. This system hides nodes that don't match
/// the current layer's filter criteria.
pub fn update_node_visibility_for_layer(
    current_layer: Res<CurrentLayer>,
    registry: Res<LayerRegistry>,
    mut node_query: Query<(&NodeEntity, &mut Visibility)>,
    added_nodes: Query<(), Added<NodeEntity>>,
) {
    // Only update when the layer changes or a node spawns; a node spawned
    // mid-session must not keep its spawn-time visibility on a filtered layer.
    if !current_layer.is_changed() && added_nodes.is_empty() {
        return;
    }

    // Get the visible instance types for the current layer
    let visible_types = get_visible_instance_types_for_layer(&registry, &current_layer);

    for (node_entity, mut visibility) in node_query.iter_mut() {
        let instance_id = node_entity.instance_id;

        // Check if this node's instance type matches any of the visible types
        let should_be_visible = visible_types
            .iter()
            .any(|instance_type| instance_id.is_type(*instance_type));

        *visibility = if should_be_visible {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

/// Get the visible instance types for a layer.
///
/// Every layer declares this on its registered [`LayerClientInfo`]. A layer with
/// no registration at all can only be reached by setting [`CurrentLayer`] by
/// hand, so it falls back to showing everything rather than to a second copy of
/// the per-layer policy.
fn get_visible_instance_types_for_layer(
    registry: &LayerRegistry,
    layer: &LayerName,
) -> &'static [InstanceType] {
    registry
        .get(layer)
        .map(|info| info.visible_instance_types)
        .unwrap_or(&[
            InstanceType::Server,
            InstanceType::Agent,
            InstanceType::Client,
        ])
}
