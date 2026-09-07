//! World-space visual effects drawn on nodes.
//!
//! Each effect is a mesh spawned as a child of the node it decorates, so it
//! follows the node for free while dragging and layout are running:
//!
//! - a single selected node gets an animated dashed ring
//! - two or more selected nodes get a flat shading disc each, which reads better
//!   as a batch than a ring per node
//! - a node the client can't reach gets a dimming scrim
//!
//! Selection *state* (the [`Selected`] marker, the click handling, the count
//! badge) lives in [`super::drag`]; this module only renders it.

use crate::gui::node::{NeedsCredentials, NodeEntity, NodeHitbox, Offline, Selected};
use crate::gui::ui::theme::{Role, Theme};
use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use sandpolis_instance::InstanceManager;
use sandpolis_instance::network::{NetworkManager, liveness};

/// How far the shading disc extends past the node's hitbox.
const RING_MARGIN: f32 = 5.0;

/// Inner radius of the dashed ring, past the node's hitbox.
const DASH_RING_INNER: f32 = 4.0;

/// Outer radius of the dashed ring, past the node's hitbox.
const DASH_RING_OUTER: f32 = 8.0;

/// Number of dashes making up the ring.
const DASH_COUNT: usize = 12;

/// How much of each dash's slice the dash itself fills; the rest is the gap.
const DASH_DUTY: f32 = 0.5;

/// Segments per dash. A dash only spans 15°, but subdividing keeps its inner and
/// outer edges from visibly flattening.
const SEGMENTS_PER_DASH: usize = 3;

/// How fast the dashed ring turns.
const SPIN_RADIANS_PER_SEC: f32 = 0.6;

/// Opacity of the selection ring and shading disc.
const SELECTION_ALPHA: f32 = 0.6;

/// Opacity of the offline scrim.
const OFFLINE_ALPHA: f32 = 0.55;

/// Opacity of the credential-warning ring.
const CREDENTIAL_ALPHA: f32 = 0.85;

/// Inner radius of the credential-warning ring, past the node's hitbox.
const CREDENTIAL_RING_INNER: f32 = 2.0;

/// Outer radius of the credential-warning ring, past the node's hitbox.
const CREDENTIAL_RING_OUTER: f32 = 6.0;

/// How a selected node is drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SelectionStyle {
    /// Single selection: an animated dashed ring.
    Ring,
    /// Multi-selection: a flat shading disc.
    Shade,
}

/// Visual component for a selected node's ring or shading disc.
#[derive(Component)]
pub struct SelectionRing {
    pub node_entity: Entity,
    pub style: SelectionStyle,
}

/// A ring of [`DASH_COUNT`] arc segments between `inner` and `outer`, centered on
/// the origin.
///
/// Dashes rather than a solid `Annulus` because a solid ring looks static however
/// fast it turns, and the motion is what separates this from the shading disc.
fn dashed_ring_mesh(inner: f32, outer: f32) -> Mesh {
    let slice = std::f32::consts::TAU / DASH_COUNT as f32;
    let dash_arc = slice * DASH_DUTY;

    let mut positions = Vec::with_capacity(DASH_COUNT * (SEGMENTS_PER_DASH + 1) * 2);
    let mut indices = Vec::with_capacity(DASH_COUNT * SEGMENTS_PER_DASH * 6);

    for dash in 0..DASH_COUNT {
        // Index of this dash's first vertex, before any are pushed for it.
        let base = positions.len() as u32;
        let start = dash as f32 * slice;

        for segment in 0..=SEGMENTS_PER_DASH {
            let angle = start + dash_arc * (segment as f32 / SEGMENTS_PER_DASH as f32);
            let (sin, cos) = angle.sin_cos();
            positions.push([cos * inner, sin * inner, 0.0]);
            positions.push([cos * outer, sin * outer, 0.0]);
        }

        // Two triangles per segment, stitching the inner/outer pair at one angle
        // step to the pair at the next.
        for segment in 0..SEGMENTS_PER_DASH as u32 {
            let quad = base + segment * 2;
            indices.extend_from_slice(&[quad, quad + 1, quad + 3]);
            indices.extend_from_slice(&[quad, quad + 3, quad + 2]);
        }
    }

    let vertex_count = positions.len();
    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0, 0.0, 1.0]; vertex_count])
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, vec![[0.0, 0.0]; vertex_count])
    .with_inserted_indices(Indices::U32(indices))
}

