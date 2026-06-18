//! Resolve a [`ModelSelection`] (the CLI > env > config merge of
//! `(api, model_name, url)`) into a ready-to-plug-in [`Provider`]
//! handle plus the registry-resolved [`ModelInfo`] and a baseline
//! [`StreamOptions`].
//!
//! The binary loads the
//! [`ModelRegistry`](aj_models::registry::ModelRegistry), picks a
//! concrete model (either an explicit `(provider, id)` pair or the
//! provider's preferred default), looks up the matching
//! [`Provider`] impl by the model's `api` string, and installs an
//! [`ApiKeyResolver`] backed by [`AuthStorage`]. The resulting bundle
//! is what
//! [`aj_agent::Agent::with_provider`] / [`aj_agent::Agent::set_provider`]
//! consume directly — no further conversion at the call site.
//!
//! Key resolution is **lazy**: rather than reading a key up front and
//! failing if none is set, the resolver is invoked by the provider
//! before each inference and walks the [`AuthStorage`] chain (runtime
//! `--api-key` override → env var → stored API key → stored OAuth,
//! auto-refreshing). This lets a session start without credentials
//! (e.g. so the user can log in later) and lets a mid-session login
//! take effect on the next turn without a restart.

use std::sync::Arc;

use aj_conf::{Config, ConfigThinkingDisplay, ConfigThinkingLevel};
use aj_models::ThinkingConfig;
use aj_models::auth::{AuthStorage, find_env_keys};
use aj_models::provider::{Provider, provider_for};
use aj_models::registry::{ModelInfo, ModelRegistry};
use aj_models::types::{ApiKeyResolver, ReasoningSummary, Speed, StreamOptions, ThinkingDisplay};
use anyhow::{Result, anyhow};

use crate::cli::args::Args;

/// Fallback provider id used when neither CLI / env / config supplies
/// one. Anthropic is the default so existing user setups keep
/// working without an explicit `MODEL_API=anthropic` env var.
pub const DEFAULT_PROVIDER_ID: &str = "anthropic";

/// Preferred default model per provider, used when the user hasn't
/// pinned a `model_name`. This is selection *policy* and deliberately
/// lives here rather than in the catalog: `models.json` is provider
/// data refreshed by `aj models update`, whereas "which model we reach
/// for by default" is ours to decide.
///
/// A provider absent from this table — or one whose preferred id isn't
/// in the current catalog — falls back to its first listed entry, so a
/// catalog refresh that drops or renames the preferred model degrades
/// gracefully instead of erroring.
const PREFERRED_DEFAULT_MODELS: &[(&str, &str)] = &[("anthropic", "claude-opus-4-8")];

/// The model-selection triple after applying CLI > env > config
/// precedence. [`merge`](ModelSelection::merge) is the single place
/// that overlay lives. The fields are the post-merge `(api, name,
/// url)` the registry lookup consumes.
pub struct ModelSelection {
    /// Provider id (catalog `provider`, e.g. `"anthropic"`). `None`
    /// defers to [`DEFAULT_PROVIDER_ID`] via
    /// [`provider_id`](ModelSelection::provider_id).
    pub api: Option<String>,
    /// Model id within the provider's catalog. `None` picks the
    /// provider's preferred default.
    pub name: Option<String>,
    /// Base-URL override applied after lookup.
    pub url: Option<String>,
}

impl ModelSelection {
    /// Overlay CLI flags over config values. `args.model_*` is already
    /// post-env because clap populates it from the `MODEL_*` env vars
    /// at parse time, so this realizes the full CLI > env > config
    /// precedence in one place.
    pub fn merge(args: &Args, config: &Config) -> ModelSelection {
        ModelSelection {
            api: args.model_api.clone().or_else(|| config.model_api.clone()),
            name: args
                .model_name
                .clone()
                .or_else(|| config.model_name.clone()),
            url: args.model_url.clone().or_else(|| config.model_url.clone()),
        }
    }

    /// Provider id with the [`DEFAULT_PROVIDER_ID`] fallback applied.
    /// Used both for the registry lookup and as the `--api-key` /
    /// credential-resolution target.
    pub fn provider_id(&self) -> &str {
        self.api.as_deref().unwrap_or(DEFAULT_PROVIDER_ID)
    }
}

