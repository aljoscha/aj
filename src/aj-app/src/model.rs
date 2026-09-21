//! Resolve a [`ModelSelection`] (the CLI > env > config merge of
//! `(api, model_name, url)`) into a ready-to-plug-in [`Provider`]
//! handle plus the registry-resolved [`ModelInfo`] and a baseline
//! [`StreamOptions`].
//!
//! The binary loads the
//! [`ModelRegistry`](aj_models::registry::ModelRegistry), picks a
//! concrete `(provider, id)` pair from the effective configuration, looks up
//! the matching
//! [`Provider`] impl by the model's `api` string, and installs an
//! [`ApiKeyResolver`] backed by [`AuthStorage`]. The resulting bundle
//! is what
//! [`aj_agent::Agent::with_provider`] / [`aj_agent::Agent::set_provider`]
//! consume directly — no further conversion at the call site.
//!
//! Key resolution is **lazy**: rather than reading a key up front and
//! failing if none is set, the resolver is invoked by the provider
//! before each inference and walks the [`AuthStorage`] chain (runtime
//! `--api-key` override → stored API key → stored OAuth → env var,
//! auto-refreshing). This lets a session start without credentials
//! (e.g. so the user can log in later) and lets a mid-session login
//! take effect on the next turn without a restart.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use aj_conf::{Config, ConfigThinkingDisplay, ConfigThinkingLevel, ConfigVerbosity};
use aj_models::ThinkingConfig;
use aj_models::auth::{AuthStorage, find_env_keys};
use aj_models::provider::{Provider, provider_for};
use aj_models::registry::{ModelInfo, ModelRegistry};
use aj_models::types::{
    ApiKeyResolver, ReasoningSummary, ResolvedApiKey, Speed, StreamOptions, ThinkingDisplay,
    Verbosity,
};
use anyhow::{Context, Result, anyhow};

use crate::cli::args::Args;

/// Provider-local account choices shared by this session's inference resolvers.
///
/// A missing provider follows its live store default. An empty label pins the
/// unnamed credential. Requests copy their choice before resolving credentials,
/// so edits affect subsequent requests without changing an in-flight request.
#[derive(Clone, Default)]
pub struct SessionAccounts(Arc<RwLock<BTreeMap<String, String>>>);

impl SessionAccounts {
    pub fn snapshot(&self) -> BTreeMap<String, String> {
        self.0.read().expect("session accounts poisoned").clone()
    }

    pub fn get(&self, provider: &str) -> Option<String> {
        self.0
            .read()
            .expect("session accounts poisoned")
            .get(provider)
            .cloned()
    }

    pub fn set(&self, provider: &str, account: Option<String>) {
        let mut choices = self.0.write().expect("session accounts poisoned");
        match account {
            Some(account) => {
                choices.insert(provider.to_string(), account);
            }
            None => {
                choices.remove(provider);
            }
        }
    }

    pub fn replace(&self, accounts: BTreeMap<String, String>) {
        *self.0.write().expect("session accounts poisoned") = accounts;
    }

    /// Bind a bundle to this session, including bundles retained by subagents.
    pub fn install(&self, options: &mut StreamOptions, auth: &AuthStorage, provider: &str) {
        let choices = self.clone();
        let auth = auth.clone();
        let provider = provider.to_string();
        options.set_api_key_resolver(Some(ApiKeyResolver::new(move || {
            let auth = auth.clone();
            let account = choices.get(&provider);
            let provider = provider.clone();
            async move {
                match auth.get_api_key(&provider, account.as_deref()).await {
                    Ok(Some(resolved)) => Ok(ResolvedApiKey {
                        key: resolved.key,
                        account: resolved.source.label().map(str::to_string),
                    }),
                    Ok(None) => Err(match account {
                        Some(label) => format!(
                            "No credential for {provider} account {}. Use /account to choose another account, or /login to authenticate it.",
                            if label.is_empty() { "(unnamed)".to_string() } else { format!("{label:?}") }
                        ),
                        None => missing_key_message(&provider),
                    }),
                    Err(err) => Err(format!("failed to resolve credentials for {provider:?}: {err}")),
                }
            }
        })));
    }
}

