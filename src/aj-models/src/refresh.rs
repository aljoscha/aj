//! Catalog refresh: fetch models.dev, normalize, write the user cache.
//!
//! Implements the `aj update-models` flow described in
//! `docs/models-spec.md` §3.4.2 and §3.4.5: pull
//! `https://models.dev/api.json`, filter to tool-capable Anthropic and
//! OpenAI models, fill provider-specific fixed values, apply the bundled
//! overrides, and atomically write the result to `~/.aj/models.json`.
//! On any failure the existing cache is left untouched — a broken fetch
//! must never brick the registry.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::registry::{
    CODEX_PROVIDER_ID, Catalog, InputModality, ModelCost, ModelInfo, apply_override,
    bundled_codex_seed, bundled_overrides, splice_codex_seed, user_cache_path,
};

/// Upstream catalog endpoint. Public so callers (tests, alternative
/// CLI wiring) can override it without re-deriving the URL.
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// OpenRouter's live model list. We fetch this in addition to
/// models.dev because OpenRouter aggregates a large, fast-moving set of
/// models that models.dev does not enumerate. Public for the same
/// override reasons as [`MODELS_DEV_URL`].
pub const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

// ---------------------------------------------------------------------------
// Provider-specific fixed values (§3.4.3).
// ---------------------------------------------------------------------------

/// Each `(provider, id)` pair in the catalog has exactly one `api`. The
/// catalog hard-codes the provider's preferred wire shape; users do not
/// pick between Chat Completions and Responses for native models.
struct ProviderFixedValues {
    /// models.dev top-level provider key.
    upstream_key: &'static str,
    /// `provider` field written into the catalog.
    provider_id: &'static str,
    /// `api` field written into the catalog.
    api: &'static str,
    /// `base_url` field written into the catalog.
    base_url: &'static str,
}

const PROVIDER_FIXED_VALUES: &[ProviderFixedValues] = &[
    ProviderFixedValues {
        upstream_key: "anthropic",
        provider_id: "anthropic",
        api: "anthropic-messages",
        base_url: "https://api.anthropic.com",
    },
    ProviderFixedValues {
        upstream_key: "openai",
        provider_id: "openai",
        api: "openai-responses",
        base_url: "https://api.openai.com/v1",
    },
];

// ---------------------------------------------------------------------------
// OpenRouter fixed values.
// ---------------------------------------------------------------------------

/// Catalog provider id for OpenRouter models.
const OPENROUTER_PROVIDER_ID: &str = "openrouter";
/// Wire shape we route OpenRouter through. OpenRouter exposes an
/// OpenAI-compatible Responses API, so the existing responses provider
/// serves it with only a `base_url` override. See
/// `docs/openrouter-spec.md`.
const OPENROUTER_API: &str = "openai-responses";
/// Base URL for OpenRouter's Responses endpoint (the provider appends
/// `/responses`).
const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

// ---------------------------------------------------------------------------
// models.dev API shape (only the fields we need).
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct RawProvider {
    #[serde(default)]
    models: BTreeMap<String, RawModel>,
}

#[derive(Deserialize, Debug)]
struct RawModel {
    /// Some providers omit `name` for in-flight aliases — fall back to
    /// the model id when that happens (matches the seed's behaviour).
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    tool_call: Option<bool>,
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    limit: Option<RawLimit>,
    #[serde(default)]
    cost: Option<RawCost>,
    #[serde(default)]
    modalities: Option<RawModalities>,
}

#[derive(Deserialize, Debug, Default)]
struct RawLimit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

#[derive(Deserialize, Debug, Default)]
struct RawCost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

#[derive(Deserialize, Debug, Default)]
struct RawModalities {
    #[serde(default)]
    input: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// OpenRouter API shape (only the fields we need).
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct OpenRouterList {
    #[serde(default)]
    data: Vec<OpenRouterModel>,
}

#[derive(Deserialize, Debug)]
struct OpenRouterModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    architecture: Option<OpenRouterArch>,
    #[serde(default)]
    pricing: Option<OpenRouterPricing>,
    #[serde(default)]
    top_provider: Option<OpenRouterTopProvider>,
    /// Capability flags. We key tool support and reasoning off this:
    /// `"tools"` gates eligibility, `"reasoning"` sets the flag.
    #[serde(default)]
    supported_parameters: Vec<String>,
}

#[derive(Deserialize, Debug, Default)]
struct OpenRouterArch {
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
}

