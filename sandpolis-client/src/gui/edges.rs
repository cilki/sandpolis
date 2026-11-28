use crate::gui::input::CurrentLayer;
use crate::gui::node::node_id;
use crate::gui::queries;
use bevy::prelude::*;
use gesso::link::Link;
use sandpolis_instance::InstanceId;
use sandpolis_instance::LayerName;

/// Component representing an edge between two nodes
///
/// This is the half of an edge that knows about instances and layers. gesso
/// draws the [`Link`] spawned alongside it, so always spawn one with [`edge`].
#[derive(Component)]
pub struct Edge {
    pub from: InstanceId,
    pub to: InstanceId,
    pub layer: LayerName,
}

/// Component for edge labels (network stats, etc.)
#[derive(Component)]
pub struct EdgeLabel {
    pub edge_entity: Entity,
    pub text: String,
}

/// Everything an edge needs: the [`Edge`] itself, the [`Link`] gesso draws, and
/// a visibility that already matches `current_layer`.
pub fn edge(
    from: InstanceId,
    to: InstanceId,
    layer: LayerName,
    current_layer: &LayerName,
) -> impl Bundle {
    (
        Link {
            from: node_id(from),
            to: node_id(to),
            color: edge_color(&layer),
        },
        edge_visibility(&layer, current_layer),
        Edge { from, to, layer },
    )
}

/// Color based on layer
fn edge_color(layer: &LayerName) -> Color {
    match layer.name() {
        "Network" => Color::srgb(0.3, 0.8, 1.0),    // Cyan
        "Filesystem" => Color::srgb(0.3, 1.0, 0.3), // Green
        "Desktop" => Color::srgb(1.0, 0.5, 0.3),    // Orange
        "Snapshot" => Color::srgb(0.3, 0.9, 0.8),   // Teal
        _ => Color::srgb(0.6, 0.6, 0.6),            // Gray
    }
}

/// Only edges for the current layer are rendered
fn edge_visibility(layer: &LayerName, current_layer: &LayerName) -> Visibility {
    if layer == current_layer {
        Visibility::Visible
    } else {
        Visibility::Hidden
    }
}

/// Create/update edges based on current layer
/// Queries database for network topology and spawns Edge entities
pub fn update_edges_for_layer(
    mut commands: Commands,
    current_layer: Res<CurrentLayer>,
    edge_query: Query<(Entity, &Edge)>,
    network_manager: Res<sandpolis_instance::network::NetworkManager>,
) {
    // Only update when layer changes or is first added
    if !current_layer.is_changed() && !current_layer.is_added() {
        return;
    }

    // Despawn all existing edges
    for (entity, _) in edge_query.iter() {
        commands.entity(entity).despawn();
    }

    // Query database for edges relevant to current layer
    match current_layer.name() {
        "Network" => {
            // Query network topology from database
            if let Ok(network_edges) = queries::query_network_topology(&network_manager) {
                for net_edge in network_edges {
                    commands.spawn(edge(
                        net_edge.from,
                        net_edge.to,
                        LayerName::from("Network"),
                        &current_layer,
                    ));
                }
            }
        }
        "Filesystem" => {
            // TODO: Query filesystem connections (file transfer paths)
            // For now, no edges
        }
        "Desktop" => {
            // TODO: Query desktop streaming connections
            // For now, no edges
        }
        _ => {
            // Other layers might not have edges
        }
    }
}

/// System to show/hide edges based on visibility
///
/// gesso's link renderer skips hidden links, so this is what keeps an edge off
/// screen while its layer isn't the current one. It has to run before
/// `gesso::link::render_links` each frame for a layer change to take effect in
/// the frame it happens.
pub fn update_edge_visibility(
    mut edge_query: Query<(&mut Visibility, &Edge)>,
    current_layer: Res<CurrentLayer>,
) {
    for (mut visibility, edge) in edge_query.iter_mut() {
        let wanted = edge_visibility(&edge.layer, &current_layer);
        if *visibility != wanted {
            *visibility = wanted;
        }
    }
}
