//! Compile-time-embedded desktop-layer assets (the `desktop/*.svg` node icons).
//!
//! These are overlaid onto the client's embedded asset bundle by the GUI
//! bootstrap so desktop node SVGs resolve through bevy's `AssetServer` in a
//! self-contained release build.

gesso::embedded_assets!();