/// Per-token USD prices as strings (OpenRouter's wire format).
#[derive(Deserialize, Debug, Default)]
struct OpenRouterPricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
    #[serde(default)]
    input_cache_read: Option<String>,
    #[serde(default)]
    input_cache_write: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct OpenRouterTopProvider {
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Diff between the catalog that was on disk before the refresh and the
/// catalog that was just written. Used by the CLI to render a one-line
/// summary; surfaced as fields so callers can render their own.
#[derive(Debug, Clone, Default)]
pub struct RefreshSummary {
    /// Newly added models, formatted as `provider/id`.
    pub added: Vec<String>,
    /// Models present in the previous cache but absent from the fresh
    /// fetch, formatted as `provider/id`.
    pub removed: Vec<String>,
    /// Models whose pricing changed between the previous cache and the
    /// fresh fetch, formatted as `provider/id`.
    pub price_changed: Vec<String>,
    /// Total models in the new catalog after overrides.
    pub total: usize,
    /// Path the new catalog was written to.
    pub destination: PathBuf,
}

impl RefreshSummary {
    /// Render the §3.4.5 short summary: "added X, removed Y, price
    /// changes on Z". Always reports the totals, even when zero, so
    /// users see the path was written successfully.
    pub fn one_line(&self) -> String {
        format!(
            "added {} models, removed {}, price changes on {} (total: {}, written to {})",
            self.added.len(),
            self.removed.len(),
            self.price_changed.len(),
            self.total,
            self.destination.display(),
        )
    }
}

/// Fetch models.dev and OpenRouter, normalize, apply overrides, and
/// atomically write the user cache at `~/.aj/models.json`. On any
/// failure the existing cache is left untouched and an error is
/// returned. models.dev is the baseline source and its failure is
/// fatal. An OpenRouter fetch failure is not: we warn and carry forward
/// the OpenRouter rows from the existing cache (if any) so a third-party
/// outage never blocks a first-party refresh.
pub async fn refresh_user_cache() -> Result<RefreshSummary> {
    refresh_user_cache_from(MODELS_DEV_URL, OPENROUTER_MODELS_URL).await
}

/// Same as [`refresh_user_cache`] but lets the caller override the
/// upstream URLs. The override form exists for tests that point at a
/// local fixture server, and for any future override needs.
pub async fn refresh_user_cache_from(
    models_dev_url: &str,
    openrouter_url: &str,
) -> Result<RefreshSummary> {
    let dest = user_cache_path()
        .context("could not determine user cache path; HOME env var may be unset")?;
    let models_dev_body = fetch_url(models_dev_url).await?;

    // A reachable OpenRouter gives the live catalog. When it is
    // unreachable we keep whatever OpenRouter rows the cache already has
    // rather than dropping the provider, which also keeps the refresh
    // diff from reporting every OpenRouter model as removed.
    let new_catalog = match fetch_url(openrouter_url).await {
        Ok(openrouter_body) => build_catalog_from_json(&models_dev_body, Some(&openrouter_body))?,
        Err(err) => {
            tracing::warn!(
                "OpenRouter model list fetch failed ({err}); keeping previously cached OpenRouter models"
            );
            let mut models = parse_models_dev(&models_dev_body)?;
            let cached = cached_openrouter_models(&dest);
            let source = if cached.is_empty() {
                "models.dev"
            } else {
                "models.dev+openrouter (cached)"
            };
            models.extend(cached);
            assemble_catalog(models, source)
        }
    };

    let summary = build_summary(&dest, &new_catalog);
    write_catalog_atomically(&dest, &new_catalog)?;
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Fetch the raw JSON body from `url`. Surfaces the HTTP status on
/// non-2xx responses so the user understands why the cache wasn't
/// touched.
async fn fetch_url(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("aj/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building reqwest client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("{url} returned status {status}: {body}");
    }
    resp.text()
        .await
        .with_context(|| format!("reading body from {url}"))
}

/// Parse the models.dev and (optional) OpenRouter payloads into a
/// normalized [`Catalog`] with the Codex seed spliced in and overrides
/// applied. Public-in-crate so the round-trip tests below can exercise
/// it without hitting the network. Pass `None` for `openrouter_body` to
/// build a models.dev-only catalog.
fn build_catalog_from_json(
    models_dev_body: &str,
    openrouter_body: Option<&str>,
) -> Result<Catalog> {
    let mut models = parse_models_dev(models_dev_body)?;

    let source = match openrouter_body {
        Some(body) => {
            models.extend(parse_openrouter(body)?);
            "models.dev+openrouter"
        }
        None => "models.dev",
    };

    Ok(assemble_catalog(models, source))
}

/// Parse a models.dev JSON payload into the mapped, tool-filtered model
/// list. Does not splice the Codex seed, sort, or apply overrides. That
/// is [`assemble_catalog`]'s job.
fn parse_models_dev(body: &str) -> Result<Vec<ModelInfo>> {
    // The top-level object is keyed by provider id; we only care about
    // a fixed subset, so parse into a flexible map and look up the keys
    // we need. Unknown providers are ignored silently.
    let raw: HashMap<String, RawProvider> =
        serde_json::from_str(body).context("parsing models.dev response as JSON")?;

    let mut models = Vec::new();
    for fixed in PROVIDER_FIXED_VALUES {
        let Some(provider) = raw.get(fixed.upstream_key) else {
            tracing::warn!(
                "models.dev response missing provider {}; skipping",
                fixed.upstream_key
            );
            continue;
        };
        for (id, m) in &provider.models {
            // §3.4.7: only tool-capable models are eligible.
            if m.tool_call != Some(true) {
                continue;
            }
            let mapped = map_model(fixed, id, m);
            // §3.4.7: Codex models are seeded by hand; defensively
            // drop any upstream re-emission so the seed below is the
            // single source of truth for `(provider="openai-codex",
            // id=*)`. models.dev does not categorize anything under
            // `openai-codex` today, so this is a guard rather than a
            // live filter, but it keeps the invariant explicit if a
            // future upstream entry leaks in.
            if mapped.provider == CODEX_PROVIDER_ID {
                continue;
            }
            models.push(mapped);
        }
    }
    Ok(models)
}

/// Parse OpenRouter's `/api/v1/models` payload into the mapped,
/// tool-filtered model list. Drops models that lack tool support or
/// cannot emit text (e.g. pure image generators).
fn parse_openrouter(body: &str) -> Result<Vec<ModelInfo>> {
    let list: OpenRouterList =
        serde_json::from_str(body).context("parsing openrouter models response as JSON")?;

    let mut models = Vec::new();
    for m in &list.data {
        // NOTE: we do not dedup by id. OpenRouter ids are unique in the
        // `/models` response, so a duplicate would indicate an upstream
        // bug. On load the registry keys by `(provider, id)` and would
        // collapse any dup anyway.
        // Agent use requires tool calling.
        if !m.supported_parameters.iter().any(|p| p == "tools") {
            continue;
        }
        // Drop models that declare output modalities none of which are
        // text. An empty/absent list is treated as text-capable.
        if let Some(arch) = &m.architecture
            && !arch.output_modalities.is_empty()
            && !arch
                .output_modalities
                .iter()
                .any(|s| s.eq_ignore_ascii_case("text"))
        {
            continue;
        }
        models.push(map_openrouter_model(m));
    }
    Ok(models)
}

/// Splice the hand-curated Codex seed, sort, apply overrides, and stamp
/// catalog metadata. Shared tail of catalog construction across sources.
fn assemble_catalog(mut models: Vec<ModelInfo>, source: &str) -> Catalog {
    // §3.4.7: re-emit Codex models from the hand-curated seed after
    // upstream filtering. Refresh writes the codex entries into the
    // user cache so subsequent refreshes diff cleanly (without the
    // codex set showing up as "removed" every run because models.dev
    // doesn't include them).
    splice_codex_seed(&mut models, bundled_codex_seed());

    // Stable sort: provider then id. Catalog ordering should not depend
    // on HashMap iteration order, otherwise diffs against the seed are
    // noisy.
    models.sort_by(|a, b| match a.provider.cmp(&b.provider) {
        Ordering::Equal => a.id.cmp(&b.id),
        other => other,
    });

    // §3.4.5: the refresh command applies overrides before writing the
    // cache. The load path applies them again on every load (idempotent
    // shallow merges), so authored corrections survive both fresh
    // fetches and stale caches.
    let overrides = bundled_overrides();
    for entry in &overrides.overrides {
        apply_override(&mut models, entry);
    }

    Catalog {
        updated_at: chrono::Utc::now().timestamp_millis(),
        source: source.to_string(),
        models,
    }
}

/// Normalize a single models.dev entry into our [`ModelInfo`] shape.
/// Missing fields fall back to spec-aligned defaults: zero costs (so we
/// never silently bill against unknown rates), 4096-token context, and
/// the upstream id when no human-readable name is supplied.
fn map_model(fixed: &ProviderFixedValues, id: &str, m: &RawModel) -> ModelInfo {
    let cost = m.cost.as_ref();
    let limit = m.limit.as_ref();
    let modalities = m.modalities.as_ref();

    // §3.4.2: `modalities.input` may include "image"; if so the model
    // accepts both text and images. Otherwise default to text-only —
    // every supported model accepts text.
    let mut input = vec![InputModality::Text];
    if let Some(mods) = modalities
        && let Some(values) = &mods.input
        && values.iter().any(|s| s.eq_ignore_ascii_case("image"))
    {
        input.push(InputModality::Image);
    }

    ModelInfo {
        id: id.to_string(),
        name: m.name.clone().unwrap_or_else(|| id.to_string()),
        api: fixed.api.to_string(),
        provider: fixed.provider_id.to_string(),
        base_url: fixed.base_url.to_string(),
        reasoning: m.reasoning.unwrap_or(false),
        // §3.4.4: `supports_adaptive_thinking` is not in models.dev.
        // Default to `true` for Anthropic reasoning models so a newly
        // released model uses the modern adaptive API rather than
        // silently falling back to budget-based thinking; legacy
        // budget-only Anthropic models are pinned `false` via
        // overrides. Always `false` for non-Anthropic and non-reasoning
        // models.
        supports_adaptive_thinking: fixed.api == "anthropic-messages"
            && m.reasoning.unwrap_or(false),
        input,
        cost: ModelCost {
            input: cost.and_then(|c| c.input).unwrap_or(0.0),
            output: cost.and_then(|c| c.output).unwrap_or(0.0),
            cache_read: cost.and_then(|c| c.cache_read).unwrap_or(0.0),
            cache_write: cost.and_then(|c| c.cache_write).unwrap_or(0.0),
        },
        context_window: limit.and_then(|l| l.context).unwrap_or(4096),
        max_tokens: limit.and_then(|l| l.output).unwrap_or(4096),
        // models.dev has no per-model headers field; the seed/overrides
        // own that for providers that need static identity headers.
        headers: None,
    }
}

/// Normalize a single OpenRouter `/models` entry into our [`ModelInfo`]
/// shape. The full slash-namespaced id is kept verbatim (e.g.
/// `anthropic/claude-sonnet-4`). All entries map to the Responses wire
/// shape against OpenRouter's base URL.
fn map_openrouter_model(m: &OpenRouterModel) -> ModelInfo {
    let arch = m.architecture.as_ref();
    let pricing = m.pricing.as_ref();

    let mut input = vec![InputModality::Text];
    if let Some(a) = arch
        && a.input_modalities
            .iter()
            .any(|s| s.eq_ignore_ascii_case("image"))
    {
        input.push(InputModality::Image);
    }

    ModelInfo {
        id: m.id.clone(),
        name: m.name.clone().unwrap_or_else(|| m.id.clone()),
        api: OPENROUTER_API.to_string(),
        provider: OPENROUTER_PROVIDER_ID.to_string(),
        base_url: OPENROUTER_BASE_URL.to_string(),
        reasoning: m.supported_parameters.iter().any(|p| p == "reasoning"),
        // Adaptive thinking is an Anthropic-native concept.
        supports_adaptive_thinking: false,
        input,
        cost: ModelCost {
            input: openrouter_price_per_million(pricing.and_then(|p| p.prompt.as_deref())),
            output: openrouter_price_per_million(pricing.and_then(|p| p.completion.as_deref())),
            cache_read: openrouter_price_per_million(
                pricing.and_then(|p| p.input_cache_read.as_deref()),
            ),
            cache_write: openrouter_price_per_million(
                pricing.and_then(|p| p.input_cache_write.as_deref()),
            ),
        },
        context_window: m.context_length.unwrap_or(4096),
        max_tokens: m
            .top_provider
            .as_ref()
            .and_then(|t| t.max_completion_tokens)
            .unwrap_or(4096),
        headers: None,
    }
}

/// OpenRouter prices are per-token USD strings. Convert to the
/// per-million-token figure our [`ModelCost`] uses. A missing or
/// unparseable price becomes zero so we never bill against an unknown
/// rate.
fn openrouter_price_per_million(raw: Option<&str>) -> f64 {
    raw.and_then(|s| s.parse::<f64>().ok())
        .map(|p| p * 1_000_000.0)
        .unwrap_or(0.0)
}

/// Compare the new catalog against whatever is currently on disk and
/// return a [`RefreshSummary`]. A missing or unparseable previous cache
/// is treated as empty — every entry counts as an addition. This is
/// intentional: the user explicitly asked to refresh, and treating a
/// broken cache like an absent one gives them a clean baseline.
fn build_summary(dest: &Path, new_catalog: &Catalog) -> RefreshSummary {
    let previous = load_previous_catalog(dest);
    let prev_index: HashMap<(String, String), &ModelInfo> = previous
        .iter()
        .flat_map(|c| c.models.iter())
        .map(|m| ((m.provider.clone(), m.id.clone()), m))
        .collect();
    let new_index: HashMap<(String, String), &ModelInfo> = new_catalog
        .models
        .iter()
        .map(|m| ((m.provider.clone(), m.id.clone()), m))
        .collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut price_changed = Vec::new();

    for ((provider, id), new) in &new_index {
        match prev_index.get(&(provider.clone(), id.clone())) {
            None => added.push(format!("{provider}/{id}")),
            Some(old) => {
                if old.cost != new.cost {
                    price_changed.push(format!("{provider}/{id}"));
                }
            }
        }
    }
    for (provider, id) in prev_index.keys() {
        if !new_index.contains_key(&(provider.clone(), id.clone())) {
            removed.push(format!("{provider}/{id}"));
        }
    }

    added.sort();
    removed.sort();
    price_changed.sort();

    RefreshSummary {
        added,
        removed,
        price_changed,
        total: new_catalog.models.len(),
        destination: dest.to_path_buf(),
    }
}

/// Best-effort read of the previous user cache. Errors are non-fatal:
/// the diff just treats the missing data as "no prior catalog".
fn load_previous_catalog(dest: &Path) -> Option<Catalog> {
    if !dest.exists() {
        return None;
    }
    let body = fs::read_to_string(dest).ok()?;
    serde_json::from_str(&body).ok()
}

/// OpenRouter rows from the existing cache, used to carry the provider
/// forward when a live OpenRouter fetch fails. Empty when there is no
/// readable prior cache.
fn cached_openrouter_models(dest: &Path) -> Vec<ModelInfo> {
    load_previous_catalog(dest)
        .map(|c| {
            c.models
                .into_iter()
                .filter(|m| m.provider == OPENROUTER_PROVIDER_ID)
                .collect()
        })
        .unwrap_or_default()
}

/// Write the catalog to `dest` atomically: serialize to a temp file in
/// the same directory and rename into place. Same-directory rename is
/// atomic on POSIX and adequate on Windows for our purposes — readers
/// of `models.json` either see the old contents or the new contents,
/// never a torn write.
fn write_catalog_atomically(dest: &Path, catalog: &Catalog) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("catalog destination {} has no parent", dest.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating parent directory {}", parent.display()))?;

    let body = serde_json::to_vec_pretty(catalog).context("serializing catalog")?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temp file in {}", parent.display()))?;
    tmp.write_all(&body)
        .context("writing catalog to temp file")?;
    tmp.flush().context("flushing catalog temp file")?;
    tmp.persist(dest)
        .with_context(|| format!("persisting catalog to {}", dest.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal models.dev-shaped fixture: two anthropic models (one
    /// tool-capable, one not), one openai model, and one provider we
    /// don't pull from. Lets us assert filtering, mapping, and ordering
    /// in one pass without hitting the network.
    const FIXTURE: &str = r#"{
        "anthropic": {
            "models": {
                "claude-test-tool": {
                    "name": "Claude Test (Tool)",
                    "tool_call": true,
                    "reasoning": true,
                    "limit": {"context": 200000, "output": 64000},
                    "cost": {"input": 3.0, "output": 15.0, "cache_read": 0.3, "cache_write": 3.75},
                    "modalities": {"input": ["text", "image"]}
                },
                "claude-test-no-tool": {
                    "name": "Claude Test (No Tool)",
                    "tool_call": false,
                    "limit": {"context": 100000, "output": 8000},
                    "cost": {"input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25},
                    "modalities": {"input": ["text"]}
                }
            }
        },
        "openai": {
            "models": {
                "gpt-test": {
                    "name": "GPT Test",
                    "tool_call": true,
                    "reasoning": false,
                    "limit": {"context": 128000, "output": 16000},
                    "cost": {"input": 2.5, "output": 10.0, "cache_read": 0.25, "cache_write": 0.0},
                    "modalities": {"input": ["text"]}
                }
            }
        },
        "google": {
            "models": {
                "gemini-test": {
                    "name": "Gemini",
                    "tool_call": true,
                    "modalities": {"input": ["text", "image"]}
                }
            }
        }
    }"#;

    #[test]
    fn build_catalog_filters_and_maps() {
        let cat = build_catalog_from_json(FIXTURE, None).expect("parses");
        // Two upstream models survive filtering plus the bundled
        // Codex seed appended at the end. google must be ignored (not
        // a target provider) and the non-tool anthropic model must be
        // filtered out.
        let codex_count = bundled_codex_seed().len();
        assert!(codex_count > 0, "codex seed must be non-empty");
        assert_eq!(cat.models.len(), 2 + codex_count);
        assert_eq!(cat.source, "models.dev");
        assert!(cat.updated_at > 0);

        // The whole catalog is sorted by (provider, id). The codex
        // seed lands last here because `openai-codex` orders after
        // `anthropic`/`openai` in this fixture.
        let upstream_identities: Vec<_> = cat
            .models
            .iter()
            .filter(|m| m.provider != "openai-codex")
            .map(|m| (m.provider.as_str(), m.id.as_str()))
            .collect();
        assert_eq!(
            upstream_identities,
            vec![("anthropic", "claude-test-tool"), ("openai", "gpt-test"),]
        );

        let claude = cat
            .models
            .iter()
            .find(|m| m.id == "claude-test-tool")
            .expect("claude entry present");
        assert_eq!(claude.api, "anthropic-messages");
        assert_eq!(claude.base_url, "https://api.anthropic.com");
        assert!(claude.reasoning);
        // Anthropic reasoning model defaults to adaptive thinking.
        assert!(claude.supports_adaptive_thinking);
        assert_eq!(
            claude.input,
            vec![InputModality::Text, InputModality::Image]
        );
        assert!((claude.cost.input - 3.0).abs() < 1e-9);
        assert_eq!(claude.context_window, 200_000);
        assert_eq!(claude.max_tokens, 64_000);

        let gpt = cat
            .models
            .iter()
            .find(|m| m.id == "gpt-test")
            .expect("gpt entry present");
        assert_eq!(gpt.api, "openai-responses");
        assert_eq!(gpt.base_url, "https://api.openai.com/v1");
        // Default modality fallback: text-only when "image" isn't in
        // the modalities list.
        assert_eq!(gpt.input, vec![InputModality::Text]);

        // Every codex entry must land under the codex provider with
        // the codex api + base url.
        for m in cat.models.iter().filter(|m| m.provider == "openai-codex") {
            assert_eq!(m.api, "openai-codex-responses");
            assert_eq!(m.base_url, "https://chatgpt.com/backend-api");
        }
    }

    #[test]
    fn missing_fields_use_safe_defaults() {
        // Bare-minimum model entry: nothing but tool_call.
        let body = r#"{
            "anthropic": {
                "models": {
                    "claude-bare": {"tool_call": true}
                }
            }
        }"#;
        let cat = build_catalog_from_json(body, None).expect("parses");
        // One upstream model + the bundled codex seed.
        assert_eq!(cat.models.len(), 1 + bundled_codex_seed().len());
        let m = cat
            .models
            .iter()
            .find(|m| m.id == "claude-bare")
            .expect("bare entry present");
        // Name falls back to id.
        assert_eq!(m.name, "claude-bare");
        assert_eq!(m.cost.input, 0.0);
        assert_eq!(m.cost.output, 0.0);
        assert_eq!(m.context_window, 4096);
        assert_eq!(m.max_tokens, 4096);
        assert_eq!(m.input, vec![InputModality::Text]);
        assert!(!m.reasoning);
    }

    #[test]
    fn write_and_diff_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("models.json");

        // First write: everything is "added".
        let cat1 = build_catalog_from_json(FIXTURE, None).expect("parses");
        let codex_count = bundled_codex_seed().len();
        let expected_total = 2 + codex_count;
        assert_eq!(cat1.models.len(), expected_total);
        write_catalog_atomically(&dest, &cat1).expect("writes");
        let summary = build_summary(&dest, &cat1);
        // After the write, the previous-on-disk equals the new catalog
        // (we built the summary against `dest` post-write), so nothing
        // should look added or removed.
        assert!(summary.added.is_empty());
        assert!(summary.removed.is_empty());
        assert!(summary.price_changed.is_empty());
        assert_eq!(summary.total, expected_total);

        // Now mutate the in-memory catalog: change a price on an
        // upstream model and remove one. Diff against the on-disk
        // previous (which is cat1).
        let mut cat2 = cat1.clone();
        let claude_idx = cat2
            .models
            .iter()
            .position(|m| m.id == "claude-test-tool")
            .expect("claude entry present");
        cat2.models[claude_idx].cost.input = 99.0;
        let gpt_idx = cat2
            .models
            .iter()
            .position(|m| m.id == "gpt-test")
            .expect("gpt entry present");
        cat2.models.remove(gpt_idx);
        let summary2 = build_summary(&dest, &cat2);
        assert_eq!(summary2.price_changed, vec!["anthropic/claude-test-tool"]);
        assert_eq!(summary2.removed, vec!["openai/gpt-test"]);
        assert!(summary2.added.is_empty());

        // Adding a brand-new model registers as an addition.
        let mut cat3 = cat1.clone();
        let mut extra = cat1.models[0].clone();
        extra.id = "claude-new".to_string();
        cat3.models.push(extra);
        let summary3 = build_summary(&dest, &cat3);
        assert_eq!(summary3.added, vec!["anthropic/claude-new"]);
    }

    #[test]
    fn one_line_format() {
        let dest = PathBuf::from("/tmp/whatever");
        let s = RefreshSummary {
            added: vec!["anthropic/x".into()],
            removed: vec![],
            price_changed: vec!["openai/y".into(), "openai/z".into()],
            total: 42,
            destination: dest,
        };
        let line = s.one_line();
        assert!(line.contains("added 1"));
        assert!(line.contains("removed 0"));
        assert!(line.contains("price changes on 2"));
        assert!(line.contains("total: 42"));
    }

    /// Refresh must preserve codex entries across rounds: the first
    /// refresh writes the codex set; the second refresh produces an
    /// identical catalog and diffs cleanly (no codex entries showing
    /// as "removed" just because models.dev doesn't list them).
    #[test]
    fn refresh_preserves_codex_entries_across_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("models.json");

        // First refresh: writes upstream + codex seed.
        let cat1 = build_catalog_from_json(FIXTURE, None).expect("parses");
        write_catalog_atomically(&dest, &cat1).expect("writes");

        let codex_count = bundled_codex_seed().len();
        let codex_in_cat1 = cat1
            .models
            .iter()
            .filter(|m| m.provider == CODEX_PROVIDER_ID)
            .count();
        assert_eq!(codex_in_cat1, codex_count);

        // Second refresh from an identical upstream feed: the catalog
        // is unchanged on disk (after rewrite, the diff is empty).
        let cat2 = build_catalog_from_json(FIXTURE, None).expect("parses");
        let summary = build_summary(&dest, &cat2);
        assert!(
            summary.removed.is_empty(),
            "second refresh must not flag codex entries as removed: {:?}",
            summary.removed
        );
        assert!(summary.added.is_empty());
        assert!(summary.price_changed.is_empty());

        // Both refreshes produced the same codex set in the same
        // positions (the seed is appended unconditionally after
        // upstream filtering).
        let codex_ids_1: Vec<_> = cat1
            .models
            .iter()
            .filter(|m| m.provider == CODEX_PROVIDER_ID)
            .map(|m| m.id.as_str())
            .collect();
        let codex_ids_2: Vec<_> = cat2
            .models
            .iter()
            .filter(|m| m.provider == CODEX_PROVIDER_ID)
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(codex_ids_1, codex_ids_2);
    }

    /// OpenRouter fixture covering the cases we map: a tool+reasoning
    /// model with image input, a tool-only non-reasoning model, a model
    /// without tool support (dropped), and a pure image-output model
    /// (dropped).
    const OPENROUTER_FIXTURE: &str = r#"{
        "data": [
            {
                "id": "vendor/reasoner-1",
                "name": "Vendor Reasoner 1",
                "context_length": 200000,
                "architecture": {
                    "input_modalities": ["text", "image"],
                    "output_modalities": ["text"]
                },
                "pricing": {
                    "prompt": "0.000002",
                    "completion": "0.000012",
                    "input_cache_read": "0.0000002",
                    "input_cache_write": "0.000000375"
                },
                "top_provider": {"max_completion_tokens": 32768},
                "supported_parameters": ["tools", "reasoning", "temperature"]
            },
            {
                "id": "vendor/chat-1",
                "name": "Vendor Chat 1",
                "context_length": 64000,
                "architecture": {
                    "input_modalities": ["text"],
                    "output_modalities": ["text"]
                },
                "pricing": {"prompt": "0.0000005", "completion": "0.0000015"},
                "supported_parameters": ["tools", "temperature"]
            },
            {
                "id": "vendor/minimal",
                "pricing": {"prompt": "not-a-number"},
                "supported_parameters": ["tools"]
            },
            {
                "id": "vendor/no-tools",
                "name": "No Tools",
                "supported_parameters": ["temperature"]
            },
            {
                "id": "vendor/image-only",
                "name": "Image Only",
                "architecture": {
                    "input_modalities": ["text"],
                    "output_modalities": ["image"]
                },
                "supported_parameters": ["tools"]
            }
        ]
    }"#;

    #[test]
    fn build_catalog_includes_openrouter() {
        let cat = build_catalog_from_json(FIXTURE, Some(OPENROUTER_FIXTURE)).expect("parses");
        assert_eq!(cat.source, "models.dev+openrouter");

        let or: Vec<_> = cat
            .models
            .iter()
            .filter(|m| m.provider == "openrouter")
            .collect();
        // Only the tool-capable, text-output models survive.
        let ids: Vec<&str> = or.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["vendor/chat-1", "vendor/minimal", "vendor/reasoner-1"]
        );

        let reasoner = or
            .iter()
            .find(|m| m.id == "vendor/reasoner-1")
            .expect("reasoner present");
        assert_eq!(reasoner.api, "openai-responses");
        assert_eq!(reasoner.base_url, "https://openrouter.ai/api/v1");
        assert!(reasoner.reasoning);
        assert!(!reasoner.supports_adaptive_thinking);
        assert_eq!(
            reasoner.input,
            vec![InputModality::Text, InputModality::Image]
        );
        assert_eq!(reasoner.context_window, 200_000);
        assert_eq!(reasoner.max_tokens, 32_768);
        // Per-token USD strings convert to per-million figures.
        assert!((reasoner.cost.input - 2.0).abs() < 1e-9);
        assert!((reasoner.cost.output - 12.0).abs() < 1e-9);
        assert!((reasoner.cost.cache_read - 0.2).abs() < 1e-9);
        assert!((reasoner.cost.cache_write - 0.375).abs() < 1e-9);

        let chat = or
            .iter()
            .find(|m| m.id == "vendor/chat-1")
            .expect("chat present");
        assert!(!chat.reasoning);
        assert_eq!(chat.input, vec![InputModality::Text]);
        // No cache pricing in the fixture defaults to zero.
        assert_eq!(chat.cost.cache_read, 0.0);
        // No `top_provider` falls back to the default output cap.
        assert_eq!(chat.max_tokens, 4096);

        // The bare entry exercises every fallback: name->id, default
        // context/output limits, text-only input, and an unparseable
        // price collapsing to zero.
        let minimal = or
            .iter()
            .find(|m| m.id == "vendor/minimal")
            .expect("minimal present");
        assert_eq!(minimal.name, "vendor/minimal");
        assert_eq!(minimal.context_window, 4096);
        assert_eq!(minimal.max_tokens, 4096);
        assert_eq!(minimal.input, vec![InputModality::Text]);
        assert!(!minimal.reasoning);
        assert_eq!(minimal.cost.input, 0.0);
    }

    #[test]
    fn openrouter_price_conversion() {
        // Per-token USD string to per-million figure.
        assert!((openrouter_price_per_million(Some("0.000002")) - 2.0).abs() < 1e-9);
        // Missing or unparseable prices collapse to zero.
        assert_eq!(openrouter_price_per_million(None), 0.0);
        assert_eq!(openrouter_price_per_million(Some("")), 0.0);
        assert_eq!(openrouter_price_per_million(Some("free")), 0.0);
    }

    #[test]
    fn cached_openrouter_models_filters_prior_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("models.json");

        // No prior cache yields nothing to carry forward.
        assert!(cached_openrouter_models(&dest).is_empty());

        let cat = build_catalog_from_json(FIXTURE, Some(OPENROUTER_FIXTURE)).expect("parses");
        write_catalog_atomically(&dest, &cat).expect("writes");

        let carried = cached_openrouter_models(&dest);
        let expected = cat
            .models
            .iter()
            .filter(|m| m.provider == "openrouter")
            .count();
        assert_eq!(carried.len(), expected);
        assert!(carried.iter().all(|m| m.provider == "openrouter"));
    }
}
