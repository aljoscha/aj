//! Re-export of the per-agent footer store, now frontend-agnostic and
//! living in [`aj_app::footer`].
//!
//! The event pump refers to it as
//! `crate::modes::interactive::footer_data::AgentFooters`, so this shim
//! keeps that path resolving after the data moved to `aj-app`.

pub use aj_app::footer::AgentFooters;
