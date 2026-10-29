//! Node panel host.
//!
//! Every visible node in the world view carries one panel, anchored beneath it.
//! The panel is *collapsed* by default — the node's identity plus whatever
//! summary the active layer wants, at a depth that follows the camera's zoom —
//! and *expands* in place to the layer's full detail view when its node is the
//! only selected node, or while it is pinned.
//!
//! This module owns the chrome (anchoring, the identity line, the pin and close
//! buttons, the mobile sheet) and defers the body to the active layer's
//! [`NodePanel`]. Panels are keyed on [`NodeHitbox`], the same component
//! selection and dragging key on, so instance nodes, probe device nodes and
//! account nodes are all covered by one host rather than three.

use crate::gui::drag::SelectionSet;
use crate::gui::input::{CurrentLayer, ZoomLevel};
use crate::gui::layer_ui::layer_icon_path;
use crate::gui::node::{
    NodeEntity, NodeHitbox, NodeIdentity, PanelIcon, PanelPinned, Selected, SubNode,
    instance_icon_path,
};
use crate::gui::queries;
use crate::gui::ui::Activate;
use crate::gui::ui::anchored::{WorldAnchored, anchored_card, card_icon};
use crate::gui::ui::gating::{BlocksWorldInput, ModalRoot, UiPointerState};
use crate::gui::ui::icon::IconCache;
use crate::gui::ui::layer::LayerRegistry;
use crate::gui::ui::node_panel::{NodePanel, PanelCtx, PanelTarget, Verbosity};
use crate::gui::ui::theme::{Role, Theme, ThemedBg, ThemedBorder, ThemedButton};
use crate::gui::ui::widgets::text;
use crate::gui::ui::z;
use bevy::image::Image;
use bevy::picking::Pickable;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_ui_widgets::Button;
use sandpolis_instance::{InstanceId, LayerName};
use std::sync::Arc;

/// Window width below which an expanded panel takes over the screen instead of
/// floating beside its node.
const MOBILE_WIDTH: f32 = 800.0;

/// Width of an expanded panel on a desktop-sized window, in logical pixels.
const EXPANDED_WIDTH: f32 = 420.0;

/// Fraction of the window height an expanded panel may grow to before its body
/// starts scrolling.
const EXPANDED_MAX_HEIGHT: f32 = 0.7;

/// How far below its node a collapsed panel sits, per verbosity level. The
/// offset shrinks with the panel so the gap stays proportionate.
fn collapsed_offset(verbosity: Verbosity) -> Vec2 {
    match verbosity {
        Verbosity::Minimal => Vec2::new(0.0, 40.0),
        _ => Vec2::new(0.0, 55.0),
    }
}

/// Icon size for a collapsed panel, in pixels.
fn collapsed_icon_px(verbosity: Verbosity) -> u32 {
    match verbosity {
        Verbosity::Minimal => 14,
        Verbosity::Normal => 20,
        Verbosity::Detailed => 24,
    }
}

/// Icon size for an expanded panel's header, in pixels.
const EXPANDED_ICON_PX: u32 = 24;

/// Which verbosity band an orthographic camera scale falls in.
///
/// The projection's scale grows as the camera pulls *out*, so the thresholds
/// read backwards from "zoom": a small scale is a close-up, and a close-up is
/// where there's room to say more.
pub fn verbosity_for_zoom(scale: f32) -> Verbosity {
    if scale <= 0.8 {
        Verbosity::Detailed
    } else if scale <= 1.4 {
        Verbosity::Normal
    } else {
        Verbosity::Minimal
    }
}

/// Whether node panels are shown at all (toggled with `P`). Pinned panels ignore
/// this — the toggle is for clearing ambient clutter, not for closing something
/// the user deliberately kept open.
#[derive(Resource)]
pub struct PanelsVisible(pub bool);

impl Default for PanelsVisible {
    fn default() -> Self {
        Self(true)
    }
}

/// The verbosity every collapsed panel is currently built at, derived from
/// [`ZoomLevel`]. Held as a resource so panels rebuild once per band change
/// rather than once per frame of a scroll.
#[derive(Resource, Default)]
pub struct PanelVerbosity(pub Verbosity);

/// What shape a panel is currently in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelState {
    /// A card under the node, built at this verbosity.
    Collapsed(Verbosity),
    /// The layer's full detail view.
    Expanded,
}

