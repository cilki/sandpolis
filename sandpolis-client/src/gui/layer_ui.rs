//! Layer indicator: a small always-on chrome element (bottom-right, above the
//! minimap) showing the active layer's icon + name. Clicking it opens the layer
//! picker.
//!
//! This is native `bevy_ui` (migrated off egui). It is spawned once and updated
//! by systems: the label/icon change when the active layer changes, and the whole
//! element fades in on a layer change.

use crate::gui::input::CurrentLayer;
use crate::gui::layer_picker::LayerPickerState;
use crate::gui::layer_toolbar::LayerToolbarRoot;
use crate::gui::ui::gating::BlocksWorldInput;
use crate::gui::ui::icon::IconCache;
use crate::gui::ui::theme::{Role, Theme, button_surface};
use crate::gui::ui::z;
use bevy::image::Image;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui::Pressed;
use bevy_ui_widgets::{Activate, Button};
use sandpolis_instance::LayerName;

/// How long the fade-in takes after a layer change, in seconds.
const FADE_IN_SECS: f32 = 0.4;
/// Icon rasterization size in pixels.
const ICON_PX: u32 = 24;

/// Marker for the bottom-right chrome column that holds the layer toolbar (top)
/// and the layer indicator (bottom, just above the minimap).
#[derive(Component)]
pub struct LayerChrome;

/// Marker for the layer indicator root (the clickable element).
#[derive(Component)]
pub struct LayerIndicator;

/// Marker for the indicator's icon child.
#[derive(Component)]
pub struct LayerIndicatorIcon;

/// Marker for the indicator's label child.
#[derive(Component)]
pub struct LayerIndicatorLabel;

/// Resource holding the fade timer for the layer indicator.
#[derive(Resource)]
pub struct LayerIndicatorState {
    pub show_timer: Timer,
}

impl Default for LayerIndicatorState {
    fn default() -> Self {
        Self {
            show_timer: Timer::from_seconds(3.0, TimerMode::Once),
        }
    }
}

/// Display name for a layer.
pub fn layer_display_name(layer: &LayerName) -> &str {
    layer.name()
}

/// SVG asset path (relative to the asset root) for a layer's icon.
pub fn layer_icon_path(layer: &LayerName) -> &'static str {
    match layer.name() {
        "Account" => "layer/Account.svg",
        "Audit" => "layer/Audit.svg",
        "Desktop" => "layer/Desktop.svg",
        "Filesystem" => "layer/Filesystem.svg",
        "Health" => "layer/Health.svg",
        "Inventory" => "layer/Inventory.svg",
        "Probe" => "layer/Probe.svg",
        "Shell" => "layer/Shell.svg",
        "Snapshot" => "layer/Snapshot.svg",
        "Tunnel" => "layer/Tunnel.svg",
        // Network and the instance-type layers share the network icon.
        _ => "layer/Network.svg",
    }
}

/// Spawn the layer indicator UI (startup).
pub fn spawn_layer_indicator(
    mut commands: Commands,
    theme: Res<Theme>,
    current_layer: Res<CurrentLayer>,
    mut images: ResMut<Assets<Image>>,
    mut icon_cache: ResMut<IconCache>,
) {
    let icon = icon_cache.get_or_rasterize(&mut images, layer_icon_path(&current_layer), ICON_PX);

    commands
        .spawn((
            LayerChrome,
            GlobalZIndex(z::CHROME),
            Node {
                position_type: PositionType::Absolute,
                right: Val::Px(10.0),
                bottom: Val::Px(168.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::FlexEnd,
                row_gap: Val::Px(6.0),
                ..default()
            },
        ))
        .with_children(|chrome| {
            // Toolbar row (above the indicator): same width as the indicator so its
            // first button's left edge aligns with the indicator's left edge;
            // buttons wrap to a second row if they exceed that width.
            chrome.spawn((
                LayerToolbarRoot,
                BlocksWorldInput,
                Node {
                    width: Val::Px(200.0),
                    flex_direction: FlexDirection::Row,
                    flex_wrap: FlexWrap::Wrap,
                    justify_content: JustifyContent::FlexStart,
                    column_gap: Val::Px(6.0),
                    row_gap: Val::Px(6.0),
                    ..default()
                },
            ));

            chrome
                .spawn((
                    LayerIndicator,
                    Button,
                    Hovered::default(),
                    BlocksWorldInput,
                    Node {
                        width: Val::Px(200.0),
                        height: Val::Px(36.0),
                        align_items: AlignItems::Center,
                        column_gap: Val::Px(6.0),
                        padding: UiRect::horizontal(Val::Px(8.0)),
                        border: UiRect::all(Val::Px(1.5)),
                        ..default()
                    },
                    BackgroundColor(theme.color(Role::Surface)),
                    BorderColor::all(theme.color(Role::Accent)),
                    children![
                        (
                            LayerIndicatorIcon,
                            ImageNode::new(icon),
                            Node {
                                width: Val::Px(ICON_PX as f32),
                                height: Val::Px(ICON_PX as f32),
                                ..default()
                            },
                        ),
                        (
                            LayerIndicatorLabel,
                            Text::new(layer_display_name(&current_layer).to_string()),
                            theme.text_font(theme.metrics.font_lg),
                            TextColor(theme.color(Role::Text)),
                        ),
                    ],
                ))
                .observe(on_indicator_click);
        });
}

/// Open the layer picker on click.
fn on_indicator_click(_activate: On<Activate>, mut picker: ResMut<LayerPickerState>) {
    picker.show = !picker.show;
}

/// Update the indicator's label/icon on layer change and apply the fade.
pub fn update_layer_indicator(
    time: Res<Time>,
    theme: Res<Theme>,
    current_layer: Res<CurrentLayer>,
    mut state: ResMut<LayerIndicatorState>,
    mut images: ResMut<Assets<Image>>,
    mut icon_cache: ResMut<IconCache>,
    mut root: Query<(&Hovered, Has<Pressed>, &mut BackgroundColor), With<LayerIndicator>>,
    mut label: Query<(&mut Text, &mut TextColor), With<LayerIndicatorLabel>>,
    mut icon: Query<&mut ImageNode, With<LayerIndicatorIcon>>,
) {
    state.show_timer.tick(time.delta());

    if current_layer.is_changed() {
        state.show_timer.reset();
        if let Ok((mut text, _)) = label.single_mut() {
            text.0 = layer_display_name(&current_layer).to_string();
        }
        let handle =
            icon_cache.get_or_rasterize(&mut images, layer_icon_path(&current_layer), ICON_PX);
        if let Ok(mut image_node) = icon.single_mut() {
            image_node.image = handle;
        }
    }

    // Fade in over FADE_IN_SECS, then stay fully visible.
    let alpha = (state.show_timer.elapsed_secs() / FADE_IN_SECS).clamp(0.0, 1.0);

    if let Ok((hovered, pressed, mut bg)) = root.single_mut() {
        let surface = theme.color(button_surface(hovered.get(), pressed));
        bg.0 = surface.with_alpha(alpha);
    }
    if let Ok((_, mut color)) = label.single_mut() {
        color.0 = theme.color(Role::Text).with_alpha(alpha);
    }
    if let Ok(mut image_node) = icon.single_mut() {
        image_node.color = Color::WHITE.with_alpha(alpha);
    }
}
