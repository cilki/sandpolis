//! Layer picker: a modal list of layers with a search box and keyboard
//! navigation. Native `bevy_ui` (migrated off egui). Opened from the layer
//! indicator or the `L` key.

use crate::gui::input::CurrentLayer;
use crate::gui::ui::gating::UiPointerState;
use crate::gui::ui::layer::LayerRegistry;
use crate::gui::ui::panel::modal_scrim;
use crate::gui::ui::text_input::text_input;
use crate::gui::ui::theme::{Role, Theme, ThemedBg, ThemedBorder};
use crate::gui::ui::widgets::{heading, muted, text};
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy_ui_widgets::{Activate, Button};
use sandpolis_instance::LayerName;

/// Layer picker state.
#[derive(Resource, Default)]
pub struct LayerPickerState {
    pub show: bool,
    pub available_layers: Vec<LayerName>,
    pub search_query: String,
    /// Highlighted item index for keyboard navigation.
    pub selected_index: usize,
}

/// Modal root marker.
#[derive(Component)]
pub struct LayerPickerRoot;

/// Search input marker.
#[derive(Component)]
pub struct LayerSearchInput;

/// Rows container marker.
#[derive(Component)]
pub struct LayerRowsContainer;

/// A single selectable layer row.
#[derive(Component)]
pub struct LayerRow {
    layer: LayerName,
}

/// The layers this build has, in the registry's canonical order.
fn available_layers(registry: &LayerRegistry) -> Vec<LayerName> {
    registry.iter().map(|info| info.layer.clone()).collect()
}

/// Description for a layer, from its registered client.
fn get_layer_description(registry: &LayerRegistry, layer: &LayerName) -> &'static str {
    registry
        .get(layer)
        .map(|info| info.description)
        .unwrap_or("No description available")
}

/// Layers matching the current search query.
fn filtered_layers(layers: &[LayerName], query: &str) -> Vec<LayerName> {
    let query = query.to_lowercase();
    layers
        .iter()
        .filter(|l| query.is_empty() || l.name().to_lowercase().contains(&query))
        .cloned()
        .collect()
}

/// Toggle the picker with the `L` key (unless a text field is focused).
pub fn handle_layer_picker_toggle(
    ui_pointer: Res<UiPointerState>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut picker: ResMut<LayerPickerState>,
) {
    // An open modal claims the keyboard (see [`UiPointerState`]), so honor that
    // gate only when this picker isn't the modal in question — otherwise `L`
    // could open the picker but never close it.
    if !picker.show && ui_pointer.wants_keyboard {
        return;
    }
    if keyboard.just_pressed(KeyCode::KeyL) {
        picker.show = !picker.show;
    }
}

/// Spawn the modal when shown, despawn it when hidden.
pub fn manage_layer_picker(
    mut commands: Commands,
    theme: Res<Theme>,
    registry: Res<LayerRegistry>,
    mut picker: ResMut<LayerPickerState>,
    mut focus: ResMut<InputFocus>,
    root: Query<Entity, With<LayerPickerRoot>>,
) {
    let exists = !root.is_empty();
    if picker.show && !exists {
        picker.selected_index = 0;
        picker.available_layers = available_layers(&registry);
        commands
            .spawn((LayerPickerRoot, modal_scrim()))
            .with_children(|scrim| {
                scrim
                    .spawn((
                        Node {
                            flex_direction: FlexDirection::Column,
                            width: Val::Px(360.0),
                            padding: UiRect::all(Val::Px(12.0)),
                            row_gap: Val::Px(8.0),
                            border: UiRect::all(Val::Px(1.0)),
                            ..default()
                        },
                        BackgroundColor(theme.color(Role::Panel)),
                        ThemedBg(Role::Panel),
                        BorderColor::all(theme.color(Role::Border)),
                        ThemedBorder(Role::Border),
                    ))
                    .with_children(|panel| {
                        panel.spawn(heading(&theme, "Layers"));
                        panel.spawn((LayerSearchInput, text_input(&theme)));
                        panel.spawn((
                            LayerRowsContainer,
                            Node {
                                flex_direction: FlexDirection::Column,
                                row_gap: Val::Px(4.0),
                                ..default()
                            },
                        ));
                    });
            });
    } else if !picker.show && exists {
        for entity in &root {
            commands.entity(entity).despawn();
        }
        focus.clear();
    }
}