/// Validate an explicit account gesture without refreshing or reading secrets
/// outside the host. Restoring a saved choice does not call this: a missing
/// restored credential must leave the session open for account selection.
pub async fn validate_account_selection(
    auth: &AuthStorage,
    provider: &str,
    account: Option<&str>,
) -> Result<(), String> {
    if provider.is_empty() {
        return Err("An account choice must name a provider.".to_string());
    }
    if auth.has_runtime_override(provider).await {
        return Err(format!(
            "{provider} uses --api-key. Account selection cannot take effect while that override is set."
        ));
    }
    if let Some(label) = account
        && auth
            .get_account(provider, label)
            .await
            .map_err(|err| err.to_string())?
            .is_none()
    {
        return Err(format!(
            "No stored {provider} account {label:?}. Use /login to add it, or /account to choose another account."
        ));
    }
    Ok(())
}

/// The model-selection triple after applying CLI > env > config
/// precedence. [`merge`](ModelSelection::merge) is the single place
/// that overlay lives. The fields are the post-merge `(api, name,
/// url)` the registry lookup consumes.
pub struct ModelSelection {
    /// Provider id (catalog `provider`, e.g. `"anthropic"`). Required for resolution.
    pub api: Option<String>,
    /// Model id within the provider's catalog. Required for resolution.
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

    /// The selected provider id, without inventing one for an incomplete selection.
    pub fn provider_id(&self) -> Result<&str> {
        self.api
            .as_deref()
            .context("model provider is not configured")
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
/// The provider and model name must both be supplied. An unavailable or
/// incomplete selection is an error, never a request for another catalog entry.
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
    let provider = selection.provider_id()?;
    let name = selection
        .name
        .as_deref()
        .context("model name is not configured")?;
    let mut model_info = registry.get(provider, name).cloned().ok_or_else(|| {
        anyhow!(
            "model {provider}/{name} not found in registry; run `aj models update` \
         or configure the provider and model name together",
        )
    })?;
    if let Some(url) = &selection.url {
        validate_model_url(url)?;
        // A custom URL trumps the catalog default, but everything else
        // (capability flags, pricing) stays sourced from the registry.
        model_info.base_url = url.clone();
    }
    from_model_info(auth, model_info, speed)
}

/// Reject endpoint overrides that cannot address an HTTP provider.
pub(crate) fn validate_model_url(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url).map_err(|err| anyhow!("invalid model url {url:?}: {err}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return Err(anyhow!(
            "model url {url:?} must be an absolute http or https URL"
        ));
    }
    Ok(())
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
    install_api_key_resolver(&mut stream_options, auth, &model_info.provider, None);
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
/// across a long-running session. Each call re-walks the resolution
/// chain and refreshes an expired OAuth token under the storage's
/// cross-process file lock. `account` names a provider-local label.
/// `None` asks the store for its default and the resolver returns the
/// label that default actually resolved to. A missing credential is
/// surfaced as a human-readable error (the provider maps it to an
/// `Auth`-category failure) rather than a hard startup bail, so a
/// session can come up uncredentialed and the user can log in later.
fn install_api_key_resolver(
    options: &mut StreamOptions,
    auth: &AuthStorage,
    provider_id: &str,
    account: Option<&str>,
) {
    let choices = SessionAccounts::default();
    choices.set(provider_id, account.map(str::to_string));
    choices.install(options, auth, provider_id);
}

/// Human-readable "no credential" message naming the env vars we'd
/// have consulted and pointing at the interactive login flow. Used
/// both by the resolver (per-request failure text) and by the
/// startup auth check.
pub fn missing_key_message(provider_id: &str) -> String {
    let vars = find_env_keys(provider_id);
    if vars.is_empty() {
        format!(
            "no credentials for provider {provider_id:?}; log in from the command palette (press /)"
        )
    } else {
        format!(
            "no credentials for provider {provider_id:?}; log in from the \
             command palette (press /) or set one of: {}",
            vars.join(", "),
        )
    }
}

