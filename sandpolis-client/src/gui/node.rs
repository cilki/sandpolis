use bevy::prelude::*;
use bevy_rapier2d::{
    dynamics::{Damping, ExternalForce, RigidBody, Velocity},
    geometry::{Collider, Restitution},
};
use bevy_svg::prelude::{Origin, Svg, Svg2d};
use sandpolis_instance::{InstanceId, InstanceType};

/// Marker component for the main world view camera.
#[derive(Component)]
pub struct WorldView;

/// Component that marks an entity as representing an instance node.
#[derive(Component)]
pub struct NodeEntity {
    pub instance_id: InstanceId,
}

/// Marker component for selected nodes.
#[derive(Component)]
pub struct Selected;

/// The one piece of `Data` that identifies a node to the user: the hostname for
/// an instance, the username for an account, the display name for a probe
/// device. Shown at the top of the node's panel.
///
/// Whoever spawns a kind of node keeps this current, which is what lets the
/// account and probe layers name their nodes without this crate knowing either
/// concept exists.
#[derive(Component, Clone, Default)]
pub struct NodeIdentity(pub String);

/// Overrides the active layer's icon on this node's panel — used by the account
/// layer, whose nodes are better identified by the service's favicon than by a
/// generic glyph.
#[derive(Component, Clone)]
pub struct PanelIcon(pub Handle<Image>);

/// Keeps this node's panel expanded after the node is deselected.
#[derive(Component)]
pub struct PanelPinned;

/// Marker component for nodes the client currently can't reach.
#[derive(Component)]
pub struct Offline;

/// Marker for nodes whose backing device is missing credentials it needs (e.g. a
/// probe discovered by scanning that no one has configured yet). Inserted and
/// removed by the layer that owns the device; the credential node effect draws
/// from it.
#[derive(Component)]
pub struct NeedsCredentials;

/// Marker for nodes that the generic node-selection handler must ignore (e.g.
/// probe/device nodes, which have their own per-device selection). These nodes
/// share their gateway's `InstanceId`, so the selection set can't tell them
/// apart. They're still draggable.
#[derive(Component)]
pub struct ExcludeFromSelection;

/// Marks a node standing in for something finer-grained than the instance in its
/// [`NodeEntity`]. Probe/device nodes borrow their gateway's `InstanceId` and
/// carry the device id here, which is what lets the controller host tell a probe
/// apart from the server it orbits.
#[derive(Component, Clone, Copy)]
pub struct SubNode(pub u64);

/// Makes a world entity clickable: the radius, in world units, within which a
/// click counts as hitting it.
///
/// Selection and dragging key on this rather than on [`NodeEntity`], so nodes
/// that aren't instances — account nodes, which have no `InstanceId` at all —
/// get the same interaction without duplicating either system. Carrying the
/// radius per entity also lets differently sized nodes coexist.
#[derive(Component)]
pub struct NodeHitbox {
    pub radius: f32,
}

impl NodeHitbox {
    /// A hitbox matching a node drawn at `diameter` world units across.
    pub fn from_diameter(diameter: f32) -> Self {
        Self {
            radius: diameter / 2.0,
        }
    }
}

/// The desired visual diameter for all nodes
const NODE_VISUAL_DIAMETER: f32 = 100.0;

/// Marker component to indicate this node's SVG needs scaling
#[derive(Component)]
pub struct NeedsScaling;

#[derive(Bundle)]
pub struct Node {
    pub id: InstanceId,
    pub node_entity: NodeEntity,
    pub hitbox: NodeHitbox,
    pub collider: Collider,
    pub rigid_body: RigidBody,
    pub velocity: Velocity,
    pub external_force: ExternalForce,
    pub damping: Damping,
    pub restitution: Restitution,
    pub transform: Transform,
    /// Without this, `update_node_visibility_for_layer` (which queries for
    /// `&mut Visibility`) never matches a node and every layer's
    /// `visible_instance_types` filter is silently inert.
    pub visibility: Visibility,
}

/// Marker component for the SVG child entity
#[derive(Component)]
pub struct NodeSvg;

pub fn spawn_node(
    asset_server: &AssetServer,
    commands: &mut Commands,
    instance_id: InstanceId,
    _os_type: os_info::Type,
    _is_server: bool,
    position: Option<Vec3>,
) {
    // Use provided position or generate random position for new nodes
    let (x, y) = if let Some(pos) = position {
        (pos.x, pos.y)
    } else {
        (
            (rand::random::<f32>() - 0.5) * 500.0,
            (rand::random::<f32>() - 0.5) * 500.0,
        )
    };

    // Start with a placeholder/default SVG that will be replaced by layer system
    // This ensures the correct SVG is loaded based on the current layer
    let svg_path = "network/agent.svg".to_string();

    // Spawn parent node with physics components
    let node_entity = commands
        .spawn((Node {
            id: instance_id,
            node_entity: NodeEntity { instance_id },
            hitbox: NodeHitbox::from_diameter(NODE_VISUAL_DIAMETER),
            collider: Collider::ball(50.0),
            rigid_body: RigidBody::Dynamic,
            velocity: Velocity::zero(),
            external_force: ExternalForce::default(),
            damping: Damping {
                linear_damping: 0.0,  // Layout system will handle damping
                angular_damping: 1.0, // Prevent rotation
            },
            restitution: Restitution::coefficient(0.7),
            transform: Transform::from_xyz(x, y, 0.0),
            visibility: Visibility::Inherited,
        },))
        .id();

    // Spawn SVG as a child entity
    commands.entity(node_entity).with_children(|parent| {
        parent.spawn((
            Svg2d(asset_server.load(svg_path)),
            Origin::Center,
            Transform::default(),
            NodeSvg,
            NeedsScaling,
        ));
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

/// System to scale SVGs to a uniform size once they're loaded
pub fn scale_node_svgs(
    mut commands: Commands,
    svg_assets: Res<Assets<Svg>>,
    mut nodes_needing_scale: Query<
        (Entity, &Svg2d, &mut Transform),
        (With<NeedsScaling>, With<NodeSvg>),
    >,
) {
    for (entity, svg_handle, mut transform) in nodes_needing_scale.iter_mut() {
        // Check if the SVG asset is loaded
        if let Some(svg) = svg_assets.get(&svg_handle.0) {
            // Get the SVG's natural dimensions
            let svg_size = svg.size;

            // Calculate scale for both dimensions to fit within NODE_VISUAL_DIAMETER
            // while maintaining aspect ratio
            let max_dimension = svg_size.x.max(svg_size.y);

            if max_dimension > 0.0 {
                // Scale to fit the largest dimension within NODE_VISUAL_DIAMETER
                // This ensures the entire SVG (including non-square ones) stays within bounds
                let scale = NODE_VISUAL_DIAMETER / max_dimension;

                // Apply uniform scale to maintain aspect ratio
                transform.scale = Vec3::splat(scale);

                // Calculate the scaled size
                let scaled_size = svg_size * scale;

                // Since Origin::Center doesn't seem to work, manually offset the child transform
                // SVGs render from top-left, so to center it we need to shift it:
                // - Left by half the width (negative x)
                // - Up by half the height (positive y in Bevy's coordinate system)
                transform.translation.x = -scaled_size.x / 2.0;
                transform.translation.y = scaled_size.y / 2.0;

                // Remove the NeedsScaling marker since we're done
                commands.entity(entity).remove::<NeedsScaling>();
            }
        }
    }
}
