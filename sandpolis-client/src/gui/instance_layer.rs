//! Debug "Instance" layer.
//!
//! A diagnostic layer that shows every instance/node regardless of type (no
//! per-type visibility filtering), so duplicate or phantom nodes are all
//! visible at once. Its node panel shows metadata about the node only: the
//! instance id, its type, whether it is the local instance, the cluster
//! id, OS info, and every `ConnectionData` row that references it (the
//! connection ids, sockets, timestamps and byte counters).
//!
//! The layer's toolbar exposes a "View database" action that opens the generic
//! database browser (see [`crate::gui::database_browser`]).

use crate::gui::node_panel::PanelTargetExt;
use crate::gui::queries;
use bevy::prelude::*;
use gesso::ui::bind::bind_text;
use gesso::ui::node_panel::{NodePanel, PanelCtx};
use gesso::ui::theme::Role;
use gesso::ui::widgets::{heading, text};
use sandpolis_instance::network::NetworkManager;
use sandpolis_instance::{InstanceId, InstanceManager};

/// The debug Instance layer's node panel.
///
/// Holds clones of the layers it reads (the [`NodePanel`] build methods do not
/// receive resources), matching how other panels carry their own data handles.
pub struct InstancePanel {
    pub network: NetworkManager,
    pub instance: InstanceManager,
}

impl NodePanel for InstancePanel {
    fn build_summary(&self, ctx: &mut PanelCtx) {
        let Some(instance) = ctx.target.instance() else {
            return;
        };
        let network = self.network.clone();
        let detailed = ctx.verbosity.is_detailed();
        let font = ctx.theme.metrics.font_sm;
        let theme = ctx.theme;

        ctx.children(|p| {
            p.spawn((
                text(theme, "", font, Role::TextMuted),
                bind_text(move || {
                    let connections = count_connections(&network, instance);
                    if detailed {
                        format!("{instance}\n{connections} connection(s)")
                    } else {
                        format!("{connections} connection(s)")
                    }
                }),
            ));
        });
    }

    fn build_detail(&self, ctx: &mut PanelCtx) {
        let Some(instance) = ctx.target.instance() else {
            return;
        };
        let is_local = instance == self.instance.instance_id;
        let cluster = self.instance.cluster_id;
        let instance_type = instance.instance_type();
        let os = queries::query_instance_metadata(instance)
            .ok()
            .map(|m| m.os_type);

        // Captured by the live connection list below.
        let network = self.network.clone();
        let theme = ctx.theme;

        ctx.children(|p| {
            p.spawn(heading(theme, "Identity"));
            p.spawn(text(
                theme,
                format!("ID: {instance}"),
                theme.metrics.font_md,
                Role::Text,
            ));
            p.spawn(text(
                theme,
                format!("Type: {instance_type:?}"),
                theme.metrics.font_md,
                Role::Text,
            ));
            p.spawn(text(
                theme,
                format!("Local: {is_local}"),
                theme.metrics.font_md,
                Role::Text,
            ));
            p.spawn(text(
                theme,
                format!("Cluster: {cluster}"),
                theme.metrics.font_md,
                Role::Text,
            ));
            if let Some(os) = os {
                p.spawn(text(
                    theme,
                    format!("OS: {os}"),
                    theme.metrics.font_md,
                    Role::Text,
                ));
            }

            p.spawn(heading(theme, "Connections"));
            // Live-updating: re-read the connection rows referencing this
            // instance every frame so newly established / torn-down connections
            // are reflected without rebuilding the panel.
            p.spawn((
                text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                bind_text(move || describe_connections(&network, instance)),
            ));
        });
    }
}

/// How many `ConnectionData` rows reference `instance` as either endpoint.
fn count_connections(network: &NetworkManager, instance: InstanceId) -> usize {
    network
        .connections
        .iter()
        .filter(|connection| {
            let cd = connection.read();
            cd._instance_id == instance || cd.remote_instance == Some(instance)
        })
        .count()
}

/// Summarize every `ConnectionData` row referencing `instance` (as either
/// endpoint) for the panel's live connection list.
fn describe_connections(network: &NetworkManager, instance: InstanceId) -> String {
    let mut lines = Vec::new();
    for connection in network.connections.iter() {
        let cd = connection.read();
        if cd._instance_id == instance || cd.remote_instance == Some(instance) {
            lines.push(format!(
                "{} -> {:?}\n  local={:?} remote={:?}\n  established={} disconnected={:?}\n  r/w={}/{}B",
                cd._instance_id,
                cd.remote_instance,
                cd.local_socket,
                cd.remote_socket,
                cd.established,
                cd.disconnected,
                cd.read_bytes,
                cd.write_bytes,
            ));
        }
    }

    if lines.is_empty() {
        "No connection rows reference this instance.".to_string()
    } else {
        format!("{} connection row(s):\n{}", lines.len(), lines.join("\n"))
    }
}

/// Toolbar callback for the Instance layer's "View database" action.
pub fn open_database_browser(commands: &mut Commands) {
    crate::gui::database_browser::open(commands);
}
