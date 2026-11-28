//! GUI components for the Account layer.
//!
//! One graph node per account, with lines drawn along every [`AccountLinkData`]
//! the server derived. Accounts are created from the layer toolbar and travel to
//! the server over the management stream; they come back through the database
//! sync subscription opened by [`open_account_subscription`].
//!
//! Account nodes deliberately carry no `NodeEntity`, because they aren't
//! instances. That keeps them out of every generic node system (layout,
//! selection, minimap, previews, layer visuals) at the cost of this module
//! owning its own layout and visibility.

use bevy::image::Image;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy_rapier2d::dynamics::{Damping, ExternalForce, RigidBody, Velocity};
use bevy_rapier2d::geometry::{Collider, Restitution};
use bevy_svg::prelude::{Origin, Svg2d};
use gesso::node::{NeedsScaling, NodeHitbox, NodeIdentity, PanelIcon, SubNode};
use gesso::terrain::TerrainMember;
use gesso::ui::Activate;
use gesso::ui::bind::bind_text;
use gesso::ui::icon::{IconCache, decode_icon_bytes};
use gesso::ui::node_panel::{NodePanel, PanelCtx};
use gesso::ui::panel::modal_scrim;
use gesso::ui::text_input::text_input;
use gesso::ui::theme::{Role, Theme, ThemedBg, ThemedBorder};
use gesso::ui::widgets::{button, heading, muted, text};
use sandpolis_client::gui::input::CurrentLayer;
use sandpolis_client::gui::ui::layer::{LayerClientInfo, RegisterLayerClient};
use sandpolis_instance::LayerName;
use std::collections::{HashMap, HashSet};
use tracing::warn;

use crate::{AccountId, AccountLinkType};

/// The layer this plugin implements.
const LAYER: &str = "Account";

/// Visual diameter of an account node.
const ACCOUNT_NODE_DIAMETER: f32 = 64.0;

/// Pixel size favicons are decoded at for the world-space node icon. Matches
/// [`ACCOUNT_NODE_DIAMETER`].
const NODE_ICON_PX: u32 = 64;

/// Pixel size favicons are decoded at for the controller box.
const CARD_ICON_PX: u32 = 20;

/// Every size a favicon is decoded at. There's no mipmapping on these textures,
/// so each display size gets its own decode rather than scaling one down.
const FAVICON_SIZES: [u32; 2] = [NODE_ICON_PX, CARD_ICON_PX];

/// Account node component, carrying the fields the icon, controller box and
/// terrain systems need so none of them has to touch the database.
#[derive(Component)]
pub struct AccountNode {
    pub account_id: AccountId,
    /// The service domain, as the user typed it. Lowercase before joining it
    /// against favicons or terrain keys.
    pub domain: String,
    /// Username, else email — see [`crate::AccountData::identity`].
    pub identity: String,
}

/// Marker for an account node's SVG child, so the generic `NodeSvg` scaling and
/// layer-visual systems leave it alone.
#[derive(Component)]
pub struct AccountNodeSvg;

/// Which icon an account node is currently displaying. Held on the icon child so
/// the swap system can tell what's already there.
///
/// The two variants can't share an entity: `Svg2d` requires `Mesh2d` and inserts
/// its own material, so adding a `Sprite` alongside it draws both. Switching
/// means despawning the child and spawning the other kind.
#[derive(Component, Clone, PartialEq)]
pub enum AccountNodeIcon {
    /// The bundled generic glyph, shown until a favicon arrives — or forever,
    /// for domains whose favicon couldn't be fetched or decoded.
    Fallback,
    /// The domain's scraped favicon.
    Favicon(Handle<Image>),
}

/// A link between two account nodes, mirroring one `AccountLinkData` row.
#[derive(Component)]
pub struct AccountLink {
    pub source: AccountId,
    pub target: AccountId,
    pub kind: AccountLinkType,
}