/// A model handle assembled by [`resolve`] (or [`from_model_info`])
/// ready to plug into [`aj_agent::Agent::with_provider`] or
/// [`aj_agent::Agent::set_provider`].
pub struct ResolvedModel {
    /// Provider implementation matching `model_info.api`. Stateless,
    /// safe to clone across sub-agents.
    pub provider: Arc<dyn Provider>,
    /// Registry-resolved metadata for the picked model. Carries the
    /// catalog `id`, `provider` id, `base_url`, capability flags,
    /// and pricing tables.
    pub model_info: Arc<ModelInfo>,
    /// Baseline [`StreamOptions`] applied to every inference call:
    /// resolved API key plus the unified knobs the binary translates
    /// from config (e.g. [`StreamOptions::speed`]). The provider turns
    /// these into provider-specific wire fields; the agent layers
    /// per-turn reasoning on top.
    pub stream_options: StreamOptions,
}

/// Build a [`ResolvedModel`] from a merged [`ModelSelection`].
///
/// `selection.provider_id()` is the catalog `provider` value (with
/// the [`DEFAULT_PROVIDER_ID`] fallback). `selection.name` selects an
/// entry from the provider's catalog; when [`None`] the helper picks
/// the provider's preferred default (see [`PREFERRED_DEFAULT_MODELS`]),
/// falling back to the first listed entry. The registry preserves
/// insertion order, so the fallback is deterministic given a fixed
/// catalog.
///
/// `selection.url` replaces `model_info.base_url` after lookup so a
/// caller can point at a staging proxy or a self-hosted endpoint
/// without editing the catalog file.
///
/// `speed` records the inference speed mode on the baseline
/// [`StreamOptions`]; the provider decides what (if anything) it means
/// on the wire. Anthropic maps `Fast` onto a request-body field plus a
/// beta header; other providers ignore it. The Speed enum lives in
/// `aj-models` because it's plumbed through the Anthropic SDK wire
/// types as well.
pub fn resolve(
    registry: &ModelRegistry,
    auth: &AuthStorage,
    selection: &ModelSelection,
    speed: Option<Speed>,
) -> Result<ResolvedModel> {
    let mut model_info = pick_model(registry, selection.provider_id(), selection.name.as_deref())?;
    if let Some(url) = &selection.url {
        // A custom URL trumps the catalog default, but everything else
        // (capability flags, pricing) stays sourced from the registry.
        model_info.base_url = url.clone();
    }
    from_model_info(auth, model_info, speed)
}

/// Build a [`ResolvedModel`] from a pre-picked [`ModelInfo`] — used by
/// the `/model` selector which already has the catalog row in hand.
///
/// Same effect as [`resolve`] minus the lookup: dispatch the
/// `model_info.api` to the matching [`Provider`] impl, install an
/// [`AuthStorage`]-backed [`ApiKeyResolver`], and record the [`Speed`]
/// on the baseline [`StreamOptions`] for the provider to interpret.
pub fn from_model_info(
    auth: &AuthStorage,
    model_info: ModelInfo,
    speed: Option<Speed>,
) -> Result<ResolvedModel> {
    let provider = provider_for(&model_info.api).ok_or_else(|| {
        anyhow!(
            "no provider registered for api {:?} (model {}/{})",
            model_info.api,
            model_info.provider,
            model_info.id,
        )
    })?;

    let mut stream_options = StreamOptions::default();
    install_api_key_resolver(&mut stream_options, auth, &model_info.provider);
    stream_options.speed = speed;

    Ok(ResolvedModel {
        provider: Arc::from(provider),
        model_info: Arc::new(model_info),
        stream_options,
    })
}

