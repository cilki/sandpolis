//! Structural containers: modal overlays and floating panels.
//!
//! `bevy_ui` has no window/dialog concept, so these provide the egui `Window`
//! affordances we rely on: a full-screen modal scrim (centered, blocks the world)
//! and a draggable, titled, closable [`FloatingPanel`] (the database browser and
//! services panel).

use super::gating::{BlocksWorldInput, ModalRoot};
use super::theme::{Role, Theme, ThemedBg, ThemedBorder};
use super::widgets::text;
use super::z;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy_ui_widgets::{Activate, Button};

/// A full-screen modal scrim that centers its child content and blocks the world
/// beneath it. Spawn the dialog body as a child.
pub fn modal_scrim() -> impl Bundle {
    (
        ModalRoot,
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(0.0),
            top: Val::Px(0.0),
            right: Val::Px(0.0),
            bottom: Val::Px(0.0),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            ..default()
        },
        BackgroundColor(Color::BLACK.with_alpha(0.55)),
        BlocksWorldInput,
        GlobalZIndex(z::MODAL),
    )
}

/// Marker for a floating panel root.
#[derive(Component)]
pub struct FloatingPanel;

/// Marks a titlebar and points back at its panel root (used by the drag observers).
#[derive(Component)]
struct PanelTitlebar(Entity);

/// Records a panel's top-left at drag start so dragging is relative.
#[derive(Component, Default)]
struct PanelDragOrigin(Vec2);

/// Marks a panel's body node; controller content is spawned as its children.
#[derive(Component)]
pub struct PanelBody;

/// Fired (targeted at the panel root) when the panel's close button is clicked.
#[derive(EntityEvent)]
pub struct PanelClosed {
    pub entity: Entity,
}

/// The entities of a spawned [`FloatingPanel`].
pub struct FloatingPanelEntities {
    /// The panel root (carries [`FloatingPanel`]).
    pub root: Entity,
    /// The body node; spawn controller content as its children.
    pub body: Entity,
}

/// Spawn a draggable, titled, closable floating panel at `pos` with the given
/// `size`. Returns the root and body entities. Closing emits [`PanelClosed`]
/// (targeted at the root) for the host to react to; it does not despawn itself.
pub fn spawn_floating_panel(
    commands: &mut Commands,
    theme: &Theme,
    title: impl Into<String>,
    pos: Vec2,
    size: Vec2,
) -> FloatingPanelEntities {
    let root = commands
        .spawn((
            FloatingPanel,
            PanelDragOrigin::default(),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(pos.x),
                top: Val::Px(pos.y),
                width: Val::Px(size.x),
                height: Val::Px(size.y),
                flex_direction: FlexDirection::Column,
                border: UiRect::all(Val::Px(1.0)),
                ..default()
            },
            BackgroundColor(theme.color(Role::Panel)),
            ThemedBg(Role::Panel),
            BorderColor::all(theme.color(Role::Border)),
            ThemedBorder(Role::Border),
            BlocksWorldInput,
            GlobalZIndex(z::PANEL),
        ))
        .id();

    // Titlebar: title on the left, close button on the right; whole bar is a drag
    // handle.
    let titlebar = commands
        .spawn((
            PanelTitlebar(root),
            Node {
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::SpaceBetween,
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
        .observe(on_titlebar_drag_start)
        .observe(on_titlebar_drag)
        .id();

    commands.entity(titlebar).with_children(|bar| {
        bar.spawn(text(theme, title, theme.metrics.font_md, Role::Text));
        bar.spawn((
            Button,
            Hovered::default(),
            Node {
                padding: UiRect::axes(Val::Px(theme.metrics.space_sm), Val::Px(0.0)),
                ..default()
            },
            children![text(theme, "✕", theme.metrics.font_md, Role::TextMuted)],
        ))
        .observe(move |_: On<Activate>, mut commands: Commands| {
            commands.trigger(PanelClosed { entity: root });
        });
    });

    let body = commands
        .spawn((
            PanelBody,
            Node {
                flex_direction: FlexDirection::Column,
                flex_grow: 1.0,
                padding: UiRect::all(Val::Px(theme.metrics.space_md)),
                row_gap: Val::Px(theme.metrics.space_sm),
                overflow: Overflow::clip(),
                ..default()
            },
        ))
        .id();

    commands.entity(root).add_children(&[titlebar, body]);

    FloatingPanelEntities { root, body }
}

/// Record the panel's current top-left when a titlebar drag begins.
fn on_titlebar_drag_start(
    drag: On<PointerDragStart>,
    titlebars: Query<&PanelTitlebar>,
    mut panels: Query<(&Node, &mut PanelDragOrigin), With<FloatingPanel>>,
) {
    let Ok(titlebar) = titlebars.get(drag.entity) else {
        return;
    };
    if let Ok((node, mut origin)) = panels.get_mut(titlebar.0) {
        let x = if let Val::Px(v) = node.left { v } else { 0.0 };
        let y = if let Val::Px(v) = node.top { v } else { 0.0 };
        origin.0 = Vec2::new(x, y);
    }
}

/// Move the panel as the titlebar is dragged.
fn on_titlebar_drag(
    drag: On<PointerDrag>,
    titlebars: Query<&PanelTitlebar>,
    mut panels: Query<(&mut Node, &PanelDragOrigin), With<FloatingPanel>>,
) {
    let Ok(titlebar) = titlebars.get(drag.entity) else {
        return;
    };
    if let Ok((mut node, origin)) = panels.get_mut(titlebar.0) {
        node.left = Val::Px(origin.0.x + drag.distance.x);
        node.top = Val::Px(origin.0.y + drag.distance.y);
    }
}
