#[cfg(not(target_os = "android"))]
pub mod cli;

#[cfg(feature = "client")]
pub mod notification;

#[cfg(feature = "client")]
pub mod service;

#[cfg(feature = "client")]
pub mod sync;

#[cfg(feature = "client")]
pub mod gui;

#[cfg(all(feature = "client", not(target_os = "android")))]
pub mod tui;

// This crate is compiled into every build for its shared types, so unlike an
// optional subsystem it has to say when the layer is actually there.
#[cfg(feature = "client")]
sandpolis_instance::register_layer!("Client", "Connected client applications");
