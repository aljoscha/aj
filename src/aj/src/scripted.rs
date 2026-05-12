//! `--scripted <NAME>` flag plumbing.
//!
//! Both [`interactive`](crate::modes::interactive) and
//! [`print`](crate::modes::print) modes share the same resolution path:
//! given the user-supplied demo name, either look it up in
//! [`aj_models::scripted::demos`] and hand back a ready-to-plug-in
//! [`Model`](aj_models::Model) handle, or — when the name is unknown or
//! the user passed the sentinel `help` / `list` / `?` — print the catalog
//! to stderr and exit so the caller can pick a real one.
//!
//! The resolver is intentionally one-shot: it consumes the flag and
//! returns a model handle. Mode code only has to swap the call site that
//! would otherwise call [`aj_models::create_model`].

use std::sync::Arc;

use aj_models::Model;
use aj_models::scripted::demos;
use aj_models::scripted::{ExhaustedBehavior, ScriptedModel};
use anyhow::{Result, bail};

/// Resolve the `--scripted <NAME>` argument into a model handle.
///
/// When `name` matches a demo, returns the corresponding
/// [`ScriptedModel`]. When the user explicitly asked for the catalog
/// (`help` / `list` / `?` / empty string), prints the listing to stderr
/// and calls [`std::process::exit(0)`]; for an unknown name, prints the
/// listing and returns an error so the binary exits with a non-zero
/// status.
///
/// The returned model defaults to [`ExhaustedBehavior::EndTurn`] so the
/// user can keep chatting (and watch the empty fallback in the TUI) after
/// the canned script runs out, rather than panicking the inference task.
pub fn resolve_or_explain(name: &str) -> Result<Arc<dyn Model>> {
    if matches!(name, "help" | "list" | "?" | "") {
        print_catalog();
        std::process::exit(0);
    }

    if let Some(scripts) = demos::lookup(name) {
        let model = ScriptedModel::new(scripts)
            .on_exhausted(ExhaustedBehavior::EndTurn)
            .with_name(format!("scripted/{name}"))
            .with_url("scripted://internal".to_string());
        return Ok(Arc::new(model));
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
