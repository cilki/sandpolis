//! Compile-time-embedded GUI assets.
//!
//! The icons generic enough to be shared (operating systems, protocols, device
//! classes) ship with `gesso`; this is the bundle specific to Sandpolis. The GUI
//! bootstrap overlays it, and each subsystem's own bundle, onto the default
//! asset source and registers it with the icon rasterizer.

gesso::embedded_assets!();
