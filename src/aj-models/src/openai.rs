use async_stream::stream;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;

use openai_sdk::client::{Client as OpenAIClient, ClientError};
use openai_sdk::types::common::{ReasoningEffort, ServiceTier as OpenAIServiceTier};
use openai_sdk::types::responses::{
    CreateResponseRequest, FunctionCallOutputContent, ImageDetail, InputRole, ItemStatus,
    MessagePhase, Reasoning, ReasoningSummary, ReasoningSummaryMode, Response, ResponseIncludable,
    ResponseInput, ResponseInputContentPart, ResponseInputItem, ResponseInputMessageContent,
    ResponseOutputContent, ResponseOutputItem, ResponseStatus, ResponseStreamEvent, ResponseTool,
    ResponseUsage,
};

use crate::conversation::{Conversation, ConversationEntry, ConversationEntryKind};
use crate::messages::{
    ApiError, ContentBlock, ContentBlockParam, DocumentSource, ImageSource, Message, MessageType,
    Role, ServiceTier, StopReason, ToolResultContent, Usage, UsageDelta,
};
use crate::streaming::StreamingEvent;
use crate::tools::Tool;
use crate::{Model, ModelArgs, ModelError, ThinkingConfig};

const DEFAULT_MODEL: &str = "gpt-5";
const OPENAI_MESSAGE_ID_LIMIT: usize = 64;

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
        let mut input = vec![system_prompt_item(&self.model_name, system_prompt)];

        let conversation_items: Vec<ResponseInputItem> = conversation
            .entries()
            .iter()
            .flat_map(Into::<Vec<ResponseInputItem>>::into)
            .collect();
        input.extend(conversation_items);

        let request = CreateResponseRequest {
            model: self.model_name.clone(),
            input: ResponseInput::Items(input),
            tools: tools.into_iter().map(Into::into).collect(),
            parallel_tool_calls: Some(true),
            reasoning: Some(reasoning_config(thinking.clone())),
            include: if thinking.is_some() {
                vec![ResponseIncludable::ReasoningEncryptedContent]
            } else {
                Vec::new()
            },
            store: Some(false),
            ..Default::default()
        };

        let stream = self
            .client
            .responses_stream(request)
            .await
            .map_err(|e| ModelError::Client(anyhow::anyhow!(e)))?;

        let processor = OpenAiResponsesStreamProcessor::new();
        Ok(processor.process_stream(stream))
    }

    fn model_name(&self) -> String {
        self.model_name.to_string()
    }

    fn model_url(&self) -> String {
        self.client.base_url()
    }
}

fn system_prompt_item(model_name: &str, system_prompt: String) -> ResponseInputItem {
    if uses_developer_role(model_name) {
        ResponseInputItem::developer_text(system_prompt)
    } else {
        ResponseInputItem::system_text(system_prompt)
    }
}

fn uses_developer_role(model_name: &str) -> bool {
    model_name.starts_with("gpt-5") || model_name.starts_with('o')
}

fn reasoning_config(thinking: Option<ThinkingConfig>) -> Reasoning {
    match thinking {
        Some(thinking) => Reasoning {
            effort: Some(thinking.into()),
            summary: Some(ReasoningSummaryMode::Auto),
        },
        None => Reasoning {
            effort: Some(ReasoningEffort::None),
            summary: None,
        },
    }
}

impl From<&ConversationEntry> for Vec<ResponseInputItem> {
    fn from(entry: &ConversationEntry) -> Self {
        match &entry.entry {
            ConversationEntryKind::Message(message) => message.into(),
            ConversationEntryKind::UserOutput(_) => Vec::new(),
        }
    }
}

impl From<&crate::messages::MessageParam> for Vec<ResponseInputItem> {
    fn from(message: &crate::messages::MessageParam) -> Self {
        match message.role {
            Role::User => user_message_to_response_items(message),
            Role::Assistant => assistant_message_to_response_items(message),
        }
    }
}

