//! Native theme picker for the Sandpolis GUI.
//!
//! The actual palette/metrics live in [`crate::gui::ui::theme`]; this module is
//! just the `T`-toggled modal that lets the user switch presets at runtime.

use bevy::picking::hover::Hovered;
use bevy::prelude::*;

use crate::gui::ui::gating::{BlocksWorldInput, ModalRoot, UiPointerState};
use crate::gui::ui::scene::{button, modal_scrim_scene, text_line};
use crate::gui::ui::theme::{Role, Theme, ThemePreset, ThemedBg, ThemedBorder};

/// Resource to track theme picker UI state.
#[derive(Resource, Default)]
pub struct ThemePickerState {
    pub show: bool,
}

/// Marker for the native theme picker modal root.
#[derive(Component)]
pub struct ThemePickerRoot;

/// Marker for a theme preset row.
#[derive(Component, Clone, Default)]
pub struct ThemePresetRow {
    pub preset: ThemePreset,
}

/// Toggle the theme picker with the `T` key (unless a text field is focused).
pub fn handle_theme_picker_toggle(
    ui_pointer: Res<UiPointerState>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut picker_state: ResMut<ThemePickerState>,
) {
    // An open modal claims the keyboard (see [`UiPointerState`]), so honor that
    // gate only when this picker isn't the modal in question — otherwise `T`
    // could open the picker but never close it.
    if !picker_state.show && ui_pointer.wants_keyboard {
        return;
    }
    if keyboard.just_pressed(KeyCode::KeyT) {
        picker_state.show = !picker_state.show;
    }
}

/// A clickable row for one theme preset.
fn preset_row(theme: &Theme, preset: ThemePreset) -> impl Scene {
    let surface = theme.color(Role::Surface);
    let labels = vec![
        text_line(theme, preset.name(), Role::Text, theme.metrics.font_md),
        text_line(
            theme,
            preset.description(),
            Role::TextMuted,
            theme.metrics.font_sm,
        ),
    ];
    bsn! {
        ThemePresetRow { preset: {preset} }
        bevy_ui_widgets::Button
        Hovered::default()
        Node {
            flex_direction: FlexDirection::Column,
            width: {Val::Percent(100.0)},
            padding: {UiRect::axes(Val::Px(8.0), Val::Px(6.0))},
            row_gap: {Val::Px(2.0)},
        }
        BackgroundColor({surface})
        on(on_preset_click)
        Children [ {labels} ]
    }
}

/// The theme picker scene, spawned with `spawn_scene` and then
/// `.insert((ThemePickerRoot, BlocksWorldInput))` on the root (those markers
/// don't derive `Clone`).
fn theme_picker_scene(theme: &Theme) -> impl Scene {
    let panel = theme.color(Role::Panel);
    let border = theme.color(Role::Border);
    let heading = text_line(theme, "Theme", Role::Text, theme.metrics.font_heading);
    let rows: Vec<_> = ThemePreset::all()
        .iter()
        .map(|preset| preset_row(theme, *preset))
        .collect();
    let close = button(theme, "Close", on_theme_close);

    bsn! {
        @{modal_scrim_scene()}
        Children [
            Node {
                flex_direction: FlexDirection::Column,
                width: {Val::Px(340.0)},
                padding: {UiRect::all(Val::Px(12.0))},
                row_gap: {Val::Px(8.0)},
                border: {UiRect::all(Val::Px(1.0))},
            }
            BackgroundColor({panel})
            ThemedBg({Role::Panel})
            BorderColor::all(border)
            ThemedBorder({Role::Border})
            Children [
                @heading
                --
                {rows}
                --
                @close
            ]
        ]
    }
}

/// Spawn/despawn the native theme picker modal.
pub fn manage_theme_picker(
    mut commands: Commands,
    theme: Res<Theme>,
    picker_state: Res<ThemePickerState>,
    root: Query<Entity, With<ThemePickerRoot>>,
) {
    let exists = !root.is_empty();
    if picker_state.show && !exists {
        commands.spawn_scene(theme_picker_scene(&theme)).insert((
            ThemePickerRoot,
            BlocksWorldInput,
            ModalRoot,
        ));
    } else if !picker_state.show && exists {
        for entity in &root {
            commands.entity(entity).despawn();
        }
    }
}

/// Highlight the row for the active preset.
pub fn update_theme_rows(
    theme: Res<Theme>,
    mut rows: Query<(Ref<ThemePresetRow>, &mut BackgroundColor)>,
) {
    let changed = theme.is_changed();
    for (row, mut bg) in &mut rows {
        if changed || row.is_added() {
            bg.0 = if row.preset == theme.preset {
                theme.color(Role::Accent)
            } else {
                theme.color(Role::Surface)
            };
        }
    }
}

fn on_preset_click(
    activate: On<bevy_ui_widgets::Activate>,
    rows: Query<&ThemePresetRow>,
    mut theme: ResMut<Theme>,
) {
    if let Ok(row) = rows.get(activate.entity) {
        theme.set_preset(row.preset);
    }
}

fn on_theme_close(
    _activate: On<bevy_ui_widgets::Activate>,
    mut picker_state: ResMut<ThemePickerState>,
) {
    picker_state.show = false;
}