/// Spawn / despawn selection visuals to match the [`Selected`] markers.
pub fn update_selection_visuals(
    mut commands: Commands,
    theme: Res<Theme>,
    selected_nodes: Query<(Entity, &NodeHitbox), With<Selected>>,
    selection_rings: Query<(Entity, &SelectionRing, &MeshMaterial2d<ColorMaterial>)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    let color = theme.color(Role::Accent).with_alpha(SELECTION_ALPHA);

    // `ColorMaterial` is outside the `ThemedBg` recolor machinery (which only
    // covers `bevy_ui`), so a theme swap has to repaint these by hand.
    if theme.is_changed() {
        for (_, _, handle) in selection_rings.iter() {
            if let Some(mut material) = materials.get_mut(&handle.0) {
                material.color = color;
            }
        }
    }

    // A batch of nodes reads better shaded than ringed, so the style depends on
    // how many are selected. Counted from the markers themselves rather than from
    // `SelectionSet`, so the visual can't disagree with what's drawn.
    let style = if selected_nodes.iter().count() > 1 {
        SelectionStyle::Shade
    } else {
        SelectionStyle::Ring
    };

    // Remove visuals for nodes no longer selected, or drawn in the other style.
    for (ring_entity, ring, _) in selection_rings.iter() {
        if !selected_nodes.contains(ring.node_entity) || ring.style != style {
            commands.entity(ring_entity).despawn();
        }
    }

    // Add visuals for newly selected nodes.
    for (node_entity, hitbox) in selected_nodes.iter() {
        let has_visual = selection_rings
            .iter()
            .any(|(_, ring, _)| ring.node_entity == node_entity && ring.style == style);
        if has_visual {
            continue;
        }

        // Sized from the node's own hitbox, so a 64-unit account node doesn't
        // get the visual drawn for a 100-unit instance node.
        let mesh = match style {
            SelectionStyle::Ring => dashed_ring_mesh(
                hitbox.radius + DASH_RING_INNER,
                hitbox.radius + DASH_RING_OUTER,
            ),
            SelectionStyle::Shade => Mesh::from(Circle::new(hitbox.radius + RING_MARGIN)),
        };

        commands.entity(node_entity).with_children(|parent| {
            parent.spawn((
                Mesh2d(meshes.add(mesh)),
                MeshMaterial2d(materials.add(ColorMaterial::from(color))),
                Transform::from_xyz(0.0, 0.0, -0.1), // Behind the node
                SelectionRing { node_entity, style },
            ));
        });
    }
}

/// Slowly rotate single-selection rings.
///
/// The shading disc is radially symmetric, so spinning it would do nothing.
pub fn spin_selection_rings(time: Res<Time>, mut rings: Query<(&SelectionRing, &mut Transform)>) {
    for (ring, mut transform) in rings.iter_mut() {
        if ring.style == SelectionStyle::Ring {
            transform.rotate_z(SPIN_RADIANS_PER_SEC * time.delta_secs());
        }
    }
}

/// Marks the dimming scrim drawn over an offline node.
#[derive(Component)]
pub struct OfflineScrim {
    pub node_entity: Entity,
}

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

