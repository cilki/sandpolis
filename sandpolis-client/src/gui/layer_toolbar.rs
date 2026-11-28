//! Layer toolbar: a row of icon buttons shown just above the layer indicator. The
//! buttons depend on the active layer and come from each layer's registered
//! [`ToolbarAction`](crate::gui::ui::layer::ToolbarAction)s.
//!
//! The toolbar root is spawned (empty) as a child of the layer-chrome column in
//! [`crate::gui::layer_ui`]; [`rebuild_layer_toolbar`] fills it with buttons
//! reactively whenever the active layer (or the registry) changes.
//!
//! A button may be *gated*: its [`ToolbarEnabledFn`] is evaluated every frame by
//! [`update_toolbar_button_enabled`], dimming the icon and ignoring clicks while
//! it returns `false`.

use crate::gui::input::CurrentLayer;
use crate::gui::ui::layer::{LayerRegistry, ToolbarEnabledFn};
use bevy::ecs::world::CommandQueue;
use bevy::image::Image;
use bevy::prelude::*;
use gesso::ui::Activate;
use gesso::ui::icon::IconCache;
use gesso::ui::theme::Theme;
use gesso::ui::tooltip::Tooltip;
use gesso::ui::widgets::icon_button;

/// Icon rasterization size (px) for toolbar buttons.
const ICON_PX: u32 = 40;

/// Marker for the toolbar row container (filled reactively).
#[derive(Component)]
pub struct LayerToolbarRoot;

/// Marker for a single toolbar button.
#[derive(Component)]
pub struct LayerToolbarButton;

/// A toolbar button's enabled predicate, evaluated each frame.
#[derive(Component)]
pub struct ToolbarButtonGate(pub ToolbarEnabledFn);

/// Rebuild the toolbar buttons when the active layer or registry changes.
pub fn rebuild_layer_toolbar(
    mut commands: Commands,
    theme: Res<Theme>,
    registry: Res<LayerRegistry>,
    current: Res<CurrentLayer>,
    mut images: ResMut<Assets<Image>>,
    mut icon_cache: ResMut<IconCache>,
    root: Query<Entity, With<LayerToolbarRoot>>,
    buttons: Query<Entity, With<LayerToolbarButton>>,
) {
    if !(current.is_changed() || registry.is_changed()) {
        return;
    }
    let Ok(root) = root.single() else {
        return;
    };

    for entity in &buttons {
        commands.entity(entity).despawn();
    }

    let actions = registry.toolbar_actions(&current);
    if actions.is_empty() {
        return;
    }

    commands.entity(root).with_children(|parent| {
        for action in actions {
            let handle = icon_cache.get_or_rasterize(&mut images, action.icon, ICON_PX);
            let on_click = action.on_click.clone();
            let enabled = action.enabled.clone();
            let click_gate = action.enabled.clone();
            parent
                .spawn((
                    LayerToolbarButton,
                    ToolbarButtonGate(enabled),
                    Tooltip::new(action.label),
                    icon_button(&theme, handle),
                ))
                .observe(move |_: On<Activate>, mut commands: Commands| {
                    let on_click = on_click.clone();
                    let click_gate = click_gate.clone();
                    // Evaluate the gate against the world, then run the action's
                    // own command-queueing closure only if enabled.
                    commands.queue(move |world: &mut World| {
                        if !(click_gate)(world) {
                            return;
                        }
                        let mut queue = CommandQueue::default();
                        let mut cmds = Commands::new(&mut queue, world);
                        (on_click)(&mut cmds);
                        queue.apply(world);
                    });
                });
        }
    });
}

/// Evaluate each gated button's predicate and dim its icon while disabled.
pub fn update_toolbar_button_enabled(world: &mut World) {
    let mut q = world.query::<(Entity, &ToolbarButtonGate)>();
    let gates: Vec<(Entity, ToolbarEnabledFn)> =
        q.iter(world).map(|(e, g)| (e, g.0.clone())).collect();
    if gates.is_empty() {
        return;
    }

    let states: Vec<(Entity, bool)> = gates.into_iter().map(|(e, f)| (e, f(world))).collect();

    for (entity, enabled) in states {
        let color = if enabled {
            Color::WHITE
        } else {
            Color::srgba(1.0, 1.0, 1.0, 0.3)
        };
        let children: Vec<Entity> = world
            .get::<Children>(entity)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        for child in children {
            if let Some(mut img) = world.get_mut::<ImageNode>(child)
                && img.color != color
            {
                img.color = color;
            }
        }
    }
}
