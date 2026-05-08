//! Event pump — maps each [`AgentEvent`] onto a component update.
//!
//! Drives the interactive mode's reactive flow: subscribe to the
//! agent's bus via [`Agent::subscribe_channel`], pull events off
//! the receiver in the [`aj_tui::tui::Tui`] select loop, and
//! dispatch each one to the component(s) registered for its
//! variant.
//!
//! Filled in by the "Interactive TUI: layout slots, event pump,
//! components" step in Phase 1.
//!
//! [`AgentEvent`]: aj_agent::events::AgentEvent
//! [`Agent::subscribe_channel`]: aj_agent::Agent::subscribe_channel
