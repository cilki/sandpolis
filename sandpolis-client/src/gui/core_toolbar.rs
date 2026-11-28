//! Core layer registration.
//!
//! The Network, Server and Client layers have no layer crate of their own, so
//! they're registered here. Server gets a toolbar button that opens the existing
//! login dialog; Network is the default layer and the only one showing every
//! kind of node at once. All three need a registry entry so the layer picker,
//! the node visibility filter and the node panel host see them.
//!
//! The Agent layer is registered by `sandpolis_agent::client::gui::AgentClientPlugin`,
//! which owns the deploy dialog and the stream behind it — neither of which this
//! crate can reach, since `sandpolis-agent` depends on it rather than the other
//! way around.

use crate::gui::input::LoginDialogState;
use crate::gui::node_panel::PanelTargetExt;
use crate::gui::ui::layer::{LayerClientInfo, RegisterLayerClient};
use bevy::prelude::*;
use gesso::ui::bind::bind_text;
use gesso::ui::node_panel::{NodePanel, PanelCtx};
use gesso::ui::theme::Role;
use gesso::ui::widgets::{heading, text};
use sandpolis_instance::network::NetworkManager;
use sandpolis_instance::{InstanceId, InstanceType};

/// The Network layer's node panel: how this client reaches the node.
pub struct NetworkPanel {
    pub network: NetworkManager,
}

impl NodePanel for NetworkPanel {
    fn build_summary(&self, ctx: &mut PanelCtx) {
        let Some(instance) = ctx.target.instance() else {
            return;
        };
        // A probe borrows its gateway server's id, so the link described here is
        // the route to the device rather than the device's own connection.
        let via_gateway = ctx.target.sub.is_some();
        let network = self.network.clone();
        let theme = ctx.theme;

        ctx.children(|p| {
            p.spawn((
                text(theme, "", theme.metrics.font_sm, Role::TextMuted),
                bind_text(move || describe_route(&network, instance, via_gateway)),
            ));
        });
    }

    fn build_detail(&self, ctx: &mut PanelCtx) {
        let Some(instance) = ctx.target.instance() else {
            return;
        };
        let via_gateway = ctx.target.sub.is_some();
        let network = self.network.clone();
        let theme = ctx.theme;

        ctx.children(|p| {
            p.spawn(heading(theme, "Connectivity"));
            p.spawn((
                text(theme, "", theme.metrics.font_md, Role::Text),
                bind_text(move || describe_route(&network, instance, via_gateway)),
            ));
            p.spawn(text(
                theme,
                if via_gateway {
                    format!("Gateway: {instance}")
                } else {
                    format!("Instance: {instance}")
                },
                theme.metrics.font_sm,
                Role::TextMuted,
            ));
        });
    }
}

/// [`describe_link`], labelled for whichever kind of node is being shown.
fn describe_route(network: &NetworkManager, instance: InstanceId, via_gateway: bool) -> String {
    let link = describe_link(network, instance);
    if via_gateway {
        format!("Via gateway — {link}")
    } else {
        link
    }
}

/// One line on how (and how much) this client is talking to `instance`.
fn describe_link(network: &NetworkManager, instance: InstanceId) -> String {
    let mut connections = 0usize;
    let mut read = 0u64;
    let mut written = 0u64;
    for connection in network.connections.iter() {
        let data = connection.read();
        if data._instance_id == instance || data.remote_instance == Some(instance) {
            connections += 1;
            read += data.read_bytes;
            written += data.write_bytes;
        }
    }
    if connections == 0 {
        return "Not connected".to_string();
    }
    format!(
        "{} connection{} — {} read / {} written",
        connections,
        if connections == 1 { "" } else { "s" },
        format_bytes(read),
        format_bytes(written),
    )
}

/// Format a byte count for a one-line summary.
fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1e9;
    const MB: f64 = 1e6;
    const KB: f64 = 1e3;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Registers the Network and Server layers' clients.
pub struct CoreLayerToolbarPlugin {
    pub network: NetworkManager,
}

impl Plugin for CoreLayerToolbarPlugin {
    fn build(&self, app: &mut App) {
        app.register_layer_client(
            LayerClientInfo::new("Network", "How instances reach each other")
                .with_panel(NetworkPanel {
                    network: self.network.clone(),
                })
                // The topology layer: everything on the canvas at once, which
                // makes it the one place a probe is drawn beside the gateway
                // server that reaches it.
                .with_visible_instance_types(&[
                    InstanceType::Server,
                    InstanceType::Agent,
                    InstanceType::Client,
                ])
                .showing_probe_nodes(),
        );
        app.register_layer_client(
            LayerClientInfo::new("Client", "Connected client applications")
                .with_visible_instance_types(&[InstanceType::Client]),
        );
        app.register_layer_client(
            LayerClientInfo::new("Server", "Server instances in the cluster")
                .with_visible_instance_types(&[InstanceType::Server])
                .with_toolbar_action("Login to server", "toolbar/login.svg", |commands| {
                    commands.queue(|world: &mut World| {
                        if let Some(mut state) = world.get_resource_mut::<LoginDialogState>() {
                            state.show = true;
                        }
                    });
                }),
        );
    }
}
