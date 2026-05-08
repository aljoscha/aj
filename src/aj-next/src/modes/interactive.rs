//! Interactive TUI mode.
//!
//! Per `docs/aj-next-plan.md` §4 the interactive mode owns:
//!
//! - the [`aj_tui::tui::Tui`] event loop (input, render throttle);
//! - a [`layout`] of named slots that components register into;
//! - an [`event_pump`] that maps each [`AgentEvent`] onto a
//!   component update;
//! - a registry of [`components`] (assistant message, tool
//!   execution, footer, header, selectors, etc.);
//! - editor extensions ([`editor_ext`]) that bolt slash-command
//!   completion and `@file` autocomplete onto the shared
//!   [`aj_tui::EditorComponent`];
//! - the keybinding map ([`keys`]).
//!
//! [`AgentEvent`]: aj_agent::events::AgentEvent

pub mod components;
pub mod editor_ext;
pub mod event_pump;
pub mod footer_data;
pub mod keys;
pub mod layout;

use anyhow::{Result, bail};

use crate::cli::args::Args;

/// Driver for a single interactive session. Owns the
/// [`aj_tui::tui::Tui`], the registered listeners on the agent's
/// bus, and the [`aj_session::ConversationLog`] for this thread.
///
/// The scaffold ships an empty placeholder so the binary's
/// dispatcher can refer to the correct entry point. The real
/// fields land alongside the layout / event-pump / components
/// implementations in the next Phase 1 steps.
pub struct InteractiveMode {}

impl InteractiveMode {
    /// Build an [`InteractiveMode`] from the parsed CLI [`Args`].
    ///
    /// Today this just stashes the args; subsequent steps wire up
    /// the agent, the conversation log, the bus listeners, and
    /// the [`aj_tui::tui::Tui`].
    pub fn from_args(_args: Args) -> Result<Self> {
        Ok(Self {})
    }

    /// Run the TUI to completion. Returns when the user quits or
    /// the agent reports a fatal error.
    ///
    /// The scaffold short-circuits with a clear error so users
    /// (and the test suite) know the mode isn't wired up yet;
    /// the real body awaits both `tui.next_event()` and the
    /// agent's bus channel, so the `async` signature stays.
    #[allow(clippy::unused_async)]
    pub async fn run(self) -> Result<()> {
        bail!("aj-next interactive mode is not yet implemented");
    }
}