/// Report known credential problems from the inference credential store.
/// Pass false for runs that do not use credentials, such as scripted fixtures.
pub(crate) async fn credential_warning(
    auth: &AuthStorage,
    provider: &str,
    credentials_required: bool,
) -> Option<String> {
    if !credentials_required {
        return None;
    }
    match auth.try_has_auth(provider).await {
        Ok(Some(true)) | Ok(None) => None,
        Ok(Some(false)) => Some(format!(
            "Heads up: {}",
            crate::model::missing_key_message(provider)
        )),
        Err(err) => Some(format!(
            "Couldn't check credentials for {provider:?}: {err}"
        )),
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

/// Map a `config.toml` verbosity value onto the unified wire enum.
pub fn config_verbosity_to_unified(verbosity: ConfigVerbosity) -> Verbosity {
    match verbosity {
        ConfigVerbosity::Low => Verbosity::Low,
        ConfigVerbosity::Medium => Verbosity::Medium,
        ConfigVerbosity::High => Verbosity::High,
    }
}

/// Apply the configured output verbosity (if any) onto `options`.
/// `None` clears the field so the provider sends no `text.verbosity`
/// and the server default applies. Providers gate the field on
/// per-model support, so this is a no-op for models and providers
/// that don't honour verbosity.
pub fn apply_verbosity(options: &mut StreamOptions, verbosity: Option<ConfigVerbosity>) {
    options.verbosity = verbosity.map(config_verbosity_to_unified);
}

/// Map a `config.toml` thinking level onto the wire-level
/// [`ThinkingConfig`] the agent runs with. [`ConfigThinkingLevel::Off`]
/// collapses to `None` (no reasoning requested), so the result type is
/// the same `Option` the agent's `set_default_thinking` takes.
pub fn default_thinking_from_config(level: Option<ConfigThinkingLevel>) -> Option<ThinkingConfig> {
    level.and_then(|level| match level {
        ConfigThinkingLevel::Off => None,
        ConfigThinkingLevel::Minimal => Some(ThinkingConfig::Minimal),
        ConfigThinkingLevel::Low => Some(ThinkingConfig::Low),
        ConfigThinkingLevel::Medium => Some(ThinkingConfig::Medium),
        ConfigThinkingLevel::High => Some(ThinkingConfig::High),
        ConfigThinkingLevel::XHigh => Some(ThinkingConfig::XHigh),
        ConfigThinkingLevel::Max => Some(ThinkingConfig::Max),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use aj_models::auth::AuthCredential;
    use aj_models::registry::{Catalog, InputModality, ModelCost, OverridesFile};

    fn sample_model(provider: &str, id: &str, api: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            family: None,
            api: api.into(),
            provider: provider.into(),
            base_url: "https://example.invalid".into(),
            reasoning: false,
            reasoning_options: Vec::new(),
            supports_verbosity: false,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 1_000,
            max_tokens: 100,
        }
    }

    fn registry(models: Vec<ModelInfo>) -> ModelRegistry {
        let catalog = Catalog {
            schema_version: aj_models::registry::CATALOG_SCHEMA_VERSION,
            updated_at: 0,
            source: "test".into(),
            models,
        };
        let overrides = OverridesFile { overrides: vec![] };
        ModelRegistry::from_catalog_with_overrides(catalog, overrides, "test-catalog")
    }

    fn auth_storage(name: &str) -> (tempfile::TempDir, AuthStorage) {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = dir.path().join(format!("{name}.json"));
        (dir, AuthStorage::with_providers(path, HashMap::new()))
    }

    fn key(value: &str) -> AuthCredential {
        AuthCredential::ApiKey {
            key: value.to_string(),
        }
    }

    #[tokio::test]
    async fn resolver_records_the_default_label_that_actually_resolved() {
        let (_dir, auth) = auth_storage("default-label");
        auth.insert_account("anthropic", "personal", key("personal-key"))
            .await
            .unwrap();
        auth.insert_account("anthropic", "work", key("work-key"))
            .await
            .unwrap();
        let mut options = StreamOptions::default();
        install_api_key_resolver(&mut options, &auth, "anthropic", None);

        let resolved = options.resolve_api_key().await.unwrap();
        assert_eq!(
            resolved.key, "personal-key",
            "the store's default must serve, otherwise the label assertion measures nothing"
        );
        assert_eq!(
            resolved.account.as_deref(),
            Some("personal"),
            "the resolver records what served, not the absent pick"
        );
    }

    #[tokio::test]
    async fn resolver_does_not_stamp_a_pick_the_runtime_override_served() {
        let (_dir, auth) = auth_storage("override");
        auth.insert_account("anthropic", "work", key("work-key"))
            .await
            .unwrap();
        auth.set_runtime_api_key("anthropic", "override-key".to_string())
            .await;
        let mut options = StreamOptions::default();
        install_api_key_resolver(&mut options, &auth, "anthropic", Some("work"));

        let resolved = options.resolve_api_key().await.unwrap();
        assert_eq!(
            resolved.key, "override-key",
            "the override must serve, otherwise the account assertion measures nothing"
        );
        assert_eq!(
            resolved.account, None,
            "an override-served turn is not stamped with the account that was asked for"
        );
    }

    #[test]
    fn thinking_levels_for_filters_to_supported_levels() {
        use aj_models::registry::ReasoningOption;
        use aj_models::types::ThinkingLevel;

        // An effort enum without off or the top rungs surfaces only those
        // rungs, matched by name against the catalog rows.
        let mut restricted = sample_model("openai", "gpt-x", "openai-responses");
        restricted.reasoning = true;
        restricted.reasoning_options = vec![ReasoningOption::Effort {
            values: vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
        }];
        let names: Vec<&str> = crate::commands::thinking_levels_for(&restricted)
            .iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(names, vec!["low", "medium", "high"]);

        // A non-reasoning model offers only off.
        let plain = sample_model("openai", "gpt-4o", "openai-responses");
        let names: Vec<&str> = crate::commands::thinking_levels_for(&plain)
            .iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(names, vec!["off"]);

        // An under-described reasoning model gets the full ladder, which
        // exercises the name bridge across all seven levels (incl. off,
        // xhigh).
        let mut full = sample_model("openai", "gpt-y", "openai-responses");
        full.reasoning = true;
        let names: Vec<&str> = crate::commands::thinking_levels_for(&full)
            .iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(
            names,
            vec!["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        );
    }

    #[test]
    fn model_resolution_uses_the_exact_selection_and_endpoint_override() {
        let (_dir, auth) = auth_storage("model-selection");
        let reg = registry(vec![
            sample_model("openai", "decoy", "openai-responses"),
            sample_model("openai", "chosen-model", "openai-responses"),
        ]);
        let selection = ModelSelection {
            api: Some("openai".into()),
            name: Some("chosen-model".into()),
            url: Some("https://proxy.example/v1".into()),
        };
        let resolved = resolve(&reg, &auth, &selection, None).expect("resolved model");
        assert_eq!(resolved.model_info.id, "chosen-model");
        assert_eq!(resolved.model_info.base_url, "https://proxy.example/v1");
    }

    #[test]
    fn model_resolution_rejects_incomplete_or_unavailable_selections() {
        let (_dir, auth) = auth_storage("missing-model");
        let reg = registry(vec![sample_model(
            "anthropic",
            "decoy",
            "anthropic-messages",
        )]);
        for (api, name, expected) in [
            (None, Some("decoy"), "model provider is not configured"),
            (Some("anthropic"), None, "model name is not configured"),
            (Some("anthropic"), Some("missing"), "anthropic/missing"),
            (Some("no-such"), Some("decoy"), "no-such/decoy"),
        ] {
            let selection = ModelSelection {
                api: api.map(String::from),
                name: name.map(String::from),
                url: None,
            };
            let error = resolve(&reg, &auth, &selection, None)
                .err()
                .expect("no substitute model")
                .to_string();
            assert!(error.contains(expected), "{error}");
            if api.is_some() && name.is_some() {
                assert!(
                    error.contains("aj models update")
                        && error.contains("provider and model name together"),
                    "{error}"
                );
            }
        }
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
    fn apply_verbosity_sets_and_clears() {
        let mut opts = StreamOptions {
            verbosity: Some(Verbosity::Low),
            ..StreamOptions::default()
        };
        apply_verbosity(&mut opts, Some(ConfigVerbosity::High));
        assert_eq!(opts.verbosity, Some(Verbosity::High));
        // `None` clears the field so the server default applies.
        apply_verbosity(&mut opts, None);
        assert!(opts.verbosity.is_none());
    }

    #[test]
    fn model_selection_cli_overrides_config() {
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
        assert_eq!(sel.provider_id().expect("provider"), "openai");
    }

    #[test]
    fn model_selection_uses_effective_config_when_cli_is_unset() {
        let args = Args::parse_from(["aj"]);
        let config = Config {
            model_name: Some("claude-x".to_string()),
            ..Config::default()
        };
        let sel = ModelSelection::merge(&args, &config);
        assert_eq!(sel.api, Config::default().model_api);
        assert_eq!(sel.name.as_deref(), Some("claude-x"));
        // The configuration supplies the provider, not the resolver.
        assert_eq!(
            sel.provider_id().expect("provider"),
            Config::default().model_api.as_deref().unwrap()
        );
    }
}
