//! Compile-time-embedded shell-layer assets (the terminal's monospace font).
//!
//! These are overlaid onto the client's embedded asset bundle by the GUI
//! bootstrap so the font resolves through bevy's `AssetServer` in a
//! self-contained release build.

gesso::embedded_assets!();
