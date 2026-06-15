//! User-facing configuration: keybindings, theming, command catalog.
//!
//! Per `docs/aj-next-plan.md` §4 the interactive mode is driven by
//! a [`KeybindingsRegistry`](keybindings::KeybindingsRegistry), a
//! [`Theme`](theme::Theme), and the command catalog in
//! [`commands`], built once at startup.

pub mod commands;
pub mod keybindings;
pub mod theme;
