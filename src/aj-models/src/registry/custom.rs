//! User-authored model entries, separate from the refreshed catalog.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{CustomAuth, InputModality, ModelInfo, ModelRegistry, ReasoningOption};
use crate::types::ThinkingLevel;

/// A custom-model failure identifies the file and offending field.
#[derive(Debug, thiserror::Error)]
#[error("{path}: {field}: {message}")]
pub struct CustomModelsError {
    pub path: PathBuf,
    pub field: String,
    pub message: String,
}

#[derive(Deserialize)]
struct CustomModelsFile {
    models: Vec<ModelInfo>,
}

fn parse(file: &Path, source: &str) -> Result<Vec<ModelInfo>, CustomModelsError> {
    let error = |field: String, message: String| CustomModelsError {
        path: file.into(),
        field,
        message,
    };
    let mut json = serde_json::Deserializer::from_str(source);
    let mut track = serde_path_to_error::Track::new();
    let deserializer = serde_path_to_error::Deserializer::new(&mut json, &mut track);
    let mut unknown = None;
    let mut config: CustomModelsFile = serde_ignored::deserialize(deserializer, |path| {
        unknown.get_or_insert_with(|| path.to_string());
    })
    .map_err(|err| error(track.path().to_string(), err.to_string()))?;
    json.end()
        .map_err(|err| error("document".into(), err.to_string()))?;
    if let Some(field) = unknown {
        return Err(error(field, "unknown field".into()));
    }
    let mut identities = HashSet::new();
    for (index, model) in config.models.iter_mut().enumerate() {
        let fail =
            |field: &str, message: &str| error(format!("models[{index}].{field}"), message.into());
        // File membership supplies the anonymous default, including entries
        // copied from the catalog. It must never inherit catalog credentials.
        model
            .custom_auth
            .get_or_insert(CustomAuth { api_key_env: None });
        for (field, value) in [
            ("provider", model.provider.as_str()),
            ("id", model.id.as_str()),
            ("name", model.name.as_str()),
            ("base_url", model.base_url.as_str()),
        ]
        .into_iter()
        .chain(model.provider_name.as_deref().map(|v| ("provider_name", v)))
        {
            if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
                return Err(fail(
                    field,
                    "must be nonempty with no surrounding whitespace or control characters",
                ));
            }
        }
        if !identities.insert((model.provider.clone(), model.id.clone())) {
            return Err(fail("id", "duplicate provider/model identity"));
        }
        if !matches!(
            model.api.as_str(),
            "anthropic-messages" | "openai-completions" | "openai-responses"
        ) {
            return Err(fail(
                "api",
                "expected anthropic-messages, openai-completions, or openai-responses",
            ));
        }
        let url = reqwest::Url::parse(&model.base_url)
            .map_err(|err| fail("base_url", &err.to_string()))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(fail(
                "base_url",
                "expected an absolute HTTP(S) URL without credentials, query, or fragment",
            ));
        }
        if let Some(env) = model
            .custom_auth
            .as_ref()
            .and_then(|auth| auth.api_key_env.as_deref())
            && (env.is_empty()
                || !env.bytes().enumerate().all(|(i, c)| {
                    c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
                }))
        {
            return Err(fail(
                "custom_auth.api_key_env",
                "expected an environment variable name",
            ));
        }
        if model.input.is_empty()
            || model
                .input
                .iter()
                .enumerate()
                .any(|(i, v)| model.input[..i].contains(v))
        {
            return Err(fail(
                "input",
                "expected nonempty, unique text/image modalities",
            ));
        }
        if model.output != [InputModality::Text] {
            return Err(fail(
                "output",
                "current adapters support only [\"text\"] output",
            ));
        }
        if model.context_window == 0 {
            return Err(fail("context_window", "must be positive"));
        }
        if model.max_tokens == 0 || model.max_tokens > model.context_window {
            return Err(fail(
                "max_tokens",
                "must be positive and no greater than context_window",
            ));
        }
        if !model.reasoning && !model.reasoning_options.is_empty() {
            return Err(fail(
                "reasoning_options",
                "reasoning controls require reasoning=true",
            ));
        }
        let mut kinds = HashSet::new();
        for option in &model.reasoning_options {
            let kind = match option {
                ReasoningOption::Toggle {} => "toggle",
                ReasoningOption::Effort { values } => {
                    if values.is_empty()
                        || values
                            .iter()
                            .enumerate()
                            .any(|(i, v)| values[..i].contains(v))
                    {
                        return Err(fail(
                            "reasoning_options",
                            "effort values must be nonempty and unique",
                        ));
                    }
                    if model.api == "anthropic-messages" && values.contains(&ThinkingLevel::Minimal)
                    {
                        return Err(fail(
                            "reasoning_options",
                            "Anthropic adaptive thinking does not support minimal",
                        ));
                    }
                    "effort"
                }
                ReasoningOption::BudgetTokens { min, max } => {
                    if model.api != "anthropic-messages" {
                        return Err(fail(
                            "reasoning_options",
                            "token budgets require anthropic-messages",
                        ));
                    }
                    if min.zip(*max).is_some_and(|(min, max)| min > max) {
                        return Err(fail(
                            "reasoning_options",
                            "minimum token budget must not exceed maximum",
                        ));
                    }
                    "budget_tokens"
                }
            };
            if !kinds.insert(kind) {
                return Err(fail("reasoning_options", "duplicate reasoning control"));
            }
        }
        if model.reasoning && model.api != "anthropic-messages" && !kinds.contains("effort") {
            return Err(fail(
                "reasoning_options",
                "OpenAI-compatible reasoning requires explicit effort levels",
            ));
        }
    }
    Ok(config.models)
}