/// Marks a node panel root and records what it was built from, so the host can
/// tell when it has gone stale.
#[derive(Component)]
pub struct NodePanelUi {
    /// The node this panel describes.
    pub node: Entity,
    pub target: PanelTarget,
    pub state: PanelState,
    /// The layer whose [`NodePanel`] built the body. Recorded so a teardown
    /// after a layer change still runs against the panel that built it.
    pub layer: LayerName,
}

/// Installs the node panel host.
pub struct NodePanelPlugin;

impl Plugin for NodePanelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PanelsVisible>()
            .init_resource::<PanelVerbosity>()
            .add_systems(
                Update,
                (
                    toggle_panels,
                    update_panel_verbosity,
                    update_instance_node_identity,
                    manage_node_panels,
                    update_panel_identities,
                    update_panel_icons,
                    update_pin_buttons,
                )
                    .chain(),
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

/// Toggle panel visibility with `P` (unless a text field is focused).
pub fn toggle_panels(
    ui_pointer: Res<UiPointerState>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut visible: ResMut<PanelsVisible>,
) {
    if ui_pointer.wants_keyboard {
        return;
    }
    if keyboard.just_pressed(KeyCode::KeyP) {
        visible.0 = !visible.0;
    }
}

/// Track the camera's zoom band.
pub fn update_panel_verbosity(zoom: Res<ZoomLevel>, mut verbosity: ResMut<PanelVerbosity>) {
    let want = verbosity_for_zoom(zoom.0);
    if verbosity.0 != want {
        verbosity.0 = want;
    }
}

/// Everything the host reads off a node to build (or retire) its panel.
///
/// Keyed on [`NodeHitbox`], the component that already marks "this is a node"
/// for selection and dragging, so instance, probe and account nodes all arrive
/// here. Everything else is optional because each kind of node carries a
/// different subset.
type PanelNodes<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Option<&'static NodeEntity>,
        Option<&'static SubNode>,
        Option<&'static NodeIdentity>,
        Option<&'static PanelIcon>,
        Option<&'static Visibility>,
        Has<PanelPinned>,
    ),
    With<NodeHitbox>,
>;

/// Whether a node with this visibility should carry a panel.
///
/// A panel tracks its node's projected screen position without ever consulting
/// the node's visibility, so hidden nodes have to be filtered here or their
/// panels float over whatever layer replaced them.
fn shown(visibility: Option<&Visibility>) -> bool {
    visibility != Some(&Visibility::Hidden)
}

/// What a node's panel should look like right now.
///
/// The rules, in the order they win:
///
/// - a hidden node has no panel at all, whatever else is true — including a pin,
///   which would otherwise leave a panel floating over whichever layer hid the
///   node it describes;
/// - a pinned node stays expanded, outranking the `P` toggle, because that
///   toggle is for clearing ambient clutter and a pinned panel is the opposite;
/// - the *only* selected node expands. A batch selection collapses everything,
///   because a screenful of expanded panels is unreadable and none of them is
///   "the" one the user is looking at.
fn panel_state(
    selected_alone: bool,
    pinned: bool,
    visible: bool,
    panels_visible: bool,
    verbosity: Verbosity,
) -> Option<PanelState> {
    if !visible {
        return None;
    }
    if pinned {
        return Some(PanelState::Expanded);
    }
    if !panels_visible {
        return None;
    }
    if selected_alone {
        return Some(PanelState::Expanded);
    }
    Some(PanelState::Collapsed(verbosity))
}

/// The icon a node's panel shows when the node doesn't override it with
/// [`PanelIcon`].
///
/// An instance is worth naming by *what it is* — a server, an agent, a client —
/// which tells the user more than the active layer's icon, already shown by the
/// layer indicator. Nodes standing in for something finer-grained than an
/// instance (a probe device, an account) have no instance type of their own, so
/// they keep the layer icon. A probe device borrows its gateway's `InstanceId`,
/// which is why the `SubNode` check has to come before the id is consulted.
fn fallback_icon_path(
    instance: Option<&NodeEntity>,
    sub: Option<&SubNode>,
    layer: &LayerName,
) -> &'static str {
    match instance
        .filter(|_| sub.is_none())
        .map(|i| i.instance_id.instance_type())
    {
        Some(instance_type) => instance_icon_path(instance_type),
        None => layer_icon_path(layer),
    }
}