fn user_message_to_response_items(
    message: &crate::messages::MessageParam,
) -> Vec<ResponseInputItem> {
    let mut items = Vec::new();
    let mut pending_content = Vec::new();

    for content_block in &message.content {
        match content_block {
            ContentBlockParam::TextBlock { .. }
            | ContentBlockParam::ImageBlock { .. }
            | ContentBlockParam::DocumentBlock { .. } => {
                if let Some(part) = content_block_to_input_part(content_block) {
                    pending_content.push(part);
                }
            }
            ContentBlockParam::ToolResultBlock {
                tool_use_id,
                content,
                ..
            } => {
                flush_user_message_content(&mut items, &mut pending_content);
                items.push(ResponseInputItem::FunctionCallOutput {
                    call_id: extract_call_id(tool_use_id),
                    output: tool_result_to_output_content(content),
                    id: None,
                    status: None,
                });
            }
            other => {
                tracing::warn!(
                    ?other,
                    "dropping unsupported user content for OpenAI Responses"
                );
            }
        }
    }

    flush_user_message_content(&mut items, &mut pending_content);
    items
}

fn flush_user_message_content(
    items: &mut Vec<ResponseInputItem>,
    pending_content: &mut Vec<ResponseInputContentPart>,
) {
    if pending_content.is_empty() {
        return;
    }

    items.push(ResponseInputItem::Message {
        id: None,
        role: InputRole::User,
        content: ResponseInputMessageContent::Array(std::mem::take(pending_content)),
        status: None,
        phase: None,
    });
}

fn assistant_message_to_response_items(
    message: &crate::messages::MessageParam,
) -> Vec<ResponseInputItem> {
    let mut items = Vec::new();
    let mut pending_content = Vec::new();
    let mut pending_id = None;
    let mut pending_phase = None;

    for content_block in &message.content {
        match content_block {
            ContentBlockParam::TextBlock {
                text, signature, ..
            } => {
                let signature = parse_text_signature(signature.as_deref());
                let next_id = signature.id.map(normalize_replay_message_id);
                let next_phase = signature.phase;

                if !pending_content.is_empty()
                    && (pending_id != next_id || pending_phase != next_phase)
                {
                    flush_assistant_message_content(
                        &mut items,
                        &mut pending_content,
                        &mut pending_id,
                        &mut pending_phase,
                    );
                }

                if pending_content.is_empty() {
                    pending_id = next_id.clone();
                    pending_phase = next_phase.clone();
                }

                pending_content.push(ResponseInputContentPart::OutputText {
                    text: text.clone(),
                    annotations: Vec::new(),
                    logprobs: None,
                });
            }
            ContentBlockParam::ThinkingBlock {
                signature,
                thinking: _,
            } => {
                flush_assistant_message_content(
                    &mut items,
                    &mut pending_content,
                    &mut pending_id,
                    &mut pending_phase,
                );

                if let Some(reasoning_item) = reasoning_item_from_signature(signature) {
                    items.push(reasoning_item);
                }
            }
            ContentBlockParam::ToolUseBlock {
                id, input, name, ..
            } => {
                flush_assistant_message_content(
                    &mut items,
                    &mut pending_content,
                    &mut pending_id,
                    &mut pending_phase,
                );

                let (call_id, item_id) = split_tool_use_id(id);
                items.push(ResponseInputItem::FunctionCall {
                    id: item_id,
                    call_id,
                    name: name.clone(),
                    arguments: input.to_string(),
                    status: Some(ItemStatus::Completed),
                });
            }
            ContentBlockParam::RedactedThinkingBlock { .. } => {}
            other => {
                tracing::warn!(
                    ?other,
                    "dropping unsupported assistant content for OpenAI Responses replay"
                );
            }
        }
    }

    flush_assistant_message_content(
        &mut items,
        &mut pending_content,
        &mut pending_id,
        &mut pending_phase,
    );
    items
}

fn flush_assistant_message_content(
    items: &mut Vec<ResponseInputItem>,
    pending_content: &mut Vec<ResponseInputContentPart>,
    pending_id: &mut Option<String>,
    pending_phase: &mut Option<MessagePhase>,
) {
    if pending_content.is_empty() {
        return;
    }

    items.push(ResponseInputItem::Message {
        id: pending_id.take(),
        role: InputRole::Assistant,
        content: ResponseInputMessageContent::Array(std::mem::take(pending_content)),
        status: Some(ItemStatus::Completed),
        phase: pending_phase.take(),
    });
}

