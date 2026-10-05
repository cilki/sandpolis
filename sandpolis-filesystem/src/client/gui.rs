//! GUI components for the Filesystem layer.
//!
//! Provides the file-browser node panel and the layer's client plugin.
//!
//! An agent and a probe device are reached in completely different ways — the
//! agent over a live [`FsSessionStream`](crate::session), the device through
//! `sandpolis_probe::filesystem` — but both answer with the same two things: a
//! directory listing and space totals. [`Browse`] is that shape, so the panel
//! bodies and the session bookkeeping below are written once over it rather
//! than once per kind of target. Local file picking (via `rfd`) and transfers
//! are still deferred.

use crate::session::FsUsage;
use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use sandpolis_client::gui::layer_visuals::utilization_tint;
use sandpolis_client::gui::ui::Activate;
use sandpolis_client::gui::ui::bind::bind_text;
use sandpolis_client::gui::ui::gauge::{GaugeValue, bind_gauge, gauge};
use sandpolis_client::gui::ui::layer::{LayerClientInfo, RegisterLayerClient};
use sandpolis_client::gui::ui::node_panel::{NodePanel, PanelCtx};
use sandpolis_client::gui::ui::theme::{Role, Theme};
#[cfg(feature = "probe")]
use sandpolis_client::gui::ui::widgets::muted;
use sandpolis_client::gui::ui::widgets::{button, heading, row, text};
use sandpolis_instance::{InstanceId, InstanceType, LayerName};
use std::collections::HashSet;
use std::path::PathBuf;

/// Width a gauge is given inside a collapsed panel, whose own width is only
/// whatever its content asks for — a percentage-width track would collapse to
/// nothing there.
const SUMMARY_GAUGE_WIDTH: f32 = 160.0;

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// One entry of a browsed directory, as the listing below draws it.
struct BrowseEntry {
    name: String,
    is_dir: bool,
    size: u64,
}

/// The last thing a browsed target's filesystem told us.
struct BrowseView {
    /// The directory most recently listed.
    cwd: PathBuf,
    /// Its entries, or `None` when nothing has been listed yet.
    entries: Option<Vec<BrowseEntry>>,
    usage: Option<FsUsage>,
    /// Set while a request is outstanding, cleared when one answers.
    busy: bool,
    /// Why the last request failed, if it did.
    error: Option<String>,
}

/// Something this layer's panel can browse.
///
/// A target is a key — an agent's id, or a device id and the protocol to reach
/// it by — rather than a handle, because the bound labels and gauges capture it
/// in projections that outlive the panel they were built for, and the session
/// bookkeeping keys on it.
trait Browse: Copy + Eq + std::hash::Hash + Send + Sync + 'static {
    /// Whatever this target's filesystem last told us.
    fn view(self) -> Option<BrowseView>;

    /// Ask for a listing of `path`, plus the space totals that go with it.
    fn browse(self, path: PathBuf);

    /// Start showing this target's files, reporting whether it got going: a
    /// target that isn't reachable yet is retried on the next frame.
    fn start(self) -> bool;

    /// Release whatever [`start`](Self::start) opened, once nothing shows this
    /// target any more.
    fn close(self) {}
}

/// Render a target's space totals as the shared disk gauge value.
fn usage_gauge<B: Browse>(target: B) -> GaugeValue {
    let Some(usage) = target
        .view()
        .and_then(|view| view.usage)
        .filter(|usage| usage.total > 0)
    else {
        return GaugeValue::new(0.0, "No filesystem data");
    };
    GaugeValue::ratio(
        usage.used,
        usage.total,
        format!(
            "{:.1} GB / {:.1} GB",
            usage.used as f64 / 1e9,
            usage.total as f64 / 1e9
        ),
    )
}

