//! Reactive data binding for retained-mode UI.
//!
//! In immediate mode you simply re-read state every frame. In retained `bevy_ui`
//! a node is spawned once, so to keep a label in sync with changing data we attach
//! a [`BindText`] holding a projection closure; [`drive_bind_text`] re-evaluates it
//! each frame and writes the result into the node's [`Text`] (diff-guarded to avoid
//! needless relayout).
//!
//! The closure form is the general escape hatch from the plan's `Bind<T: Data>`
//! design: it captures whatever it needs (an `InstanceId`, a `Resident<T>` clone)
//! and returns the string to show. A typed `Bind<Resident<T>>` variant can be added
//! once the per-layer `queries.rs` consumers return live data instead of stubs.

use bevy::prelude::*;
use std::sync::Arc;

/// A label whose text is produced by a projection closure, refreshed each frame.
#[derive(Component, Clone)]
pub struct BindText(pub Arc<dyn Fn() -> String + Send + Sync>);

/// Produces an empty-string projection, so `BindText` satisfies the
/// `Clone + Default` bound `bsn!` places on component values (which immediately
/// overwrites the default with the real projection).
impl Default for BindText {
    fn default() -> Self {
        BindText(Arc::new(String::new))
    }
}

/// Build a [`BindText`] from a closure.
pub fn bind_text(project: impl Fn() -> String + Send + Sync + 'static) -> BindText {
    BindText(Arc::new(project))
}

/// Re-evaluate every [`BindText`] and update its [`Text`] when the value changes.
pub fn drive_bind_text(mut query: Query<(&BindText, &mut Text)>) {
    for (bind, mut text) in &mut query {
        let value = (bind.0)();
        if text.0 != value {
            text.0 = value;
        }
    }
}

/// Installs the binding drivers.
pub struct BindPlugin;

impl Plugin for BindPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, drive_bind_text);
    }
}
