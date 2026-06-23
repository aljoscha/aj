use serde::{
    Deserialize, Serialize,
    de::{self, Unexpected},
};
use serde_json::Value;
use thiserror::Error;

// Error types

#[derive(Serialize, Deserialize, Clone, Debug, Error)]
#[error("OpenAI API error: {message}")]
pub struct ApiError {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    /// Provider error code. Deserialized from either a JSON string or
    /// number (OpenRouter sends numeric codes like `402`). Always
    /// stored as a string.
    #[serde(
        default,
        deserialize_with = "deserialize_code",
        skip_serializing_if = "Option::is_none"
    )]
    pub code: Option<String>,
}

/// Deserialize a provider error code from either a JSON string or
/// number. OpenRouter sends numeric codes (e.g. `402`) while OpenAI
/// itself sends string codes (e.g. `"context_length_exceeded"`).
fn deserialize_code<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: de::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(s) => Ok(Some(s)),
        Value::Number(n) => Ok(Some(n.to_string())),
        Value::Null => Ok(None),
        _ => Err(de::Error::invalid_type(
            Unexpected::Other("a non-string, non-number value"),
            &"a string or number",
        )),
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiErrorResponse {
    pub error: ApiError,
}

// Shared enums

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ReasoningEffort {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "minimal")]
    Minimal,
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Verbosity {
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum PromptCacheRetention {
    #[serde(rename = "in-memory")]
    InMemory,
    #[serde(rename = "24h")]
    TwentyFourHours,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ServiceTier {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "flex")]
    Flex,
    #[serde(rename = "scale")]
    Scale,
    #[serde(rename = "priority")]
    Priority,
}

// Shared structured output types

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JsonSchemaDefinition {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_display_is_stable() {
        // ClientError composes this string, so keep it byte-stable.
        let error = ApiError {
            message: "boom".to_string(),
            r#type: None,
            param: None,
            code: None,
        };
        assert_eq!(error.to_string(), "OpenAI API error: boom");
    }

    #[test]
    fn error_code_string() {
        let error: ApiError =
            serde_json::from_str(r#"{"message": "boom", "code": "context_length_exceeded"}"#)
                .unwrap();
        assert_eq!(error.code.as_deref(), Some("context_length_exceeded"));
    }

    #[test]
    fn error_code_number() {
        // OpenRouter sends numeric codes like `402`.
        let error: ApiError = serde_json::from_str(r#"{"message": "boom", "code": 402}"#).unwrap();
        assert_eq!(error.code.as_deref(), Some("402"));
    }

    #[test]
    fn error_code_null() {
        let error: ApiError = serde_json::from_str(r#"{"message": "boom", "code": null}"#).unwrap();
        assert_eq!(error.code, None);
    }

    #[test]
    fn error_code_absent() {
        let error: ApiError = serde_json::from_str(r#"{"message": "boom"}"#).unwrap();
        assert_eq!(error.code, None);
    }

    #[test]
    fn error_code_invalid_type() {
        assert!(
            serde_json::from_str::<ApiError>(r#"{"message": "boom", "code": ["nope"]}"#).is_err()
        );
    }
}
