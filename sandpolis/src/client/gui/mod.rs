use crate::{InstanceState, RuntimeOptions};
use anyhow::Result;
use bevy::{
    app::PluginGroup,
    asset::{
        AssetApp,
        io::{AssetSourceBuilder, AssetSourceId},
    },
    audio::AudioSinkPlayback,
    ecs::schedule::IntoScheduleConfigs,
    prelude::{
        App, AssetServer, AudioSink, Camera2d, Commands, DefaultPlugins, Entity, MessageReader,
        MonitorSelection, PostUpdate, Query, Res, ResMut, Startup, Timer, TimerMode, Update, Vec2,
        Vec3, Window, WindowPlugin, default, info,
    },
    window::{AppLifecycle, WindowMode},
};
use bevy_rapier2d::prelude::{NoUserData, RapierConfiguration, RapierPhysicsPlugin};
use sandpolis_client::gui::assets::EmbeddedDirReader;
use sandpolis_client::gui::instance_layer::{InstancePanel, open_database_browser};
use sandpolis_client::gui::ui::layer::{LayerClientInfo, RegisterLayerClient};
use sandpolis_instance::{InstanceType, LayerName};

// Import GUI types from sandpolis-client submodules
use sandpolis_client::gui::activity::{
    animate_activity_lines, cleanup_layer_activity_lines, despawn_completed_activity_lines,
    spawn_network_activity_lines, spawn_transfer_activity_lines, update_activity_line_positions,
};
use sandpolis_client::gui::core_toolbar::CoreLayerToolbarPlugin;
use sandpolis_client::gui::drag::{
    DragState, SelectionSet, disable_forces_while_dragging, handle_node_selection, start_node_drag,
    stop_node_drag, update_node_drag, update_selection_ui,
};
use sandpolis_client::gui::edges::{render_edges, update_edge_visibility, update_edges_for_layer};
use sandpolis_client::gui::input::{
    CurrentLayer, HelpScreenState, LayerChangeTimer, LoginDialogState, MousePressed, PanningState,
    ZoomLevel, handle_camera, handle_zoom, manage_help_panel, toggle_diagnostics_overlay,
    toggle_help,
};
use sandpolis_client::gui::layer_picker::{
    LayerPickerState, focus_layer_search, handle_layer_picker_toggle, layer_picker_keys,
    manage_layer_picker, rebuild_layer_rows, sync_layer_search,
};
use sandpolis_client::gui::layer_toolbar::{rebuild_layer_toolbar, update_toolbar_button_enabled};
use sandpolis_client::gui::layer_ui::{
    LayerIndicatorState, spawn_layer_indicator, update_layer_indicator,
};
use sandpolis_client::gui::layer_visuals::{
    update_node_colors_for_layer, update_node_svgs_for_layer, update_node_visibility_for_layer,
};
use sandpolis_client::gui::layout::{
    LayoutConfig, LayoutState, apply_damping, apply_repulsion_forces, apply_spring_forces,
    check_stabilization,
};
use sandpolis_client::gui::listeners::{
    DatabaseUpdate, DatabaseUpdateChannel, DatabaseUpdateSender, setup_all_listeners,
};
use sandpolis_client::gui::login::{
    LoginOperation, check_saved_servers, focus_login_input, handle_login_phase1,
    handle_login_phase2, manage_login, prompt_login_on_connect, rotate_login_logo,
    spawn_login_logo, sync_login_inputs, update_login_error,
};
use sandpolis_client::gui::minimap::{MinimapViewport, spawn_minimap, update_minimap};
use sandpolis_client::gui::node::{NodeEntity, WorldView, scale_node_svgs, spawn_node};
use sandpolis_client::gui::node_effects::{
    spin_selection_rings, update_credential_visuals, update_offline_markers, update_offline_visuals,
    update_selection_visuals,
};
use sandpolis_client::gui::node_panel::NodePanelPlugin;
use sandpolis_client::gui::node_picker::{
    NodePickerState, focus_node_search, handle_node_picker_toggle, manage_node_picker,
    node_picker_keys, rebuild_node_rows, sync_node_search,
};
use sandpolis_client::gui::queries::{query_all_instances, query_instance_metadata};
use sandpolis_client::gui::realm_select::{
    RealmCertDir, RealmSelectOperation, RealmSelectState, drive_realm_connect, drive_realm_pick,
    manage_realm_select, prompt_realm_select_on_start, update_realm_select_error,
};
use sandpolis_client::gui::responsive::update_responsive_ui;
use sandpolis_client::gui::terrain::{
    TerrainConfig, TerrainMembersDirty, rebuild_terrains, sync_instance_terrain_members,
    update_terrain_bounds,
};
use sandpolis_client::gui::terrain_layout::{apply_terrain_cohesion, relax_terrain_overlap};
use sandpolis_client::gui::theme::{
    ThemePickerState, handle_theme_picker_toggle, manage_theme_picker, update_theme_rows,
};

