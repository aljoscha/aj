//! Renders the session-picker overlay (`/session`). Drives off
//! [`aj_session::ConversationPersistence::list_threads`] and
//! supports fuzzy filtering through [`aj_tui::fuzzy`].
//!
//! Filled in by the "Selectors and theming" step in Phase 1 of
//! `docs/aj-next-plan.md`.