#[derive(Bundle)]
struct AccountNodeBundle {
    account_node: AccountNode,
    terrain_member: TerrainMember,
    /// Carries the account id. An account node has no `NodeEntity` at all, so
    /// this is the whole of its [`PanelTarget`](gesso::ui::node_panel::PanelTarget).
    sub_node: SubNode,
    /// The username (else email) shown at the top of the node's panel.
    identity: NodeIdentity,
    /// Opts these nodes into the generic selection and drag systems, which key
    /// on the hitbox rather than on `NodeEntity`.
    hitbox: NodeHitbox,
    collider: Collider,
    rigid_body: RigidBody,
    velocity: Velocity,
    external_force: ExternalForce,
    damping: Damping,
    restitution: Restitution,
    transform: Transform,
    visibility: Visibility,
}

/// Open the account sync subscription once a connection is available.
fn open_account_subscription(mut done: Local<bool>) {
    if *done {
        return;
    }
    if sandpolis_client::sync::connection().is_some() {
        super::subscribe();
        *done = true;
    }
}

/// How often the synced account/link tables are re-read. Each poll opens a read
/// transaction, so this runs well below frame rate.
const POLL_INTERVAL: f32 = 0.25;

/// How often the favicon table is re-read. Slower than [`POLL_INTERVAL`] because
/// each poll materializes every favicon's image bytes.
const FAVICON_POLL_INTERVAL: f32 = 2.0;

/// Whether a polling system should re-read the database this frame. Polling only
/// happens while the Account layer is active.
fn should_poll(
    current_layer: &Res<CurrentLayer>,
    time: &Time,
    elapsed: &mut f32,
    interval: f32,
) -> bool {
    if current_layer.name() != LAYER {
        return false;
    }
    *elapsed += time.delta_secs();
    if *elapsed < interval && !current_layer.is_changed() {
        return false;
    }
    *elapsed = 0.0;
    true
}

/// Decoded favicon textures, keyed by lowercased domain.
///
/// Favicons are per-domain rather than per-account, so accounts on the same
/// service share one set of textures.
#[derive(Resource, Default)]
pub struct FaviconTextures {
    entries: HashMap<String, FaviconEntry>,
}

#[derive(Default)]
struct FaviconEntry {
    /// The `fetched_at` of the row these textures were decoded from, so a
    /// re-scrape replaces them and an unchanged row doesn't.
    fetched_at: i64,
    /// One texture per size in [`FAVICON_SIZES`]. `None` means the bytes
    /// couldn't be decoded, which is remembered so it isn't retried every poll.
    sizes: HashMap<u32, Option<Handle<Image>>>,
}

impl FaviconTextures {
    /// The texture for a domain at the given size, if one was decoded.
    pub fn get(&self, domain: &str, size: u32) -> Option<&Handle<Image>> {
        self.entries
            .get(&normalize_domain(domain))?
            .sizes
            .get(&size)?
            .as_ref()
    }
}

/// Favicon rows are stored under a trimmed, lowercased domain while accounts
/// keep the domain as the user typed it, so every join goes through this.
fn normalize_domain(domain: &str) -> String {
    domain.trim().to_lowercase()
}

/// Place an account node in the domain terrain hierarchy. The key is normalized
/// so accounts and instances on the same domain share one region regardless of
/// how each was typed; the display name keeps the original spelling.
fn terrain_member(domain: &str) -> TerrainMember {
    let key = normalize_domain(domain);
    if key.is_empty() {
        return TerrainMember::default();
    }
    TerrainMember {
        segments: vec![(key, domain.trim().to_string())],
    }
}