/// Spawn, rebuild and despawn node panels to match the world.
#[allow(clippy::too_many_arguments)]
pub fn manage_node_panels(
    mut commands: Commands,
    theme: Res<Theme>,
    registry: Res<LayerRegistry>,
    current_layer: Res<CurrentLayer>,
    panels_visible: Res<PanelsVisible>,
    verbosity: Res<PanelVerbosity>,
    selection: Res<SelectionSet>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut images: ResMut<Assets<Image>>,
    mut icon_cache: ResMut<IconCache>,
    nodes: PanelNodes,
    panels: Query<(Entity, &NodePanelUi)>,
) {
    let solo_selected = match selection.selected_nodes.as_slice() {
        [entity] => Some(*entity),
        _ => None,
    };

    let desired = |node: Entity, visibility: Option<&Visibility>, pinned: bool| {
        panel_state(
            solo_selected == Some(node),
            pinned,
            shown(visibility),
            panels_visible.0,
            verbosity.0,
        )
    };

    // Retire panels whose node is gone, hidden, or whose shape no longer matches.
    // An expanded panel is only rebuilt when its target or layer changes, never
    // for a verbosity change, so scrolling the wheel can't restart a terminal.
    for (panel_entity, panel) in &panels {
        let node = nodes.get(panel.node).ok();
        let want = node.and_then(|(entity, _, _, _, _, visibility, pinned)| {
            desired(entity, visibility, pinned)
        });
        let stale = match (want, panel.state) {
            (Some(PanelState::Expanded), PanelState::Expanded) => panel.layer != **current_layer,
            (Some(PanelState::Collapsed(want)), PanelState::Collapsed(have)) => {
                want != have || panel.layer != **current_layer
            }
            (Some(_), _) => true,
            (None, _) => true,
        };
        if !stale {
            continue;
        }
        if panel.state == PanelState::Expanded
            && let Some(built_by) = registry.get(&panel.layer).and_then(|i| i.panel.clone())
        {
            built_by.on_collapse(&mut commands, panel.target);
        }
        commands.entity(panel_entity).despawn();
    }

    let layer_panel = registry.get(&current_layer).and_then(|i| i.panel.clone());
    let window = windows.single().ok();
    let window_size = window
        .map(|w| Vec2::new(w.width(), w.height()))
        .unwrap_or(Vec2::new(1280.0, 720.0));

    for (node, instance, sub, identity, icon, visibility, pinned) in &nodes {
        let Some(state) = desired(node, visibility, pinned) else {
            continue;
        };
        // Panels retired above are despawned through the command queue, so the
        // stale one is still in `panels` this frame; matching on the node alone
        // would then skip respawning it. Matching on the shape as well means the
        // rebuild lands in the same frame as the teardown.
        let already = panels.iter().any(|(_, panel)| {
            panel.node == node && panel.state == state && panel.layer == **current_layer
        });
        if already {
            continue;
        }

        let target = PanelTarget {
            instance: instance.map(|i| i.instance_id),
            sub: sub.map(|s| s.0),
        };
        let icon_px = match state {
            PanelState::Collapsed(verbosity) => collapsed_icon_px(verbosity),
            PanelState::Expanded => EXPANDED_ICON_PX,
        };
        // Both of these are only the starting values. The identity and the icon
        // are then driven off the node by `update_panel_identities` /
        // `update_panel_icons`, because a node's name and a domain's favicon both
        // arrive after the node does — a value baked in here would be whatever
        // was known on the frame the panel happened to be spawned.
        let fallback_icon = icon_cache.get_or_rasterize(
            &mut images,
            fallback_icon_path(instance, sub, &current_layer),
            icon_px,
        );
        let icon = icon
            .map(|i| i.0.clone())
            .unwrap_or_else(|| fallback_icon.clone());
        let identity = identity.map(|i| i.0.clone()).unwrap_or_default();

        let body = match state {
            PanelState::Collapsed(verbosity) => spawn_collapsed(
                CardParts {
                    commands: &mut commands,
                    theme: &theme,
                    node,
                    identity,
                    icon,
                    fallback_icon,
                    icon_px,
                },
                verbosity,
            ),
            PanelState::Expanded => spawn_expanded(
                CardParts {
                    commands: &mut commands,
                    theme: &theme,
                    node,
                    identity,
                    icon,
                    fallback_icon,
                    icon_px,
                },
                window_size,
            ),
        };
        commands.entity(body.root).insert(NodePanelUi {
            node,
            target,
            state,
            layer: (**current_layer).clone(),
        });

        let Some(layer_panel) = layer_panel.as_ref() else {
            continue;
        };
        build_body(
            &mut commands,
            layer_panel,
            &theme,
            body.body,
            node,
            target,
            state,
        );
    }
}

