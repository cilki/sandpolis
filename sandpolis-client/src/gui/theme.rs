//! Native theme picker for the Sandpolis GUI.
//!
//! The theming machinery lives in [`gesso::ui::theme`]; this module is the set
//! of palettes Sandpolis offers and the `T`-toggled modal that lets the user
//! switch between them at runtime.

use bevy::picking::hover::Hovered;
use bevy::prelude::*;

use gesso::ui::gating::{BlocksWorldInput, ModalRoot, UiPointerState};
use gesso::ui::scene::{button, modal_scrim_scene, text_line};
use gesso::ui::theme::{Palette, Role, Theme, ThemedBg, ThemedBorder};

/// Available theme presets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ThemePreset {
    #[default]
    SandpolisDark,
    SandpolisLight,
    Classic,
    HighContrast,
}

impl ThemePreset {
    /// Human-readable name.
    pub fn name(&self) -> &'static str {
        match self {
            ThemePreset::SandpolisDark => "Sandpolis Dark",
            ThemePreset::SandpolisLight => "Sandpolis Light",
            ThemePreset::Classic => "Classic Dark",
            ThemePreset::HighContrast => "High Contrast",
        }
    }

    /// One-line description.
    pub fn description(&self) -> &'static str {
        match self {
            ThemePreset::SandpolisDark => "Default dark theme with blue and purple accents",
            ThemePreset::SandpolisLight => "Light theme for bright environments",
            ThemePreset::Classic => "Standard dark theme",
            ThemePreset::HighContrast => "High contrast theme for accessibility",
        }
    }

    /// All presets, for building a theme picker.
    pub fn all() -> &'static [ThemePreset] {
        &[
            ThemePreset::SandpolisDark,
            ThemePreset::SandpolisLight,
            ThemePreset::Classic,
            ThemePreset::HighContrast,
        ]
    }

    /// The color palette for this preset.
    pub fn palette(&self) -> Palette {
        match self {
            // The palette gesso starts with, so the picker's first row
            // matches what is on screen before anyone picks.
            ThemePreset::SandpolisDark => Palette::default(),
            ThemePreset::SandpolisLight => Palette {
                background: Color::srgb_u8(240, 242, 245),
                panel: Color::srgb_u8(250, 251, 253),
                surface: Color::srgb_u8(255, 255, 255),
                surface_hover: Color::srgb_u8(232, 238, 248),
                surface_active: Color::srgb_u8(220, 228, 246),
                accent: Color::srgb_u8(60, 90, 180),
                text: Color::srgb_u8(40, 44, 52),
                text_muted: Color::srgb_u8(110, 115, 125),
                border: Color::srgb_u8(200, 210, 230),
                warn: Color::srgb_u8(180, 140, 40),
                error: Color::srgb_u8(180, 60, 60),
            },
            ThemePreset::Classic => Palette {
                background: Color::srgb_u8(27, 27, 27),
                panel: Color::srgb_u8(37, 37, 37),
                surface: Color::srgb_u8(60, 60, 60),
                surface_hover: Color::srgb_u8(75, 75, 75),
                surface_active: Color::srgb_u8(90, 90, 90),
                accent: Color::srgb_u8(90, 140, 200),
                text: Color::srgb_u8(240, 240, 240),
                text_muted: Color::srgb_u8(160, 160, 160),
                border: Color::srgb_u8(70, 70, 70),
                warn: Color::srgb_u8(230, 200, 90),
                error: Color::srgb_u8(220, 90, 90),
            },
            ThemePreset::HighContrast => Palette {
                background: Color::BLACK,
                panel: Color::srgb_u8(20, 20, 20),
                surface: Color::srgb_u8(30, 30, 30),
                surface_hover: Color::srgb_u8(60, 60, 60),
                surface_active: Color::srgb_u8(90, 75, 0),
                accent: Color::srgb_u8(255, 200, 0),
                text: Color::WHITE,
                text_muted: Color::srgb_u8(200, 200, 200),
                border: Color::srgb_u8(255, 200, 0),
                warn: Color::srgb_u8(255, 220, 0),
                error: Color::srgb_u8(255, 60, 60),
            },
        }
    }
}

/// The preset the [`Theme`]'s palette was last set from.
#[derive(Resource, Default, Clone, Copy, PartialEq, Eq)]
pub struct ActiveThemePreset(pub ThemePreset);

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
    active: Res<ActiveThemePreset>,
    mut rows: Query<(Ref<ThemePresetRow>, &mut BackgroundColor)>,
) {
    let changed = theme.is_changed();
    for (row, mut bg) in &mut rows {
        if changed || row.is_added() {
            bg.0 = if row.preset == active.0 {
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
    mut active: ResMut<ActiveThemePreset>,
) {
    if let Ok(row) = rows.get(activate.entity) {
        active.0 = row.preset;
        theme.palette = row.preset.palette();
    }
}

fn on_theme_close(
    _activate: On<bevy_ui_widgets::Activate>,
    mut picker_state: ResMut<ThemePickerState>,
) {
    picker_state.show = false;
}
