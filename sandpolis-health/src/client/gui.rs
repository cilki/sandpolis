//! GUI components for the Health layer.
//!
//! Surfaces systemd unit status through a node panel and client plugin.

use super::{SystemdUnitInfo, query_systemd_units};
use crate::systemd::ActiveState;
use bevy::prelude::*;
use gesso::ui::bind::bind_text;
use gesso::ui::node_panel::{NodePanel, PanelCtx};
use gesso::ui::table::{TableData, TableRow, bind_table, table};
use gesso::ui::theme::Role;
use gesso::ui::widgets::{heading, text};
use sandpolis_client::gui::node_panel::PanelTargetExt;
use sandpolis_client::gui::ui::layer::{LayerClientInfo, RegisterLayerClient};
use sandpolis_instance::{InstanceType, LayerName};

/// The health layer's node panel (service status).
pub struct HealthPanel;

impl NodePanel for HealthPanel {
    fn build_summary(&self, ctx: &mut PanelCtx) {
        // A sub-node here is a probe device, whose services the probe
        // subsystem reaches for us.
        #[cfg(feature = "probe")]
        if let Some(device_id) = ctx.target.sub {
            probe::build_summary(ctx, device_id);
            return;
        }

        let Some(instance) = ctx.target.instance() else {
            return;
        };
        super::subscribe(instance);

        let detailed = ctx.verbosity.is_detailed();
        let theme = ctx.theme;

        ctx.children(|p| {
            p.spawn((
                text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                bind_text(move || {
                    let units = query_systemd_units(instance).unwrap_or_default();
                    if units.is_empty() {
                        return "No unit data".into();
                    }
                    let failed = count(&units, ActiveState::Failed);
                    let summary = format!("{} units — {} failed", units.len(), failed);
                    if !detailed || failed == 0 {
                        return summary;
                    }
                    // Zoomed right in there's room to name what's actually
                    // broken, which is the only part anyone acts on.
                    let names: Vec<&str> = units
                        .iter()
                        .filter(|unit| unit.active_state == ActiveState::Failed)
                        .map(|unit| unit.name.as_str())
                        .take(3)
                        .collect();
                    format!("{}\n{}", summary, names.join(", "))
                }),
            ));
        });
    }

    fn build_detail(&self, ctx: &mut PanelCtx) {
        #[cfg(feature = "probe")]
        if let Some(device_id) = ctx.target.sub {
            probe::build_detail(ctx, device_id);
            return;
        }

        let Some(instance) = ctx.target.instance() else {
            return;
        };
        // Subscribe to live systemd updates for this instance.
        super::subscribe(instance);

        let theme = ctx.theme;
        ctx.children(|p| {
            p.spawn(heading(theme, "systemd"));
            p.spawn((
                text(theme, "", theme.metrics.font_md, Role::Text),
                bind_text(move || {
                    let units = query_systemd_units(instance).unwrap_or_default();
                    if units.is_empty() {
                        return "No unit data".into();
                    }
                    format!(
                        "{} units — {} active, {} failed",
                        units.len(),
                        count(&units, ActiveState::Active),
                        count(&units, ActiveState::Failed),
                    )
                }),
            ));

            // Every unit, failed ones first (most actionable) and tinted so
            // they read as broken without parsing the state column.
            p.spawn(heading(theme, "Units"));
            p.spawn((
                table(theme, None),
                bind_table(move || {
                    let mut units = query_systemd_units(instance).unwrap_or_default();
                    units.sort_by(|a, b| {
                        (a.active_state != ActiveState::Failed, &a.name)
                            .cmp(&(b.active_state != ActiveState::Failed, &b.name))
                    });
                    let mut data = TableData::new(["Unit", "Active", "Sub", "Description"])
                        .with_placeholder("No unit data");
                    for unit in units {
                        let row = TableRow::new([
                            unit.name.clone(),
                            unit.active_state.to_string(),
                            unit.sub_state.clone().unwrap_or_default(),
                            unit.description.clone().unwrap_or_default(),
                        ]);
                        data.push_row(if unit.active_state == ActiveState::Failed {
                            row.with_role(Role::Error)
                        } else {
                            row
                        });
                    }
                    data
                }),
            ));

            p.spawn(text(
                theme,
                format!("Instance: {instance}"),
                theme.metrics.font_sm,
                Role::TextMuted,
            ));
        });
    }
}

