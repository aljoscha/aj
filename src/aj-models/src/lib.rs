use futures::Stream;
use std::pin::Pin;
use thiserror::Error;

use crate::conversation::Conversation;
use crate::streaming::StreamingEvent;
use crate::tools::Tool;

pub mod anthropic;
pub mod conversation;
pub mod messages;
pub mod streaming;
pub mod tools;

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
    ) -> Result<Pin<Box<dyn Stream<Item = StreamingEvent> + Send + '_>>, ModelError>;
}

/// Thinking configuration for models that support it
#[derive(Debug, Clone)]
pub enum ThinkingConfig {
    Enabled { budget_tokens: u64 },
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("Client error: {0}")]
    Client(#[from] anyhow::Error),
    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),
}

// /// Convert a list of conversation messages to message parameters
// pub fn to_message_params(messages: &[ConversationMessage]) -> Vec<MessageParam> {
//     messages
//         .iter()
//         .map(|msg| MessageParam {
//             role: msg.role.clone(),
//             content: msg.content.clone(),
//         })
//         .collect()
// }