/// Call into the layer's panel for the body of a freshly spawned panel.
fn build_body(
    commands: &mut Commands,
    panel: &Arc<dyn NodePanel>,
    theme: &Theme,
    body: Entity,
    node: Entity,
    target: PanelTarget,
    state: PanelState,
) {
    let mut ctx = PanelCtx {
        commands,
        body,
        node,
        target,
        theme,
        verbosity: match state {
            PanelState::Collapsed(verbosity) => verbosity,
            PanelState::Expanded => Verbosity::Detailed,
        },
    };
    match state {
        PanelState::Collapsed(_) => panel.build_summary(&mut ctx),
        PanelState::Expanded => panel.build_detail(&mut ctx),
    }
}

/// A spawned panel's root and the entity its body content hangs off.
struct SpawnedPanel {
    root: Entity,
    body: Entity,
}

/// The chrome both panel shapes are built from.
struct CardParts<'a, 'w, 's> {
    commands: &'a mut Commands<'w, 's>,
    theme: &'a Theme,
    /// The node this panel describes.
    node: Entity,
    identity: String,
    icon: Handle<Image>,
    /// The active layer's icon, used whenever the node stops overriding it.
    fallback_icon: Handle<Image>,
    icon_px: u32,
}

/// Spawn the collapsed card: an icon, the identity line, and room for a summary.
fn spawn_collapsed(parts: CardParts, verbosity: Verbosity) -> SpawnedPanel {
    let CardParts {
        commands,
        theme,
        node,
        identity,
        icon,
        fallback_icon,
        icon_px,
    } = parts;

    let font = match verbosity {
        Verbosity::Minimal => theme.metrics.font_sm,
        _ => theme.metrics.font_md,
    };
    let padding = match verbosity {
        Verbosity::Minimal => theme.metrics.space_xs,
        _ => theme.metrics.space_sm + 2.0,
    };

    let root = commands
        .spawn((
            anchored_card(theme, node, collapsed_offset(verbosity), padding),
            // A collapsed panel is a label, not a control: clicks belong to the
            // node underneath it.
            Pickable::IGNORE,
        ))
        .id();

    let column = commands
        .spawn((
            Node {
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(theme.metrics.space_xs),
                ..default()
            },
            children![(
                PanelIdentityLabel { node },
                text(theme, identity, font, Role::Text)
            )],
        ))
        .id();

    // The layer's summary hangs off `body` rather than off the column, so a
    // layer that writes nothing still leaves the identity line intact.
    let body = commands
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(theme.metrics.space_xs),
            ..default()
        })
        .id();
    commands.entity(column).add_child(body);

    let icon_entity = commands
        .spawn((
            PanelIconImage {
                node,
                fallback: fallback_icon,
            },
            card_icon(icon, icon_px as f32),
        ))
        .id();
    commands.entity(root).add_children(&[icon_entity, column]);

    SpawnedPanel { root, body }
}

