//! Diagnostics for a layer's background services.
//!
//! Opened from the layer toolbar (see
//! [`LayerClientInfo::with_services`](super::ui::layer::LayerClientInfo::with_services)),
//! this lists the services the active layer registered — across the server and
//! every connected agent — with their schedule, run counts, and last error, and
//! lets each one be switched on or off or prodded into running now.
//!
//! Reads come from the client's synced replica; writes go over the service
//! control stream to whichever instance hosts the service. Since `bind_text`
//! closures run every frame, all database reads sit behind ~1s caches.

use crate::gui::ui::Activate;
use crate::gui::ui::bind::bind_text;
use crate::gui::ui::panel::{PanelClosed, spawn_floating_panel};
use crate::gui::ui::theme::{Role, Theme, ThemedBorder, ThemedButton};
use crate::gui::ui::widgets::{row, text};
use crate::service;
use bevy::ecs::world::CommandQueue;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy_ui_widgets::Button;
use chrono::Utc;
use sandpolis_instance::LayerName;
use sandpolis_instance::service::ServiceData;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Marker for the (single) services panel, carrying the layer it's showing.
#[derive(Component)]
pub struct ServicesPanel(pub LayerName);

/// Minimum age before a cached query is re-run.
const REFRESH: Duration = Duration::from_secs(1);

#[derive(Default)]
struct PanelState {
    /// Selected service, as `(instance, key)` — the pair that identifies a row.
    selected: Option<(sandpolis_instance::InstanceId, String)>,
    /// Cached rows for this layer, refreshed as a set.
    cache: Option<(Instant, Vec<ServiceData>)>,
}

type Shared = Arc<Mutex<PanelState>>;

impl PanelState {
    /// This layer's services, re-read at most once per [`REFRESH`].
    fn services(&mut self, layer: &str) -> &[ServiceData] {
        let stale = self
            .cache
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= REFRESH);
        if stale {
            let services = service::query_layer_services(layer).unwrap_or_default();
            self.cache = Some((Instant::now(), services));
        }
        self.cache
            .as_ref()
            .map(|(_, s)| s.as_slice())
            .unwrap_or(&[])
    }

    /// The selected service's current row, if it still exists.
    fn selected(&mut self, layer: &str) -> Option<ServiceData> {
        let selected = self.selected.clone()?;
        self.services(layer)
            .iter()
            .find(|s| (s._instance_id, s.key.clone()) == selected)
            .cloned()
    }
}

/// Whether `layer` has any services, for gating its toolbar button.
///
/// Cached because the toolbar evaluates every button's gate every frame.
pub fn has_services(layer: &LayerName) -> bool {
    static CACHE: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);

    let mut cache = CACHE.lock().unwrap();
    let stale = cache.as_ref().is_none_or(|(at, _)| at.elapsed() >= REFRESH);
    if stale {
        let layers = service::query_services()
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.layer)
            .collect();
        *cache = Some((Instant::now(), layers));
    }
    cache
        .as_ref()
        .is_some_and(|(_, layers)| layers.iter().any(|l| l == &layer.0))
}

/// Open the services panel for `layer`, replacing one showing another layer.
pub fn open(layer: LayerName, commands: &mut Commands) {
    // The toolbar callback only carries `Commands`, so reach the `Theme` and
    // window size through a world command (same pattern as the toolbar itself).
    commands.queue(move |world: &mut World| {
        let existing: Vec<(Entity, LayerName)> = world
            .query::<(Entity, &ServicesPanel)>()
            .iter(world)
            .map(|(e, p)| (e, p.0.clone()))
            .collect();
        for (entity, open_layer) in existing {
            if open_layer == layer {
                return;
            }
            // Switching layers with the panel open: replace it rather than
            // stacking a second one.
            world.entity_mut(entity).despawn();
        }

        let theme = world.resource::<Theme>().clone();
        let (win_w, win_h) = world
            .query::<&Window>()
            .iter(world)
            .next()
            .map(|w| (w.width(), w.height()))
            .unwrap_or((1280.0, 720.0));
        let size = Vec2::new((win_w * 0.8).min(820.0), (win_h * 0.8).min(520.0));
        let pos = Vec2::new((win_w - size.x) / 2.0, (win_h - size.y) / 2.0);

        service::subscribe();

        let mut queue = CommandQueue::default();
        let mut commands = Commands::new(&mut queue, world);
        let panel = spawn_floating_panel(
            &mut commands,
            &theme,
            format!("{} services", layer.0),
            pos,
            size,
        );
        commands
            .entity(panel.root)
            .insert(ServicesPanel(layer.clone()))
            // The panel's close button only triggers `PanelClosed`; the host
            // is responsible for despawning.
            .observe(|closed: On<PanelClosed>, mut commands: Commands| {
                service::unsubscribe();
                commands.entity(closed.entity).despawn();
            });
        build_body(&mut commands, panel.body, &theme, layer);
        queue.apply(world);
    });
}

