use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;

use crate::conversation::Conversation;
use crate::streaming::StreamingEvent;
use crate::tools::Tool;

pub mod anthropic;
pub mod conversation;
pub mod messages;
pub mod openai;
pub mod streaming;
pub mod tools;
pub mod types;

/// Trait for LLM model providers
#[async_trait::async_trait]
pub trait Model: Send + Sync {
    /// Run streaming inference with the given messages
    async fn run_inference_streaming(
        &self,
        conversation: &Conversation,
        system_prompt: String,
        tools: Vec<Tool>,
        thinking: Option<ThinkingConfig>,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamingEvent> + Send>>, ModelError>;

    /// Returns the name of the model.
    fn model_name(&self) -> String;

    /// Returns the URL of the model endpoint.
    fn model_url(&self) -> String;
}

/// Thinking configuration for models that support it
#[derive(Debug, Clone)]
pub enum ThinkingConfig {
    Low,
    Medium,
    High,
}

/// Configuration arguments for model client creation.
#[derive(Debug, Clone)]
pub struct ModelArgs {
    /// The model API to use (e.g., "anthropic")
    pub api: String,
    /// Optional custom endpoint URL
    pub url: Option<String>,
    /// Optional model name to use
    pub model_name: Option<String>,
}

/// Create a model instance based on the provided [ModelArgs].
pub fn create_model(model_args: ModelArgs) -> Result<Arc<dyn Model>, ModelError> {
    match model_args.api.as_str() {
        "anthropic" => {
            let api_key = std::env::var("ANTHROPIC_OAUTH_TOKEN")
                .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
                .map_err(|_| {
                    ModelError::Client(anyhow::anyhow!(
                        "Neither ANTHROPIC_OAUTH_TOKEN nor ANTHROPIC_API_KEY found in environment variables"
                    ))
                })?;

            let model = crate::anthropic::AnthropicModel::new(model_args, api_key);

            Ok(Arc::new(model))
        }
        "openai" => {
            let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
                ModelError::Client(anyhow::anyhow!(
                    "OPENAI_API_KEY not found in environment variables"
                ))
            })?;

            let model = crate::openai::OpenAiModel::new(model_args, api_key);

            Ok(Arc::new(model))
        }
        _ => Err(ModelError::Client(anyhow::anyhow!(
            "Unsupported model API: {}",
            model_args.api
        ))),
    }
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("Client error: {0}")]
    Client(#[from] anyhow::Error),
    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),
}