/// How many of `units` are in the given state.
fn count(units: &[SystemdUnitInfo], state: ActiveState) -> usize {
    units
        .iter()
        .filter(|unit| unit.active_state == state)
        .count()
}

/// Controlling probe devices (Docker, libvirt).
///
/// Everything protocol-specific lives behind [`sandpolis_probe::service`]: this
/// module asks for a device's service instances (containers, virtual machines)
/// and renders whatever comes back with start/stop/restart controls.
#[cfg(feature = "probe")]
mod probe {
    use super::*;
    use gesso::ui::Activate;
    use gesso::ui::theme::Theme;
    use gesso::ui::widgets::{button, muted, row};
    use sandpolis_probe::ProbeType;
    use sandpolis_probe::service::{ServiceState, client as probe_service};
    use std::hash::{DefaultHasher, Hash, Hasher};

    /// The service protocols a device exposes.
    fn protocols(device_id: u64) -> Vec<ProbeType> {
        sandpolis_probe::REGISTERED_DEVICES
            .read()
            .ok()
            .and_then(|devices| {
                devices
                    .iter()
                    .find(|d| d.id.body() == device_id)
                    .map(|d| d.device.service_protocols())
            })
            .unwrap_or_default()
    }

    /// One line describing what runs on the device.
    fn summary(device_id: u64) -> String {
        let Some(view) = probe_service::view(device_id) else {
            return "Loading…".into();
        };
        if let Some(error) = &view.error {
            return error.clone();
        }
        let mut parts = Vec::new();
        if let Some(containers) = &view.containers {
            let running = containers
                .iter()
                .filter(|c| c.state == ServiceState::Running)
                .count();
            parts.push(format!(
                "{} containers ({} running)",
                containers.len(),
                running
            ));
        }
        if let Some(domains) = &view.domains {
            let running = domains
                .iter()
                .filter(|d| d.state == ServiceState::Running)
                .count();
            parts.push(format!("{} VMs ({} running)", domains.len(), running));
        }
        if parts.is_empty() {
            return if view.busy {
                "Loading…".into()
            } else {
                "No service data".into()
            };
        }
        parts.join(", ")
    }

    pub(super) fn build_summary(ctx: &mut PanelCtx, device_id: u64) {
        let theme = ctx.theme;
        ctx.children(|p| {
            p.spawn((
                text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                bind_text(move || summary(device_id)),
            ));
        });
    }

