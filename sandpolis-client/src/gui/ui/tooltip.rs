//! Hover tooltips for `bevy_ui` nodes.
//!
//! Attach a [`Tooltip`] to any hoverable widget (anything with a
//! [`Hovered`]); while it is hovered, a single transient popup follows the
//! cursor showing the tooltip's text. There is at most one popup at a time.
//!
//! World-space hovers have no `Hovered` to hang a [`Tooltip`] on, so they
//! drive the same popup through the [`WorldTooltip`] resource instead.

use super::theme::{Role, Theme, ThemedBg, ThemedBorder};
use super::widgets::text;
use super::z;
use bevy::picking::Pickable;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;

/// Installs the tooltip popup system.
pub struct TooltipPlugin;

impl Plugin for TooltipPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WorldTooltip>()
            .add_systems(Update, manage_tooltip);
    }
}

/// Tooltip text requested by a world-space hover, where there's no `bevy_ui`
/// node to carry a [`Tooltip`]. Whoever is hovering sets it each frame and
/// clears it when the hover ends; a widget tooltip takes precedence.
#[derive(Resource, Default)]
pub struct WorldTooltip(pub Option<String>);

/// Full-text tooltip shown while the host widget is hovered.
#[derive(Component)]
pub struct Tooltip {
    pub text: String,
}

impl Tooltip {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// Marker for the transient popup node.
#[derive(Component)]
pub struct TooltipPopup;

/// Marker for the popup's text label.
#[derive(Component)]
pub struct TooltipText;

/// Spawn / move / despawn the single tooltip popup to track the hovered widget.
pub fn manage_tooltip(
    mut commands: Commands,
    theme: Res<Theme>,
    windows: Query<&Window>,
    hovered: Query<(&Hovered, &Tooltip)>,
    world: Res<WorldTooltip>,
    popup: Query<Entity, With<TooltipPopup>>,
    mut popup_node: Query<&mut Node, With<TooltipPopup>>,
    mut popup_text: Query<&mut Text, With<TooltipText>>,
) {
    let target = hovered
        .iter()
        .find_map(|(hovered, tooltip)| hovered.get().then(|| tooltip.text.clone()))
        .or_else(|| world.0.clone());

    let Some(label) = target else {
        for entity in &popup {
            commands.entity(entity).despawn();
        }
        return;
    };

    let Ok(window) = windows.single() else {
        return;
    };
    let Some(cursor) = window.cursor_position() else {
        return;
    };
    let (x, y) = (cursor.x + 12.0, cursor.y + 18.0);

    if let Ok(mut node) = popup_node.single_mut() {
        node.left = Val::Px(x);
        node.top = Val::Px(y);
        if let Ok(mut current) = popup_text.single_mut()
            && current.0 != label
        {
            current.0 = label;
        }
    } else {
        commands.spawn((
            TooltipPopup,
            Pickable::IGNORE,
            GlobalZIndex(z::POPUP),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(x),
                top: Val::Px(y),
                padding: UiRect::axes(Val::Px(8.0), Val::Px(4.0)),
                border: UiRect::all(Val::Px(1.0)),
                ..default()
            },
            BackgroundColor(theme.color(Role::Panel)),
            ThemedBg(Role::Panel),
            BorderColor::all(theme.color(Role::Border)),
            ThemedBorder(Role::Border),
            children![(
                TooltipText,
                Pickable::IGNORE,
                text(&theme, label, theme.metrics.font_sm, Role::Text),
            )],
        ));
    }
}
