//! `bsn!` scene authoring for the Sandpolis GUI.
//!
//! Bevy's next-generation scenes: the [`bsn!`] macro builds an entity tree
//! declaratively, and [`Commands::spawn_scene`] spawns it. This module
//! establishes the conventions for using it alongside the codebase's existing
//! retained-mode theming, and provides the shared scene-returning widget helpers
//! ([`text_line`], [`bound_text`], [`button`], [`text_input`],
//! [`modal_scrim_scene`] — scene counterparts of the `impl Bundle` helpers in
//! [`super::widgets`], which cannot be spliced into `bsn!`).
//!
//! # Pattern
//!
//! - **Spawn** a scene with `commands.spawn_scene(my_scene(&theme))`. The returned
//!   `EntityCommands` lets you `.insert(...)` extra components on the root (handy
//!   for marker components that don't implement `Clone`/`Default`, which `bsn!`
//!   requires). Markers on *child* entities must be declared inside the scene, so
//!   they need `Clone + Default` derives.
//! - **Build into an existing entity** (e.g. a panel body handed to
//!   [`super::node_panel::NodePanel`]) with
//!   `commands.entity(body).apply_scene(bsn! { Children [ .. ] })`.
//! - **Theme colors** are captured into locals and embedded with `{...}`:
//!   `let panel = theme.color(Role::Panel); bsn! { BackgroundColor({panel}) }`.
//!   Always also attach the matching `Themed*` marker ([`ThemedBg`],
//!   [`ThemedText`], [`ThemedBorder`]) so the recolor systems in
//!   [`super::theme`] repaint the node when the theme preset changes.
//! - **Runtime component values** (anything built by a call, e.g.
//!   `BorderColor::all(..)`, or a local holding a component) are written as a
//!   bare expression — `BorderColor::all(border)` or `cursor` — which works for
//!   any `Clone + Default` component. [`TextFont`] is the notable exception: it
//!   has a manual `FromTemplate` (asset-templated font field), so patch
//!   `font_size` with field syntax instead.
//! - **Behavior** is attached with [`on(..)`](on), which registers an entity
//!   observer for an [`EntityEvent`] (e.g. `on(on_help_close)` for an
//!   `On<Activate>` handler).
//! - **Scene references** (a scene held in a variable or returned by a function)
//!   are prefixed with `@`: `@{modal_scrim_scene()}` splices that scene into the
//!   current entity.
//! - **Dynamic children** go in a `Children [..]` list with sibling entries
//!   separated by `--`: a single scene held in a local is `@scene`, and a
//!   `Vec<impl Scene>` (any `SceneList`) is spliced with `{list}`. Beware that
//!   scene helpers are generic: two calls with different closure/string argument
//!   types return *different* `impl Scene` types and cannot share one `Vec`.
//!
//! # What is *not* migrated
//!
//! Scenes that must hand back child entity ids synchronously at spawn time — most
//! notably [`super::panel::spawn_floating_panel`], whose callers spawn their
//! content into the returned `body` entity — don't fit the `spawn_scene` model
//! cleanly and are intentionally left imperative. The full migration status is
//! tracked in `AGENTS.md`.

use super::bind::bind_text;
use super::gating::WantsKeyboard;
use super::theme::{Role, Theme, ThemedBg, ThemedBorder, ThemedText};
use super::z;
use crate::gui::input::on_help_close;
use bevy::ecs::system::IntoObserverSystem;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{EditableText, TextCursorStyle};
use bevy_ui_widgets::{Activate, Button, TextInput};

/// A single themed text line.
pub fn text_line(theme: &Theme, content: impl Into<String>, role: Role, size: f32) -> impl Scene {
    let content = content.into();
    let color = theme.color(role);
    // `TextFont` has a manual `FromTemplate` (its `font` field is asset-templated),
    // so it can't be written as a plain value; patch the size with field syntax.
    // A custom `Theme::font` isn't applied here — the default font is used, which
    // matches the app's default configuration.
    bsn! {
        Text::new(content)
        TextFont { font_size: {size} }
        TextColor({color})
        ThemedText({role})
    }
}

/// A themed text line whose content is kept in sync by a [`super::bind::BindText`]
/// projection.
pub fn bound_text(
    theme: &Theme,
    role: Role,
    size: f32,
    project: impl Fn() -> String + Send + Sync + 'static,
) -> impl Scene {
    let bound = bind_text(project);
    bsn! {
        @{text_line(theme, "", role, size)}
        bound
    }
}

