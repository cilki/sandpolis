//! Client-wide UI state (the current layer, the login dialog) and the help
//! screen. Camera input lives in `gesso::input`.

use bevy::prelude::*;
use bevy_ui_widgets::Activate;
use gesso::ui::gating::{BlocksWorldInput, ModalRoot, UiPointerState};
use gesso::ui::scene::{button, text_line};
use gesso::ui::theme::{Role, Theme, ThemedBg, ThemedBorder};
use gesso::ui::z;
use sandpolis_instance::LayerName;

/// Only one layer can be selected at a time.
#[derive(Resource, Deref, DerefMut, Debug)]
pub struct CurrentLayer(pub LayerName);

impl Default for CurrentLayer {
    fn default() -> Self {
        Self(LayerName::from("Network"))
    }
}

#[derive(Resource, Deref, DerefMut)]
pub struct LayerChangeTimer(pub Timer);

#[derive(Resource, Default)]
pub struct HelpScreenState {
    pub show: bool,
}

#[derive(Resource, Default)]
pub struct LoginDialogState {
    pub show: bool,
    pub phase: LoginPhase,
    pub server_address: String,
    pub username: String,
    pub password: String,
    /// Second copy of the password when setting one for the first time.
    pub password_confirm: String,
    pub otp: String,
    pub error_message: Option<String>,
    pub loading: bool,
}

#[derive(Default, PartialEq)]
pub enum LoginPhase {
    #[default]
    ServerAddress,
    Credentials {
        banner: sandpolis_server::ServerBanner,
    },
    /// The user exists but has no password yet: choose one (with confirmation).
    PasswordSetup {
        banner: sandpolis_server::ServerBanner,
        totp_required: bool,
    },
    /// The realm requires TOTP and a secret was just generated: show it and ask
    /// for a first code to finish logging in.
    TotpEnroll { otpauth_url: String },
}

/// Modal root marker for the native help screen.
#[derive(Component)]
pub struct HelpRoot;

/// Toggle the help screen with `H` (unless a text field is focused).
pub fn toggle_help(
    ui_pointer: Res<UiPointerState>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut help_state: ResMut<HelpScreenState>,
) {
    // An open modal claims the keyboard (see [`UiPointerState`]), so honor that
    // gate only when this screen isn't the modal in question — otherwise `H`
    // could open help but never close it.
    if !help_state.show && ui_pointer.wants_keyboard {
        return;
    }
    if keyboard.just_pressed(KeyCode::KeyH) {
        help_state.show = !help_state.show;
    }
}

/// Spawn/despawn the native help modal.
pub fn manage_help_panel(
    mut commands: Commands,
    theme: Res<Theme>,
    help_state: Res<HelpScreenState>,
    root: Query<Entity, With<HelpRoot>>,
) {
    const SHORTCUTS: &[&str] = &[
        "Arrow keys / drag  -  Pan camera",
        "Mouse wheel  -  Zoom",
        "N  -  Find node",
        "T  -  Theme picker",
        "P  -  Toggle node panels",
        "F3  -  Diagnostics overlay",
        "H  -  This help",
        "Click layer indicator  -  Switch layers",
        "Click a node  -  Expand its panel",
        "Ctrl-click  -  Select several nodes",
    ];

    let exists = !root.is_empty();
    if help_state.show && !exists {
        // `HelpRoot`/`BlocksWorldInput` don't derive `Clone`, so they're inserted
        // on the scene root rather than declared inside `bsn!`.
        commands
            .spawn_scene(help_modal_scene(&theme, SHORTCUTS))
            .insert((HelpRoot, BlocksWorldInput, ModalRoot));
    } else if !help_state.show && exists {
        for entity in &root {
            commands.entity(entity).despawn();
        }
    }
}

fn on_help_close(_activate: On<Activate>, mut help_state: ResMut<HelpScreenState>) {
    help_state.show = false;
}

/// The keyboard-shortcuts help modal, built with `bsn!`.
///
/// Spawn with `commands.spawn_scene(help_modal_scene(&theme, SHORTCUTS))` and then
/// `.insert((HelpRoot, BlocksWorldInput))` on the returned root (those markers
/// don't derive `Clone`, so they're added imperatively rather than in `bsn!`).
fn help_modal_scene(theme: &Theme, shortcuts: &[&str]) -> impl Scene {
    let scrim = Color::BLACK.with_alpha(0.55);
    let panel = theme.color(Role::Panel);
    let border = theme.color(Role::Border);

    let heading = text_line(
        theme,
        "Keyboard Shortcuts",
        Role::Text,
        theme.metrics.font_heading,
    );
    let lines: Vec<_> = shortcuts
        .iter()
        .map(|line| text_line(theme, *line, Role::TextMuted, theme.metrics.font_md))
        .collect();
    let close = button(theme, "Close", on_help_close);

    bsn! {
        Node {
            position_type: PositionType::Absolute,
            left: {Val::Px(0.0)},
            top: {Val::Px(0.0)},
            right: {Val::Px(0.0)},
            bottom: {Val::Px(0.0)},
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
        }
        BackgroundColor({scrim})
        GlobalZIndex({z::MODAL})
        Children [
            Node {
                flex_direction: FlexDirection::Column,
                width: {Val::Px(360.0)},
                padding: {UiRect::all(Val::Px(16.0))},
                row_gap: {Val::Px(6.0)},
                border: {UiRect::all(Val::Px(1.0))},
            }
            BackgroundColor({panel})
            ThemedBg({Role::Panel})
            BorderColor::all(border)
            ThemedBorder({Role::Border})
            Children [
                @heading
                --
                {lines}
                --
                @close
            ]
        ]
    }
}