/// Decode newly synced favicons into textures.
///
/// Takes its resources through [`Res`] and upgrades to [`ResMut`] only when
/// there's something to store, so the poll doesn't mark [`FaviconTextures`] (and
/// with it every downstream `is_changed()` guard) dirty on every sweep.
fn update_favicon_textures(
    current_layer: Res<CurrentLayer>,
    time: Res<Time>,
    mut elapsed: Local<f32>,
    mut images: ResMut<Assets<Image>>,
    mut textures: ResMut<FaviconTextures>,
) {
    if !should_poll(&current_layer, &time, &mut elapsed, FAVICON_POLL_INTERVAL) {
        return;
    }

    let favicons = match super::query_favicons() {
        Ok(favicons) => favicons,
        Err(e) => {
            warn!(error = %e, "Failed to query favicons");
            return;
        }
    };

    let mut live: HashSet<String> = HashSet::new();
    for row in &favicons {
        let domain = normalize_domain(&row.domain);
        live.insert(domain.clone());

        let known = textures.entries.get(&domain);
        if known.is_some_and(|entry| entry.fetched_at == row.fetched_at) {
            continue;
        }

        // A failed fetch still writes a row; record it so the fallback glyph
        // sticks without re-attempting a decode on every poll.
        let sizes = if row.error.is_some() || row.bytes.is_empty() {
            FAVICON_SIZES.iter().map(|&size| (size, None)).collect()
        } else {
            FAVICON_SIZES
                .iter()
                .map(|&size| {
                    let decoded = decode_icon_bytes(&row.bytes, row.content_type.as_deref(), size)
                        .map(|image| images.add(image));
                    if decoded.is_none() {
                        warn!(domain = %domain, "Failed to decode favicon");
                    }
                    (size, decoded)
                })
                .collect()
        };

        textures.entries.insert(
            domain,
            FaviconEntry {
                fetched_at: row.fetched_at,
                sizes,
            },
        );
    }

    // Only touch the map when a row actually disappeared; an unconditional
    // `retain` would mark the resource changed on every sweep.
    if textures.entries.keys().any(|domain| !live.contains(domain)) {
        textures.entries.retain(|domain, _| live.contains(domain));
    }
}

/// The icon an account node should be showing for its domain.
fn desired_icon(textures: &FaviconTextures, domain: &str) -> AccountNodeIcon {
    match textures.get(domain, NODE_ICON_PX) {
        Some(handle) => AccountNodeIcon::Favicon(handle.clone()),
        None => AccountNodeIcon::Fallback,
    }
}

/// Attach an icon child of the given kind to an account node.
fn spawn_account_icon(
    commands: &mut Commands,
    asset_server: &AssetServer,
    node: Entity,
    icon: AccountNodeIcon,
) {
    let child = match &icon {
        AccountNodeIcon::Fallback => commands
            .spawn((
                Svg2d(asset_server.load("account/account.svg")),
                Origin::Center,
                Transform::default(),
                NeedsScaling,
                AccountNodeSvg,
            ))
            .id(),
        // Sprites are center-anchored and `custom_size` does the scaling, so
        // this needs none of the manual recentring the SVG path does.
        AccountNodeIcon::Favicon(handle) => commands
            .spawn((
                Sprite {
                    image: handle.clone(),
                    custom_size: Some(Vec2::splat(ACCOUNT_NODE_DIAMETER)),
                    ..default()
                },
                Transform::default(),
            ))
            .id(),
    };
    commands.entity(child).insert(icon);
    commands.entity(node).add_child(child);
}

/// Swap an account node's icon child when its favicon arrives (or changes).
fn update_account_node_icons(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    textures: Res<FaviconTextures>,
    nodes: Query<(Entity, &AccountNode, Option<&Children>)>,
    icons: Query<&AccountNodeIcon>,
) {
    if !textures.is_changed() {
        return;
    }

    for (entity, node, children) in nodes.iter() {
        let desired = desired_icon(&textures, &node.domain);
        let current = children
            .into_iter()
            .flatten()
            .find_map(|child| icons.get(*child).ok().map(|icon| (*child, icon)));

        match current {
            Some((_, icon)) if *icon == desired => continue,
            Some((child, _)) => commands.entity(child).despawn(),
            None => {}
        }
        spawn_account_icon(&mut commands, &asset_server, entity, desired);
    }
}