/// A themed button with no label child, so callers can attach a live
/// [`bind_text`] label instead of the static one baked into `widgets::button`.
fn blank_button(theme: &Theme) -> impl Bundle {
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
    )
}

fn build_body(commands: &mut Commands, body: Entity, theme: &Theme, layer: LayerName) {
    let state: Shared = Arc::new(Mutex::new(PanelState::default()));

    commands.entity(body).with_children(|p| {
        p.spawn(Node {
            flex_direction: FlexDirection::Row,
            column_gap: Val::Px(theme.metrics.space_md),
            flex_grow: 1.0,
            overflow: Overflow::clip(),
            ..default()
        })
        .with_children(|content| {
            // The service list is rebuilt every frame from one bound label
            // rather than as buttons, since services come and go with agents;
            // selection cycles through them from a single button instead.
            content
                .spawn(Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: Val::Px(theme.metrics.space_xs),
                    width: Val::Px(280.0),
                    flex_shrink: 0.0,
                    overflow: Overflow::scroll_y(),
                    ..default()
                })
                .with_children(|list| {
                    let cycle_state = state.clone();
                    let cycle_layer = layer.clone();
                    let label_state = state.clone();
                    let label_layer = layer.clone();
                    list.spawn(row(theme.metrics.space_sm)).with_children(|r| {
                        r.spawn(blank_button(theme))
                            .observe(move |_: On<Activate>| {
                                cycle_selection(&cycle_state, &cycle_layer.0)
                            })
                            .with_child((
                                text(theme, "", theme.metrics.font_sm, Role::Text),
                                bind_text(move || selection_label(&label_state, &label_layer.0)),
                            ));
                    });

                    let list_state = state.clone();
                    let list_layer = layer.clone();
                    list.spawn((
                        text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                        bind_text(move || services_list(&list_state, &list_layer.0)),
                    ));
                });

            content
                .spawn(Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: Val::Px(theme.metrics.space_sm),
                    flex_grow: 1.0,
                    overflow: Overflow::scroll_y(),
                    ..default()
                })
                .with_children(|pane| {
                    pane.spawn(row(theme.metrics.space_sm)).with_children(|r| {
                        let toggle_state = state.clone();
                        let toggle_layer = layer.clone();
                        let toggle_label_state = state.clone();
                        let toggle_label_layer = layer.clone();
                        r.spawn(blank_button(theme))
                            .observe(move |_: On<Activate>| {
                                toggle_selected(&toggle_state, &toggle_layer.0)
                            })
                            .with_child((
                                text(theme, "", theme.metrics.font_sm, Role::Text),
                                bind_text(move || {
                                    toggle_label(&toggle_label_state, &toggle_label_layer.0)
                                }),
                            ));

                        let run_state = state.clone();
                        let run_layer = layer.clone();
                        r.spawn(crate::gui::ui::widgets::button(theme, "Run now"))
                            .observe(move |_: On<Activate>| run_selected(&run_state, &run_layer.0));
                    });

                    let detail_state = state.clone();
                    let detail_layer = layer.clone();
                    pane.spawn((
                        text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                        bind_text(move || detail_text(&detail_state, &detail_layer.0)),
                    ));
                });
        });
    });
}

