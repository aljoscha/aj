//! `--scripted <NAME>` flag plumbing.
//!
//! Both [`interactive`](crate::modes::interactive) and
//! [`print`](crate::modes::print) modes share the same resolution path:
//! given the user-supplied demo name, either look it up in
//! [`aj_models::scripted::demos`] and hand back a ready-to-plug-in
//! [`Provider`](aj_models::provider::Provider) handle, or — when the name
//! is unknown or the user passed the sentinel `help` / `list` / `?` — print
//! the catalog to stderr and exit so the caller can pick a real one.
//!
//! Internally the legacy [`ScriptedModel`] (still authored against the
//! legacy [`StreamingEvent`](aj_models::streaming::StreamingEvent) shape)
//! is wrapped in
//! [`LegacyProviderAdapter`](aj_models::compat::LegacyProviderAdapter)
//! so the same agent inference path covers both scripted and real-provider
//! runs. Step 6.8 of `docs/aj-next-progress.md` migrates this resolver onto
//! `ScriptedProvider` directly; until then the wrap stays as the only
//! place the legacy [`Model`](aj_models::Model) trait crosses into the
//! binary.
//!
//! The resolver is intentionally one-shot: it consumes the flag and
//! returns the resolved triple.

use std::sync::Arc;

use aj_models::Model;
use aj_models::compat::{LegacyProviderAdapter, synthetic_model_info};
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::scripted::demos;
use aj_models::scripted::{ExhaustedBehavior, ScriptedModel};
use anyhow::{Result, bail};

/// Resolved [`ScriptedModel`] in the same shape the real-provider path
/// returns from [`crate::model::resolve`]: a provider handle plus the
/// registry-style metadata stamped onto every inference.
pub struct ResolvedScriptedModel {
    pub provider: Arc<dyn Provider>,
    pub model_info: Arc<ModelInfo>,
}

/// Resolve the `--scripted <NAME>` argument into a provider handle and
/// synthesized [`ModelInfo`].
///
/// When `name` matches a demo, returns the corresponding
/// [`ScriptedModel`] wrapped in a [`LegacyProviderAdapter`] alongside the
/// synthesized [`ModelInfo`] that callers feed into
/// [`Agent::with_provider`](aj_agent::Agent::with_provider). When the
/// user explicitly asked for the catalog (`help` / `list` / `?` / empty
/// string), prints the listing to stderr and calls
/// [`std::process::exit(0)`]; for an unknown name, prints the listing
/// and returns an error so the binary exits with a non-zero status.
///
/// The returned model defaults to [`ExhaustedBehavior::EndTurn`] so the
/// user can keep chatting (and watch the empty fallback in the TUI)
/// after the canned script runs out, rather than panicking the
/// inference task.
pub fn resolve_or_explain(name: &str) -> Result<ResolvedScriptedModel> {
    if matches!(name, "help" | "list" | "?" | "") {
        print_catalog();
        std::process::exit(0);
    }

    if let Some(scripts) = demos::lookup(name) {
        let model: Arc<dyn Model> = Arc::new(
            ScriptedModel::new(scripts)
                .on_exhausted(ExhaustedBehavior::EndTurn)
                .with_name(format!("scripted/{name}"))
                .with_url("scripted://internal".to_string()),
        );
        let model_info = Arc::new(synthetic_model_info(&model));
        let provider: Arc<dyn Provider> = Arc::new(LegacyProviderAdapter::new(model));
        return Ok(ResolvedScriptedModel {
            provider,
            model_info,
        });
    }

    print_catalog();
    bail!("unknown demo for --scripted: {name:?}");
}

/// Print the demo catalog (name + one-line summary) to stderr.
fn print_catalog() {
    eprintln!("available --scripted demos:");
    for (name, summary, _) in demos::catalog() {
        eprintln!("  {name:<24}  {summary}");
    }
}