/// Render the current directory as one label. A reusable scrolling table is
/// still on the client subsystem's list; until it lands, one bound label is what
/// keeps this honest about live data rather than faking a widget.
fn listing<B: Browse>(target: B) -> String {
    let Some(view) = target.view() else {
        return "Loading…".to_string();
    };
    if let Some(error) = view.error {
        return error;
    }
    let Some(entries) = view.entries else {
        return if view.busy {
            "Loading…".to_string()
        } else {
            "Not listed yet".to_string()
        };
    };
    if entries.is_empty() {
        return "(Empty directory)".to_string();
    }
    entries
        .iter()
        .map(|entry| {
            if entry.is_dir {
                format!("{}/", entry.name)
            } else {
                format!("{}  ({})", entry.name, human_size(entry.size))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The directory currently shown for a target. A view exists before any listing
/// has landed, so an empty path falls back to the root too.
fn cwd<B: Browse>(target: B) -> PathBuf {
    target
        .view()
        .map(|view| view.cwd)
        .filter(|cwd| cwd.components().next().is_some())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The collapsed body: how full the target's disk is, and nothing else.
fn summary_body<B: Browse>(ctx: &mut PanelCtx, target: B) {
    let theme = ctx.theme;
    ctx.children(|p| {
        p.spawn(Node {
            width: Val::Px(SUMMARY_GAUGE_WIDTH),
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .with_children(|slot| {
            slot.spawn((
                gauge(theme, "Disk", usage_gauge(target)),
                bind_gauge(move || usage_gauge(target)),
            ));
        });
    });
}

/// The expanded body: path bar, listing, `actions`, disk gauge.
fn detail_body<B: Browse>(
    ctx: &mut PanelCtx,
    target: B,
    actions: impl FnOnce(&mut ChildSpawnerCommands, &Theme),
) {
    let theme = ctx.theme;
    ctx.children(|p| {
        // Path bar.
        p.spawn(row(theme.metrics.space_sm)).with_children(|bar| {
            bar.spawn(button(theme, "Home"))
                .observe(move |_: On<Activate>| target.browse(PathBuf::from("/")));
            bar.spawn(button(theme, "Up"))
                .observe(move |_: On<Activate>| {
                    let parent = cwd(target)
                        .parent()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("/"));
                    target.browse(parent);
                });
            bar.spawn(button(theme, "Refresh"))
                .observe(move |_: On<Activate>| target.browse(cwd(target)));
            bar.spawn((
                text(theme, "/", theme.metrics.font_md, Role::Text),
                bind_text(move || cwd(target).display().to_string()),
            ));
        });

        p.spawn(heading(theme, "Remote Files"));
        p.spawn((
            // Opening the panel is the request to see the directory; the
            // listing starts itself (see `manage_browsers`).
            Browser(target),
            text(theme, "", theme.metrics.font_md, Role::Text),
            bind_text(move || listing(target)),
        ));

        actions(p, theme);

        p.spawn(heading(theme, "Disk Usage"));
        p.spawn((
            gauge(theme, "Disk", usage_gauge(target)),
            bind_gauge(move || usage_gauge(target)),
        ));
    });
}

/// A panel body currently showing a target's files.
#[derive(Component)]
struct Browser<B: Browse>(B);

/// Targets already started, so the auto-browse below doesn't refire every
/// frame.
#[derive(Resource)]
struct Browsing<B: Browse>(HashSet<B>);

impl<B: Browse> Default for Browsing<B> {
    fn default() -> Self {
        Self(HashSet::new())
    }
}

/// Start any target whose panel is open but hasn't loaded yet, and release any
/// target whose panel went away.
///
/// Checked every frame rather than on `Added` so a target that couldn't be
/// reached (no connection) is retried until it can be. Closing the panel
/// releases the target and reopening it starts over, which is how an agent or
/// file server that was unreachable gets retried.
fn manage_browsers<B: Browse>(
    mut browsing: ResMut<Browsing<B>>,
    browsers: Query<&Browser<B>>,
    mut open: Local<HashSet<B>>,
) {
    open.clear();
    for Browser(target) in &browsers {
        open.insert(*target);
        if browsing.0.contains(target) {
            continue;
        }
        if target.start() {
            browsing.0.insert(*target);
        }
    }
    browsing.0.retain(|target| {
        if open.contains(target) {
            return true;
        }
        target.close();
        false
    });
}

/// The filesystem layer's node panel (file browser).
pub struct FilesystemPanel;

impl NodePanel for FilesystemPanel {
    fn build_summary(&self, ctx: &mut PanelCtx) {
        // A sub-node here is a probe device, whose filesystem the probe
        // subsystem reaches for us.
        #[cfg(feature = "probe")]
        if let Some(device_id) = ctx.target.sub {
            if let Some(device) = probe::Device::resolve(device_id) {
                summary_body(ctx, device);
            }
            return;
        }

        if let Some(instance) = ctx.target.instance {
            summary_body(ctx, agent::Agent(instance));
        }
    }

    fn build_detail(&self, ctx: &mut PanelCtx) {
        #[cfg(feature = "probe")]
        if let Some(device_id) = ctx.target.sub {
            match probe::Device::resolve(device_id) {
                Some(device) => detail_body(ctx, device, |_, _| {}),
                None => {
                    let theme = ctx.theme;
                    ctx.children(|p| {
                        p.spawn(muted(
                            theme,
                            "This device exposes no filesystem protocol.",
                            theme.metrics.font_md,
                        ));
                    });
                }
            }
            return;
        }

        if let Some(instance) = ctx.target.instance {
            detail_body(ctx, agent::Agent(instance), agent::transfers(instance));
        }
    }
}

/// Browsing an agent over the [`FsSessionStream`](crate::session).
mod agent {
    use super::*;
    use crate::session::{FileKind, client as fs_client};

    /// An agent, browsed by its instance id.
    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    pub(super) struct Agent(pub InstanceId);

    impl Browse for Agent {
        fn view(self) -> Option<BrowseView> {
            let view = fs_client::view(self.0)?;
            Some(BrowseView {
                cwd: view.cwd,
                entries: view.entries.map(|entries| {
                    entries
                        .into_iter()
                        .map(|entry| BrowseEntry {
                            name: entry.name,
                            is_dir: entry.kind == FileKind::Dir,
                            size: entry.size,
                        })
                        .collect()
                }),
                usage: view.usage,
                busy: view.busy,
                error: view.error,
            })
        }

        fn browse(self, path: PathBuf) {
            let _ = fs_client::browse(self.0, path);
        }

        fn start(self) -> bool {
            // The session resumes wherever it left off, so a reopened panel
            // shows the directory the user was last in.
            fs_client::browse(self.0, cwd(self)).is_ok()
        }

        fn close(self) {
            fs_client::close(self.0);
        }
    }

    /// The transfer buttons, which only an agent panel offers (and which the
    /// layer's roadmap still owes an implementation).
    pub(super) fn transfers(
        instance: InstanceId,
    ) -> impl FnOnce(&mut ChildSpawnerCommands, &Theme) {
        move |p, theme| {
            p.spawn(row(theme.metrics.space_sm))
                .with_children(|actions| {
                    actions
                        .spawn(button(theme, "Download"))
                        .observe(move |_: On<Activate>| {
                            info!("Filesystem: download from {}", instance)
                        });
                    actions
                        .spawn(button(theme, "Upload"))
                        .observe(move |_: On<Activate>| {
                            info!("Filesystem: upload to {}", instance)
                        });
                });
        }
    }
}

/// Browsing probe devices (NFS, SMB).
///
/// Everything protocol-specific lives behind [`sandpolis_probe::filesystem`]:
/// this module asks for a directory listing and space totals for a device id and
/// renders whatever comes back, without knowing which protocol answered.
#[cfg(feature = "probe")]
mod probe {
    use super::*;
    use sandpolis_probe::ProbeType;
    use sandpolis_probe::filesystem::{FileKind, client as probe_fs};

    /// A probe device, browsed over one of its filesystem protocols.
    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    pub(super) struct Device {
        id: u64,
        protocol: ProbeType,
    }

    impl Device {
        /// The device behind a node's sub id, reached by its first filesystem
        /// protocol since a device rarely exports the same tree over two. `None`
        /// for a device that exports none, which this layer can't browse.
        pub(super) fn resolve(device_id: u64) -> Option<Self> {
            let protocol = sandpolis_probe::REGISTERED_DEVICES
                .read()
                .ok()?
                .iter()
                .find(|d| d.id.body() == device_id)
                .and_then(|d| d.device.filesystem_protocols().first().copied())?;
            Some(Self {
                id: device_id,
                protocol,
            })
        }
    }

    impl Browse for Device {
        fn view(self) -> Option<BrowseView> {
            let view = probe_fs::view(self.id)?;
            Some(BrowseView {
                cwd: view.cwd,
                entries: view.entries.map(|entries| {
                    entries
                        .into_iter()
                        .map(|entry| BrowseEntry {
                            name: entry.name,
                            is_dir: entry.kind == FileKind::Dir,
                            size: entry.size,
                        })
                        .collect()
                }),
                usage: view.usage.map(|usage| FsUsage {
                    total: usage.total,
                    used: usage.used,
                    free: usage.free,
                }),
                busy: view.busy,
                error: view.error,
            })
        }

        fn browse(self, path: PathBuf) {
            probe_fs::browse(self.id, self.protocol, path);
        }

        fn start(self) -> bool {
            if probe_fs::connection_for(self.id).is_none() {
                return false;
            }
            // Every request is one-shot, so there's no session to resume: a
            // reopened panel starts at the root again.
            probe_fs::browse(self.id, self.protocol, PathBuf::from("/"));
            true
        }
    }
}

/// Tint a node by its disk usage while the Filesystem layer is active.
///
/// The layer keeps its OS icon: what distinguishes a node here is how full it
/// is, not what it runs. Only browsed agents have usage data; the rest stay
/// untinted.
fn node_tint(id: InstanceId) -> Color {
    match crate::session::client::view(id).and_then(|view| view.usage) {
        Some(usage) => utilization_tint(usage.used, usage.total),
        None => Color::WHITE,
    }
}

/// The filesystem layer's client plugin.
pub struct FilesystemClientPlugin;

impl Plugin for FilesystemClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Browsing<agent::Agent>>();
        app.add_systems(Update, manage_browsers::<agent::Agent>);

        #[cfg(feature = "probe")]
        {
            app.init_resource::<Browsing<probe::Device>>();
            app.add_systems(Update, manage_browsers::<probe::Device>);
        }

        let info = LayerClientInfo::new(
            LayerName::from("Filesystem"),
            "Browse and manage remote filesystems",
        )
        .with_panel(FilesystemPanel)
        .with_visible_instance_types(&[InstanceType::Agent])
        .with_node_tint(node_tint);

        // NFS and SMB probes are browsable here just like agents. Devices that
        // expose nothing this layer can drive stay hidden.
        #[cfg(feature = "probe")]
        let info = info.showing_probe_nodes_for(&["NFS", "SMB"]);

        app.register_layer_client(info);
    }
}
