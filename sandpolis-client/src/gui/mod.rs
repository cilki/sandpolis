//! Core GUI components for the Sandpolis client.
//!
//! The world view itself — nodes, links, terrains, the camera, node panels and
//! the UI toolkit — is the `gesso` crate. This module is what Sandpolis adds on
//! top: layers, login, realm selection, and the systems that turn the instance
//! database into what gesso draws.

pub mod activity;
pub mod assets;
pub mod core_toolbar;
pub mod database_browser;
pub mod edges;
pub mod input;
pub mod instance_layer;
pub mod layer_picker;
pub mod layer_toolbar;
pub mod layer_ui;
pub mod layer_visuals;
pub mod listeners;
pub mod login;
pub mod node;
pub mod node_effects;
pub mod node_panel;
pub mod node_picker;
pub mod queries;
pub mod realm_select;
pub mod services_panel;
pub mod terrain;
pub mod theme;
pub mod toast;
pub mod ui;
