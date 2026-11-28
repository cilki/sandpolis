//! Which terrain each instance node belongs to.
//!
//! Terrains themselves — the regions, their shapes, the layout that keeps them
//! apart — are drawn by `gesso::terrain` from [`TerrainMember`], which anything
//! positioned in the world can carry. This module derives that membership for
//! instance nodes from [`TerrainConfig`]: each level in the config is an
//! instance attribute, and successive levels nest, so a terrain can contain
//! sub-terrains. Layers with their own non-instance nodes (accounts) insert
//! their own `TerrainMember`.

use crate::gui::node::NodeEntity;
use crate::gui::queries::{InstanceMetadata, query_instance_metadata};
use bevy::prelude::*;
use gesso::terrain::{TerrainLevels, TerrainMember};
use sandpolis_instance::{InstanceId, InstanceManager, InstanceType};

/// A groupable instance attribute. Each variant maps an instance to an optional
/// `(key, display)` pair; the key defines terrain identity and the display names
/// the terrain. Adding a new attribute is a variant plus one match arm —
/// rendering is unaffected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TerrainAttribute {
    /// Group by instance role (Server / Agent / Client).
    InstanceType,
    /// Group by operating system.
    OperatingSystem,
    /// Group by the instance's configured domain.
    Domain,
}

impl TerrainAttribute {
    /// Resolve this attribute for an instance into a `(key, display)` pair, or
    /// `None` if the instance has no value for it.
    pub fn resolve(
        &self,
        id: InstanceId,
        meta: &InstanceMetadata,
        instance: &InstanceManager,
    ) -> Option<(String, String)> {
        match self {
            TerrainAttribute::InstanceType => {
                let label = match id.instance_type() {
                    InstanceType::Server => "Servers",
                    InstanceType::Agent => "Agents",
                    InstanceType::Client => "Clients",
                };
                Some((label.to_string(), label.to_string()))
            }
            TerrainAttribute::OperatingSystem => {
                let os = meta.os_type.to_string();
                Some((os.clone(), os))
            }
            TerrainAttribute::Domain => {
                // An instance belongs to a domain only once it's been assigned
                // to one, so most contribute no terrain rather than an unnamed
                // one. Key on the normalized domain so an instance's
                // "Example.com" and an account's "example.com" land in one
                // region.
                let domain = instance.domain_of(id)?;
                Some((domain.to_lowercase(), domain))
            }
        }
    }
}

/// Ordered list of attributes that define the terrain hierarchy. This is the
/// configuration surface a future UI picker plugs into; changing it rebuilds all
/// terrains.
#[derive(Resource, Clone)]
pub struct TerrainConfig {
    pub levels: Vec<TerrainAttribute>,
}

impl Default for TerrainConfig {
    fn default() -> Self {
        // Group nodes into one terrain per domain.
        Self {
            levels: vec![TerrainAttribute::Domain],
        }
    }
}

/// Set when the domains an instance could belong to have changed, so membership
/// is resolved again. Domains replicate in after login — well after the nodes
/// themselves are spawned — so without this an assignment would never appear.
#[derive(Resource, Default)]
pub struct TerrainMembersDirty(pub bool);

/// Derive [`TerrainMember`] for instance nodes from [`TerrainConfig`].
///
/// Only newly added nodes are resolved unless the config or the domain set
/// changed. Resolving is not free — it goes through `query_instance_metadata`,
/// which calls `os_info::get()` — so this must not become a per-frame scan.
pub fn sync_instance_terrain_members(
    mut commands: Commands,
    config: Res<TerrainConfig>,
    instance: Res<InstanceManager>,
    mut dirty: ResMut<TerrainMembersDirty>,
    nodes: Query<(Entity, Ref<NodeEntity>)>,
) {
    let resolve_all = config.is_changed() || dirty.0;
    dirty.0 = false;

    for (entity, node) in nodes.iter() {
        if !resolve_all && !node.is_added() {
            continue;
        }
        commands.entity(entity).insert(TerrainMember {
            segments: node_segments(node.instance_id, &config, &instance),
        });
    }
}

/// Full ordered `(key, display)` segments for a node under the current config.
/// Stops at the first attribute that yields `None`.
fn node_segments(
    id: InstanceId,
    cfg: &TerrainConfig,
    instance: &InstanceManager,
) -> Vec<(String, String)> {
    let Ok(meta) = query_instance_metadata(id) else {
        return Vec::new();
    };
    let mut segments = Vec::new();
    for attr in &cfg.levels {
        match attr.resolve(id, &meta, instance) {
            Some(seg) => segments.push(seg),
            None => break,
        }
    }
    segments
}

/// Tell gesso how deep the terrain hierarchy goes, which it needs for padding
/// outer terrains and which is also what makes it rebuild every terrain when
/// the config changes.
pub fn sync_terrain_levels(config: Res<TerrainConfig>, mut levels: ResMut<TerrainLevels>) {
    if config.is_changed() {
        *levels = TerrainLevels(config.levels.len());
    }
}
