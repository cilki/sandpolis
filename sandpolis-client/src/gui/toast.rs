//! In-app notification toasts.
//!
//! A stack in the top-right corner — away from the layer indicator and toolbar,
//! which own the bottom-right — where each toast fades out and disappears on its
//! own. They are the foreground half of notification delivery; when the window
//! doesn't have focus, `sandpolis_client::notification` sends the same
//! notification to the operating system instead.
//!
//! Toasts never take input. They are `Pickable::IGNORE` and carry neither
//! [`BlocksWorldInput`](gesso::ui::gating::BlocksWorldInput) nor
//! [`ModalRoot`](gesso::ui::gating::ModalRoot), so panning and selecting in
//! the world view keep working underneath one.

use bevy::prelude::*;
use gesso::ui::theme::{Role, Theme, ThemedBg, ThemedBorder};
use gesso::ui::widgets::{column, text};
use gesso::ui::z;
use sandpolis_instance::notification::{NotificationData, Severity};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// How long a toast stays up before it starts fading, in seconds.
const HOLD_SECS: f32 = 6.0;
/// How long the fade-out takes, in seconds.
const FADE_SECS: f32 = 1.0;
/// Most toasts on screen at once. Beyond this the oldest goes early, so a burst
/// of notifications can't cover the world view.
const MAX_VISIBLE: usize = 5;
/// Toast width in pixels.
const WIDTH_PX: f32 = 320.0;

/// Receives notifications from the (non-ECS) database watcher.
#[derive(Resource)]
pub struct ToastChannel {
    receiver: UnboundedReceiver<NotificationData>,
}

/// Marker for the container that stacks toasts.
#[derive(Component)]
struct ToastStack;

/// A live toast and its remaining lifetime.
#[derive(Component)]
struct Toast {
    timer: Timer,
    /// The severity colour, kept so the fade can rebuild it from the current
    /// theme rather than darkening whatever is already there.
    role: Role,
}

/// Installs the toast stack and registers this client as a toast destination.
///
/// Adding the plugin is what tells `sandpolis_client::notification` a GUI
/// exists; without it every notification is delivered natively.
pub struct ToastPlugin;

impl Plugin for ToastPlugin {
    fn build(&self, app: &mut App) {
        // The watcher runs off the ECS (on the Tokio runtime), so notifications
        // arrive over a channel and are drained in a system — the same bridge
        // the database listeners use.
        let (sender, receiver) = unbounded_channel();
        crate::notification::set_toast_sink(sender);

        app.insert_resource(ToastChannel { receiver })
            .add_systems(Startup, spawn_toast_stack)
            .add_systems(Update, (track_foreground, drain_toasts, expire_toasts));
    }
}

/// Tell the notification watcher whether the user is looking at the client, so
/// it can route between a toast and an OS notification.
#[cfg(not(target_os = "android"))]
fn track_foreground(windows: Query<&Window>) {
    if let Some(window) = windows.iter().next() {
        crate::notification::set_foreground(window.focused);
    }
}

/// As above, but Android has no window focus to speak of — whether the app is
/// in front is what `AppLifecycle` reports.
#[cfg(target_os = "android")]
fn track_foreground(mut lifecycle: MessageReader<bevy::window::AppLifecycle>) {
    for event in lifecycle.read() {
        crate::notification::set_foreground(matches!(event, bevy::window::AppLifecycle::Running));
    }
}

fn spawn_toast_stack(mut commands: Commands, theme: Res<Theme>) {
    commands.spawn((
        ToastStack,
        Pickable::IGNORE,
        GlobalZIndex(z::TOAST),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(theme.metrics.space_md),
            right: Val::Px(theme.metrics.space_md),
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(theme.metrics.space_sm),
            align_items: AlignItems::FlexEnd,
            ..default()
        },
    ));
}

/// Turn queued notifications into toasts.
fn drain_toasts(
    mut commands: Commands,
    mut channel: ResMut<ToastChannel>,
    theme: Res<Theme>,
    stack: Query<Entity, With<ToastStack>>,
    toasts: Query<Entity, With<Toast>>,
) {
    let Ok(stack) = stack.single() else {
        return;
    };

    // Oldest first, so dropping from the front sheds the stalest toasts.
    let mut live: Vec<Entity> = toasts.iter().collect();

    while let Ok(notification) = channel.receiver.try_recv() {
        if live.len() >= MAX_VISIBLE {
            let oldest = live.remove(0);
            commands.entity(oldest).despawn();
        }

        live.push(spawn_toast(&mut commands, &theme, stack, &notification));
    }
}

/// Fade each toast out over its last [`FADE_SECS`], then despawn it.
fn expire_toasts(
    mut commands: Commands,
    time: Res<Time>,
    theme: Res<Theme>,
    mut toasts: Query<(
        Entity,
        &mut Toast,
        &mut BackgroundColor,
        &mut BorderColor,
        &Children,
    )>,
    mut texts: Query<&mut TextColor>,
) {
    for (entity, mut toast, mut background, mut border, children) in &mut toasts {
        toast.timer.tick(time.delta());

        if toast.timer.is_finished() {
            commands.entity(entity).despawn();
            continue;
        }

        let remaining = toast.timer.remaining_secs();
        if remaining >= FADE_SECS {
            continue;
        }

        // Background and border are rebuilt from the theme so a theme switch
        // part-way through a fade lands on the new palette. Text keeps its own
        // colour, which differs per line (title vs. body).
        let alpha = (remaining / FADE_SECS).clamp(0.0, 1.0);
        background.0 = theme.color(Role::Panel).with_alpha(alpha);
        *border = BorderColor::all(theme.color(toast.role).with_alpha(alpha));
        for child in children.iter() {
            if let Ok(mut color) = texts.get_mut(child) {
                color.0 = color.0.with_alpha(alpha);
            }
        }
    }
}

/// The theme role a severity is drawn in.
fn severity_role(severity: Severity) -> Role {
    match severity {
        Severity::Info => Role::Accent,
        Severity::Warn => Role::Warn,
        Severity::Error => Role::Error,
    }
}

/// Spawn one toast under `stack` and return it.
fn spawn_toast(
    commands: &mut Commands,
    theme: &Theme,
    stack: Entity,
    notification: &NotificationData,
) -> Entity {
    let role = severity_role(notification.severity);

    // The layer is what makes an otherwise bare line ("sshd.service failed")
    // attributable, so it leads the title.
    let heading = format!("{} — {}", notification.layer, notification.title);
    let body = notification.body.clone();
    let toast = commands
        .spawn((
            Toast {
                timer: Timer::from_seconds(HOLD_SECS, TimerMode::Once),
                role,
            },
            Pickable::IGNORE,
            Node {
                width: Val::Px(WIDTH_PX),
                padding: UiRect::all(Val::Px(theme.metrics.space_md)),
                // A thicker left edge carries the severity colour without
                // tinting the whole toast, which would fight the theme.
                border: UiRect {
                    left: Val::Px(3.0),
                    ..UiRect::all(Val::Px(1.0))
                },
                ..column(theme.metrics.space_xs)
            },
            BackgroundColor(theme.color(Role::Panel)),
            ThemedBg(Role::Panel),
            BorderColor::all(theme.color(role)),
            ThemedBorder(role),
        ))
        .id();

    commands.entity(toast).with_children(|parent| {
        parent.spawn(text(theme, heading, theme.metrics.font_md, Role::Text));
        if let Some(body) = body {
            parent.spawn(text(theme, body, theme.metrics.font_sm, Role::TextMuted));
        }
    });

    commands.entity(stack).add_child(toast);
    toast
}
