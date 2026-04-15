use async_openai::config::Config;
use async_openai::types::{
    ChatCompletionMessageToolCall, ChatCompletionRequestAssistantMessage,
    ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessage, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, ChatCompletionStreamOptions, ChatCompletionTool,
    ChatCompletionToolType, CompletionUsage, CreateChatCompletionRequest,
    CreateChatCompletionStreamResponse, FinishReason as OpenAIFinishReason, FunctionCall,
    FunctionObject, ServiceTierResponse,
};
use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use async_stream::stream;
use futures::{Stream, StreamExt};
use std::collections::{HashMap, hash_map};
use std::pin::Pin;

use crate::conversation::{Conversation, ConversationEntry, ConversationEntryKind};
use crate::messages::{
    ContentBlock, ContentBlockParam, Message, MessageParam, MessageType, Role, ServiceTier,
    StopReason, Usage, UsageDelta,
};
use crate::streaming::StreamingEvent;
use crate::tools::Tool;
use crate::{Model, ModelArgs, ModelError, ThinkingConfig};

const DEFAULT_MODEL: &str = "gpt-4o";

/// OpenAI GPT model implementation
pub struct OpenAiModel {
    client: OpenAIClient<OpenAIConfig>,
    model_name: String,
}

impl OpenAiModel {
    pub fn new(model_args: ModelArgs, api_key: String) -> Self {
        let config = OpenAIConfig::new().with_api_key(api_key);

        let config = if let Some(base_url) = model_args.url {
            config.with_api_base(base_url)
        } else {
            config
        };

        let client = OpenAIClient::with_config(config);
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
        _thinking: Option<ThinkingConfig>,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamingEvent> + Send>>, ModelError> {
        // Convert system prompt to system message
        let mut messages = vec![ChatCompletionRequestMessage::System(
            ChatCompletionRequestSystemMessage {
                content: system_prompt.into(),
                name: None,
            },
        )];

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
        let openai_tools: Vec<ChatCompletionTool> = tools.into_iter().map(|t| t.into()).collect();

        // Build the request
        let request = CreateChatCompletionRequest {
            model: self.model_name.clone(),
            messages,
            max_tokens: Some(32_000u32),
            stream: Some(true),
            stream_options: Some(ChatCompletionStreamOptions {
                include_usage: true,
            }),
            tools: Some(openai_tools),
            ..Default::default()
        };

        // tracing::debug!("open ai request: {:#?}", request);

        // Note: OpenAI doesn't have a direct equivalent to Anthropic's thinking mode
        // We ignore the thinking parameter for now

        let response_stream = self
            .client
            .chat()
            .create_stream(request)
            .await
            .map_err(|e| ModelError::Client(anyhow::anyhow!(e)))?;

        // Use our stream processor to convert OpenAI format to internal format
        let processor = OpenAiStreamProcessor::new();
        let stream = processor.process_stream(response_stream);

        Ok(stream.boxed())
    }

    fn model_name(&self) -> String {
        self.model_name.to_string()
    }

    fn model_url(&self) -> String {
        self.client.config().api_base().to_string()
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
                            let user_message = ChatCompletionRequestUserMessage {
                                content: ChatCompletionRequestUserMessageContent::Text(
                                    text.clone(),
                                ),
                                name: None,
                            };

                            let user_message = ChatCompletionRequestMessage::User(user_message);

                            result.push(user_message);
                        }
                        ContentBlockParam::ToolResultBlock {
                            tool_use_id,
                            content,
                            is_error: _,
                        } => {
                            let tool_message = ChatCompletionRequestToolMessage {
                                content: ChatCompletionRequestToolMessageContent::Text(
                                    content.text(),
                                ),
                                tool_call_id: tool_use_id.clone(),
                            };

                            let user_message = ChatCompletionRequestMessage::Tool(tool_message);

                            result.push(user_message);
                        }
                        other => panic!("unexpected content block for user role: {:?}", other),
                    }
                }
            }
            Role::Assistant => {
                for content_block in message.content.iter() {
                    match content_block {
                        #[allow(deprecated)]
                        ContentBlockParam::TextBlock { text, citations: _ } => {
                            let assistant_message = ChatCompletionRequestAssistantMessage {
                                content: Some(ChatCompletionRequestAssistantMessageContent::Text(
                                    text.clone(),
                                )),
                                refusal: None,
                                function_call: None,
                                tool_calls: None,
                                name: None,
                            };

                            let assistant_message =
                                ChatCompletionRequestMessage::Assistant(assistant_message);

                            result.push(assistant_message);
                        }
                        #[allow(deprecated)]
                        ContentBlockParam::ToolUseBlock {
                            id, input, name, ..
                        } => {
                            let tool_calls = vec![ChatCompletionMessageToolCall {
                                id: id.clone(),
                                r#type: ChatCompletionToolType::Function,
                                function: FunctionCall {
                                    name: name.clone(),
                                    arguments: input.to_string(),
                                },
                            }];
                            let assistant_message = ChatCompletionRequestAssistantMessage {
                                content: None,
                                refusal: None,
                                function_call: None,
                                tool_calls: Some(tool_calls),
                                name: None,
                            };
                            let assistant_message =
                                ChatCompletionRequestMessage::Assistant(assistant_message);

                            result.push(assistant_message);
                        }
                        other => panic!("unexpected content block for user role: {:?}", other),
                    }
                }
            }
        }

        result
    }
}

impl From<&MessageParam> for ChatCompletionRequestMessage {
    fn from(message: &MessageParam) -> Self {
        match message.role {
            Role::User => {
                // Combine all content blocks into a single string for OpenAI
                let content = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlockParam::TextBlock { text, .. } => Some(text.clone()),
                        ContentBlockParam::ToolResultBlock { content, .. } => Some(content.text()),
                        // For other types, we'll need more sophisticated handling
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: content.into(),
                    name: None,
                })
            }
            Role::Assistant => {
                // Extract text content and tool calls
                let content = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlockParam::TextBlock { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                let tool_calls: Vec<ChatCompletionMessageToolCall> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlockParam::ToolUseBlock {
                            id, name, input, ..
                        } => Some(ChatCompletionMessageToolCall {
                            id: id.clone(),
                            r#type: ChatCompletionToolType::Function,
                            function: async_openai::types::FunctionCall {
                                name: name.clone(),
                                arguments: input.to_string(),
                            },
                        }),
                        _ => None,
                    })
                    .collect();

                #[allow(deprecated)]
                ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
                    content: if content.is_empty() {
                        None
                    } else {
                        Some(content.into())
                    },
                    name: None,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    function_call: None,
                    refusal: None,
                })
            }
        }
    }
}

impl From<Tool> for ChatCompletionTool {
    fn from(tool: Tool) -> Self {
        Self {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: tool.name,
                description: Some(tool.description),
                parameters: Some(tool.input_schema),
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

impl From<ServiceTierResponse> for ServiceTier {
    fn from(tier: ServiceTierResponse) -> Self {
        // TODO: This translation might not be correct...
        match tier {
            ServiceTierResponse::Scale => Self::Priority,
            ServiceTierResponse::Default => Self::Standard,
        }
    }
}

impl From<CompletionUsage> for UsageDelta {
    fn from(usage: CompletionUsage) -> Self {
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

    pub fn process_stream<S>(mut self, stream: S) -> impl Stream<Item = StreamingEvent>
    where
        S: Stream<
            Item = Result<CreateChatCompletionStreamResponse, async_openai::error::OpenAIError>,
        >,
    {
        stream! {
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
        }
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
            let tool_calls = choice.delta.tool_calls.unwrap_or_default();

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
