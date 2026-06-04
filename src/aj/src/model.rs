//! Resolve a CLI / config triple `(api, model_name, url)` into a
//! ready-to-plug-in [`Provider`] handle plus the
//! registry-resolved [`ModelInfo`] and a baseline [`StreamOptions`].
//!
//! The binary loads the
//! [`ModelRegistry`](aj_models::registry::ModelRegistry), picks a
//! concrete model (either an explicit `(provider, id)` pair or the
//! provider's first listed entry), looks up the matching
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
//! (e.g. so the user can run `/login`) and lets a mid-session login
//! take effect on the next turn without a restart.

use std::collections::HashMap;
use std::sync::Arc;

use aj_conf::ConfigThinkingDisplay;
use aj_models::auth::{AuthStorage, find_env_keys};
use aj_models::provider::{Provider, provider_for};
use aj_models::registry::{ModelInfo, ModelRegistry};
use aj_models::types::{ApiKeyResolver, ReasoningSummary, Speed, StreamOptions, ThinkingDisplay};
use anyhow::{Result, anyhow};

/// Beta header value that opts an Anthropic request into the
/// fast-inference variant of the model. Stamped onto the outgoing
/// request via the `anthropic-beta` header when the user set
/// `--speed fast` (or the equivalent config key) and the resolved
/// provider is `anthropic-messages`.
///
/// Kept inline here rather than re-exported from `aj-models` because
/// the routing decision (Speed → beta header) is a binary-level
/// policy: the provider crate stays generic over the headers list
/// and the binary picks which extras to opt into per user request.
pub const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";

/// Fallback provider id used when neither CLI / env / config supplies
/// one. Anthropic is the default so existing user setups keep
/// working without an explicit `MODEL_API=anthropic` env var.
pub const DEFAULT_PROVIDER_ID: &str = "anthropic";

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
    /// resolved API key plus any extra HTTP headers (e.g. the fast-
    /// inference beta). The agent layers per-turn reasoning on top.
    pub stream_options: StreamOptions,
}

/// Build a [`ResolvedModel`] from a CLI / config triple.
///
/// `provider_id` is the catalog `provider` value (e.g. `"anthropic"`,
/// `"openai"`, `"openai-codex"`). When [`None`] the helper falls back
/// to [`DEFAULT_PROVIDER_ID`] so an unconfigured run still works.
///
/// `model_id` selects an entry from the provider's catalog. When
/// [`None`] the helper picks the first listed model for that
/// provider; the registry preserves insertion order so the choice is
/// deterministic given a fixed catalog.
///
/// `url_override` replaces `model_info.base_url` after lookup so a
/// caller can point at a staging proxy or a self-hosted endpoint
/// without editing the catalog file.
///
/// `speed` opts into provider-specific fast-inference variants by
/// stamping extra HTTP headers on every request. Today only
/// `anthropic-messages` participates (via the `anthropic-beta`
/// header); other providers ignore the field. The Speed enum lives
/// in `aj-models` because it's plumbed through the Anthropic SDK
/// wire types as well.
pub fn resolve(
    registry: &ModelRegistry,
    auth: &AuthStorage,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    url_override: Option<&str>,
    speed: Option<Speed>,
) -> Result<ResolvedModel> {
    let provider_id = provider_id.unwrap_or(DEFAULT_PROVIDER_ID);
    let mut model_info = pick_model(registry, provider_id, model_id)?;
    if let Some(url) = url_override {
        // Mirrors the legacy `ModelArgs::url` precedence: a custom
        // URL trumps the catalog default but everything else
        // (capability flags, pricing) stays sourced from the
        // registry.
        model_info.base_url = url.to_string();
    }
    from_model_info(auth, model_info, speed)
}

/// Build a [`ResolvedModel`] from a pre-picked [`ModelInfo`] — used by
/// the `/model` selector which already has the catalog row in hand.
///
/// Same effect as [`resolve`] minus the lookup: dispatch the
/// `model_info.api` to the matching [`Provider`] impl, install an
/// [`AuthStorage`]-backed [`ApiKeyResolver`], and stamp speed-driven
/// headers onto the baseline [`StreamOptions`].
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
    apply_speed_headers(&mut stream_options, &model_info, speed);

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
/// uncredentialed and the user can `/login`.
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
            "no credentials for provider {provider_id:?}; run /login, \
             or set a `headers` override in models.json"
        )
    } else {
        format!(
            "no credentials for provider {provider_id:?}; run /login \
             or set one of: {}",
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

/// Pick the first model the registry knows about for `provider_id`.
///
/// The registry preserves insertion order from the catalog, so the
/// "first" entry is whatever ships at the top of the bundled
/// `models.json` (or the user's refreshed cache). A richer
/// "preferred default per provider" hook is reserved for the §4.3
/// SettingsManager work; for now the catalog order is the source of
/// truth.
fn default_model_for<'a>(registry: &'a ModelRegistry, provider_id: &str) -> Option<&'a ModelInfo> {
    registry.models(provider_id).into_iter().next()
}

