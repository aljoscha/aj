//! Persistence listener wiring shared by both modes.
//!
//! The `aj-session` crate ships the actual listener factory in
//! [`aj_session::persistence_listener`]. Both the print and
//! interactive modes use it the same way: build a shared
//! `Arc<TokioMutex<ConversationLog>>` once, register the
//! listener on the agent's bus, and let `Agent::prompt` /
//! `continue_run` drive writes synchronously through the bus.
//!
//! This module exists per `docs/aj-next-plan.md` §4 to give the
//! two modes a single home for any `aj-next`-specific persistence
//! decisions (e.g. observability hooks, `.bak` migration triggers
//! on first launch). The scaffold leaves it empty; the print mode
//! step adds the first wiring helper.
