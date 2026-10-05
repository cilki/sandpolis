//! Core GUI components for the Sandpolis client.
//!
//! This module provides the complete GUI infrastructure for the Sandpolis client,
//! including layer-agnostic components and Bevy systems.

pub mod activity;
pub mod assets;
pub mod core_toolbar;
pub mod database_browser;
pub mod drag;
pub mod edges;
pub mod input;
pub mod instance_layer;
pub mod layer_picker;
pub mod layer_toolbar;
pub mod layer_ui;
pub mod layer_visuals;
pub mod layout;
pub mod listeners;
pub mod login;
pub mod minimap;
pub mod node;
pub mod node_effects;
pub mod node_panel;
pub mod node_picker;
pub mod queries;
pub mod realm_select;
pub mod responsive;
pub mod services_panel;
pub mod terrain;
pub mod terrain_layout;
pub mod theme;
pub mod toast;
pub mod ui;

/// The environment variables winit consults to find a display server on Linux,
/// in the order it tries them.
#[cfg(all(target_os = "linux", not(target_os = "android")))]
const DISPLAY_VARS: [&str; 3] = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY"];

/// Whether any of [`DISPLAY_VARS`] names a display server. `lookup` is the
/// environment, taken as an argument so this is testable without mutating the
/// process's own.
#[cfg(all(target_os = "linux", not(target_os = "android")))]
fn display_is_available(lookup: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    DISPLAY_VARS
        .iter()
        // An empty value is no value: winit hands it to the compositor/X11
        // connect call, which rejects it the same way a missing one does.
        .any(|var| lookup(var).is_some_and(|value| !value.is_empty()))
}

/// Refuse to open the GUI when there's no display server to open it on.
///
/// `bevy_winit` panics outright in that case ("Failed to build event loop:
/// ... neither WAYLAND_DISPLAY nor WAYLAND_SOCKET nor DISPLAY is set"), which is
/// what `sandpolis client` gets on a headless host — over SSH, from a service
/// unit, inside a container started without a socket. That's a configuration
/// mistake, not a crash, so say so and point at the subcommands that do work
/// there. The counterpart for the TUI subcommands is
/// [`crate::tui::require_terminal`].
#[cfg(all(target_os = "linux", not(target_os = "android")))]
pub fn require_display() -> anyhow::Result<()> {
    if display_is_available(|var| std::env::var_os(var)) {
        return Ok(());
    }

    anyhow::bail!(
        "the client's GUI needs a display server, but none of {} is set; \
         run a client subcommand instead (e.g. `sandpolis agents list`)",
        DISPLAY_VARS.join(", ")
    )
}

/// Platforms whose window system needs no environment to be found.
#[cfg(not(all(target_os = "linux", not(target_os = "android"))))]
pub fn require_display() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "linux", not(target_os = "android")))]
mod test_require_display {
    use std::ffi::OsString;

    fn lookup(set: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<OsString> {
        move |var| {
            set.iter()
                .find(|(name, _)| *name == var)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    /// Any one of the three is enough, which is exactly what winit accepts.
    #[test]
    fn any_display_var_counts() {
        for var in super::DISPLAY_VARS {
            assert!(
                super::display_is_available(lookup(vec![(var, "something")])),
                "{var} alone should be enough"
            );
        }
    }

    /// A headless host: nothing set, or set to nothing.
    #[test]
    fn no_display_var_is_headless() {
        assert!(!super::display_is_available(lookup(vec![])));
        assert!(!super::display_is_available(lookup(vec![
            ("WAYLAND_DISPLAY", ""),
            ("DISPLAY", ""),
        ])));
    }

    /// An unrelated variable doesn't count.
    #[test]
    fn unrelated_vars_do_not_count() {
        assert!(!super::display_is_available(lookup(vec![(
            "TERM",
            "xterm-256color"
        )])));
    }

    /// The error names every variable that would have worked, so the user knows
    /// what to set.
    #[test]
    fn the_error_names_every_display_var() {
        // There is only an error to inspect when this test host really is
        // headless; on a developer's desktop `require_display` rightly succeeds.
        if super::display_is_available(|var| std::env::var_os(var)) {
            return;
        }

        let message = super::require_display()
            .expect_err("a headless host has no display")
            .to_string();
        for var in super::DISPLAY_VARS {
            assert!(message.contains(var), "{message:?} should mention {var}");
        }
    }
}