fn content_block_to_input_part(
    content_block: &ContentBlockParam,
) -> Option<ResponseInputContentPart> {
    match content_block {
        ContentBlockParam::TextBlock { text, .. } => {
            Some(ResponseInputContentPart::InputText { text: text.clone() })
        }
        ContentBlockParam::ImageBlock { source } => Some(image_source_to_input_part(source)),
        ContentBlockParam::DocumentBlock { source, .. } => document_source_to_input_part(source),
        _ => None,
    }
}

fn image_source_to_input_part(source: &ImageSource) -> ResponseInputContentPart {
    match source {
        ImageSource::Base64 { data, media_type } => ResponseInputContentPart::InputImage {
            image_url: Some(data_url(media_type, data)),
            file_id: None,
            detail: Some(ImageDetail::Auto),
        },
        ImageSource::Url { url } => ResponseInputContentPart::InputImage {
            image_url: Some(url.clone()),
            file_id: None,
            detail: Some(ImageDetail::Auto),
        },
        ImageSource::File { file_id } => ResponseInputContentPart::InputImage {
            image_url: None,
            file_id: Some(file_id.clone()),
            detail: Some(ImageDetail::Auto),
        },
    }
}

fn document_source_to_input_part(source: &DocumentSource) -> Option<ResponseInputContentPart> {
    match source {
        DocumentSource::PdfBase64 { data, media_type } => {
            Some(ResponseInputContentPart::InputFile {
                file_id: None,
                file_data: Some(data_url(media_type, data)),
                filename: None,
            })
        }
        DocumentSource::PlainText { data, .. } => Some(ResponseInputContentPart::InputFile {
            file_id: None,
            file_data: Some(data.clone()),
            filename: None,
        }),
        DocumentSource::File { file_id } => Some(ResponseInputContentPart::InputFile {
            file_id: Some(file_id.clone()),
            file_data: None,
            filename: None,
        }),
        DocumentSource::PdfUrl { .. } | DocumentSource::Content { .. } => None,
    }
}

fn tool_result_to_output_content(content: &ToolResultContent) -> FunctionCallOutputContent {
    match content {
        ToolResultContent::Text(text) => FunctionCallOutputContent::String(text.clone()),
        ToolResultContent::Blocks(blocks) => {
            let parts: Vec<ResponseInputContentPart> = blocks
                .iter()
                .filter_map(content_block_to_input_part)
                .collect();

            if parts.is_empty() {
                FunctionCallOutputContent::String(content.text())
            } else {
                FunctionCallOutputContent::Array(parts)
            }
        }
    }
}

fn data_url(media_type: &str, data: &str) -> String {
    format!("data:{media_type};base64,{data}")
}

fn extract_call_id(tool_use_id: &str) -> String {
    split_tool_use_id(tool_use_id).0
}

fn split_tool_use_id(tool_use_id: &str) -> (String, Option<String>) {
    if let Some((call_id, item_id)) = tool_use_id.split_once('|') {
        (call_id.to_string(), Some(item_id.to_string()))
    } else {
        (tool_use_id.to_string(), None)
    }
}

fn compose_tool_use_id(call_id: &str, item_id: Option<&str>) -> String {
    match item_id {
        Some(item_id) if !item_id.is_empty() => format!("{call_id}|{item_id}"),
        _ => call_id.to_string(),
    }
}

fn reasoning_item_from_signature(signature: &str) -> Option<ResponseInputItem> {
    match serde_json::from_str::<ResponseInputItem>(signature) {
        Ok(reasoning @ ResponseInputItem::Reasoning { .. }) => Some(reasoning),
        _ => None,
    }
}

fn normalize_replay_message_id(id: String) -> String {
    if id.len() <= OPENAI_MESSAGE_ID_LIMIT {
        id
    } else {
        id.chars().take(OPENAI_MESSAGE_ID_LIMIT).collect()
    }
}

