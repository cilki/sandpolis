//! Themed widget spawn-helpers built on `bevy_ui` + `bevy_ui_widgets`.
//!
//! These return `impl Bundle` so they compose with `children![]` and
//! `commands.spawn(...)`. Colors come from the [`Theme`] at spawn time and are
//! kept in sync afterward by the recolor systems via the `Themed*` markers.
//!
//! Heavier widgets (text input, scroll view, dropdown, floating panel, modal,
//! list view) are added in later migration phases as their consumers arrive.

use super::theme::{Role, Theme, ThemedBorder, ThemedButton, ThemedText};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy_ui_widgets::Button;

/// A text node in the given role/size.
pub fn text(theme: &Theme, content: impl Into<String>, size: f32, role: Role) -> impl Bundle {
    (
        Text::new(content),
        theme.text_font(size),
        TextColor(theme.color(role)),
        ThemedText(role),
    )
}

/// A heading-sized text node.
pub fn heading(theme: &Theme, content: impl Into<String>) -> impl Bundle {
    text(theme, content, theme.metrics.font_heading, Role::Text)
}

/// A de-emphasized text node.
pub fn muted(theme: &Theme, content: impl Into<String>, size: f32) -> impl Bundle {
    text(theme, content, size, Role::TextMuted)
}

/// A vertical container with panel styling (fill + border + rounded corners).
pub fn panel(theme: &Theme) -> impl Bundle {
    (
        Node {
            flex_direction: FlexDirection::Column,
            padding: UiRect::all(Val::Px(theme.metrics.space_md)),
            row_gap: Val::Px(theme.metrics.space_sm),
            border: UiRect::all(Val::Px(1.0)),
            ..default()
        },
        BackgroundColor(theme.color(Role::Panel)),
        super::theme::ThemedBg(Role::Panel),
        BorderColor::all(theme.color(Role::Border)),
        ThemedBorder(Role::Border),
    )
}

/// A titled block: a heading over a gapped column its caller fills.
///
/// Panel bodies are mostly a stack of these, so they're worth not rebuilding by
/// hand each time.
pub fn section(theme: &Theme, title: impl Into<String>) -> impl Bundle {
    (
        Node {
            flex_direction: FlexDirection::Column,
            width: Val::Percent(100.0),
            row_gap: Val::Px(theme.metrics.space_sm),
            margin: UiRect::bottom(Val::Px(theme.metrics.space_sm)),
            ..default()
        },
        children![text(theme, title, theme.metrics.font_lg, Role::Text)],
    )
}

/// A horizontal flex row with centered items and the given column gap.
pub fn row(gap: f32) -> Node {
    Node {
        flex_direction: FlexDirection::Row,
        align_items: AlignItems::Center,
        column_gap: Val::Px(gap),
        ..default()
    }
}

/// A vertical flex column with the given row gap.
pub fn column(gap: f32) -> Node {
    Node {
        flex_direction: FlexDirection::Column,
        row_gap: Val::Px(gap),
        ..default()
    }
}

/// A themed button with a centered text label.
///
/// Emits [`bevy_ui_widgets::Activate`] on click; attach an observer on the spawned
/// entity for the action, e.g. `.observe(|_: On<Activate>, ...| { ... })`.
pub fn button(theme: &Theme, label: impl Into<String>) -> impl Bundle {
    (
        Button,
        ThemedButton,
        Hovered::default(),
        Node {
            padding: UiRect::axes(
                Val::Px(theme.metrics.space_md),
                Val::Px(theme.metrics.space_sm),
            ),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border: UiRect::all(Val::Px(1.0)),
            ..default()
        },
        BackgroundColor(theme.color(Role::Surface)),
        BorderColor::all(theme.color(Role::Border)),
        ThemedBorder(Role::Border),
        children![text(theme, label, theme.metrics.font_md, Role::Text)],
    )
}

/// A themed, square icon-only button.
///
/// The caller rasterizes the SVG (via `IconCache`) and passes the resulting
/// [`Handle<Image>`]. Emits [`bevy_ui_widgets::Activate`] on click. Pair it with a
/// [`super::tooltip::Tooltip`] to surface the action's full label on hover.
pub fn icon_button(theme: &Theme, icon: Handle<Image>) -> impl Bundle {
    const BUTTON_PX: f32 = 30.0;
    const ICON_PX: f32 = 20.0;
    (
        Button,
        ThemedButton,
        Hovered::default(),
        Node {
            width: Val::Px(BUTTON_PX),
            height: Val::Px(BUTTON_PX),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border: UiRect::all(Val::Px(1.0)),
            ..default()
        },
        BackgroundColor(theme.color(Role::Surface)),
        BorderColor::all(theme.color(Role::Border)),
        ThemedBorder(Role::Border),
        children![(
            ImageNode::new(icon),
            Node {
                width: Val::Px(ICON_PX),
                height: Val::Px(ICON_PX),
                ..default()
            },
        )],
    )
}
