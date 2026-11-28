//! What the client knows about each layer.
//!
//! Layers register themselves with [`RegisterLayerClient::register_layer_client`]
//! from their `LayerClientPlugin`, replacing the compile-time `inventory`
//! collection. The [`LayerRegistry`] resource then drives the node panel host,
//! the layer picker, the layer toolbar, and per-layer node visibility.

use bevy::prelude::*;
use gesso::ui::node_panel::NodePanel;
use sandpolis_instance::{InstanceId, InstanceType, LayerName};
use std::sync::Arc;

/// The order layers appear in the picker, broadest first: the topology layer,
/// then the instance-type layers, then the ones that drive a particular
/// subsystem.
pub const LAYER_ORDER: &[&str] = &[
    "Network",
    "Server",
    "Client",
    "Agent",
    "Probe",
    "Account",
    "Filesystem",
    "Instance",
    "Desktop",
    "Health",
    "Shell",
    "Inventory",
    "Snapshot",
    "Tunnel",
];

/// A layer's position in [`LAYER_ORDER`]. Layers missing from it sort last, in
/// registration order.
pub fn layer_rank(layer: &LayerName) -> usize {
    LAYER_ORDER
        .iter()
        .position(|name| *name == layer.name())
        .unwrap_or(LAYER_ORDER.len())
}

/// Callback run when a layer toolbar button is clicked. It receives `Commands`
/// so it can queue work (e.g. open a dialog) without the registry needing to know
/// any layer-specific resource types.
pub type ToolbarCallback = Arc<dyn Fn(&mut Commands) + Send + Sync>;

/// Predicate deciding whether a toolbar button is currently enabled. It reads the
/// `World` (e.g. a layer-specific selection resource) so the registry needs no
/// knowledge of layer-specific types. Evaluated every frame.
pub type ToolbarEnabledFn = Arc<dyn Fn(&World) -> bool + Send + Sync>;

/// Picks the node icon for an instance while a layer is active. Returns a path
/// under the SVG asset root (e.g. `"shell/terminal.svg"`).
pub type NodeIconFn = Arc<dyn Fn(InstanceId) -> &'static str + Send + Sync>;

/// Tints a node's sprite for an instance while a layer is active — the layer's
/// chance to say something about the node's state at a glance (disk usage,
/// memory pressure) without owning a system of its own.
pub type NodeTintFn = Arc<dyn Fn(InstanceId) -> Color + Send + Sync>;

/// A single button shown in the layer toolbar while a layer is active.
#[derive(Clone)]
pub struct ToolbarAction {
    /// Full-text label, shown as the button's hover tooltip.
    pub label: &'static str,
    /// SVG icon path under the icon asset root (e.g. `"toolbar/login.svg"`).
    pub icon: &'static str,
    /// Invoked when the button is clicked.
    pub on_click: ToolbarCallback,
    /// Whether the button is currently enabled. Disabled buttons are dimmed and
    /// ignore clicks. Defaults to always-enabled.
    pub enabled: ToolbarEnabledFn,
}

/// Everything the client needs to know about a layer, registered by its
/// `LayerClientPlugin`.
#[derive(Clone)]
pub struct LayerClientInfo {
    /// The layer this describes.
    pub layer: LayerName,
    /// One-line description (shown in the layer picker).
    pub description: &'static str,
    /// Which instance types are visible while this layer is active.
    pub visible_instance_types: &'static [InstanceType],
    /// Whether probe nodes are shown while this layer is active.
    pub show_probe_nodes: bool,
    /// Which probe protocols are shown while this layer is active, named by
    /// `ProbeType::display_name()` (e.g. `"SSH"`, `"VNC"`). Empty means every
    /// protocol. Only meaningful when [`show_probe_nodes`](Self::show_probe_nodes)
    /// is set. Kept as strings so this crate needn't depend on the probe subsystem.
    pub probe_protocols: &'static [&'static str],
    /// The layer's node panel, if it has one. Layers without one still get a
    /// panel per node; it just shows the node's identity and nothing else.
    pub panel: Option<Arc<dyn NodePanel>>,
    /// Buttons shown in the layer toolbar while this layer is active.
    pub toolbar_actions: Vec<ToolbarAction>,
    /// Node icon override. `None` leaves nodes on their OS icon.
    pub node_icon: Option<NodeIconFn>,
    /// Node sprite tint. `None` leaves nodes untinted.
    pub node_tint: Option<NodeTintFn>,
}

impl LayerClientInfo {
    /// Create an info for `layer` with sensible defaults (servers + agents visible,
    /// no probes, no panel).
    pub fn new(layer: impl Into<LayerName>, description: &'static str) -> Self {
        Self {
            layer: layer.into(),
            description,
            visible_instance_types: &[InstanceType::Server, InstanceType::Agent],
            show_probe_nodes: false,
            probe_protocols: &[],
            panel: None,
            toolbar_actions: Vec::new(),
            node_icon: None,
            node_tint: None,
        }
    }

    /// Choose the node icon shown while this layer is active.
    pub fn with_node_icon(
        mut self,
        icon: impl Fn(InstanceId) -> &'static str + Send + Sync + 'static,
    ) -> Self {
        self.node_icon = Some(Arc::new(icon));
        self
    }

