use async_stream::stream;
use futures::{Stream, StreamExt};
use std::collections::{HashMap, hash_map};
use std::pin::Pin;

use openai_sdk::client::{Client as OpenAIClient, ClientError};
use openai_sdk::types::{
    ChatCompletionRequestMessage, ChatCompletionTextContent, ChatCompletionUserContent,
    CreateChatCompletionRequest, CreateChatCompletionStreamResponse,
    FinishReason as OpenAIFinishReason, FunctionCall, FunctionDefinition, ReasoningEffort,
    ServiceTier as OpenAIServiceTier, StreamOptions, Tool as OpenAITool,
    ToolCall as OpenAIToolCall, Usage as OpenAIUsage,
};

use crate::conversation::{Conversation, ConversationEntry, ConversationEntryKind};
use crate::messages::{
    ContentBlock, ContentBlockParam, Message, MessageType, Role, ServiceTier, StopReason, Usage,
    UsageDelta,
};
use crate::streaming::StreamingEvent;
use crate::tools::Tool;
use crate::{Model, ModelArgs, ModelError, ThinkingConfig};

const DEFAULT_MODEL: &str = "gpt-5";

/// OpenAI GPT model implementation using the in-tree openai-sdk crate.
pub struct OpenAiModel {
    client: OpenAIClient,
    model_name: String,
}

impl OpenAiModel {
    pub fn new(model_args: ModelArgs, api_key: String) -> Self {
        let client = OpenAIClient::new(model_args.url, api_key);

        let model_name = model_args
            .model_name
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        Self { client, model_name }
    }
}

#[async_trait::async_trait]
impl Model for OpenAiModel {
    async fn run_inference_streaming(
        &self,
        conversation: &Conversation,
        system_prompt: String,
        tools: Vec<Tool>,
        thinking: Option<ThinkingConfig>,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamingEvent> + Send>>, ModelError> {
        // Convert system prompt to system message
        let mut messages = vec![ChatCompletionRequestMessage::System {
            content: ChatCompletionTextContent::String(system_prompt),
        }];

        // Convert conversation entries to OpenAI messages
        let conv_messages: Vec<ChatCompletionRequestMessage> = conversation
            .entries()
            .iter()
            .flat_map(
                |entry: &ConversationEntry| -> Vec<ChatCompletionRequestMessage> { entry.into() },
            )
            .collect();

        messages.extend(conv_messages);

        // Convert tools to OpenAI format
        let openai_tools: Vec<OpenAITool> = tools.into_iter().map(|t| t.into()).collect();

        // Build the request
        let request = CreateChatCompletionRequest {
            model: self.model_name.clone(),
            messages,
            max_completion_tokens: Some(32_000),
            stream: Some(true),
            tools: openai_tools,
            parallel_tool_calls: Some(true),
            reasoning_effort: thinking.map(Into::into),
            stream_options: Some(StreamOptions {
                include_usage: Some(true),
            }),
            ..Default::default()
        };

        // Get the streaming response
        let stream = self
            .client
            .chat_completions_stream(request)
            .await
            .map_err(|e| ModelError::Client(anyhow::anyhow!(e)))?;

        // Use our stream processor to convert OpenAI format to internal format
        let processor = OpenAiStreamProcessor::new();
        let processed_stream = processor.process_stream(stream);

        Ok(processed_stream)
    }

    fn model_name(&self) -> String {
        self.model_name.to_string()
    }

    fn model_url(&self) -> String {
        self.client.base_url()
    }
}

impl From<&ConversationEntry> for Vec<ChatCompletionRequestMessage> {
    fn from(entry: &ConversationEntry) -> Self {
        let message = match &entry.entry {
            ConversationEntryKind::Message(message) => message,
            ConversationEntryKind::UserOutput(_) => return Vec::new(),
        };

        let mut result = Vec::new();
        match message.role {
            Role::User => {
                for content_block in message.content.iter() {
                    match content_block {
                        ContentBlockParam::TextBlock { text, citations: _ } => {
                            result.push(ChatCompletionRequestMessage::User {
                                content: ChatCompletionUserContent::String(text.clone()),
                            });
                        }
                        ContentBlockParam::ToolResultBlock {
                            tool_use_id,
                            content,
                            is_error: _,
                        } => {
                            result.push(ChatCompletionRequestMessage::Tool {
                                content: ChatCompletionTextContent::String(content.text()),
                                tool_call_id: tool_use_id.clone(),
                            });
                        }
                        other => panic!("unexpected content block for user role: {:?}", other),
                    }
                }
            }
            Role::Assistant => {
                let text_content: Vec<String> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlockParam::TextBlock { text, citations: _ } => Some(text.clone()),
                        _ => None,
                    })
                    .collect();

                let tool_calls: Vec<OpenAIToolCall> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlockParam::ToolUseBlock {
                            id, input, name, ..
                        } => Some(OpenAIToolCall::Function {
                            id: id.clone(),
                            function: FunctionCall {
                                name: name.clone(),
                                arguments: input.to_string(),
                            },
                        }),
                        _ => None,
                    })
                    .collect();