/// Focus the search box as soon as it is spawned.
pub fn focus_layer_search(
    inputs: Query<Entity, Added<LayerSearchInput>>,
    mut focus: ResMut<InputFocus>,
) {
    if let Ok(entity) = inputs.single() {
        focus.set(entity, FocusCause::Navigated);
    }
}

/// Mirror the search field's contents into [`LayerPickerState`], but only when the
/// text actually changes. Bevy marks [`EditableText`] changed every frame (its
/// layout system mutably touches the editor), so row rebuilds must key off this
/// resource rather than the component's change detection.
pub fn sync_layer_search(
    mut picker: ResMut<LayerPickerState>,
    search: Query<&EditableText, With<LayerSearchInput>>,
) {
    if let Ok(input) = search.single() {
        let value = input.value().to_string();
        if picker.search_query != value {
            picker.search_query = value;
        }
    }
}

/// Rebuild the row list when the query, selection, or active layer changes.
pub fn rebuild_layer_rows(
    mut commands: Commands,
    theme: Res<Theme>,
    registry: Res<LayerRegistry>,
    picker: Res<LayerPickerState>,
    current: Res<CurrentLayer>,
    container: Query<Entity, With<LayerRowsContainer>>,
    new_container: Query<(), Added<LayerRowsContainer>>,
    rows: Query<Entity, With<LayerRow>>,
) {
    let Ok(container_entity) = container.single() else {
        return;
    };
    // Rebuild on the frame the panel first appears, then only when the query,
    // selection (both on the resource), or active layer change.
    if !(picker.is_changed() || current.is_changed() || !new_container.is_empty()) {
        return;
    }

    for entity in &rows {
        commands.entity(entity).despawn();
    }

    let filtered = filtered_layers(&picker.available_layers, &picker.search_query);
    let selected = picker.selected_index.min(filtered.len().saturating_sub(1));

    commands.entity(container_entity).with_children(|parent| {
        if filtered.is_empty() {
            parent.spawn(muted(&theme, "No matching layers", theme.metrics.font_md));
            return;
        }
        for (index, layer) in filtered.into_iter().enumerate() {
            let role = if **current == layer {
                Role::Accent
            } else if index == selected {
                Role::SurfaceActive
            } else {
                Role::Surface
            };
            parent
                .spawn((
                    LayerRow {
                        layer: layer.clone(),
                    },
                    Button,
                    Hovered::default(),
                    Node {
                        flex_direction: FlexDirection::Column,
                        width: Val::Percent(100.0),
                        padding: UiRect::axes(Val::Px(8.0), Val::Px(6.0)),
                        row_gap: Val::Px(2.0),
                        ..default()
                    },
                    BackgroundColor(theme.color(role)),
                    children![
                        text(
                            &theme,
                            layer.name().to_string(),
                            theme.metrics.font_md,
                            Role::Text
                        ),
                        muted(
                            &theme,
                            get_layer_description(&registry, &layer),
                            theme.metrics.font_sm
                        ),
                    ],
                ))
                .observe(on_row_click);
        }
    });
}

/// Select the clicked layer and close the picker.
fn on_row_click(
    activate: On<Activate>,
    rows: Query<&LayerRow>,
    mut current: ResMut<CurrentLayer>,
    mut picker: ResMut<LayerPickerState>,
) {
    if let Ok(row) = rows.get(activate.entity) {
        **current = row.layer.clone();
        picker.show = false;
    }
}

/// Keyboard navigation: arrows move the selection, Enter confirms, Escape closes.
pub fn layer_picker_keys(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut picker: ResMut<LayerPickerState>,
    mut current: ResMut<CurrentLayer>,
) {
    if !picker.show {
        return;
    }
    if keyboard.just_pressed(KeyCode::Escape) {
        picker.show = false;
        return;
    }

    let filtered = filtered_layers(&picker.available_layers, &picker.search_query);
    if filtered.is_empty() {
        return;
    }

    if keyboard.just_pressed(KeyCode::ArrowDown) {
        picker.selected_index = (picker.selected_index + 1) % filtered.len();
    }
    if keyboard.just_pressed(KeyCode::ArrowUp) {
        picker.selected_index = picker
            .selected_index
            .checked_sub(1)
            .unwrap_or(filtered.len() - 1);
    }
    if keyboard.just_pressed(KeyCode::Enter) {
        let index = picker.selected_index.min(filtered.len() - 1);
        **current = filtered[index].clone();
        picker.show = false;
    }
}