impl ModelRegistry {
    /// Merge a custom file over the active catalog. Missing files are optional.
    /// Parsing and validation finish before any registry entries change.
    pub fn load_custom_file(&mut self, path: &Path) -> Result<(), CustomModelsError> {
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(CustomModelsError {
                    path: path.into(),
                    field: "document".into(),
                    message: e.to_string(),
                });
            }
        };
        for model in parse(path, &source)? {
            if !self.by_provider.contains_key(&model.provider) {
                self.provider_order.push(model.provider.clone());
            }
            let entries = self.by_provider.entry(model.provider.clone()).or_default();
            if !entries.contains_key(&model.id) {
                self.model_order
                    .entry(model.provider.clone())
                    .or_default()
                    .push(model.id.clone());
            }
            entries.insert(model.id.clone(), model);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ModelCost, supported_thinking_levels};
    use serde_json::{Value, json};

    fn config() -> Value {
        serde_json::from_str(CONFIG).unwrap()
    }

    fn parsed(value: Value) -> Vec<ModelInfo> {
        parse(Path::new("models-custom.json"), &value.to_string()).unwrap()
    }

    const CONFIG: &str = r#"
{
  "models": [
    {
      "id": "qwen3.8-27b",
      "name": "Qwen 3.8 27B",
      "provider": "local",
      "provider_name": "llama.cpp",
      "api": "openai-responses",
      "base_url": "https://llm.tailfb5ac6.ts.net/v1",
      "custom_auth": {
        "api_key_env": null
      },
      "supports_tools": true,
      "input": [
        "text",
        "image"
      ],
      "output": [
        "text"
      ],
      "reasoning": true,
      "reasoning_options": [
        {
          "type": "toggle"
        },
        {
          "type": "effort",
          "values": [
            "low",
            "medium",
            "xhigh"
          ]
        }
      ],
      "context_window": 49152,
      "max_tokens": 16384
    }
  ]
}
"#;

    #[test]
    fn catalog_entries_preserve_metadata_and_default_to_anonymous_zero_cost() {
        let mut value = config();
        value["models"][0]
            .as_object_mut()
            .unwrap()
            .remove("custom_auth");
        let mut model = parsed(value).remove(0);
        assert_eq!(model.custom_auth, Some(CustomAuth { api_key_env: None }));
        assert_eq!(model.cost, ModelCost::default());
        assert_eq!(model.provider_name.as_deref(), Some("llama.cpp"));
        assert_eq!(
            supported_thinking_levels(&model),
            [
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::XHigh,
            ]
        );
        // Serialize the real catalog type rather than maintaining a second
        // input schema. Optional catalog metadata must survive unchanged.
        model.family = Some("qwen".into());
        model.supports_verbosity = true;
        model.cost.input = 0.5;
        model.custom_auth.as_mut().unwrap().api_key_env = Some("LOCAL_KEY".into());
        let serialized = serde_json::to_value(&model).unwrap();
        assert_eq!(
            serde_json::to_value(parsed(json!({"models": [serialized.clone()]})).remove(0))
                .unwrap(),
            serialized,
        );
        let mut explicit_null = config();
        explicit_null["models"][0]["custom_auth"] = Value::Null;
        assert!(parsed(explicit_null)[0].custom_auth.is_some());
        assert!(parsed(json!({"models": []})).is_empty());
    }

    #[test]
    fn validation_identifies_file_and_field() {
        for (pointer, value, field) in [
            ("/api", json!("unknown"), "api"),
            ("/base_url", json!("file:///tmp/model"), "base_url"),
            (
                "/base_url",
                json!("https://user:pass@example.org"),
                "base_url",
            ),
            (
                "/base_url",
                json!("https://example.org/?key=secret"),
                "base_url",
            ),
            (
                "/custom_auth/api_key_env",
                json!("bad-key"),
                "custom_auth.api_key_env",
            ),
            ("/name", json!(" "), "name"),
            ("/supports_tools", json!(1), "supports_tools"),
            ("/input", json!([]), "input"),
            ("/input", json!(["audio"]), "input"),
            ("/input", json!(["text", "text"]), "input"),
            ("/output", json!(["image"]), "output"),
            ("/context_window", json!(0), "context_window"),
            ("/max_tokens", json!(99999), "max_tokens"),
            ("/max_tokens", json!(-1), "max_tokens"),
            ("/reasoning", json!(false), "reasoning_options"),
            (
                "/reasoning_options/1/values",
                json!(["low", "low"]),
                "reasoning_options",
            ),
            (
                "/reasoning_options",
                json!([{"type":"toggle"}]),
                "reasoning_options",
            ),
            (
                "/reasoning_options",
                json!([{"type":"budget_tokens","min":2,"max":1}]),
                "reasoning_options",
            ),
        ] {
            let mut source = config();
            *source["models"][0].pointer_mut(pointer).unwrap() = value;
            let error = parse(
                Path::new("/fixture/models-custom.json"),
                &source.to_string(),
            )
            .unwrap_err();
            assert_eq!(error.path, Path::new("/fixture/models-custom.json"));
            assert!(error.field.contains(field), "{pointer}: {error}");
        }
        for source in ["{", r#"{"models":[]} trailing"#, "{}"] {
            assert!(parse(Path::new("models-custom.json"), source).is_err());
        }
        for pointer in [
            "/models/0",
            "/models/0/custom_auth",
            "/models/0/reasoning_options/0",
            "/models/0/reasoning_options/1",
            "",
        ] {
            let mut source = config();
            source
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("typo".into(), json!(true));
            let error = parse(Path::new("models-custom.json"), &source.to_string()).unwrap_err();
            assert!(error.to_string().contains("typo"), "{error}");
        }
        let mut duplicate = config();
        duplicate["models"]
            .as_array_mut()
            .unwrap()
            .push(config()["models"][0].clone());
        assert!(
            parse(Path::new("models-custom.json"), &duplicate.to_string())
                .unwrap_err()
                .message
                .contains("duplicate")
        );
    }

    #[test]
    fn catalog_reasoning_controls_are_accepted() {
        let mut value = config();
        // The catalog schema permits off directly in an effort vocabulary.
        value["models"][0]["reasoning_options"] = json!([{"type":"effort","values":["off","low"]}]);
        assert_eq!(
            supported_thinking_levels(&parsed(value.clone())[0]),
            [ThinkingLevel::Off, ThinkingLevel::Low]
        );
        value["models"][0]["api"] = json!("anthropic-messages");
        value["models"][0]["reasoning_options"] =
            json!([{"type":"budget_tokens","min":1024,"max":8192}]);
        assert!(!crate::registry::supports_adaptive_thinking(
            &parsed(value)[0]
        ));
    }

    #[test]
    fn merge_replaces_identity_preserves_neighbors_and_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models-custom.json");
        let mut registry = ModelRegistry::load_catalog();
        let catalog_model = registry.models("openai")[0].clone();
        let before = registry.models("openai").len();
        let updated_at = registry.updated_at;
        registry.load_custom_file(&path).unwrap();
        let mut source = config();
        source["models"][0]["provider"] = json!("openai");
        source["models"][0]["id"] = json!(catalog_model.id);
        std::fs::write(&path, source.to_string()).unwrap();
        registry.load_custom_file(&path).unwrap();
        registry.load_custom_file(&path).unwrap();
        assert_eq!(registry.models("openai").len(), before);
        assert_eq!(
            registry.get("openai", &catalog_model.id).unwrap().name,
            "Qwen 3.8 27B"
        );
        assert_eq!(registry.updated_at, updated_at);
        assert!(
            registry
                .models("openai")
                .iter()
                .filter(|m| m.id != catalog_model.id)
                .all(|m| m.custom_auth.is_none())
        );
        let mut invalid = config();
        let mut bad = invalid["models"][0].clone();
        bad["id"] = json!("invalid");
        bad["api"] = json!("invalid");
        invalid["models"].as_array_mut().unwrap().push(bad);
        std::fs::write(&path, invalid.to_string()).unwrap();
        assert!(registry.load_custom_file(&path).is_err());
        assert!(registry.get("local", "qwen3.8-27b").is_none());
        std::fs::write(&path, CONFIG).unwrap();
        registry.load_custom_file(&path).unwrap();
        assert_eq!(registry.providers().last(), Some(&"local"));
        assert_eq!(registry.models("local").len(), 1);
        assert!(registry.load_custom_file(dir.path()).is_err());
    }
}
