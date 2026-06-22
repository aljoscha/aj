//! Live integration test for OpenRouter via the OpenAI Responses
//! provider. Gated behind `OPENROUTER_API_KEY` and `#[ignore]`d so it
//! never runs in CI. Run manually with:
//!
//! ```text
//! OPENROUTER_API_KEY=... cargo test -p aj-models --test openrouter_live -- --ignored --nocapture
//! ```
//!
//! It exercises the same path the binary uses: a `ModelInfo` pointed at
//! OpenRouter's Responses endpoint, driven through the registered
//! `openai-responses` provider.

use aj_models::provider::complete_simple;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::types::{
    AssistantContent, Context, Message, SimpleStreamOptions, StopReason, StreamOptions,
    TextContent, ThinkingLevel, ToolChoice, ToolDefinition, UserContent, UserMessage,
};

/// A free, tool- and reasoning-capable model. If OpenRouter retires it,
/// pick another `:free` model that lists `tools` in its capabilities.
const MODEL_ID: &str = "openai/gpt-oss-20b:free";

fn model() -> ModelInfo {
    ModelInfo {
        id: MODEL_ID.into(),
        name: MODEL_ID.into(),
        api: "openai-responses".into(),
        provider: "openrouter".into(),
        base_url: "https://openrouter.ai/api/v1".into(),
        reasoning: true,
        supports_adaptive_thinking: false,
        supports_verbosity: false,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 131_072,
        max_tokens: 32_768,
        headers: None,
    }
}

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        content: vec![UserContent::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        timestamp: 0,
    })
}

fn options(key: String) -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: StreamOptions {
            api_key: Some(key),
            max_tokens: Some(2000),
            tool_choice: Some(ToolChoice::Auto),
            ..Default::default()
        },
        reasoning: Some(ThinkingLevel::Low),
    }
}

#[tokio::test]
#[ignore = "live network; requires OPENROUTER_API_KEY"]
async fn responses_text_and_reasoning() {
    let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY");
    let ctx = Context {
        system_prompt: Some("You are a terse assistant.".into()),
        messages: vec![user("What is 17 times 23? Show brief reasoning.")],
        tools: vec![],
    };

    let msg = complete_simple(&model(), &ctx, &options(key)).await;

    assert_eq!(msg.stop_reason, StopReason::Stop, "error: {:?}", msg.error);
    assert!(
        msg.content
            .iter()
            .any(|b| matches!(b, AssistantContent::Text(t) if !t.text.is_empty())),
        "expected non-empty assistant text"
    );
}

#[tokio::test]
#[ignore = "live network; requires OPENROUTER_API_KEY"]
async fn responses_tool_call() {
    let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY");
    let ctx = Context {
        system_prompt: Some("Use tools when appropriate.".into()),
        messages: vec![user(
            "What is the weather in Paris? Call the get_weather tool.",
        )],
        tools: vec![ToolDefinition {
            name: "get_weather".into(),
            description: "Get the current weather for a city.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        }],
    };

    let msg = complete_simple(&model(), &ctx, &options(key)).await;

    assert_eq!(
        msg.stop_reason,
        StopReason::ToolUse,
        "error: {:?}",
        msg.error
    );
    assert!(
        msg.content
            .iter()
            .any(|b| matches!(b, AssistantContent::ToolCall(tc) if tc.name == "get_weather")),
        "expected a get_weather tool call"
    );
}