/// Spawn / despawn the dimming scrim to match the [`Offline`] markers.
pub fn update_offline_visuals(
    mut commands: Commands,
    theme: Res<Theme>,
    offline_nodes: Query<(Entity, &NodeHitbox), With<Offline>>,
    scrims: Query<(Entity, &OfflineScrim, &MeshMaterial2d<ColorMaterial>)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    // Fading toward the app background rather than toward black, which would read
    // as a highlight under a light theme.
    let color = theme.color(Role::Background).with_alpha(OFFLINE_ALPHA);

    if theme.is_changed() {
        for (_, _, handle) in scrims.iter() {
            if let Some(mut material) = materials.get_mut(&handle.0) {
                material.color = color;
            }
        }
    }

    for (scrim_entity, scrim, _) in scrims.iter() {
        if !offline_nodes.contains(scrim.node_entity) {
            commands.entity(scrim_entity).despawn();
        }
    }

    for (node_entity, hitbox) in offline_nodes.iter() {
        let has_scrim = scrims
            .iter()
            .any(|(_, scrim, _)| scrim.node_entity == node_entity);
        if has_scrim {
            continue;
        }

        commands.entity(node_entity).with_children(|parent| {
            parent.spawn((
                Mesh2d(meshes.add(Circle::new(hitbox.radius))),
                MeshMaterial2d(materials.add(ColorMaterial::from(color))),
                Transform::from_xyz(0.0, 0.0, 0.1), // In front of the node
                OfflineScrim { node_entity },
            ));
        });
    }
}

/// Marks the warning ring drawn around a node missing credentials.
#[derive(Component)]
pub struct CredentialWarning {
    pub node_entity: Entity,
}

/// Spawn / despawn the credential-warning ring to match the
/// [`NeedsCredentials`] markers.
///
/// A solid [`Role::Warn`] annulus hugging the node's edge, drawn in front of it
/// (and of the offline scrim) so a probe that still needs credentials reads
/// distinctly from the accent selection ring and the background offline scrim.
pub fn update_credential_visuals(
    mut commands: Commands,
    theme: Res<Theme>,
    flagged_nodes: Query<(Entity, &NodeHitbox), With<NeedsCredentials>>,
    rings: Query<(Entity, &CredentialWarning, &MeshMaterial2d<ColorMaterial>)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    let color = theme.color(Role::Warn).with_alpha(CREDENTIAL_ALPHA);

    if theme.is_changed() {
        for (_, _, handle) in rings.iter() {
            if let Some(mut material) = materials.get_mut(&handle.0) {
                material.color = color;
            }
        }
    }

    for (ring_entity, ring, _) in rings.iter() {
        if !flagged_nodes.contains(ring.node_entity) {
            commands.entity(ring_entity).despawn();
        }
    }

    for (node_entity, hitbox) in flagged_nodes.iter() {
        let has_ring = rings
            .iter()
            .any(|(_, ring, _)| ring.node_entity == node_entity);
        if has_ring {
            continue;
        }

        let mesh = Mesh::from(Annulus::new(
            hitbox.radius + CREDENTIAL_RING_INNER,
            hitbox.radius + CREDENTIAL_RING_OUTER,
        ));

        commands.entity(node_entity).with_children(|parent| {
            parent.spawn((
                Mesh2d(meshes.add(mesh)),
                MeshMaterial2d(materials.add(ColorMaterial::from(color))),
                Transform::from_xyz(0.0, 0.0, 0.2), // In front, above the offline scrim
                CredentialWarning { node_entity },
            ));
        });
    }
}

#[cfg(test)]
mod test_node_effects {
    use super::*;

    #[test]
    fn dashed_ring_mesh_is_well_formed() {
        let mesh = dashed_ring_mesh(10.0, 14.0);

        let expected_vertices = DASH_COUNT * (SEGMENTS_PER_DASH + 1) * 2;
        assert_eq!(mesh.count_vertices(), expected_vertices);

        let Some(Indices::U32(indices)) = mesh.indices() else {
            panic!("expected u32 indices");
        };
        assert_eq!(indices.len(), DASH_COUNT * SEGMENTS_PER_DASH * 6);

        // Every index must address a vertex that exists; an off-by-one in the
        // per-dash base would otherwise only show up as corrupt geometry.
        assert!(indices.iter().all(|i| (*i as usize) < expected_vertices));
    }
}