/// Install an [`ApiKeyResolver`] on `options` that resolves
/// `provider_id`'s bearer token through [`AuthStorage`] on every
/// inference.
///
/// Cloning the [`AuthStorage`] is cheap (it's `Arc`-backed) and the
/// resolver closure is `Fn`, so the provider can call it repeatedly
/// across a long-running session — each call re-walks the resolution
/// chain and refreshes an expired OAuth token under the storage's
/// cross-process file lock. A missing credential is surfaced as a
/// human-readable error (the provider maps it to an `Auth`-category
/// failure) rather than a hard startup bail, so a session can come up
/// uncredentialed and the user can log in later.
fn install_api_key_resolver(options: &mut StreamOptions, auth: &AuthStorage, provider_id: &str) {
    let auth = auth.clone();
    let provider_id = provider_id.to_string();
    options.set_api_key_resolver(Some(ApiKeyResolver::new(move || {
        let auth = auth.clone();
        let provider_id = provider_id.clone();
        async move {
            match auth.get_api_key(&provider_id).await {
                Ok(Some(key)) => Ok(key),
                Ok(None) => Err(missing_key_message(&provider_id)),
                Err(err) => Err(format!(
                    "failed to resolve credentials for {provider_id:?}: {err}"
                )),
            }
        }
    })));
}

/// Human-readable "no credential" message naming the env vars we'd
/// have consulted and pointing at the interactive login flow. Used
/// both by the resolver (per-request failure text) and by the
/// startup auth check.
pub fn missing_key_message(provider_id: &str) -> String {
    let vars = find_env_keys(provider_id);
    if vars.is_empty() {
        format!(
            "no credentials for provider {provider_id:?}; log in from the \
             command palette (press /), or set a `headers` override in models.json"
        )
    } else {
        format!(
            "no credentials for provider {provider_id:?}; log in from the \
             command palette (press /) or set one of: {}",
            vars.join(", "),
        )
    }
}

/// Pick a [`ModelInfo`] for the given `(provider, model)` pair.
///
/// Errors with a structured message if the provider is unknown or
/// the named model isn't in its listing — the caller wraps the
/// result with extra context (CLI flag, config key) before showing
/// it to the user.
fn pick_model(
    registry: &ModelRegistry,
    provider_id: &str,
    model_id: Option<&str>,
) -> Result<ModelInfo> {
    match model_id {
        Some(id) => registry
            .get(provider_id, id)
            .cloned()
            .ok_or_else(|| anyhow!("model {provider_id}/{id} not found in registry")),
        None => default_model_for(registry, provider_id)
            .cloned()
            .ok_or_else(|| anyhow!("no models listed for provider {provider_id:?}")),
    }
}

/// Pick the default model for `provider_id` when the user hasn't named
/// one.
///
/// Prefers the provider's entry in [`PREFERRED_DEFAULT_MODELS`] when
/// that id is present in the catalog, otherwise falls back to the first
/// listed model. The registry preserves catalog insertion order, so the
/// fallback is deterministic given a fixed catalog.
fn default_model_for<'a>(registry: &'a ModelRegistry, provider_id: &str) -> Option<&'a ModelInfo> {
    // Honor the preferred-default policy only when the model still
    // exists in the catalog; a refresh that drops it must not break
    // startup.
    let preferred = PREFERRED_DEFAULT_MODELS
        .iter()
        .find(|(provider, _)| *provider == provider_id)
        .and_then(|(_, id)| registry.get(provider_id, id));
    preferred.or_else(|| registry.models(provider_id).into_iter().next())
}

/// Fan the configured [`ConfigThinkingDisplay`] (if any) out onto
/// both provider-specific wire fields on [`StreamOptions`]: Anthropic
/// consumes `thinking_display`, OpenAI Responses consumes
/// `reasoning_summary`, and each ignores the other. The mapping
/// table lives on [`ConfigThinkingDisplay`]'s doc comment.
///
/// This single helper preserves a one-knob user experience while
/// keeping the wire layer's two-field separation — the latter is
/// load-bearing because the two providers' reasoning APIs are
/// genuinely different axes (visibility vs. verbosity), and merging
/// them at the wire level would lose information.
pub fn apply_thinking_display(options: &mut StreamOptions, display: Option<ConfigThinkingDisplay>) {
    let Some(display) = display else {
        options.thinking_display = None;
        options.reasoning_summary = None;
        return;
    };
    let (anthropic, openai) = match display {
        ConfigThinkingDisplay::Summarized => (
            Some(ThinkingDisplay::Summarized),
            Some(ReasoningSummary::Concise),
        ),
        ConfigThinkingDisplay::Detailed => (
            // Anthropic adaptive has no Detailed variant; degrade to
            // Summarized so the user still gets *some* visible
            // reasoning rather than a silent fallback to the
            // provider default.
            Some(ThinkingDisplay::Summarized),
            Some(ReasoningSummary::Detailed),
        ),
        ConfigThinkingDisplay::Omitted => (
            Some(ThinkingDisplay::Omitted),
            // OpenAI Responses has no "must suppress" knob — leaving
            // `reasoning_summary` unset means we don't request a
            // summary, which is the closest analogue.
            None,
        ),
    };
    options.thinking_display = anthropic;
    options.reasoning_summary = openai;
}