    pub(super) fn build_detail(ctx: &mut PanelCtx, device_id: u64) {
        let theme = ctx.theme;
        let protocols = protocols(device_id);
        if protocols.is_empty() {
            ctx.children(|p| {
                p.spawn(muted(
                    theme,
                    "This device exposes no service protocol.",
                    theme.metrics.font_md,
                ));
            });
            return;
        }

        ctx.children(|p| {
            p.spawn(row(theme.metrics.space_sm)).with_children(|bar| {
                bar.spawn((
                    text(theme, "", theme.metrics.font_md, Role::Text),
                    bind_text(move || summary(device_id)),
                ));
                let refresh = protocols.clone();
                bar.spawn(button(theme, "Refresh"))
                    .observe(move |_: On<Activate>| {
                        for protocol in &refresh {
                            probe_service::list(device_id, *protocol);
                        }
                    });
            });

            for protocol in &protocols {
                p.spawn(heading(
                    theme,
                    match protocol {
                        ProbeType::Docker => "Containers",
                        _ => "Virtual Machines",
                    },
                ));
                // Opening the panel is the request to see the services; the
                // listing loads itself (see `start_pending_service_queries`)
                // and this list drives its own children because rows carry
                // action buttons, which the frame-driven table widget can't.
                p.spawn((
                    ServiceList {
                        device_id,
                        protocol: *protocol,
                        rendered: 0,
                    },
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(theme.metrics.space_sm),
                        ..default()
                    },
                ));
            }
        });
    }

    /// An open service listing for a probe device, rebuilt when the underlying
    /// view changes.
    #[derive(Component)]
    pub(super) struct ServiceList {
        device_id: u64,
        protocol: ProbeType,
        /// Hash of the content currently spawned, so unchanged data isn't
        /// rebuilt.
        rendered: u64,
    }

    /// Devices already asked to list, so the auto-query below doesn't refire
    /// every frame.
    #[derive(Resource, Default)]
    pub(super) struct QueriedServices(std::collections::HashSet<u64>);

    /// List the services of any panel that hasn't loaded yet.
    ///
    /// Checked every frame rather than on `Added` so a listing that couldn't
    /// start (no connection) is retried until it can.
    pub(super) fn start_pending_service_queries(
        mut queried: ResMut<QueriedServices>,
        lists: Query<&ServiceList>,
        mut open: Local<std::collections::HashSet<u64>>,
    ) {
        open.clear();
        for list in &lists {
            open.insert(list.device_id);
            if queried.0.contains(&list.device_id) {
                continue;
            }
            if probe_service::connection_for(list.device_id).is_none() {
                continue;
            }
            for protocol in protocols(list.device_id) {
                probe_service::list(list.device_id, protocol);
            }
            queried.0.insert(list.device_id);
        }
        // Forget closed panels, so reopening one re-lists — which is how a
        // daemon that was unreachable gets retried.
        queried.0.retain(|device_id| open.contains(device_id));
    }

    /// Rebuild service rows whenever a device's view changes.
    pub(super) fn drive_service_lists(
        mut commands: Commands,
        theme: Res<Theme>,
        mut lists: Query<(Entity, &mut ServiceList)>,
    ) {
        for (entity, mut list) in lists.iter_mut() {
            let device_id = list.device_id;
            let protocol = list.protocol;
            let view = probe_service::view(device_id).unwrap_or_default();

            let mut hasher = DefaultHasher::new();
            view.busy.hash(&mut hasher);
            view.error.hash(&mut hasher);
            match protocol {
                ProbeType::Docker => {
                    for c in view.containers.iter().flatten() {
                        (&c.id, &c.name, &c.image, c.state, &c.status).hash(&mut hasher);
                    }
                }
                _ => {
                    for d in view.domains.iter().flatten() {
                        (&d.name, &d.uuid, d.state, &d.status).hash(&mut hasher);
                    }
                }
            }
            let hash = hasher.finish();
            if hash == list.rendered {
                continue;
            }
            list.rendered = hash;

            commands.entity(entity).despawn_related::<Children>();
            commands.entity(entity).with_children(|p| {
                if let Some(error) = &view.error {
                    p.spawn(text(
                        &theme,
                        error.clone(),
                        theme.metrics.font_sm,
                        Role::Error,
                    ));
                }
                match protocol {
                    ProbeType::Docker => {
                        let Some(containers) = &view.containers else {
                            p.spawn(muted(&theme, "No container data", theme.metrics.font_md));
                            return;
                        };
                        if containers.is_empty() {
                            p.spawn(muted(&theme, "No containers", theme.metrics.font_md));
                        }
                        for container in containers {
                            spawn_service_row(
                                p,
                                &theme,
                                ServiceRow {
                                    device_id,
                                    protocol,
                                    id: container.id.clone(),
                                    label: format!("{} — {}", container.name, container.image),
                                    state: container.state,
                                    status: container.status.clone(),
                                    busy: view.busy,
                                },
                            );
                        }
                    }
                    _ => {
                        let Some(domains) = &view.domains else {
                            p.spawn(muted(&theme, "No domain data", theme.metrics.font_md));
                            return;
                        };
                        if domains.is_empty() {
                            p.spawn(muted(&theme, "No virtual machines", theme.metrics.font_md));
                        }
                        for domain in domains {
                            spawn_service_row(
                                p,
                                &theme,
                                ServiceRow {
                                    device_id,
                                    protocol,
                                    id: domain.name.clone(),
                                    label: domain.name.clone(),
                                    state: domain.state,
                                    status: domain.status.clone(),
                                    busy: view.busy,
                                },
                            );
                        }
                    }
                }
            });
        }
    }

    /// Everything one rendered row needs to know.
    struct ServiceRow {
        device_id: u64,
        protocol: ProbeType,
        /// What actions address: container id or domain name.
        id: String,
        label: String,
        state: ServiceState,
        status: Option<String>,
        busy: bool,
    }

    /// One container/VM with its state and the actions that make sense for it.
    fn spawn_service_row(parent: &mut ChildSpawnerCommands, theme: &Theme, service: ServiceRow) {
        let ServiceRow {
            device_id,
            protocol,
            id,
            label,
            state,
            status,
            busy,
        } = service;
        let state_text = status.unwrap_or_else(|| state.label().into());
        parent
            .spawn(row(theme.metrics.space_sm))
            .with_children(|r| {
                r.spawn(text(theme, label, theme.metrics.font_md, Role::Text));
                r.spawn(text(
                    theme,
                    state_text,
                    theme.metrics.font_sm,
                    match state {
                        ServiceState::Running => Role::Text,
                        _ => Role::TextMuted,
                    },
                ));
                // Actions stack while a request is in flight, so hold them
                // back until the view answers.
                if busy {
                    return;
                }
                match state {
                    ServiceState::Running | ServiceState::Paused => {
                        {
                            let id = id.clone();
                            r.spawn(button(theme, "Stop"))
                                .observe(move |_: On<Activate>| {
                                    probe_service::stop(device_id, protocol, id.clone(), false);
                                });
                        }
                        if protocol == ProbeType::Libvirt {
                            let id = id.clone();
                            r.spawn(button(theme, "Force Off"))
                                .observe(move |_: On<Activate>| {
                                    probe_service::stop(device_id, protocol, id.clone(), true);
                                });
                        }
                        r.spawn(button(theme, "Restart"))
                            .observe(move |_: On<Activate>| {
                                probe_service::restart(device_id, protocol, id.clone());
                            });
                    }
                    ServiceState::Stopped => {
                        r.spawn(button(theme, "Start"))
                            .observe(move |_: On<Activate>| {
                                probe_service::start(device_id, protocol, id.clone());
                            });
                    }
                    ServiceState::Other => {}
                }
            });
    }
}

/// The health layer's client plugin.
pub struct HealthClientPlugin;

impl Plugin for HealthClientPlugin {
    fn build(&self, app: &mut App) {
        #[cfg(feature = "probe")]
        {
            app.init_resource::<probe::QueriedServices>();
            app.add_systems(
                Update,
                (
                    probe::start_pending_service_queries,
                    probe::drive_service_lists,
                ),
            );
        }

        let info = LayerClientInfo::new(LayerName::from("Health"), "Service and host health")
            .with_panel(HealthPanel)
            .with_visible_instance_types(&[InstanceType::Agent])
            .with_services();

        // Docker and libvirt probes are controllable here just like agents.
        // Devices that expose nothing this layer can drive stay hidden.
        #[cfg(feature = "probe")]
        let info = info.showing_probe_nodes_for(&["Docker", "libvirt"]);

        app.register_layer_client(info);
    }
}