                result.push(ChatCompletionRequestMessage::Assistant {
                    content: if text_content.is_empty() {
                        None
                    } else {
                        Some(text_content.join("\n"))
                    },
                    tool_calls,
                });
            }
        }

        result
    }
}

impl From<Tool> for OpenAITool {
    fn from(tool: Tool) -> Self {
        OpenAITool::Function {
            function: FunctionDefinition {
                name: tool.name,
                description: Some(tool.description),
                parameters: tool.input_schema,
                strict: None,
            },
        }
    }
}

impl From<&OpenAIFinishReason> for StopReason {
    fn from(reason: &OpenAIFinishReason) -> Self {
        match reason {
            OpenAIFinishReason::Stop => Self::EndTurn,
            OpenAIFinishReason::Length => Self::MaxTokens,
            OpenAIFinishReason::ToolCalls => Self::ToolUse,
            OpenAIFinishReason::ContentFilter => Self::Refusal,
            OpenAIFinishReason::FunctionCall => Self::ToolUse,
        }
    }
}

impl From<OpenAIServiceTier> for ServiceTier {
    fn from(tier: OpenAIServiceTier) -> Self {
        match tier {
            OpenAIServiceTier::Auto => Self::Standard,
            OpenAIServiceTier::Default => Self::Standard,
            OpenAIServiceTier::Flex => Self::Standard,
            OpenAIServiceTier::Scale => Self::Priority,
            OpenAIServiceTier::Priority => Self::Priority,
        }
    }
}

impl From<OpenAIUsage> for UsageDelta {
    fn from(usage: OpenAIUsage) -> Self {
        let cache_read_input_tokens = if let Some(details) = usage.prompt_tokens_details {
            details.cached_tokens.map(u64::from)
        } else {
            None
        };

        Self {
            cache_creation: None,
            cache_creation_input_tokens: None,
            cache_read_input_tokens,
            input_tokens: Some(u64::from(usage.prompt_tokens)),
            output_tokens: Some(u64::from(usage.completion_tokens)),
            server_tool_use: None,
            service_tier: None,
        }
    }
}

impl From<ThinkingConfig> for ReasoningEffort {
    fn from(thinking: ThinkingConfig) -> Self {
        match thinking {
            ThinkingConfig::Low => ReasoningEffort::Low,
            ThinkingConfig::Medium => ReasoningEffort::Medium,
            // OpenAI only supports low/medium/high; XHigh maps to High.
            ThinkingConfig::High | ThinkingConfig::XHigh => ReasoningEffort::High,
        }
    }
}

/// OpenAI stream processor to convert OpenAI's streaming format to our internal
/// format.
#[derive(Debug)]
pub struct OpenAiStreamProcessor {
    current_message: Option<Message>,
    text_content: HashMap<u32, String>,
    tool_calls: HashMap<i32, PartialToolCall>,
}

