//! Round-trip test suite for the invariant.
//!
//! This suite enforces the
//! "provider SSE → unified `AssistantMessage` → provider request item"
//! round-trip invariant for each supported provider, plus the
//! cross-provider transform rules.
//!
//! Layout:
//! - `fixtures/<api>/<scenario>.sse` — captured / hand-crafted SSE wire
//!   dumps. Each frame is parsed with the same `serde_json` shape the
//!   live SDK expects, so the fixtures double as serialization examples
//!   for the provider's stream protocol.
//! - `fixtures/<api>/<scenario>.request.json` — hand-crafted golden
//!   `messages[]` request items, used for byte-stable serialize asserts.
//!
//! Each provider gets three test shapes (parse / serialize / semantic
//! round-trip). Cross-provider directions live in `cross_provider.rs`,
//! one end-to-end transform test per direction. The Codex provider's
//! suite (`openai_codex_responses.rs`) adds a fourth scenario that
//! exercises the event-normalization layer (legacy
//! `response.done` rewrites).

// Submodules live in a sibling directory matching this file's stem.
// Default Rust module-path resolution searches the parent of *this*
// file (`tests/`), so `#[path]` redirects each module into the
// sibling `tests/roundtrip/` directory that holds fixtures and helpers.
#[path = "roundtrip/common.rs"]
mod common;

#[path = "roundtrip/anthropic.rs"]
mod anthropic;

#[path = "roundtrip/openai_completions.rs"]
mod openai_completions;

#[path = "roundtrip/openai_responses.rs"]
mod openai_responses;

#[path = "roundtrip/openai_codex_responses.rs"]
mod openai_codex_responses;

#[path = "roundtrip/cross_provider.rs"]
mod cross_provider;