/// Spawn/despawn account nodes to match the synced account list.
fn update_account_nodes(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    current_layer: Res<CurrentLayer>,
    textures: Res<FaviconTextures>,
    time: Res<Time>,
    mut elapsed: Local<f32>,
    mut existing: Query<(Entity, &mut AccountNode)>,
) {
    if !should_poll(&current_layer, &time, &mut elapsed, POLL_INTERVAL) {
        return;
    }

    let accounts = match super::query_accounts() {
        Ok(accounts) => accounts,
        Err(e) => {
            warn!(error = %e, "Failed to query accounts");
            return;
        }
    };

    // Refresh the fields the icon, panel and terrain systems read, so an edited
    // account doesn't need its node respawned.
    for (entity, mut node) in existing.iter_mut() {
        let Some(account) = accounts.iter().find(|a| a.account_id == node.account_id) else {
            continue;
        };
        if node.domain != account.domain {
            node.domain = account.domain.clone();
            commands.entity(entity).insert(terrain_member(&node.domain));
        }
        if node.identity != account.identity() {
            node.identity = account.identity().to_string();
            commands
                .entity(entity)
                .insert(NodeIdentity(node.identity.clone()));
        }
    }

    let known: HashSet<AccountId> = existing.iter().map(|(_, node)| node.account_id).collect();
    let mut placed = known.len();

    for account in &accounts {
        if known.contains(&account.account_id) {
            continue;
        }

        // Golden-angle placement keeps new nodes off each other until the layout
        // settles. `placed` (not `known.len()`) so several accounts arriving in
        // one poll don't stack.
        let index = placed as f32;
        placed += 1;
        let angle = index * 0.618_034 * std::f32::consts::TAU;
        let radius = 160.0 + index * 12.0;

        let entity = commands
            .spawn(AccountNodeBundle {
                account_node: AccountNode {
                    account_id: account.account_id,
                    domain: account.domain.clone(),
                    identity: account.identity().to_string(),
                },
                terrain_member: terrain_member(&account.domain),
                sub_node: SubNode(account.account_id.body()),
                identity: NodeIdentity(account.identity().to_string()),
                hitbox: NodeHitbox::from_diameter(ACCOUNT_NODE_DIAMETER),
                collider: Collider::ball(ACCOUNT_NODE_DIAMETER / 2.0),
                rigid_body: RigidBody::Dynamic,
                velocity: Velocity::zero(),
                external_force: ExternalForce::default(),
                damping: Damping {
                    linear_damping: 0.0,
                    angular_damping: 1.0,
                },
                restitution: Restitution::coefficient(0.7),
                transform: Transform::from_xyz(radius * angle.cos(), radius * angle.sin(), 0.0),
                // Polling only runs while this layer is active, so a new node is
                // always immediately visible.
                visibility: Visibility::Inherited,
            })
            .id();

        // Spawn the final icon straight away rather than always starting on the
        // fallback, so an account whose favicon is already cached doesn't flash
        // the generic glyph first.
        let icon = desired_icon(&textures, &account.domain);
        spawn_account_icon(&mut commands, &asset_server, entity, icon);
    }

    let live: HashSet<AccountId> = accounts.iter().map(|a| a.account_id).collect();
    for (entity, node) in existing.iter() {
        if !live.contains(&node.account_id) {
            commands.entity(entity).despawn();
        }
    }
}

/// Scale account SVGs once the asset loads.
fn scale_account_node_svgs(
    mut commands: Commands,
    svg_assets: Res<Assets<bevy_svg::prelude::Svg>>,
    mut needing_scale: Query<
        (Entity, &Svg2d, &mut Transform),
        (With<NeedsScaling>, With<AccountNodeSvg>),
    >,
) {
    for (entity, svg_handle, mut transform) in needing_scale.iter_mut() {
        if let Some(svg) = svg_assets.get(&svg_handle.0) {
            let max_dimension = svg.size.x.max(svg.size.y);
            if max_dimension > 0.0 {
                let scale = ACCOUNT_NODE_DIAMETER / max_dimension;
                transform.scale = Vec3::splat(scale);

                // SVGs render from their top-left, so recenter manually.
                let scaled = svg.size * scale;
                transform.translation.x = -scaled.x / 2.0;
                transform.translation.y = scaled.y / 2.0;

                commands.entity(entity).remove::<NeedsScaling>();
            }
        }
    }
}