#[derive(Debug, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl OpenAiStreamProcessor {
    pub fn new() -> Self {
        Self {
            current_message: None,
            text_content: Default::default(),
            tool_calls: Default::default(),
        }
    }

    pub fn process_stream<S>(
        mut self,
        stream: S,
    ) -> Pin<Box<dyn Stream<Item = StreamingEvent> + Send>>
    where
        S: Stream<Item = Result<CreateChatCompletionStreamResponse, ClientError>> + Send + 'static,
    {
        let stream = stream! {
            let mut stream = std::pin::pin!(stream);

            while let Some(chunk_result) = futures::StreamExt::next(&mut stream).await {
                match chunk_result {
                    Ok(chunk) => {
                        // println!("{:?}", chunk);
                        let events = self.process_chunk(chunk);
                        for event in events {
                            // tracing::debug!(?event, "openai stream processor");
                            yield event;
                        }
                    }
                    Err(e) => {
                        yield StreamingEvent::ProtocolError {
                            error: format!("OpenAI stream error: {}", e),
                        };
                    }
                }
            }

            for final_event in self.finalize() {
                yield final_event;
            }
        };

        stream.boxed()
    }

    fn process_chunk(&mut self, chunk: CreateChatCompletionStreamResponse) -> Vec<StreamingEvent> {
        let mut events = Vec::new();

        // Initialize message if this is the first chunk
        if self.current_message.is_none() && !chunk.id.is_empty() {
            let mut usage = Usage::default();
            usage.service_tier = chunk.service_tier.map(Into::into);
            let message = Message {
                id: chunk.id.clone(),
                r#type: MessageType::Message,
                role: Role::Assistant,
                content: Vec::new(),
                model: chunk.model,
                stop_reason: None,
                stop_sequence: None,
                usage,
                container: None,
            };

            self.current_message = Some(message.clone());
            events.push(StreamingEvent::MessageStart { message });
        }

        if let Some(usage) = chunk.usage {
            let usage_delta: UsageDelta = usage.into();

            if let Some(current_message) = self.current_message.as_mut() {
                current_message.usage.add(&usage_delta);
            }

            events.push(StreamingEvent::UsageUpdate { usage: usage_delta });
            return events;
        }

        assert!(chunk.choices.len() <= 1, "we implicitly give n=1");
        for choice in chunk.choices {
            if let Some(finish_reason) = choice.finish_reason {
                // We don't finish. We requested a usage update, which we will
                // get as the last chunk, after the last message with a stop
                // reason.
                self.current_message
                    .as_mut()
                    .expect("missing message")
                    .stop_reason
                    .replace((&finish_reason).into());
            }

            if let Some(content) = choice.delta.content {
                let current_text = self.text_content.entry(choice.index);

                match current_text {
                    hash_map::Entry::Occupied(mut occupied_entry) => {
                        occupied_entry.get_mut().push_str(&content);
                    }
                    hash_map::Entry::Vacant(vacant_entry) => {
                        if content.len() > 0 {
                            vacant_entry.insert(content);
                        }
                    }
                }
            }
            let tool_calls = choice.delta.tool_calls;

            for tool_call in tool_calls {
                let partial_call =
                    self.tool_calls
                        .entry(tool_call.index)
                        .or_insert_with(|| PartialToolCall {
                            id: "".to_string(),
                            name: "".to_string(),
                            arguments: "".to_string(),
                        });

                if let Some(id) = tool_call.id {
                    partial_call.id.push_str(&id);
                }
                if let Some(function) = tool_call.function {
                    if let Some(name) = function.name {
                        partial_call.name.push_str(&name);
                    }
                    if let Some(arguments) = function.arguments {
                        partial_call.arguments.push_str(&arguments);
                    }
                }
            }
        }

        events
    }

    fn finalize(&mut self) -> Vec<StreamingEvent> {
        let mut final_events = Vec::new();

        let mut message = if let Some(message) = self.current_message.take() {
            message
        } else {
            return final_events;
        };

        let mut text_ids: Vec<_> = self.text_content.keys().copied().collect();
        text_ids.sort();
        for text_id in text_ids {
            let text = self.text_content.remove(&text_id).expect("known to exist");

            if text.len() > 0 {
                message.content.push(ContentBlock::TextBlock {
                    text: text.clone(),
                    citations: Vec::new(),
                });

                final_events.push(StreamingEvent::TextStart {
                    text: text.clone(),
                    citations: Vec::new(),
                });
                final_events.push(StreamingEvent::TextStop { text });
            }
        }

        let mut tool_call_ids: Vec<_> = self.tool_calls.keys().copied().collect();
        tool_call_ids.sort();

        for tool_call_id in tool_call_ids {
            let tool_call = self
                .tool_calls
                .remove(&tool_call_id)
                .expect("known to exist");

            let input = match serde_json::from_str(&tool_call.arguments) {
                Ok(input) => input,
                Err(e) => {
                    final_events.push(StreamingEvent::ParseError {
                        error: format!("failed to parse tool use input json: {e}"),
                        raw_data: tool_call.arguments,
                    });
                    continue;
                }
            };

            message.content.push(ContentBlock::ToolUseBlock {
                id: tool_call.id,
                name: tool_call.name,
                input,
            });
        }

        final_events.push(StreamingEvent::FinalizedMessage { message });

        final_events
    }
}