/// Apply per-call HTTP headers derived from the [`Speed`] knob.
///
/// Today the only consumer is `anthropic-messages`, which accepts
/// the `fast-mode-2026-02-01` beta header for opt-in low-latency
/// inference. Other providers see the speed flag but ignore the
/// headers (we still set them — the providers strip what they don't
/// know how to handle).
fn apply_speed_headers(options: &mut StreamOptions, model: &ModelInfo, speed: Option<Speed>) {
    match speed {
        Some(Speed::Fast) if model.api == "anthropic-messages" => {
            let headers = options.headers.get_or_insert_with(HashMap::new);
            add_beta_header(headers, "anthropic-beta", FAST_MODE_BETA);
        }
        // Standard / unset / non-Anthropic providers: nothing to
        // stamp. Future fast-mode equivalents on other providers
        // plug in here.
        _ => {}
    }
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

/// Append `value` to a comma-separated `name` header in `headers`
/// without duplicating it. If the header isn't set yet, create it.
///
/// Anthropic accepts comma-separated values for `anthropic-beta`,
/// and the SDK already coalesces caller-supplied betas with its own
/// (e.g. `fine-grained-tool-streaming-2025-05-14`), so the binary's
/// extension here only has to worry about the user-facing axis
/// (Speed::Fast → fast-mode beta).
fn add_beta_header(headers: &mut HashMap<String, String>, name: &str, value: &str) {
    headers
        .entry(name.to_string())
        .and_modify(|existing| {
            if !existing.split(',').any(|b| b.trim() == value) {
                if !existing.is_empty() {
                    existing.push(',');
                }
                existing.push_str(value);
            }
        })
        .or_insert_with(|| value.to_string());
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
        // Insertion order: catalog order is preserved by
        // `from_catalog_with_overrides`, so the first listed entry
        // wins. The Codex seed is spliced in after but only injects
        // openai-codex models, so the anthropic-first stays stable.
        assert_eq!(m.id, "claude-x");
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
    fn apply_speed_headers_skips_when_speed_unset() {
        let mut opts = StreamOptions::default();
        let model = sample_model("anthropic", "claude-x", "anthropic-messages");
        apply_speed_headers(&mut opts, &model, None);
        assert!(opts.headers.is_none());
    }

    #[test]
    fn apply_speed_headers_skips_standard_speed() {
        let mut opts = StreamOptions::default();
        let model = sample_model("anthropic", "claude-x", "anthropic-messages");
        apply_speed_headers(&mut opts, &model, Some(Speed::Standard));
        assert!(opts.headers.is_none());
    }

    #[test]
    fn apply_speed_headers_stamps_fast_mode_beta_on_anthropic() {
        let mut opts = StreamOptions::default();
        let model = sample_model("anthropic", "claude-x", "anthropic-messages");
        apply_speed_headers(&mut opts, &model, Some(Speed::Fast));
        let headers = opts.headers.expect("headers populated");
        assert_eq!(
            headers.get("anthropic-beta").map(String::as_str),
            Some(FAST_MODE_BETA)
        );
    }

    #[test]
    fn apply_speed_headers_is_noop_for_non_anthropic_providers() {
        let mut opts = StreamOptions::default();
        let model = sample_model("openai", "gpt-x", "openai-responses");
        apply_speed_headers(&mut opts, &model, Some(Speed::Fast));
        assert!(opts.headers.is_none());
    }

    #[test]
    fn add_beta_header_creates_when_missing() {
        let mut headers = HashMap::new();
        add_beta_header(&mut headers, "anthropic-beta", "alpha-1");
        assert_eq!(
            headers.get("anthropic-beta").map(String::as_str),
            Some("alpha-1")
        );
    }

    #[test]
    fn add_beta_header_appends_to_existing_value() {
        let mut headers = HashMap::new();
        headers.insert("anthropic-beta".into(), "existing-1".into());
        add_beta_header(&mut headers, "anthropic-beta", "alpha-1");
        assert_eq!(
            headers.get("anthropic-beta").map(String::as_str),
            Some("existing-1,alpha-1"),
        );
    }

    #[test]
    fn add_beta_header_skips_duplicate() {
        let mut headers = HashMap::new();
        headers.insert("anthropic-beta".into(), "alpha-1,beta-2".into());
        add_beta_header(&mut headers, "anthropic-beta", "alpha-1");
        assert_eq!(
            headers.get("anthropic-beta").map(String::as_str),
            Some("alpha-1,beta-2"),
        );
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
}
