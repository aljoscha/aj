//! Slash-command registry.
//!
//! The interactive editor recognises `/clear`, `/model`,
//! `/thinking`, `/session`, `/help`, etc. Each command is a
//! struct that knows its name, completion shape, and how to apply
//! itself to the current [`Agent`](aj_agent::Agent) /
//! [`ConversationLog`](aj_session::ConversationLog) pair.
//!
//! Filled in by the "Selectors and theming" step in Phase 1.