/// Advance the selection to the next service, wrapping around.
fn cycle_selection(state: &Shared, layer: &str) {
    let mut s = state.lock().unwrap();
    let current = s.selected.clone();
    let keys: Vec<_> = s
        .services(layer)
        .iter()
        .map(|s| (s._instance_id, s.key.clone()))
        .collect();
    if keys.is_empty() {
        s.selected = None;
        return;
    }
    s.selected = match current {
        None => keys.first().cloned(),
        Some(current) => keys
            .iter()
            .position(|k| *k == current)
            .map(|i| keys[(i + 1) % keys.len()].clone())
            .or_else(|| keys.first().cloned()),
    };
}

fn selection_label(state: &Shared, layer: &str) -> String {
    let mut s = state.lock().unwrap();
    let total = s.services(layer).len();
    match s.selected(layer) {
        Some(service) => format!("{} ({} total) — click to cycle", service.name, total),
        None if total == 0 => "No services".into(),
        None => format!("Select a service ({total} total)"),
    }
}

/// The whole list, one line each, with the selected one marked.
fn services_list(state: &Shared, layer: &str) -> String {
    let mut s = state.lock().unwrap();
    let selected = s.selected.clone();
    let services = s.services(layer);
    if services.is_empty() {
        return "Nothing registered this layer.".into();
    }
    services
        .iter()
        .map(|service| {
            let marker = if selected
                .as_ref()
                .is_some_and(|(i, k)| *i == service._instance_id && k == &service.key)
            {
                ">"
            } else {
                " "
            };
            format!("{marker} {} — {}", service.name, state_label(service))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A service's state, with "disabled" taking precedence over whatever the last
/// pass left behind.
fn state_label(service: &ServiceData) -> String {
    if service.enabled {
        service.state.to_string()
    } else {
        "disabled".into()
    }
}

fn toggle_label(state: &Shared, layer: &str) -> String {
    match state.lock().unwrap().selected(layer) {
        Some(service) if service.enabled => "Disable".into(),
        Some(_) => "Enable".into(),
        None => "Enable/disable".into(),
    }
}

fn toggle_selected(state: &Shared, layer: &str) {
    if let Some(service) = state.lock().unwrap().selected(layer) {
        service::set_enabled(service._instance_id, service.key, !service.enabled);
    }
}

fn run_selected(state: &Shared, layer: &str) {
    if let Some(service) = state.lock().unwrap().selected(layer) {
        service::run_now(service._instance_id, service.key);
    }
}

/// Everything known about the selected service.
fn detail_text(state: &Shared, layer: &str) -> String {
    let Some(service) = state.lock().unwrap().selected(layer) else {
        return "Select a service to see its diagnostics.".into();
    };

    let mut out = String::new();
    out.push_str(&format!("{}\n\n", service.description));
    out.push_str(&format!("Host:      {}\n", service._instance_id));
    out.push_str(&format!("Schedule:  {}\n", service.schedule));
    out.push_str(&format!("State:     {}\n", state_label(&service)));
    out.push_str(&format!(
        "Runs:      {} ({} failed)\n",
        service.runs, service.failures
    ));
    out.push_str(&format!("Updated:   {} items\n", service.items_updated));
    if service.last_failed_items > 0 {
        out.push_str(&format!(
            "Last pass: {} items could not be fetched\n",
            service.last_failed_items
        ));
    }
    out.push_str(&format!("Last run:  {}\n", ago(service.last_run)));
    out.push_str(&format!("Last ok:   {}\n", ago(service.last_success)));
    if let Some(error) = &service.last_error {
        out.push_str(&format!("\nLast error: {error}\n"));
    }
    out
}

/// Render a millisecond timestamp as how long ago it was.
fn ago(at: Option<i64>) -> String {
    let Some(at) = at else {
        return "never".into();
    };
    let seconds = (Utc::now().timestamp_millis() - at).max(0) / 1000;
    match seconds {
        s if s < 2 => "just now".into(),
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}