// Re-export submodules for external access
pub use sandpolis_client::gui::node_panel;

/// Initialize and start rendering the UI.
pub async fn main(options: RuntimeOptions, state: InstanceState) -> Result<()> {
    crate::client::spawn_client_sync(state.clone());

    // Create channel for database updates from resident listeners
    let (db_update_tx, db_update_rx) = tokio::sync::mpsc::unbounded_channel();

    // Spawn background task for database listeners
    let network = state.network.clone();
    let instance = state.instance.clone();
    let db_update_tx_clone = db_update_tx.clone();
    tokio::spawn(async move {
        setup_all_listeners(network, instance, db_update_tx_clone).await;
    });

    // Where a cert imported through the realm-selection dialog is persisted:
    // the data directory when there is one, otherwise the default cert dir the
    // startup scan also reads (on Android the data directory always exists).
    #[cfg(not(target_os = "android"))]
    let fallback_cert_dir = crate::client::default_realm_cert_dir();
    #[cfg(target_os = "android")]
    let fallback_cert_dir = None;
    let realm_cert_dir = RealmCertDir(options.database.storage.clone().or(fallback_cert_dir));

    let mut app = App::new();

    // Serve all GUI assets from compile-time-embedded bundles so release builds
    // are self-contained. Each asset-owning crate contributes its own embedded
    // directory; they're overlaid (earlier shadows later) behind the default
    // asset source. Must be registered before `AssetPlugin` (part of
    // `DefaultPlugins`) is built.
    let mut asset_dirs = vec![sandpolis_client::gui::assets::dir()];
    #[cfg(feature = "desktop")]
    asset_dirs.push(sandpolis_desktop::client::assets::dir());
    #[cfg(feature = "inventory")]
    asset_dirs.push(sandpolis_inventory::client::assets::dir());
    #[cfg(feature = "probe")]
    asset_dirs.push(sandpolis_probe::client::assets::dir());
    #[cfg(feature = "shell")]
    asset_dirs.push(sandpolis_shell::client::assets::dir());
    app.register_asset_source(
        AssetSourceId::Default,
        AssetSourceBuilder::new(move || Box::new(EmbeddedDirReader::new(asset_dirs.clone()))),
    );

    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: Some(Window {
                    resizable: true,
                    mode: if cfg!(target_os = "android") {
                        WindowMode::BorderlessFullscreen(MonitorSelection::Current)
                    } else {
                        WindowMode::Windowed
                    },
                    // on iOS, gestures must be enabled.
                    // This doesn't work on Android
                    recognize_rotation_gesture: true,
                    ..default()
                }),
                ..default()
            })
            .disable::<bevy::log::LogPlugin>(),
    )
    .add_plugins(RapierPhysicsPlugin::<NoUserData>::pixels_per_meter(100.0))
    // .add_plugins(RapierDebugRenderPlugin::default())
    // FPS/frame-time diagnostics, surfaced by the F3 overlay (see toggle_diagnostics_overlay).
    .add_plugins(bevy::diagnostic::FrameTimeDiagnosticsPlugin::default())
    .add_plugins(bevy::dev_tools::diagnostics_overlay::DiagnosticsOverlayPlugin)
    .add_plugins(bevy_svg::prelude::SvgPlugin)
    .add_plugins(sandpolis_client::gui::ui::UiPlugin)
    .add_plugins(NodePanelPlugin)
    .add_plugins(CoreLayerToolbarPlugin {
        network: state.network.clone(),
    })
    // Notification toasts. Adding this is also what tells the notification
    // watcher a GUI exists, so notifications raised while the window has focus
    // land here instead of going to the OS.
    .add_plugins(sandpolis_client::gui::toast::ToastPlugin)
    .insert_resource(CurrentLayer(LayerName::from("Desktop")))
    .insert_resource(ZoomLevel(1.0))
    .insert_resource(LayerChangeTimer(Timer::from_seconds(3.0, TimerMode::Once)))
    .insert_resource(MinimapViewport::default())
    .insert_resource(LayerIndicatorState::default())
    .insert_resource(DatabaseUpdateChannel {
        receiver: db_update_rx,
    })
    .insert_resource(DatabaseUpdateSender {
        sender: db_update_tx.clone(),
    })
    .insert_resource(LayoutConfig::default())
    .insert_resource(LayoutState::default())
    .insert_resource(DragState::default())
    .insert_resource(LayerPickerState::default())
    .insert_resource(NodePickerState::default())
    .insert_resource(HelpScreenState::default())
    .insert_resource(LoginDialogState::default())
    .insert_resource(LoginOperation::default())
    .insert_resource(RealmSelectState::default())
    .insert_resource(RealmSelectOperation::default())
    .insert_resource(realm_cert_dir)
    .insert_resource(SelectionSet::default())
    .insert_resource(ThemePickerState::default())
    .insert_resource(TerrainMembersDirty::default())
    .insert_resource(state.instance.clone())
    .insert_resource(state.network.clone())
    .insert_resource(state.server.clone())
    .insert_resource(options)
    .insert_resource(MousePressed(false))
    .insert_resource(PanningState::default())
    .insert_resource(TerrainConfig::default())
    .add_systems(Startup, setup)
    // Native bevy_ui chrome (migrated off egui).
    .add_systems(Startup, (spawn_minimap, spawn_layer_indicator))
    .add_systems(Update, (update_minimap, update_layer_indicator))
    .add_systems(
        Update,
        (
            (
                manage_layer_picker,
                focus_layer_search,
                sync_layer_search,
                rebuild_layer_rows,
                rebuild_layer_toolbar,
                update_toolbar_button_enabled,
                layer_picker_keys,
                manage_node_picker,
                focus_node_search,
                sync_node_search,
                rebuild_node_rows,
                node_picker_keys,
                manage_theme_picker,
                update_theme_rows,
            ),
            // Help / login modals (native)
            (
                toggle_help,
                toggle_diagnostics_overlay,
                manage_help_panel,
                manage_login,
                focus_login_input,
                sync_login_inputs,
                update_login_error,
                manage_realm_select,
                update_realm_select_error,
            ),
        ),
    )
    .add_systems(
        Update,
        (
            // Input handling (desktop)
            #[cfg(not(target_os = "android"))]
            handle_zoom,
            #[cfg(not(target_os = "android"))]
            handle_camera,
            // Input handling (mobile)
            #[cfg(target_os = "android")]
            sandpolis_client::gui::input::handle_touch_camera,
            #[cfg(target_os = "android")]
            sandpolis_client::gui::input::handle_touch_zoom,
            handle_layer_picker_toggle,
            handle_node_picker_toggle,
            handle_theme_picker_toggle,
            handle_lifetime,
            // Responsive UI updates
            update_responsive_ui,
            // Realm selection comes before login, since without a realm cert
            // there is no server to log into.
            prompt_realm_select_on_start,
            drive_realm_pick,
            drive_realm_connect,
            // Login systems
            check_saved_servers,
            prompt_login_on_connect,
            handle_login_phase1,
            handle_login_phase2,
            spawn_login_logo,
            rotate_login_logo,
        ),
    )
    .add_systems(
        Update,
        (
            // Selection systems (must run before drag)
            handle_node_selection,
            update_selection_visuals,
            spin_selection_rings,
            // Offline node effect. The scrim follows the marker in the same frame.
            (update_offline_markers, update_offline_visuals).chain(),
            // Credential-warning ring. The marker is toggled by the probe layer;
            // this only renders it.
            update_credential_visuals,
            // Drag systems
            start_node_drag,
            update_node_drag,
            stop_node_drag,
            disable_forces_while_dragging,
            // Native selection-count badge (migrated off egui)
            update_selection_ui,
        ),
    )
    .add_systems(
        Update,
        (
            // Layout systems
            apply_repulsion_forces,
            apply_spring_forces,
            apply_damping,
            check_stabilization,
        ),
    )
    // Terrain layout writes node transforms directly (like dragging), so it runs
    // after the drag systems have placed the held node and before rapier picks
    // up transform changes in PostUpdate.
    .add_systems(
        Update,
        (relax_terrain_overlap, apply_terrain_cohesion)
            .chain()
            .after(update_node_drag),
    )
    .add_systems(
        PostUpdate,
        (
            // Edge systems
            render_edges,
            update_edges_for_layer,
            update_edge_visibility,
            // Layer visuals
            update_node_svgs_for_layer,
            update_node_colors_for_layer,
            update_node_visibility_for_layer,
            // Node SVG scaling - MUST run after update_node_svgs_for_layer
            scale_node_svgs.after(update_node_svgs_for_layer),
            // Terrain bounds follow the frame's final node positions
            sync_instance_terrain_members,
            rebuild_terrains.after(sync_instance_terrain_members),
            update_terrain_bounds.after(rebuild_terrains),
            // Database updates
            process_database_updates,
        ),
    )
    .add_systems(
        Update,
        (
            // Activity line systems
            spawn_transfer_activity_lines,
            spawn_network_activity_lines,
            update_activity_line_positions,
            animate_activity_lines,
            despawn_completed_activity_lines,
            cleanup_layer_activity_lines,
        ),
    );

    // Per-layer client plugins (controllers, node visibility, probe systems).
    // These replace the old `inventory`-collected `LayerGuiExtension`s.
    // The agent subsystem has no `` feature: it ships with every build, and
    // this is what registers its deploy dialog.
    app.add_plugins(sandpolis_agent::client::gui::AgentClientPlugin);
    #[cfg(feature = "inventory")]
    app.add_plugins(sandpolis_inventory::client::gui::InventoryClientPlugin);
    #[cfg(feature = "desktop")]
    app.add_plugins(sandpolis_desktop::client::gui::DesktopClientPlugin);
    #[cfg(feature = "filesystem")]
    app.add_plugins(sandpolis_filesystem::client::gui::FilesystemClientPlugin);
    #[cfg(feature = "health")]
    app.add_plugins(sandpolis_health::client::gui::HealthClientPlugin);
    #[cfg(feature = "shell")]
    app.add_plugins(sandpolis_shell::client::gui::ShellClientPlugin);

    #[cfg(feature = "snapshot")]
    app.add_plugins(sandpolis_snapshot::client::gui::SnapshotClientPlugin);
    #[cfg(feature = "tunnel")]
    app.add_plugins(sandpolis_tunnel::client::gui::TunnelClientPlugin);
    #[cfg(feature = "probe")]
    app.add_plugins(sandpolis_probe::client::gui::ProbeClientPlugin);
    #[cfg(feature = "account")]
    app.add_plugins(sandpolis_account::client::gui::AccountClientPlugin);

    // Debug "Instance" layer: shows every node regardless of type, with a
    // node panel that surfaces that node's identity and connection rows.
    // The controller carries its own manager handles (build() gets no resources).
    app.register_layer_client(
        LayerClientInfo::new(LayerName::from("Instance"), "All instances (debug)")
            .with_visible_instance_types(&[
                InstanceType::Server,
                InstanceType::Agent,
                InstanceType::Client,
            ])
            .with_panel(InstancePanel {
                network: state.network.clone(),
                instance: state.instance.clone(),
            })
            .with_toolbar_action("View database", "toolbar/database.svg", |commands| {
                open_database_browser(commands)
            }),
    );

    app.run();
    Ok(())
}