/// A themed button with a centered text label whose `On<Activate>` is handled by
/// `observer`.
pub fn button<M: 'static>(
    theme: &Theme,
    label: impl Into<String>,
    observer: impl IntoObserverSystem<Activate, M> + Clone + Sync,
) -> impl Scene {
    let label = text_line(theme, label, Role::Text, theme.metrics.font_md);
    let surface = theme.color(Role::Surface);
    let border = theme.color(Role::Border);
    let pad_x = theme.metrics.space_md;
    let pad_y = theme.metrics.space_sm;
    bsn! {
        Button
        super::theme::ThemedButton
        Hovered::default()
        Node {
            padding: {UiRect::axes(Val::Px(pad_x), Val::Px(pad_y))},
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border: {UiRect::all(Val::Px(1.0))},
        }
        BackgroundColor({surface})
        BorderColor::all(border)
        ThemedBorder({Role::Border})
        on(observer)
        Children [ @label ]
    }
}

/// A themed single-line [`EditableText`] field; scene counterpart of
/// [`super::text_input::text_input`]. Attach the caller's query marker on the
/// same entity, e.g. `( MyInput {scene::text_input(&theme)} )`.
pub fn text_input(theme: &Theme) -> impl Scene {
    let font_size = theme.metrics.font_md;
    let text_color = theme.color(Role::Text);
    let surface = theme.color(Role::Surface);
    let border = theme.color(Role::Border);
    let cursor = TextCursorStyle {
        color: theme.color(Role::Text),
        selection_color: theme.color(Role::Accent),
        ..default()
    };
    bsn! {
        WantsKeyboard
        TextInput
        EditableText { allow_newlines: false }
        TextLayout::no_wrap()
        cursor
        TextFont { font_size: {font_size} }
        TextColor({text_color})
        ThemedText({Role::Text})
        TabIndex(0)
        Node {
            min_width: {Val::Px(220.0)},
            padding: {UiRect::axes(Val::Px(8.0), Val::Px(5.0))},
            border: {UiRect::all(Val::Px(1.0))},
            align_items: AlignItems::Center,
            overflow: {Overflow::clip()},
            flex_shrink: 0.0,
        }
        BackgroundColor({surface})
        ThemedBg({Role::Surface})
        BorderColor::all(border)
        ThemedBorder({Role::Border})
    }
}

/// A full-screen modal scrim that centers its child content; scene counterpart of
/// [`super::panel::modal_scrim`]. `BlocksWorldInput` isn't `Clone`, so callers
/// must insert it (with their root marker) on the spawned root.
pub fn modal_scrim_scene() -> impl Scene {
    let scrim = Color::BLACK.with_alpha(0.55);
    bsn! {
        Node {
            position_type: PositionType::Absolute,
            left: {Val::Px(0.0)},
            top: {Val::Px(0.0)},
            right: {Val::Px(0.0)},
            bottom: {Val::Px(0.0)},
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
        }
        BackgroundColor({scrim})
        GlobalZIndex({z::MODAL})
    }
}

/// The keyboard-shortcuts help modal, built with `bsn!`.
///
/// Spawn with `commands.spawn_scene(help_modal_scene(&theme, SHORTCUTS))` and then
/// `.insert((HelpRoot, BlocksWorldInput))` on the returned root (those markers
/// don't derive `Clone`, so they're added imperatively rather than in `bsn!`).
pub fn help_modal_scene(theme: &Theme, shortcuts: &[&str]) -> impl Scene {
    let scrim = Color::BLACK.with_alpha(0.55);
    let panel = theme.color(Role::Panel);
    let border = theme.color(Role::Border);

    let heading = text_line(
        theme,
        "Keyboard Shortcuts",
        Role::Text,
        theme.metrics.font_heading,
    );
    let lines: Vec<_> = shortcuts
        .iter()
        .map(|line| text_line(theme, *line, Role::TextMuted, theme.metrics.font_md))
        .collect();
    let close = button(theme, "Close", on_help_close);

    bsn! {
        Node {
            position_type: PositionType::Absolute,
            left: {Val::Px(0.0)},
            top: {Val::Px(0.0)},
            right: {Val::Px(0.0)},
            bottom: {Val::Px(0.0)},
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
        }
        BackgroundColor({scrim})
        GlobalZIndex({z::MODAL})
        Children [
            Node {
                flex_direction: FlexDirection::Column,
                width: {Val::Px(360.0)},
                padding: {UiRect::all(Val::Px(16.0))},
                row_gap: {Val::Px(6.0)},
                border: {UiRect::all(Val::Px(1.0))},
            }
            BackgroundColor({panel})
            ThemedBg({Role::Panel})
            BorderColor::all(border)
            ThemedBorder({Role::Border})
            Children [
                @heading
                --
                {lines}
                --
                @close
            ]
        ]
    }
}
