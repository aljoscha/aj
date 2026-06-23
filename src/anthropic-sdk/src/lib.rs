//! A minimal SDK for the Anthropic API, as described at
//! https://docs.anthropic.com/en/api/overview.
//!
//! The type set covers what AJ needs from the Messages API, not the full
//! wire protocol. Some public items have no caller in `aj-models` today
//! (e.g. the `Client` beta setters, the response-to-request conversions
//! on `Message`/`ContentBlock`, `Usage::apply_delta`). We keep them on
//! purpose: this is a reusable client we may drive for other things, so
//! the public surface tracks the wire API rather than only the current
//! consumer. Public items here are intentional, not leaked.

pub mod client;
pub mod messages;
mod stealth;
pub mod usage;