/// Spawn/despawn `AccountLink` entities to match the synced link rows.
fn update_account_links(
    mut commands: Commands,
    current_layer: Res<CurrentLayer>,
    time: Res<Time>,
    mut elapsed: Local<f32>,
    existing: Query<(Entity, &AccountLink)>,
    mut cached: Local<Vec<(AccountId, AccountId, AccountLinkType)>>,
) {
    if !should_poll(&current_layer, &time, &mut elapsed, POLL_INTERVAL) {
        return;
    }

    let links = match super::query_links() {
        Ok(links) => links,
        Err(e) => {
            warn!(error = %e, "Failed to query account links");
            return;
        }
    };

    // The server replaces derived link rows wholesale, so compare the whole set
    // rather than its size: a delete plus an insert can leave the count intact.
    let mut current: Vec<(AccountId, AccountId, AccountLinkType)> = links
        .into_iter()
        .map(|l| (l.source, l.target, l.r#type))
        .collect();
    current.sort_by_key(|a| (a.0, a.1));

    if current == *cached && existing.iter().count() == current.len() {
        return;
    }
    *cached = current.clone();

    for (entity, _) in existing.iter() {
        commands.entity(entity).despawn();
    }
    for (source, target, kind) in current {
        commands.spawn(AccountLink {
            source,
            target,
            kind,
        });
    }
}

/// Color a link by what the two accounts have in common.
fn link_color(kind: &AccountLinkType) -> Color {
    match kind {
        AccountLinkType::CommonUsername(_) => Color::srgb(0.4, 0.7, 1.0),
        AccountLinkType::CommonEmail(_) => Color::srgb(0.6, 1.0, 0.6),
        AccountLinkType::CommonPassword { .. } => Color::srgb(1.0, 0.5, 0.4),
        AccountLinkType::Recovery => Color::srgb(1.0, 0.8, 0.3),
        AccountLinkType::SshAuthorizedKey { .. } => Color::srgb(0.8, 0.6, 1.0),
    }
}

/// Draw account links as gizmo lines, like the instance-graph edges.
fn render_account_links(
    mut gizmos: Gizmos,
    current_layer: Res<CurrentLayer>,
    links: Query<&AccountLink>,
    nodes: Query<(&Transform, &AccountNode)>,
) {
    if current_layer.name() != LAYER {
        return;
    }

    let positions: HashMap<AccountId, Vec2> = nodes
        .iter()
        .map(|(transform, node)| (node.account_id, transform.translation.truncate()))
        .collect();

    for link in links.iter() {
        let (Some(&from), Some(&to)) = (positions.get(&link.source), positions.get(&link.target))
        else {
            continue;
        };
        gizmos.line_2d(from, to, link_color(&link.kind));
    }
}

/// Force-directed layout over account nodes.
///
/// The generic systems in `gesso::layout` are keyed on
/// `NodeId`, which account nodes don't have, so the same repulsion / spring /
/// damping model is applied here over `AccountNode` instead.
fn apply_account_layout(
    links: Query<&AccountLink>,
    mut nodes: Query<(Entity, &Transform, &mut ExternalForce, &AccountNode)>,
    mut velocities: Query<&mut Velocity, With<AccountNode>>,
) {
    const REPULSION: f32 = 50_000.0;
    const SPRING: f32 = 0.1;
    const REST_LENGTH: f32 = 220.0;
    const MAX_FORCE: f32 = 1000.0;
    const DAMPING: f32 = 0.85;

    let positions: Vec<Vec3> = nodes
        .iter()
        .map(|(_, transform, _, _)| transform.translation)
        .collect();

    for (index, (_, transform, mut force, _)) in nodes.iter_mut().enumerate() {
        let position = transform.translation;
        for (other_index, other) in positions.iter().enumerate() {
            if index == other_index {
                continue;
            }
            let delta = position - *other;
            let distance = delta.length().max(1.0);
            let magnitude = (REPULSION / (distance * distance)).min(MAX_FORCE);
            force.force += (delta.normalize_or_zero() * magnitude).truncate();
        }
    }

    let by_account: HashMap<AccountId, Entity> = nodes
        .iter()
        .map(|(entity, _, _, node)| (node.account_id, entity))
        .collect();

    for link in links.iter() {
        let (Some(&a), Some(&b)) = (by_account.get(&link.source), by_account.get(&link.target))
        else {
            continue;
        };
        let Ok([(_, transform_a, _, _), (_, transform_b, _, _)]) = nodes.get_many([a, b]) else {
            continue;
        };

        let delta = transform_b.translation - transform_a.translation;
        let distance = delta.length().max(1.0);
        let magnitude = (SPRING * (distance - REST_LENGTH)).clamp(-MAX_FORCE, MAX_FORCE);
        let spring = (delta.normalize_or_zero() * magnitude).truncate();

        if let Ok((_, _, mut force, _)) = nodes.get_mut(a) {
            force.force += spring;
        }
        if let Ok((_, _, mut force, _)) = nodes.get_mut(b) {
            force.force -= spring;
        }
    }

    for mut velocity in velocities.iter_mut() {
        velocity.linear *= DAMPING;
        velocity.angular *= DAMPING;
    }
}

/// Show account nodes only while the Account layer is active.
fn update_account_node_visibility(
    current_layer: Res<CurrentLayer>,
    mut nodes: Query<&mut Visibility, With<AccountNode>>,
) {
    if !current_layer.is_changed() {
        return;
    }
    let visible = current_layer.name() == LAYER;
    for mut visibility in nodes.iter_mut() {
        *visibility = if visible {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

/// The account layer's node panel.
///
/// Account nodes have no `InstanceId`, so their whole target is the account id
/// in their `SubNode`; everything shown here is looked up from that.
pub struct AccountPanel;

impl NodePanel for AccountPanel {
    fn build_summary(&self, ctx: &mut PanelCtx) {
        let Some(account_id) = ctx.target.sub.map(AccountId) else {
            return;
        };
        // The identity line above already carries the username, so the summary
        // is the service it belongs to.
        let theme = ctx.theme;
        ctx.children(|p| {
            p.spawn((
                text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                bind_text(move || domain_of(account_id).unwrap_or_else(|| "Unknown".into())),
            ));
        });
    }

    fn build_detail(&self, ctx: &mut PanelCtx) {
        let Some(account_id) = ctx.target.sub.map(AccountId) else {
            return;
        };
        let theme = ctx.theme;
        ctx.children(|p| {
            p.spawn(heading(theme, "Account"));
            p.spawn((
                text(theme, "", theme.metrics.font_md, Role::Text),
                bind_text(move || describe_account(account_id)),
            ));

            p.spawn(heading(theme, "Links"));
            p.spawn((
                text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                bind_text(move || describe_links(account_id)),
            ));
        });
    }
}

/// The domain an account belongs to.
fn domain_of(account_id: AccountId) -> Option<String> {
    super::query_accounts()
        .ok()?
        .into_iter()
        .find(|account| account.account_id == account_id)
        .map(|account| account.domain)
}

/// Everything the synced row says about an account, in a few lines.
fn describe_account(account_id: AccountId) -> String {
    let Ok(accounts) = super::query_accounts() else {
        return "No account data".into();
    };
    let Some(account) = accounts
        .into_iter()
        .find(|account| account.account_id == account_id)
    else {
        return "This account is no longer registered.".into();
    };
    let mut lines = vec![format!("Domain: {}", account.domain)];
    if let Some(username) = account.username.as_ref() {
        lines.push(format!("Username: {username}"));
    }
    if let Some(email) = account.email.as_ref() {
        lines.push(format!("Email: {email}"));
    }
    lines.join("\n")
}

/// What this account has in common with the rest of the estate.
///
/// These are the derived rows the server computes; they're the whole reason an
/// account is worth drawing as a node rather than listing in a table.
fn describe_links(account_id: AccountId) -> String {
    let Ok(links) = super::query_links() else {
        return "No link data".into();
    };
    let related: Vec<String> = links
        .iter()
        .filter(|link| link.source == account_id || link.target == account_id)
        .map(|link| format!("{:?}", link.r#type))
        .collect();
    if related.is_empty() {
        "No links to other accounts.".into()
    } else {
        format!("{} link(s):\n{}", related.len(), related.join("\n"))
    }
}

/// The icon an account node's panel should show: the domain's favicon, else the
/// same generic glyph the node itself falls back to.
fn card_icon_handle(
    textures: &FaviconTextures,
    icon_cache: &mut IconCache,
    images: &mut Assets<Image>,
    domain: &str,
) -> Handle<Image> {
    match textures.get(domain, CARD_ICON_PX) {
        Some(handle) => handle.clone(),
        None => icon_cache.get_or_rasterize(images, "account/account.svg", CARD_ICON_PX),
    }
}

/// Give each account node the favicon its panel should be titled with.
///
/// Rasterizing and looking up handles unconditionally would mean allocating (and
/// dirtying `Assets<Image>`) on every single frame, so this only runs when a
/// favicon arrives or an account is edited.
fn update_account_panel_icons(
    mut commands: Commands,
    textures: Res<FaviconTextures>,
    mut images: ResMut<Assets<Image>>,
    mut icon_cache: ResMut<IconCache>,
    nodes: Query<(Entity, &AccountNode, Option<&PanelIcon>)>,
    changed_nodes: Query<(), Changed<AccountNode>>,
) {
    if !textures.is_changed() && changed_nodes.is_empty() {
        return;
    }

    for (entity, node, current) in nodes.iter() {
        let handle = card_icon_handle(&textures, &mut icon_cache, &mut images, &node.domain);
        if current.map(|icon| &icon.0) != Some(&handle) {
            commands.entity(entity).insert(PanelIcon(handle));
        }
    }
}

/// State of the "add account" dialog.
#[derive(Resource, Default)]
pub struct AddAccountDialogState {
    pub show: bool,
    pub domain: String,
    pub username: String,
    pub email: String,
    pub error_message: Option<String>,
}

#[derive(Component)]
struct AddAccountRoot;
#[derive(Component)]
struct DomainInput;
#[derive(Component)]
struct UsernameInput;
#[derive(Component)]
struct EmailInput;
#[derive(Component)]
struct AddAccountErrorText;

/// Spawn/despawn the add-account modal.
fn manage_add_account(
    mut commands: Commands,
    theme: Res<Theme>,
    state: Res<AddAccountDialogState>,
    root: Query<Entity, With<AddAccountRoot>>,
    mut focus: ResMut<InputFocus>,
) {
    let exists = !root.is_empty();
    if state.show && !exists {
        commands
            .spawn((AddAccountRoot, modal_scrim()))
            .with_children(|scrim| {
                scrim
                    .spawn((
                        Node {
                            flex_direction: FlexDirection::Column,
                            width: Val::Px(360.0),
                            padding: UiRect::all(Val::Px(16.0)),
                            row_gap: Val::Px(6.0),
                            border: UiRect::all(Val::Px(1.0)),
                            ..default()
                        },
                        BackgroundColor(theme.color(Role::Panel)),
                        ThemedBg(Role::Panel),
                        BorderColor::all(theme.color(Role::Border)),
                        ThemedBorder(Role::Border),
                    ))
                    .with_children(|p| {
                        p.spawn(heading(&theme, "Add Account"));
                        p.spawn(muted(&theme, "Domain", theme.metrics.font_sm));
                        p.spawn((DomainInput, text_input(&theme)));
                        p.spawn(muted(&theme, "Username", theme.metrics.font_sm));
                        p.spawn((UsernameInput, text_input(&theme)));
                        p.spawn(muted(&theme, "Email", theme.metrics.font_sm));
                        p.spawn((EmailInput, text_input(&theme)));
                        p.spawn((
                            AddAccountErrorText,
                            text(&theme, "", theme.metrics.font_sm, Role::Error),
                        ));

                        p.spawn(Node {
                            column_gap: Val::Px(8.0),
                            ..default()
                        })
                        .with_children(|row| {
                            row.spawn(button(&theme, "Add"))
                                .observe(on_add_account_submit);
                            row.spawn(button(&theme, "Cancel"))
                                .observe(on_add_account_cancel);
                        });
                    });
            });
    } else if !state.show && exists {
        for entity in &root {
            commands.entity(entity).despawn();
        }
        focus.clear();
    }
}

/// Focus the domain field when the dialog opens.
fn focus_add_account_input(
    inputs: Query<Entity, Added<DomainInput>>,
    mut focus: ResMut<InputFocus>,
) {
    if let Ok(entity) = inputs.single() {
        focus.set(entity, FocusCause::Navigated);
    }
}

/// Copy dialog input contents into [`AddAccountDialogState`].
fn sync_add_account_inputs(
    mut state: ResMut<AddAccountDialogState>,
    domain: Query<&EditableText, With<DomainInput>>,
    username: Query<&EditableText, With<UsernameInput>>,
    email: Query<&EditableText, With<EmailInput>>,
) {
    if let Ok(input) = domain.single() {
        let value = input.value().to_string();
        if state.domain != value {
            state.domain = value;
        }
    }
    if let Ok(input) = username.single() {
        let value = input.value().to_string();
        if state.username != value {
            state.username = value;
        }
    }
    if let Ok(input) = email.single() {
        let value = input.value().to_string();
        if state.email != value {
            state.email = value;
        }
    }
}

/// Mirror the dialog's error message into its label.
fn update_add_account_error(
    state: Res<AddAccountDialogState>,
    mut label: Query<&mut Text, With<AddAccountErrorText>>,
) {
    if let Ok(mut text) = label.single_mut() {
        let message = state.error_message.clone().unwrap_or_default();
        if text.0 != message {
            text.0 = message;
        }
    }
}

fn on_add_account_submit(_activate: On<Activate>, mut state: ResMut<AddAccountDialogState>) {
    let domain = state.domain.trim().to_string();
    let username = non_empty(&state.username);
    let email = non_empty(&state.email);

    if domain.is_empty() {
        state.error_message = Some("A domain is required".into());
        return;
    }
    if username.is_none() && email.is_none() {
        state.error_message = Some("A username or email is required".into());
        return;
    }

    let Some(conn) = sandpolis_client::sync::connection() else {
        state.error_message = Some("Not connected to a server".into());
        return;
    };

    crate::management::create_account(conn, domain, username, email);
    *state = AddAccountDialogState::default();
}

fn on_add_account_cancel(_activate: On<Activate>, mut state: ResMut<AddAccountDialogState>) {
    *state = AddAccountDialogState::default();
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The account layer's client plugin.
pub struct AccountClientPlugin;

impl Plugin for AccountClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AddAccountDialogState>();
        app.init_resource::<FaviconTextures>();
        app.add_systems(
            Update,
            (
                open_account_subscription,
                update_favicon_textures,
                update_account_nodes,
                update_account_node_icons,
                update_account_links,
                scale_account_node_svgs,
                // Ordered so the drag handler gets the last word on a node the
                // user is holding, instead of the layout's forces.
                apply_account_layout.before(gesso::drag::disable_forces_while_dragging),
                update_account_panel_icons,
                manage_add_account,
                focus_add_account_input,
                sync_add_account_inputs,
                update_add_account_error,
            ),
        );
        app.add_systems(
            PostUpdate,
            (
                render_account_links,
                // Must run after the generic visibility system, which also runs
                // on layer change and would otherwise fight over these nodes,
                // and before terrain reads visibility to decide which regions
                // still have members.
                update_account_node_visibility
                    .after(sandpolis_client::gui::layer_visuals::update_node_visibility_for_layer)
                    .before(gesso::terrain::update_terrain_bounds),
            ),
        );
        app.register_layer_client(
            LayerClientInfo::new(
                LayerName::from(LAYER),
                "Online accounts and their relationships",
            )
            .with_panel(AccountPanel)
            // Accounts get the whole canvas; instance nodes are hidden.
            .with_visible_instance_types(&[])
            .with_toolbar_action(
                "Add account",
                "toolbar/add_account.svg",
                |commands| {
                    commands.queue(|world: &mut World| {
                        if let Some(mut state) = world.get_resource_mut::<AddAccountDialogState>() {
                            state.show = true;
                        }
                    });
                },
            )
            .with_services(),
        );
    }
}