#[derive(Debug, Default)]
struct ParsedTextSignature {
    id: Option<String>,
    phase: Option<MessagePhase>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TextSignatureV1 {
    v: u8,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<MessagePhase>,
}

fn parse_text_signature(signature: Option<&str>) -> ParsedTextSignature {
    let Some(signature) = signature else {
        return ParsedTextSignature::default();
    };

    if let Ok(parsed) = serde_json::from_str::<TextSignatureV1>(signature) {
        return ParsedTextSignature {
            id: Some(parsed.id),
            phase: parsed.phase,
        };
    }

    ParsedTextSignature {
        id: Some(signature.to_string()),
        phase: None,
    }
}

fn serialize_text_signature(id: String, phase: Option<MessagePhase>) -> String {
    let signature = TextSignatureV1 { v: 1, id, phase };
    serde_json::to_string(&signature).unwrap_or_else(|_| {
        tracing::warn!("failed to serialize text signature");
        String::new()
    })
}

impl From<Tool> for ResponseTool {
    fn from(tool: Tool) -> Self {
        ResponseTool::Function {
            name: tool.name,
            description: Some(tool.description),
            parameters: Some(tool.input_schema),
            strict: Some(false),
        }
    }
}

impl From<ThinkingConfig> for ReasoningEffort {
    fn from(thinking: ThinkingConfig) -> Self {
        match thinking {
            ThinkingConfig::Low => ReasoningEffort::Low,
            ThinkingConfig::Medium => ReasoningEffort::Medium,
            ThinkingConfig::High => ReasoningEffort::High,
            ThinkingConfig::XHigh => ReasoningEffort::XHigh,
            // OpenAI's effort ladder tops out at xhigh; collapse Max onto it.
            ThinkingConfig::Max => ReasoningEffort::XHigh,
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

fn usage_delta_from_response(
    usage: &ResponseUsage,
    service_tier: Option<&OpenAIServiceTier>,
) -> UsageDelta {
    let cached_tokens = usage
        .input_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .map(u64::from)
        .unwrap_or(0);
    let input_tokens = u64::from(usage.input_tokens).saturating_sub(cached_tokens);

    UsageDelta {
        cache_creation: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: Some(cached_tokens),
        input_tokens: Some(input_tokens),
        iterations: None,
        output_tokens: Some(u64::from(usage.output_tokens)),
        server_tool_use: None,
        service_tier: service_tier.cloned().map(Into::into),
        speed: None,
    }
}

fn usage_from_response(
    usage: Option<&ResponseUsage>,
    service_tier: Option<&OpenAIServiceTier>,
) -> Usage {
    let mut result = Usage::default();
    if let Some(usage) = usage {
        result.apply_delta(&usage_delta_from_response(usage, service_tier));
    } else if let Some(service_tier) = service_tier {
        result.service_tier = Some(service_tier.clone().into());
    }
    result
}

fn api_error_from_code(code: Option<&str>, message: String) -> ApiError {
    match code {
        Some(code) if code.contains("rate_limit") => ApiError::RateLimitError { message },
        Some(code) if code.contains("permission") => ApiError::PermissionError { message },
        Some(code) if code.contains("auth") => ApiError::AuthenticationError { message },
        Some(code) if code.contains("not_found") => ApiError::NotFoundError { message },
        Some(code) if code.contains("timeout") => ApiError::GatewayTimeoutError { message },
        Some(code) if code.contains("overloaded") => ApiError::OverloadedError { message },
        Some(code) if code.contains("invalid") => ApiError::InvalidRequestError { message },
        _ => ApiError::ApiError { message },
    }
}

fn response_error(response: &Response) -> ApiError {
    if let Some(error) = &response.error {
        return api_error_from_code(Some(error.code.as_str()), error.message.clone());
    }

    let message = response
        .incomplete_details
        .as_ref()
        .and_then(|details| details.reason.clone())
        .unwrap_or_else(|| "OpenAI response failed".to_string());

    ApiError::ApiError { message }
}

fn stop_reason_from_response(response: &Response, has_tool_use: bool) -> StopReason {
    match response.status {
        Some(ResponseStatus::Completed) if has_tool_use => StopReason::ToolUse,
        Some(ResponseStatus::Completed) => StopReason::EndTurn,
        Some(ResponseStatus::Incomplete) => {
            match response
                .incomplete_details
                .as_ref()
                .and_then(|details| details.reason.as_deref())
            {
                Some("content_filter") => StopReason::Refusal,
                _ => StopReason::MaxTokens,
            }
        }
        Some(ResponseStatus::Cancelled) | Some(ResponseStatus::Failed) => StopReason::Refusal,
        Some(ResponseStatus::InProgress) | Some(ResponseStatus::Queued) | None => {
            StopReason::EndTurn
        }
    }
}

fn response_output_to_content(
    output: Vec<ResponseOutputItem>,
) -> (Vec<ContentBlock>, Vec<StreamingEvent>) {
    let mut content = Vec::new();
    let mut events = Vec::new();

    for item in output {
        match item {
            ResponseOutputItem::Message {
                id,
                content: parts,
                phase,
                ..
            } => {
                let signature = serialize_text_signature(id, phase);
                let signature = if signature.is_empty() {
                    None
                } else {
                    Some(signature)
                };

                for part in parts {
                    match part {
                        ResponseOutputContent::OutputText { text, .. } => {
                            content.push(ContentBlock::TextBlock {
                                text,
                                citations: Vec::new(),
                                signature: signature.clone(),
                            });
                        }
                        ResponseOutputContent::Refusal { refusal } => {
                            content.push(ContentBlock::TextBlock {
                                text: refusal,
                                citations: Vec::new(),
                                signature: signature.clone(),
                            });
                        }
                    }
                }
            }
            ResponseOutputItem::FunctionCall {
                id,
                call_id,
                name,
                arguments,
                ..
            } => match serde_json::from_str::<Value>(&arguments) {
                Ok(input) => content.push(ContentBlock::ToolUseBlock {
                    id: compose_tool_use_id(&call_id, id.as_deref()),
                    name,
                    input,
                }),
                Err(error) => events.push(StreamingEvent::ParseError {
                    error: format!("failed to parse tool use input json: {error}"),
                    raw_data: arguments,
                }),
            },
            ResponseOutputItem::Reasoning {
                id,
                summary,
                content: reasoning_content,
                encrypted_content,
                status,
            } => {
                let signature = serde_json::to_string(&ResponseInputItem::Reasoning {
                    id,
                    summary: summary.clone(),
                    content: reasoning_content,
                    encrypted_content,
                    status,
                });

                match signature {
                    Ok(signature) => content.push(ContentBlock::ThinkingBlock {
                        signature,
                        thinking: join_reasoning_summary(&summary),
                    }),
                    Err(error) => events.push(StreamingEvent::ParseError {
                        error: format!("failed to serialize reasoning signature: {error}"),
                        raw_data: String::new(),
                    }),
                }
            }
            ResponseOutputItem::WebSearchCall { .. } | ResponseOutputItem::Other(_) => {}
        }
    }

    (content, events)
}

fn join_reasoning_summary(summary: &[ReasoningSummary]) -> String {
    summary
        .iter()
        .map(|part| part.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn response_to_message(response: Response) -> (Message, Vec<StreamingEvent>) {
    let stop_reason = stop_reason_from_response(
        &response,
        response
            .output
            .iter()
            .any(|item| matches!(item, ResponseOutputItem::FunctionCall { .. })),
    );
    let (content, events) = response_output_to_content(response.output.clone());

    let message = Message {
        id: response.id,
        r#type: MessageType::Message,
        role: Role::Assistant,
        content,
        model: response.model,
        stop_reason: Some(stop_reason),
        stop_sequence: None,
        stop_details: None,
        usage: usage_from_response(response.usage.as_ref(), response.service_tier.as_ref()),
        container: None,
        context_management: None,
    };

    (message, events)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TextPartKey {
    item_id: String,
    content_index: u32,
}

#[derive(Debug, Default)]
struct OpenAiResponsesStreamProcessor {
    started: bool,
    finalized: bool,
    text_parts: HashMap<TextPartKey, String>,
    started_text_parts: HashSet<TextPartKey>,
    thinking_items: HashMap<String, String>,
    started_thinking_items: HashSet<String>,
}

impl OpenAiResponsesStreamProcessor {
    fn new() -> Self {
        Self::default()
    }

    fn process_stream<S>(mut self, stream: S) -> Pin<Box<dyn Stream<Item = StreamingEvent> + Send>>
    where
        S: Stream<Item = Result<ResponseStreamEvent, ClientError>> + Send + 'static,
    {
        let stream = stream! {
            let mut stream = std::pin::pin!(stream);

            while let Some(event_result) = futures::StreamExt::next(&mut stream).await {
                match event_result {
                    Ok(event) => {
                        for output_event in self.process_event(event) {
                            yield output_event;
                        }
                    }
                    Err(error) => {
                        yield StreamingEvent::ProtocolError {
                            error: format!("OpenAI stream error: {error}"),
                        };
                    }
                }
            }

            if self.started && !self.finalized {
                yield StreamingEvent::ProtocolError {
                    error: "OpenAI Responses stream ended without a terminal response event".to_string(),
                };
            }
        };

        stream.boxed()
    }

    fn process_event(&mut self, event: ResponseStreamEvent) -> Vec<StreamingEvent> {
        let mut events = Vec::new();

        match event {
            ResponseStreamEvent::ResponseCreated { response, .. }
            | ResponseStreamEvent::ResponseQueued { response, .. }
            | ResponseStreamEvent::ResponseInProgress { response, .. } => {
                self.ensure_message_started(&response, &mut events);
            }
            ResponseStreamEvent::OutputTextDelta {
                delta,
                item_id,
                content_index,
                ..
            }
            | ResponseStreamEvent::RefusalDelta {
                delta,
                item_id,
                content_index,
                ..
            } => {
                let key = TextPartKey {
                    item_id,
                    content_index,
                };
                let snapshot = self.text_parts.entry(key.clone()).or_default();
                snapshot.push_str(&delta);

                if self.started_text_parts.insert(key) {
                    events.push(StreamingEvent::TextStart {
                        text: delta,
                        citations: Vec::new(),
                    });
                } else {
                    events.push(StreamingEvent::TextUpdate {
                        diff: delta,
                        snapshot: snapshot.clone(),
                    });
                }
            }
            ResponseStreamEvent::OutputTextDone {
                text,
                item_id,
                content_index,
                ..
            } => {
                self.finish_text_part(item_id, content_index, text, &mut events);
            }
            ResponseStreamEvent::RefusalDone {
                refusal,
                item_id,
                content_index,
                ..
            } => {
                self.finish_text_part(item_id, content_index, refusal, &mut events);
            }
            ResponseStreamEvent::OutputItemAdded { item, .. } => {
                if let ResponseOutputItem::Reasoning { id, .. } = item {
                    self.thinking_items.entry(id.clone()).or_default();
                    if self.started_thinking_items.insert(id) {
                        events.push(StreamingEvent::ThinkingStart {
                            thinking: String::new(),
                        });
                    }
                }
            }
            ResponseStreamEvent::ReasoningSummaryTextDelta { delta, item_id, .. } => {
                let snapshot = self.thinking_items.entry(item_id.clone()).or_default();
                snapshot.push_str(&delta);

                if self.started_thinking_items.insert(item_id) {
                    events.push(StreamingEvent::ThinkingStart { thinking: delta });
                } else {
                    events.push(StreamingEvent::ThinkingUpdate {
                        diff: delta,
                        snapshot: snapshot.clone(),
                    });
                }
            }
            ResponseStreamEvent::ReasoningSummaryPartDone { item_id, .. } => {
                let snapshot = self.thinking_items.entry(item_id.clone()).or_default();
                if !snapshot.is_empty() {
                    snapshot.push_str("\n\n");
                    events.push(StreamingEvent::ThinkingUpdate {
                        diff: "\n\n".to_string(),
                        snapshot: snapshot.clone(),
                    });
                }
            }
            ResponseStreamEvent::OutputItemDone { item, .. } => {
                if let ResponseOutputItem::Reasoning { id, .. } = item {
                    if self.started_thinking_items.remove(&id) {
                        self.thinking_items.remove(&id);
                        events.push(StreamingEvent::ThinkingStop);
                    }
                }
            }
            ResponseStreamEvent::ResponseCompleted { response, .. }
            | ResponseStreamEvent::ResponseIncomplete { response, .. } => {
                self.ensure_message_started(&response, &mut events);

                if let Some(usage) = response.usage.as_ref() {
                    events.push(StreamingEvent::UsageUpdate {
                        usage: usage_delta_from_response(usage, response.service_tier.as_ref()),
                    });
                }

                let (message, mut parse_events) = response_to_message(response);
                events.append(&mut parse_events);
                events.push(StreamingEvent::FinalizedMessage { message });
                self.finalized = true;
            }
            ResponseStreamEvent::ResponseFailed { response, .. } => {
                self.ensure_message_started(&response, &mut events);
                events.push(StreamingEvent::Error {
                    error: response_error(&response),
                });
                self.finalized = true;
            }
            ResponseStreamEvent::Error { code, message, .. } => {
                events.push(StreamingEvent::Error {
                    error: api_error_from_code(code.as_deref(), message),
                });
                self.finalized = true;
            }
            ResponseStreamEvent::ContentPartAdded { .. }
            | ResponseStreamEvent::ContentPartDone { .. }
            | ResponseStreamEvent::OutputTextAnnotationAdded { .. }
            | ResponseStreamEvent::FunctionCallArgumentsDelta { .. }
            | ResponseStreamEvent::FunctionCallArgumentsDone { .. }
            | ResponseStreamEvent::ReasoningTextDelta { .. }
            | ResponseStreamEvent::ReasoningTextDone { .. }
            | ResponseStreamEvent::ReasoningSummaryPartAdded { .. }
            | ResponseStreamEvent::ReasoningSummaryTextDone { .. }
            | ResponseStreamEvent::WebSearchCallInProgress { .. }
            | ResponseStreamEvent::WebSearchCallSearching { .. }
            | ResponseStreamEvent::WebSearchCallCompleted { .. }
            | ResponseStreamEvent::Other(_) => {}
        }

        events
    }

    fn ensure_message_started(&mut self, response: &Response, events: &mut Vec<StreamingEvent>) {
        if self.started {
            return;
        }

        self.started = true;
        events.push(StreamingEvent::MessageStart {
            message: Message {
                id: response.id.clone(),
                r#type: MessageType::Message,
                role: Role::Assistant,
                content: Vec::new(),
                model: response.model.clone(),
                stop_reason: None,
                stop_sequence: None,
                stop_details: None,
                usage: usage_from_response(None, response.service_tier.as_ref()),
                container: None,
                context_management: None,
            },
        });
    }

    fn finish_text_part(
        &mut self,
        item_id: String,
        content_index: u32,
        text: String,
        events: &mut Vec<StreamingEvent>,
    ) {
        let key = TextPartKey {
            item_id,
            content_index,
        };
        let started = self.started_text_parts.remove(&key);
        let snapshot = self.text_parts.remove(&key).unwrap_or_else(|| text.clone());

        if !started {
            events.push(StreamingEvent::TextStart {
                text: text.clone(),
                citations: Vec::new(),
            });
        } else if snapshot != text {
            events.push(StreamingEvent::TextUpdate {
                diff: text.strip_prefix(&snapshot).unwrap_or_default().to_string(),
                snapshot: text.clone(),
            });
        }

        events.push(StreamingEvent::TextStop { text });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_replay_preserves_text_signatures_and_tool_call_ids() {
        let signature =
            serialize_text_signature("msg_123".to_string(), Some(MessagePhase::Commentary));
        let message = crate::messages::MessageParam {
            role: Role::Assistant,
            content: vec![
                ContentBlockParam::TextBlock {
                    text: "partial".to_string(),
                    citations: None,
                    signature: Some(signature),
                },
                ContentBlockParam::ToolUseBlock {
                    id: "call_123|fc_123".to_string(),
                    input: serde_json::json!({"x": 1}),
                    name: "do_work".to_string(),
                    caller: None,
                },
            ],
        };

        let items = assistant_message_to_response_items(&message);

        assert_eq!(items.len(), 2);

        match &items[0] {
            ResponseInputItem::Message {
                id,
                role,
                content,
                status,
                phase,
            } => {
                assert_eq!(id.as_deref(), Some("msg_123"));
                assert_eq!(*role, InputRole::Assistant);
                assert_eq!(*status, Some(ItemStatus::Completed));
                assert_eq!(*phase, Some(MessagePhase::Commentary));

                match content {
                    ResponseInputMessageContent::Array(parts) => {
                        assert!(matches!(
                            parts.first(),
                            Some(ResponseInputContentPart::OutputText { text, .. }) if text == "partial"
                        ));
                    }
                    ResponseInputMessageContent::String(_) => panic!("expected array content"),
                }
            }
            other => panic!("unexpected item: {other:?}"),
        }

        match &items[1] {
            ResponseInputItem::FunctionCall {
                id,
                call_id,
                name,
                arguments,
                status,
            } => {
                assert_eq!(id.as_deref(), Some("fc_123"));
                assert_eq!(call_id, "call_123");
                assert_eq!(name, "do_work");
                assert_eq!(arguments, r#"{"x":1}"#);
                assert_eq!(*status, Some(ItemStatus::Completed));
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[test]
    fn response_conversion_preserves_signatures_and_tool_use() {
        let response = Response {
            id: "resp_123".to_string(),
            object: "response".to_string(),
            created_at: 0.0,
            model: "gpt-5".to_string(),
            output: vec![
                ResponseOutputItem::Reasoning {
                    id: "rs_123".to_string(),
                    summary: vec![
                        ReasoningSummary {
                            text: "first".to_string(),
                            r#type: "summary_text".to_string(),
                        },
                        ReasoningSummary {
                            text: "second".to_string(),
                            r#type: "summary_text".to_string(),
                        },
                    ],
                    content: None,
                    encrypted_content: Some("secret".to_string()),
                    status: Some(ItemStatus::Completed),
                },
                ResponseOutputItem::Message {
                    id: "msg_123".to_string(),
                    content: vec![ResponseOutputContent::OutputText {
                        text: "hello".to_string(),
                        annotations: Vec::new(),
                        logprobs: None,
                    }],
                    role: "assistant".to_string(),
                    status: ItemStatus::Completed,
                    phase: Some(MessagePhase::FinalAnswer),
                },
                ResponseOutputItem::FunctionCall {
                    id: Some("fc_123".to_string()),
                    call_id: "call_123".to_string(),
                    name: "do_work".to_string(),
                    arguments: "{}".to_string(),
                    status: Some(ItemStatus::Completed),
                },
            ],
            parallel_tool_calls: true,
            tools: Vec::new(),
            background: None,
            completed_at: None,
            previous_response_id: None,
            conversation: None,
            max_tool_calls: None,
            prompt: None,
            error: None,
            incomplete_details: None,
            instructions: None,
            metadata: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            reasoning: None,
            status: Some(ResponseStatus::Completed),
            text: None,
            truncation: None,
            usage: Some(ResponseUsage {
                input_tokens: 10,
                output_tokens: 4,
                total_tokens: 14,
                input_tokens_details: Some(
                    openai_sdk::types::responses::ResponseInputTokensDetails {
                        cached_tokens: Some(3),
                    },
                ),
                output_tokens_details: Some(
                    openai_sdk::types::responses::ResponseOutputTokensDetails {
                        reasoning_tokens: Some(2),
                    },
                ),
            }),
            service_tier: Some(OpenAIServiceTier::Priority),
            tool_choice: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            safety_identifier: None,
            top_logprobs: None,
            #[allow(deprecated)]
            user: None,
        };

        let (message, parse_events) = response_to_message(response);

        assert!(parse_events.is_empty());
        assert_eq!(message.id, "resp_123");
        assert!(matches!(message.stop_reason, Some(StopReason::ToolUse)));
        assert_eq!(message.usage.input_tokens, 7);
        assert_eq!(message.usage.cache_read_input_tokens, Some(3));
        assert_eq!(message.usage.output_tokens, 4);
        assert!(matches!(
            message.usage.service_tier,
            Some(ServiceTier::Priority)
        ));

        assert!(matches!(
            &message.content[0],
            ContentBlock::ThinkingBlock { thinking, .. } if thinking == "first\n\nsecond"
        ));
        assert!(matches!(
            &message.content[1],
            ContentBlock::TextBlock {
                text,
                signature: Some(_),
                ..
            } if text == "hello"
        ));
        assert!(matches!(
            &message.content[2],
            ContentBlock::ToolUseBlock { id, name, .. } if id == "call_123|fc_123" && name == "do_work"
        ));
    }
}
