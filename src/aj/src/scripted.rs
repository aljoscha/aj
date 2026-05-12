//! `--scripted <NAME>` flag plumbing.
//!
//! Both [`interactive`](crate::modes::interactive) and
//! [`print`](crate::modes::print) modes share the same resolution path:
//! given the user-supplied demo name, either look it up in
//! [`aj_models::scripted::demos`] and hand back a ready-to-
//! plug-in [`Provider`](aj_models::provider::Provider) handle, or —
//! when the name is unknown or the user passed the sentinel `help` /
//! `list` / `?` — print the catalog to stderr and exit so the caller
//! can pick a real one.
//!
//! The resolver is intentionally one-shot: it consumes the flag and
//! returns the resolved pair in the same shape
//! [`crate::model::resolve`] produces for real providers, so the two
//! call sites (`interactive.rs`, `print.rs`) can plug either path into
//! [`Agent::with_provider`](aj_agent::Agent::with_provider) without a
//! conditional.

use std::sync::Arc;

use aj_models::provider::Provider;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::scripted::demos;
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use anyhow::{Result, bail};

/// Resolved scripted provider in the same shape the real-provider path
/// returns from [`crate::model::resolve`]: a provider handle plus the
/// registry-style metadata stamped onto every inference.
pub struct ResolvedScriptedModel {
    pub provider: Arc<dyn Provider>,
    pub model_info: Arc<ModelInfo>,
}

/// Resolve the `--scripted <NAME>` argument into a provider handle and
/// synthesized [`ModelInfo`].
///
/// When `name` matches a demo, returns a [`ScriptedProvider`] preloaded
/// with that demo's per-inference scripts alongside the synthesized
/// [`ModelInfo`] that callers feed into
/// [`Agent::with_provider`](aj_agent::Agent::with_provider). When the
/// user explicitly asked for the catalog (`help` / `list` / `?` /
/// empty string), prints the listing to stderr and calls
/// [`std::process::exit(0)`]; for an unknown name, prints the listing
/// and returns an error so the binary exits with a non-zero status.
///
/// The returned provider defaults to [`ExhaustedBehavior::EndTurn`] so
/// the user can keep chatting (and watch the empty fallback in the TUI)
/// after the canned script runs out, rather than panicking the
/// inference task. Tests that want strict scripting can construct a
/// [`ScriptedProvider`] directly with
/// [`ExhaustedBehavior::Panic`](aj_models::scripted::ExhaustedBehavior::Panic).
pub fn resolve_or_explain(name: &str) -> Result<ResolvedScriptedModel> {
    if matches!(name, "help" | "list" | "?" | "") {
        print_catalog();
        std::process::exit(0);
    }

    if let Some(scripts) = demos::lookup(name) {
        let provider: Arc<dyn Provider> =
            Arc::new(ScriptedProvider::new(scripts).on_exhausted(ExhaustedBehavior::EndTurn));
        let model_info = Arc::new(scripted_model_info(name));
        return Ok(ResolvedScriptedModel {
            provider,
            model_info,
        });
    }

    print_catalog();
    bail!("unknown demo for --scripted: {name:?}");
}

/// Build a [`ModelInfo`] for the scripted demo named `name`.
///
/// `api` / `provider` match what the [`ScriptedProvider`]'s demos stamp
/// onto every emitted [`AssistantMessage`](aj_models::types::AssistantMessage)
/// partial (both `"scripted"`), so usage attribution and the
/// exhausted-queue fallback agree on identity. The id is namespaced by
/// demo (`scripted/<name>`) so the TUI footer's
/// `format!("{} @ {}", info.id, info.base_url)` rendering surfaces
/// which canned flow is running.
///
/// Reasoning flags are `false` (demos don't exercise provider
/// reasoning); modality is text-only; context/cost are zero (the
/// scripted path is free).
fn scripted_model_info(name: &str) -> ModelInfo {
    ModelInfo {
        id: format!("scripted/{name}"),
        name: format!("scripted: {name}"),
        api: "scripted".to_string(),
        provider: "scripted".to_string(),
        base_url: "scripted://internal".to_string(),
        reasoning: false,
        supports_xhigh: false,
        supports_adaptive_thinking: false,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
    }
}

/// Print the demo catalog (name + one-line summary) to stderr.
fn print_catalog() {
    eprintln!("available --scripted demos:");
    for (name, summary, _) in demos::catalog() {
        eprintln!("  {name:<24}  {summary}");
    }
}
