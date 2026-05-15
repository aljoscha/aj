//! User-facing configuration: keybindings, theming, slash commands.
//!
//! Per `docs/aj-next-plan.md` §4 the interactive mode is driven by
//! a [`KeybindingsRegistry`](keybindings::KeybindingsRegistry), a
//! [`Theme`](theme::Theme), and a [`SlashCommandRegistry`](slash_commands::SlashCommandRegistry)
//! built once at startup. The scaffold only declares the modules;
//! the "Selectors and theming" step in Phase 1 fills them in.

pub mod keybindings;
pub mod slash_commands;
pub mod theme;