/// Spawn the expanded panel: a header with the identity and the pin / close
/// buttons over a scrolling body.
///
/// On a narrow window the panel stops following its node and takes over the
/// screen, tagged [`ModalRoot`] so world gestures are locked out until it is
/// closed — there is no room to work in a floating panel on a phone.
fn spawn_expanded(parts: CardParts, window_size: Vec2) -> SpawnedPanel {
    let CardParts {
        commands,
        theme,
        node,
        identity,
        icon,
        fallback_icon,
        icon_px,
    } = parts;

    let mobile = window_size.x < MOBILE_WIDTH;

    let layout = if mobile {
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(0.0),
            top: Val::Px(0.0),
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            border: UiRect::all(Val::Px(1.0)),
            ..default()
        }
    } else {
        Node {
            position_type: PositionType::Absolute,
            width: Val::Px(EXPANDED_WIDTH),
            max_height: Val::Px(window_size.y * EXPANDED_MAX_HEIGHT),
            flex_direction: FlexDirection::Column,
            border: UiRect::all(Val::Px(1.0)),
            ..default()
        }
    };

    let mut root = commands.spawn((
        GlobalZIndex(z::PANEL),
        layout,
        BackgroundColor(theme.color(Role::Panel)),
        ThemedBg(Role::Panel),
        BorderColor::all(theme.color(Role::Border)),
        ThemedBorder(Role::Border),
        BlocksWorldInput,
    ));
    if mobile {
        root.insert(ModalRoot);
    } else {
        root.insert(WorldAnchored {
            target: node,
            offset: Vec2::new(0.0, 55.0),
            clamp: true,
        });
    }
    let root = root.id();

    let header = commands
        .spawn((
            Node {
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::SpaceBetween,
                column_gap: Val::Px(theme.metrics.space_sm),
                width: Val::Percent(100.0),
                padding: UiRect::axes(
                    Val::Px(theme.metrics.space_md),
                    Val::Px(theme.metrics.space_sm),
                ),
                ..default()
            },
            BackgroundColor(theme.color(Role::Surface)),
            ThemedBg(Role::Surface),
        ))
        .id();

    commands.entity(header).with_children(|bar| {
        bar.spawn(Node {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: Val::Px(theme.metrics.space_sm),
            ..default()
        })
        .with_children(|left| {
            left.spawn((
                PanelIconImage {
                    node,
                    fallback: fallback_icon,
                },
                card_icon(icon, icon_px as f32),
            ));
            left.spawn((
                PanelIdentityLabel { node },
                text(theme, identity, theme.metrics.font_lg, Role::Text),
            ));
        });

        bar.spawn(Node {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: Val::Px(theme.metrics.space_sm),
            ..default()
        })
        .with_children(|right| {
            right
                .spawn((PanelPinButton { node }, header_button(theme, "Pin")))
                .observe(
                    move |_: On<Activate>,
                          mut commands: Commands,
                          pinned: Query<(), With<PanelPinned>>| {
                        let Ok(mut entity) = commands.get_entity(node) else {
                            return;
                        };
                        if pinned.contains(node) {
                            entity.remove::<PanelPinned>();
                        } else {
                            entity.insert(PanelPinned);
                        }
                    },
                );
            right.spawn(header_button(theme, "✕")).observe(
                move |_: On<Activate>,
                      mut commands: Commands,
                      mut selection: ResMut<SelectionSet>| {
                    if let Ok(mut entity) = commands.get_entity(node) {
                        entity.remove::<PanelPinned>();
                        entity.remove::<Selected>();
                    }
                    selection
                        .selected_nodes
                        .retain(|&selected| selected != node);
                },
            );
        });
    });

    let body = commands
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            flex_grow: 1.0,
            min_height: Val::Px(0.0),
            row_gap: Val::Px(theme.metrics.space_sm),
            padding: UiRect::all(Val::Px(theme.metrics.space_md)),
            overflow: Overflow::scroll_y(),
            ..default()
        })
        .id();

    commands.entity(root).add_children(&[header, body]);

    SpawnedPanel { root, body }
}

/// Marks a panel's identity line and points back at the node it names.
#[derive(Component)]
pub struct PanelIdentityLabel {
    pub node: Entity,
}

/// Marks a panel's icon and points back at the node it stands for, along with
/// the active layer's icon to fall back on.
#[derive(Component)]
pub struct PanelIconImage {
    pub node: Entity,
    pub fallback: Handle<Image>,
}

/// Keep every panel's identity line in step with its node's [`NodeIdentity`].
///
/// A node is named by whoever owns it, which for an instance means a metadata
/// lookup that lands a frame after the node itself — so a name baked into the
/// panel at spawn time would be blank, permanently, for every node.
pub fn update_panel_identities(
    nodes: Query<&NodeIdentity>,
    mut labels: Query<(&PanelIdentityLabel, &mut Text)>,
) {
    for (label, mut text) in &mut labels {
        let Ok(identity) = nodes.get(label.node) else {
            continue;
        };
        if text.0 != identity.0 {
            text.0 = identity.0.clone();
        }
    }
}

/// Keep every panel's icon in step with its node's [`PanelIcon`], falling back
/// to the active layer's icon when the node doesn't override it.
///
/// Same reason as the identity above: an account's favicon is scraped, so it
/// arrives long after the node it belongs to.
pub fn update_panel_icons(
    overrides: Query<&PanelIcon>,
    mut icons: Query<(&PanelIconImage, &mut ImageNode)>,
) {
    for (icon, mut image) in &mut icons {
        let want = match overrides.get(icon.node) {
            Ok(PanelIcon(handle)) => handle,
            Err(_) => &icon.fallback,
        };
        if image.image != *want {
            image.image = want.clone();
        }
    }
}

/// Marks an expanded panel's pin button and points back at the node it pins.
#[derive(Component)]
pub struct PanelPinButton {
    pub node: Entity,
}