/// Map a `config.toml` thinking level onto the wire-level
/// [`ThinkingConfig`] the agent runs with. [`ConfigThinkingLevel::Off`]
/// collapses to `None` (no reasoning requested), so the result type is
/// the same `Option` the agent's `set_default_thinking` takes.
pub fn default_thinking_from_config(level: Option<ConfigThinkingLevel>) -> Option<ThinkingConfig> {
    level.and_then(|level| match level {
        ConfigThinkingLevel::Off => None,
        ConfigThinkingLevel::Low => Some(ThinkingConfig::Low),
        ConfigThinkingLevel::Medium => Some(ThinkingConfig::Medium),
        ConfigThinkingLevel::High => Some(ThinkingConfig::High),
        ConfigThinkingLevel::XHigh => Some(ThinkingConfig::XHigh),
        ConfigThinkingLevel::Max => Some(ThinkingConfig::Max),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_models::registry::{Catalog, InputModality, ModelCost, OverridesFile};

    fn sample_model(provider: &str, id: &str, api: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            api: api.into(),
            provider: provider.into(),
            base_url: "https://example.invalid".into(),
            reasoning: false,
            supports_adaptive_thinking: false,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 1_000,
            max_tokens: 100,
            headers: None,
        }
    }

    fn registry(models: Vec<ModelInfo>) -> ModelRegistry {
        // Build a tiny in-memory registry so we don't reach for the
        // bundled catalog (which would include unrelated providers
        // and make the per-provider defaults non-deterministic).
        let catalog = Catalog {
            updated_at: 0,
            source: "test".into(),
            models,
        };
        let overrides = OverridesFile { overrides: vec![] };
        ModelRegistry::from_catalog_with_overrides(catalog, overrides, "test-catalog")
    }

    #[test]
    fn pick_model_returns_explicit_match() {
        let reg = registry(vec![
            sample_model("anthropic", "claude-x", "anthropic-messages"),
            sample_model("anthropic", "claude-y", "anthropic-messages"),
        ]);
        let m = pick_model(&reg, "anthropic", Some("claude-y")).expect("found");
        assert_eq!(m.id, "claude-y");
    }

    #[test]
    fn pick_model_falls_back_to_first_listed() {
        let reg = registry(vec![
            sample_model("anthropic", "claude-x", "anthropic-messages"),
            sample_model("anthropic", "claude-y", "anthropic-messages"),
        ]);
        let m = pick_model(&reg, "anthropic", None).expect("found");
        // The preferred default for anthropic isn't in this catalog, so
        // selection falls back to the first listed entry. Catalog order
        // is preserved by `from_catalog_with_overrides`, so that's
        // deterministic.
        assert_eq!(m.id, "claude-x");
    }

    #[test]
    fn pick_model_prefers_provider_default_when_present() {
        // The preferred default is honored even when it isn't first in
        // the catalog. "claude-opus-4-8" is anthropic's entry in
        // `PREFERRED_DEFAULT_MODELS`.
        let reg = registry(vec![
            sample_model("anthropic", "claude-x", "anthropic-messages"),
            sample_model("anthropic", "claude-opus-4-8", "anthropic-messages"),
        ]);
        let m = pick_model(&reg, "anthropic", None).expect("found");
        assert_eq!(m.id, "claude-opus-4-8");
    }

    #[test]
    fn pick_model_errors_for_unknown_provider() {
        let reg = registry(vec![sample_model(
            "anthropic",
            "claude-x",
            "anthropic-messages",
        )]);
        let err = pick_model(&reg, "no-such", None).expect_err("error");
        assert!(err.to_string().contains("no models listed"), "{err}");
    }

    #[test]
    fn pick_model_errors_for_unknown_model_in_known_provider() {
        let reg = registry(vec![sample_model(
            "anthropic",
            "claude-x",
            "anthropic-messages",
        )]);
        let err = pick_model(&reg, "anthropic", Some("claude-z")).expect_err("error");
        assert!(err.to_string().contains("not found in registry"), "{err}");
    }

    #[test]
    fn apply_thinking_display_unset_clears_both_wire_fields() {
        // Pre-seed both fields to make sure the helper actively
        // clears them, not just leaves an unset default alone.
        let mut opts = StreamOptions {
            thinking_display: Some(ThinkingDisplay::Summarized),
            reasoning_summary: Some(ReasoningSummary::Concise),
            ..StreamOptions::default()
        };
        apply_thinking_display(&mut opts, None);
        assert!(opts.thinking_display.is_none());
        assert!(opts.reasoning_summary.is_none());
    }

    #[test]
    fn apply_thinking_display_summarized_fans_out_to_both_providers() {
        let mut opts = StreamOptions::default();
        apply_thinking_display(&mut opts, Some(ConfigThinkingDisplay::Summarized));
        assert!(matches!(
            opts.thinking_display,
            Some(ThinkingDisplay::Summarized)
        ));
        assert!(matches!(
            opts.reasoning_summary,
            Some(ReasoningSummary::Concise)
        ));
    }

    #[test]
    fn apply_thinking_display_detailed_degrades_anthropic_to_summarized() {
        // Anthropic adaptive has no Detailed variant; the user
        // still gets a visible summary instead of falling back to
        // the provider default.
        let mut opts = StreamOptions::default();
        apply_thinking_display(&mut opts, Some(ConfigThinkingDisplay::Detailed));
        assert!(matches!(
            opts.thinking_display,
            Some(ThinkingDisplay::Summarized)
        ));
        assert!(matches!(
            opts.reasoning_summary,
            Some(ReasoningSummary::Detailed)
        ));
    }

    #[test]
    fn apply_thinking_display_omitted_clears_openai_summary() {
        // OpenAI Responses has no "must suppress" knob; the
        // closest analogue is to not request a summary at all.
        let mut opts = StreamOptions {
            reasoning_summary: Some(ReasoningSummary::Auto),
            ..StreamOptions::default()
        };
        apply_thinking_display(&mut opts, Some(ConfigThinkingDisplay::Omitted));
        assert!(matches!(
            opts.thinking_display,
            Some(ThinkingDisplay::Omitted)
        ));
        assert!(opts.reasoning_summary.is_none());
    }

    #[test]
    fn model_selection_cli_overrides_config() {
        use clap::Parser;
        let args = Args::parse_from(["aj", "--model-api", "openai", "--model-name", "gpt-x"]);
        let config = Config {
            model_api: Some("anthropic".to_string()),
            model_name: Some("claude-x".to_string()),
            model_url: Some("https://config.example".to_string()),
            ..Config::default()
        };
        let sel = ModelSelection::merge(&args, &config);
        assert_eq!(sel.api.as_deref(), Some("openai"));
        assert_eq!(sel.name.as_deref(), Some("gpt-x"));
        // No `--model-url` on the CLI, so it falls back to config.
        assert_eq!(sel.url.as_deref(), Some("https://config.example"));
        assert_eq!(sel.provider_id(), "openai");
    }

    #[test]
    fn model_selection_falls_back_to_config_then_default() {
        use clap::Parser;
        let args = Args::parse_from(["aj"]);
        let config = Config {
            model_name: Some("claude-x".to_string()),
            ..Config::default()
        };
        let sel = ModelSelection::merge(&args, &config);
        assert!(sel.api.is_none());
        assert_eq!(sel.name.as_deref(), Some("claude-x"));
        // No provider anywhere falls back to the built-in default.
        assert_eq!(sel.provider_id(), DEFAULT_PROVIDER_ID);
    }
}
