//! User-facing configuration: keybindings, theming, command catalog.
//!
//! The interactive mode is driven by
//! a [`KeybindingsRegistry`](keybindings::KeybindingsRegistry), a
//! [`Theme`](theme::Theme), and the command catalog in
//! [`commands`], built once at startup.

pub use aj_app::commands;

pub mod keybindings;
pub mod theme;