fn setup(
    mut commands: Commands,
    mut rapier_config: Query<&mut RapierConfiguration>,
    asset_server: Res<AssetServer>,
    instance_manager: Res<sandpolis_instance::InstanceManager>,
    network_manager: Res<sandpolis_instance::network::NetworkManager>,
) {
    // Set zero gravity for floating nodes
    for mut rapier_config in &mut rapier_config {
        rapier_config.gravity = Vec2::ZERO;
    }

    // Spawn main camera with WorldView marker. Mark it as the default UI camera so
    // native bevy_ui always targets it (the login dialog's logo spawns a second,
    // higher-order camera that would otherwise capture the UI).
    commands.spawn((
        Camera2d,
        WorldView,
        bevy::ui::IsDefaultUiCamera,
        // MSAA makes some Android devices panic, this is under investigation
        // https://github.com/bevyengine/bevy/issues/8229
        #[cfg(target_os = "android")]
        bevy::render::view::Msaa::Off,
    ));

    // Query database for initial instances and spawn nodes
    if let Ok(instances) = query_all_instances(&instance_manager, &network_manager) {
        for instance_id in instances {
            if let Ok(metadata) = query_instance_metadata(instance_id) {
                // Spawn local instance at center, others at random positions
                let position = if metadata.instance_id == instance_manager.instance_id {
                    Some(Vec3::ZERO) // Center of screen
                } else {
                    None // Random position
                };

                spawn_node(
                    &asset_server,
                    &mut commands,
                    metadata.instance_id,
                    metadata.os_type,
                    metadata.is_server,
                    position,
                );
            }
        }
    }
}