    /// Tint node sprites while this layer is active.
    pub fn with_node_tint(
        mut self,
        tint: impl Fn(InstanceId) -> Color + Send + Sync + 'static,
    ) -> Self {
        self.node_tint = Some(Arc::new(tint));
        self
    }

    /// Attach a node panel.
    pub fn with_panel(mut self, panel: impl NodePanel) -> Self {
        self.panel = Some(Arc::new(panel));
        self
    }

    /// Add a button to this layer's toolbar.
    pub fn with_toolbar_action(
        mut self,
        label: &'static str,
        icon: &'static str,
        on_click: impl Fn(&mut Commands) + Send + Sync + 'static,
    ) -> Self {
        self.toolbar_actions.push(ToolbarAction {
            label,
            icon,
            on_click: Arc::new(on_click),
            enabled: Arc::new(|_| true),
        });
        self
    }

    /// Add a toolbar button whose enabled state is decided each frame by
    /// `enabled` (e.g. a button active only while something is selected).
    pub fn with_toolbar_action_gated(
        mut self,
        label: &'static str,
        icon: &'static str,
        on_click: impl Fn(&mut Commands) + Send + Sync + 'static,
        enabled: impl Fn(&World) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.toolbar_actions.push(ToolbarAction {
            label,
            icon,
            on_click: Arc::new(on_click),
            enabled: Arc::new(enabled),
        });
        self
    }

    /// Add a "Services" button opening the shared services panel for this layer.
    ///
    /// Gated on the layer actually having services, so it dims until some
    /// instance reports one — a layer whose services are all switched off in
    /// config never lights it up at all.
    pub fn with_services(mut self) -> Self {
        let open_layer = self.layer.clone();
        let gate_layer = self.layer.clone();
        self.toolbar_actions.push(ToolbarAction {
            label: "Services",
            icon: "toolbar/services.svg",
            on_click: Arc::new(move |commands: &mut Commands| {
                crate::gui::services_panel::open(open_layer.clone(), commands)
            }),
            enabled: Arc::new(move |_| crate::gui::services_panel::has_services(&gate_layer)),
        });
        self
    }

    /// Override which instance types are visible.
    pub fn with_visible_instance_types(mut self, types: &'static [InstanceType]) -> Self {
        self.visible_instance_types = types;
        self
    }

    /// Show every probe node while this layer is active. Probe lifecycle
    /// (registration, deletion) stays with the Probe layer's toolbar; this only
    /// decides what's on screen.
    pub fn showing_probe_nodes(mut self) -> Self {
        self.show_probe_nodes = true;
        self
    }

    /// Show only the probe nodes exposing one of `protocols`, named by
    /// `ProbeType::display_name()`.
    pub fn showing_probe_nodes_for(mut self, protocols: &'static [&'static str]) -> Self {
        self.show_probe_nodes = true;
        self.probe_protocols = protocols;
        self
    }
}

/// Registry of all layers' client info. Populated at app build time by each
/// `LayerClientPlugin` via [`RegisterLayerClient`], and kept in [`LAYER_ORDER`]
/// so consumers (the picker above all) needn't sort it themselves.
#[derive(Resource, Default)]
pub struct LayerRegistry {
    layers: Vec<LayerClientInfo>,
}

impl LayerRegistry {
    /// Look up a layer's info.
    pub fn get(&self, layer: &LayerName) -> Option<&LayerClientInfo> {
        self.layers.iter().find(|info| &info.layer == layer)
    }

    /// Iterate over all registered layers, in [`LAYER_ORDER`].
    pub fn iter(&self) -> impl Iterator<Item = &LayerClientInfo> {
        self.layers.iter()
    }

    /// Whether the given layer shows probe nodes.
    pub fn show_probe_nodes(&self, layer: &LayerName) -> bool {
        self.get(layer).map(|i| i.show_probe_nodes).unwrap_or(false)
    }

    /// Which probe protocols the given layer shows. Empty means every protocol
    /// (and also covers layers that show no probes at all).
    pub fn probe_protocols(&self, layer: &LayerName) -> &'static [&'static str] {
        self.get(layer).map(|i| i.probe_protocols).unwrap_or(&[])
    }

    /// The toolbar actions for the given layer (empty when unregistered).
    pub fn toolbar_actions(&self, layer: &LayerName) -> &[ToolbarAction] {
        self.get(layer)
            .map(|i| i.toolbar_actions.as_slice())
            .unwrap_or(&[])
    }
}

/// App extension for registering a layer's client info from its plugin.
pub trait RegisterLayerClient {
    /// Register a layer's [`LayerClientInfo`]. Idempotent w.r.t. the registry
    /// resource (it is created on first use).
    fn register_layer_client(&mut self, info: LayerClientInfo) -> &mut Self;
}

impl RegisterLayerClient for App {
    fn register_layer_client(&mut self, info: LayerClientInfo) -> &mut Self {
        self.init_resource::<LayerRegistry>();
        let mut registry = self.world_mut().resource_mut::<LayerRegistry>();
        // Insert in [`LAYER_ORDER`] rather than pushing: plugins register in
        // whatever order the app assembles them, and which layers register at
        // all depends on the build's features.
        let rank = layer_rank(&info.layer);
        let index = registry
            .layers
            .iter()
            .position(|existing| layer_rank(&existing.layer) > rank)
            .unwrap_or(registry.layers.len());
        registry.layers.insert(index, info);
        self
    }
}