/// Label the pin button for what pressing it will do.
///
/// Pinning doesn't change the panel's shape, so nothing rebuilds it — without
/// this the button would keep reading "Pin" on an already-pinned panel and the
/// pin would have no visible effect at all.
pub fn update_pin_buttons(
    buttons: Query<(&PanelPinButton, &Children)>,
    pinned: Query<(), With<PanelPinned>>,
    mut labels: Query<&mut Text>,
) {
    for (button, children) in &buttons {
        let want = if pinned.contains(button.node) {
            "Unpin"
        } else {
            "Pin"
        };
        for child in children.iter() {
            if let Ok(mut label) = labels.get_mut(child)
                && label.0 != want
            {
                label.0 = want.to_string();
            }
        }
    }
}

/// A compact text button for the expanded panel's header.
fn header_button(theme: &Theme, label: &str) -> impl Bundle {
    (
        Button,
        ThemedButton,
        Hovered::default(),
        Node {
            padding: UiRect::axes(
                Val::Px(theme.metrics.space_sm),
                Val::Px(theme.metrics.space_xs),
            ),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border: UiRect::all(Val::Px(1.0)),
            ..default()
        },
        BackgroundColor(theme.color(Role::Surface)),
        BorderColor::all(theme.color(Role::Border)),
        ThemedBorder(Role::Border),
        children![text(
            theme,
            label.to_string(),
            theme.metrics.font_sm,
            Role::Text
        )],
    )
}

#[cfg(test)]
mod test_node_panel {
    use super::*;
    use sandpolis_instance::InstanceType;

    #[test]
    fn verbosity_follows_zoom() {
        // The projection scale grows as the camera pulls out, so a close-up is
        // the *most* verbose. Getting this backwards would be easy and would
        // only show up as panels that shrink when you zoom in.
        assert_eq!(verbosity_for_zoom(0.5), Verbosity::Detailed);
        assert_eq!(verbosity_for_zoom(1.0), Verbosity::Normal);
        assert_eq!(verbosity_for_zoom(2.0), Verbosity::Minimal);
    }

    /// `panel_state` with everything ordinary: visible, panels on, normal zoom.
    fn state(selected_alone: bool, pinned: bool) -> Option<PanelState> {
        panel_state(selected_alone, pinned, true, true, Verbosity::Normal)
    }

    #[test]
    fn selecting_one_node_expands_only_that_node() {
        assert_eq!(state(true, false), Some(PanelState::Expanded));
        assert_eq!(
            state(false, false),
            Some(PanelState::Collapsed(Verbosity::Normal))
        );
    }

    #[test]
    fn pin_survives_deselection_and_the_visibility_toggle() {
        assert_eq!(state(false, true), Some(PanelState::Expanded));
        assert_eq!(
            panel_state(false, true, true, false, Verbosity::Normal),
            Some(PanelState::Expanded)
        );
    }

    #[test]
    fn hiding_the_node_beats_everything() {
        // Including a pin: a pinned panel over a node the active layer doesn't
        // show would be describing something that isn't on screen.
        assert_eq!(
            panel_state(true, true, false, true, Verbosity::Normal),
            None
        );
    }

    #[test]
    fn the_toggle_clears_unpinned_panels() {
        assert_eq!(
            panel_state(false, false, true, false, Verbosity::Normal),
            None
        );
        assert_eq!(
            panel_state(true, false, true, false, Verbosity::Normal),
            None
        );
    }

    #[test]
    fn collapsed_panels_carry_the_current_verbosity() {
        assert_eq!(
            panel_state(false, false, true, true, Verbosity::Minimal),
            Some(PanelState::Collapsed(Verbosity::Minimal))
        );
        assert_eq!(
            panel_state(false, false, true, true, Verbosity::Detailed),
            Some(PanelState::Collapsed(Verbosity::Detailed))
        );
    }

    /// A node standing in for an instance of the given type.
    fn instance_node(instance_type: InstanceType) -> NodeEntity {
        NodeEntity {
            instance_id: InstanceId::random(instance_type),
        }
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
                    fallback_icon_path(Some(&instance_node(instance_type)), None, &layer),
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
                Some(&instance_node(InstanceType::Server)),
                Some(&SubNode(7)),
                &layer
            ),
            "layer/Probe.svg"
        );

        // An account node has no instance at all.
        assert_eq!(
            fallback_icon_path(None, Some(&SubNode(7)), &LayerName("Account".to_string())),
            "layer/Account.svg"
        );
    }

}
