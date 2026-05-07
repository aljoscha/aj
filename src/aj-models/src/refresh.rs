//! Catalog refresh: fetch models.dev, normalize, write the user cache.
//!
//! Implements the `aj models update` flow described in
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
    Catalog, InputModality, ModelCost, ModelInfo, apply_override, bundled_overrides,
    user_cache_path,
};

/// Upstream catalog endpoint. Public so callers (tests, alternative
/// CLI wiring) can override it without re-deriving the URL.
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

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

/// Fetch models.dev, normalize, apply overrides, and atomically write
/// the user cache at `~/.aj/models.json`. On any failure (network
/// error, non-200 response, parse failure, write error) the existing
/// cache is left untouched and an error is returned.
pub async fn refresh_user_cache() -> Result<RefreshSummary> {
    refresh_user_cache_from(MODELS_DEV_URL).await
}

/// Same as [`refresh_user_cache`] but lets the caller override the
/// upstream URL. The two-arg form exists for tests that point at a
/// local fixture server, and for any future override needs.
pub async fn refresh_user_cache_from(url: &str) -> Result<RefreshSummary> {
    let dest = user_cache_path()
        .context("could not determine user cache path; HOME env var may be unset")?;
    let body = fetch_models_dev(url).await?;
    let new_catalog = build_catalog_from_json(&body)?;
    let summary = build_summary(&dest, &new_catalog);
    write_catalog_atomically(&dest, &new_catalog)?;
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Fetch the raw JSON body from models.dev. Surfaces the HTTP status on
/// non-2xx responses so the user understands why the cache wasn't
/// touched.
async fn fetch_models_dev(url: &str) -> Result<String> {
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

/// Parse a models.dev JSON payload into a normalized [`Catalog`] with
/// overrides applied. Public-in-crate so the round-trip test below can
/// exercise it without hitting the network.
fn build_catalog_from_json(body: &str) -> Result<Catalog> {
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
            models.push(map_model(fixed, id, m));
        }
    }

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

    Ok(Catalog {
        updated_at: chrono::Utc::now().timestamp_millis(),
        source: "models.dev".to_string(),
        models,
    })
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
        // §3.4.4: `supports_xhigh` and `supports_adaptive_thinking` are
        // not in models.dev. Default to `false` here; overrides flip
        // them on for the models that need it.
        supports_xhigh: false,
        supports_adaptive_thinking: false,
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
        let cat = build_catalog_from_json(FIXTURE).expect("parses");
        // google must be ignored (not a target provider) and the
        // non-tool anthropic model must be filtered out.
        assert_eq!(cat.models.len(), 2);
        assert_eq!(cat.source, "models.dev");
        assert!(cat.updated_at > 0);

        // Sorted: anthropic first, then openai.
        let identities: Vec<_> = cat
            .models
            .iter()
            .map(|m| (m.provider.as_str(), m.id.as_str()))
            .collect();
        assert_eq!(
            identities,
            vec![("anthropic", "claude-test-tool"), ("openai", "gpt-test"),]
        );

        let claude = &cat.models[0];
        assert_eq!(claude.api, "anthropic-messages");
        assert_eq!(claude.base_url, "https://api.anthropic.com");
        assert!(claude.reasoning);
        assert!(!claude.supports_xhigh);
        assert!(!claude.supports_adaptive_thinking);
        assert_eq!(
            claude.input,
            vec![InputModality::Text, InputModality::Image]
        );
        assert!((claude.cost.input - 3.0).abs() < 1e-9);
        assert_eq!(claude.context_window, 200_000);
        assert_eq!(claude.max_tokens, 64_000);

        let gpt = &cat.models[1];
        assert_eq!(gpt.api, "openai-responses");
        assert_eq!(gpt.base_url, "https://api.openai.com/v1");
        // Default modality fallback: text-only when "image" isn't in
        // the modalities list.
        assert_eq!(gpt.input, vec![InputModality::Text]);
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
        let cat = build_catalog_from_json(body).expect("parses");
        assert_eq!(cat.models.len(), 1);
        let m = &cat.models[0];
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
        let cat1 = build_catalog_from_json(FIXTURE).expect("parses");
        write_catalog_atomically(&dest, &cat1).expect("writes");
        let summary = build_summary(&dest, &cat1);
        // After the write, the previous-on-disk equals the new catalog
        // (we built the summary against `dest` post-write), so nothing
        // should look added or removed.
        assert!(summary.added.is_empty());
        assert!(summary.removed.is_empty());
        assert!(summary.price_changed.is_empty());
        assert_eq!(summary.total, 2);

        // Now mutate the in-memory catalog: change a price and remove
        // one model. Diff against the on-disk previous (which is cat1).
        let mut cat2 = cat1.clone();
        cat2.models[0].cost.input = 99.0;
        cat2.models.pop();
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
}