/// Process database updates from resident listeners
fn process_database_updates(
    mut update_channel: ResMut<DatabaseUpdateChannel>,
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut terrain_members_dirty: ResMut<TerrainMembersDirty>,
    node_query: Query<(Entity, &NodeEntity)>,
) {
    // Nodes spawned via `commands` in this pass aren't visible to `node_query`
    // until the command queue flushes, so track ids spawned here as well to
    // avoid duplicates within a single batch of updates.
    let mut spawned_this_pass = std::collections::HashSet::new();

    // Process all pending updates
    while let Ok(update) = update_channel.receiver.try_recv() {
        match update {
            DatabaseUpdate::InstanceAdded(instance_id) => {
                // One instance can produce several connections (a reconnect
                // that overlaps the old socket), each firing InstanceAdded.
                // Only spawn one node per InstanceId.
                let already_spawned = spawned_this_pass.contains(&instance_id)
                    || node_query
                        .iter()
                        .any(|(_, node)| node.instance_id == instance_id);
                if !already_spawned {
                    spawned_this_pass.insert(instance_id);
                    if let Ok(metadata) = query_instance_metadata(instance_id) {
                        spawn_node(
                            &asset_server,
                            &mut commands,
                            metadata.instance_id,
                            metadata.os_type,
                            metadata.is_server,
                            None, // Random position for dynamically added nodes
                        );
                    }
                }
            }
            DatabaseUpdate::InstanceRemoved(instance_id) => {
                // Despawn node entity matching the instance_id
                for (entity, node_entity) in node_query.iter() {
                    if node_entity.instance_id == instance_id {
                        commands.entity(entity).despawn();
                        info!("Despawned node for removed instance: {}", instance_id);
                        break;
                    }
                }
            }
            DatabaseUpdate::NetworkTopologyChanged => {
                // Edges will be rebuilt by update_edges_for_layer system
            }
            DatabaseUpdate::DomainsChanged => {
                // Every node's membership is re-resolved by
                // sync_instance_terrain_members on the next pass.
                terrain_members_dirty.0 = true;
            }
            _ => {
                // Other updates will be handled in later phases
            }
        }
    }
}

// Pause audio when app goes into background and resume when it returns.
// This is handled by the OS on iOS, but not on Android.
fn handle_lifetime(
    mut lifecycle_events: MessageReader<AppLifecycle>,
    music_controller: Query<&AudioSink>,
) {
    let Ok(music_controller) = music_controller.single() else {
        return;
    };

    for event in lifecycle_events.read() {
        match event {
            AppLifecycle::Idle | AppLifecycle::WillSuspend | AppLifecycle::WillResume => {}
            AppLifecycle::Suspended => music_controller.pause(),
            AppLifecycle::Running => music_controller.play(),
        }
    }
}
