use anthropic_sdk::client::Client;
use anthropic_sdk::messages::{
    ApiError as AnthropicApiError, CacheControl, CacheCreation as AnthropicCacheCreation,
    Citation as AnthropicCitation, Container as AnthropicContainer,
    ContentBlock as AnthropicContentBlock, ContentBlockParam as AnthropicContentBlockParam,
    DocumentSource as AnthropicDocumentSource, ImageSource as AnthropicImageSource,
    Message as AnthropicMessage, MessageParam as AnthropicMessageParam,
    MessageType as AnthropicMessageType, Messages, Role as AnthropicRole,
    ServerToolUsage as AnthropicServerToolUsage, ServiceTier as AnthropicServiceTier,
    StopReason as AnthropicStopReason, Thinking as AnthropicThinking, Tool as AnthropicTool,
    Usage as AnthropicUsage, UsageDelta as AnthropicUsageDelta,
    WebSearchToolResultContent as AnthropicWebSearchToolResultContent,
};
use anthropic_sdk::streaming::StreamingEvent as AnthropicStreamingEvent;
use futures::{Stream, StreamExt};
use std::pin::Pin;

use crate::conversation::{Conversation, ConversationEntry, ConversationEntryKind};
use crate::messages::{
    ApiError, CacheCreation, Citation, Container, ContentBlock, ContentBlockParam, DocumentSource,
    ImageSource, Message, MessageParam, MessageType, Role, ServerToolUsage, ServiceTier,
    StopReason, Usage, UsageDelta, WebSearchToolResultContent,
};
use crate::streaming::StreamingEvent;
use crate::tools::Tool;
use crate::{Model, ModelError, ThinkingConfig};

/// Anthropic Claude model implementation
pub struct AnthropicModel {
    client: Client,
}

impl AnthropicModel {
    pub fn new(api_key: String) -> Self {
        let client = Client::new(api_key);
        Self { client }
    }
}

#[async_trait::async_trait]
impl Model for AnthropicModel {
    async fn run_inference_streaming(
        &self,
        conversation: &Conversation,
        system_prompt: String,
        tools: Vec<Tool>,
        thinking: Option<ThinkingConfig>,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamingEvent> + Send + '_>>, ModelError> {
        let system_prompt = vec![AnthropicContentBlockParam::TextBlock {
            text: system_prompt,
            cache_control: Some(CacheControl::Ephemeral { ttl: None }),
            citations: None,
        }];
        let tools: Vec<AnthropicTool> = tools.into_iter().map(|t| t.into()).collect();
        let thinking: Option<AnthropicThinking> = thinking.map(Into::into);

        let mut messages: Vec<AnthropicMessageParam> = conversation
            .entries()
            .iter()
            .filter_map(Into::<Option<AnthropicMessageParam>>::into)
            .collect();

        let last_user_message = messages
            .iter_mut()
            .filter(|m| matches!(m.role, AnthropicRole::User))
            .next_back();

        if let Some(last_user_message) = last_user_message {
            let last_content = last_user_message.content.iter_mut().last();
            if let Some(last_content) = last_content {
                last_content.set_cache_control(CacheControl::Ephemeral { ttl: None });
            }
        }

        let last_assistant_message = messages
            .iter_mut()
            .filter(|m| matches!(m.role, AnthropicRole::Assistant))
            .next_back();

        if let Some(last_assistant_message) = last_assistant_message {
            let last_content = last_assistant_message.content.iter_mut().last();
            if let Some(last_content) = last_content {
                last_content.set_cache_control(CacheControl::Ephemeral { ttl: None });
            }
        }

        let messages_request = Messages {
            model: "claude-sonnet-4-20250514".to_string(),
            system: Some(system_prompt),
            thinking,
            max_tokens: 32_000,
            messages,
            tools,
            ..Default::default()
        };

        let response_stream = self
            .client
            .messages_stream(messages_request)
            .await
            .map(|stream| stream.map(|event| event.into()))
            .map_err(|e| ModelError::Client(e.into()))?;

        Ok(response_stream.boxed())
    }
}

impl From<&ConversationEntry> for Option<AnthropicMessageParam> {
    fn from(entry: &ConversationEntry) -> Self {
        match &entry.entry {
            ConversationEntryKind::Message(message) => Some(message.into()),
            ConversationEntryKind::UserOutput(_) => None,
        }
    }
}

impl From<&MessageParam> for AnthropicMessageParam {
    fn from(message: &MessageParam) -> Self {
        let MessageParam { role, content } = message;

        Self {
            role: role.into(),
            content: content.iter().map(Into::into).collect(),
        }
    }
}

impl From<&ContentBlockParam> for AnthropicContentBlockParam {
    fn from(content_block: &ContentBlockParam) -> Self {
        match content_block {
            ContentBlockParam::TextBlock { text, citations } => {
                AnthropicContentBlockParam::TextBlock {
                    text: text.clone(),
                    cache_control: None,
                    citations: citations
                        .as_ref()
                        .map(|c| c.iter().map(Into::into).collect()),
                }
            }
            ContentBlockParam::ImageBlock { source } => AnthropicContentBlockParam::ImageBlock {
                source: source.into(),
                cache_control: None,
            },
            ContentBlockParam::DocumentBlock {
                source,
                citations,
                context,
                title,
            } => AnthropicContentBlockParam::DocumentBlock {
                source: source.into(),
                citations: citations
                    .as_ref()
                    .map(|c| c.iter().map(Into::into).collect()),
                context: context.clone(),
                title: title.clone(),
                cache_control: None,
            },
            ContentBlockParam::ThinkingBlock {
                signature,
                thinking,
            } => AnthropicContentBlockParam::ThinkingBlock {
                signature: signature.clone(),
                thinking: thinking.clone(),
            },
            ContentBlockParam::RedactedThinkingBlock { data } => {
                AnthropicContentBlockParam::RedactedThinkingBlock { data: data.clone() }
            }
            ContentBlockParam::ToolUseBlock { id, input, name } => {
                AnthropicContentBlockParam::ToolUseBlock {
                    id: id.clone(),
                    input: input.clone(),
                    name: name.clone(),
                    cache_control: None,
                }
            }
            ContentBlockParam::ToolResultBlock {
                tool_use_id,
                content,
                is_error,
            } => AnthropicContentBlockParam::ToolResultBlock {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
                cache_control: None,
            },
            ContentBlockParam::ServerToolUseBlock { id, input, name } => {
                AnthropicContentBlockParam::ServerToolUseBlock {
                    id: id.clone(),
                    input: input.clone(),
                    name: name.clone(),
                    cache_control: None,
                }
            }
            ContentBlockParam::WebSearchToolResultBlock {
                content,
                tool_use_id,
            } => AnthropicContentBlockParam::WebSearchToolResultBlock {
                content: content.iter().map(Into::into).collect(),
                tool_use_id: tool_use_id.clone(),
                cache_control: None,
            },
            ContentBlockParam::CodeExecutionToolResultBlock {
                content,
                tool_use_id,
            } => AnthropicContentBlockParam::CodeExecutionToolResultBlock {
                content: content.clone(),
                tool_use_id: tool_use_id.clone(),
                cache_control: None,
            },
            ContentBlockParam::MCPToolUseBlock {
                id,
                input,
                name,
                server_name,
            } => AnthropicContentBlockParam::MCPToolUseBlock {
                id: id.clone(),
                input: input.clone(),
                name: name.clone(),
                server_name: server_name.clone(),
                cache_control: None,
            },
            ContentBlockParam::MCPToolResultBlock {
                tool_use_id,
                content,
                is_error,
            } => AnthropicContentBlockParam::MCPToolResultBlock {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
                cache_control: None,
            },
            ContentBlockParam::ContainerUploadBlock { file_id } => {
                AnthropicContentBlockParam::ContainerUploadBlock {
                    file_id: file_id.clone(),
                    cache_control: None,
                }
            }
        }
    }
}

impl From<&Role> for AnthropicRole {
    fn from(role: &Role) -> Self {
        match role {
            Role::User => Self::User,
            Role::Assistant => Self::Assistant,
        }
    }
}

impl From<Tool> for AnthropicTool {
    fn from(tool: Tool) -> Self {
        let Tool {
            name,
            description,
            input_schema,
            r#type,
        } = tool;

        Self {
            name,
            description,
            input_schema,
            r#type,
            cache_control: None,
        }
    }
}

impl From<&ImageSource> for AnthropicImageSource {
    fn from(source: &ImageSource) -> Self {
        match source {
            ImageSource::Base64 { data, media_type } => Self::Base64 {
                data: data.clone(),
                media_type: media_type.clone(),
            },
            ImageSource::Url { url } => Self::Url { url: url.clone() },
            ImageSource::File { file_id } => Self::File {
                file_id: file_id.clone(),
            },
        }
    }
}

impl From<&DocumentSource> for AnthropicDocumentSource {
    fn from(source: &DocumentSource) -> Self {
        match source {
            DocumentSource::PdfBase64 { data, media_type } => Self::PdfBase64 {
                data: data.clone(),
                media_type: media_type.clone(),
            },
            DocumentSource::PdfUrl { url } => Self::PdfUrl { url: url.clone() },
            DocumentSource::PlainText { data, media_type } => Self::PlainText {
                data: data.clone(),
                media_type: media_type.clone(),
            },
            DocumentSource::File { file_id } => Self::File {
                file_id: file_id.clone(),
            },
        }
    }
}

impl From<&Citation> for AnthropicCitation {
    fn from(citation: &Citation) -> Self {
        match citation {
            Citation::CharLocation {
                cited_text,
                document_index,
                document_title,
                end_char_index,
                start_char_index,
            } => Self::CharLocation {
                cited_text: cited_text.clone(),
                document_index: *document_index,
                document_title: document_title.clone(),
                end_char_index: *end_char_index,
                start_char_index: *start_char_index,
            },
            Citation::PageLocation {
                cited_text,
                document_index,
                document_title,
                end_page_number,
                start_page_number,
            } => Self::PageLocation {
                cited_text: cited_text.clone(),
                document_index: *document_index,
                document_title: document_title.clone(),
                end_page_number: *end_page_number,
                start_page_number: *start_page_number,
            },
            Citation::ContentBlockLocation {
                cited_text,
                document_index,
                document_title,
                end_block_index,
                start_block_index,
            } => Self::ContentBlockLocation {
                cited_text: cited_text.clone(),
                document_index: *document_index,
                document_title: document_title.clone(),
                end_block_index: *end_block_index,
                start_block_index: *start_block_index,
            },
            Citation::WebSearchResultLocation {
                cited_text,
                encrypted_index,
                title,
                url,
            } => Self::WebSearchResultLocation {
                cited_text: cited_text.clone(),
                encrypted_index: encrypted_index.clone(),
                title: title.clone(),
                url: url.clone(),
            },
        }
    }
}

impl From<&AnthropicCitation> for Citation {
    fn from(citation: &AnthropicCitation) -> Self {
        match citation {
            AnthropicCitation::CharLocation {
                cited_text,
                document_index,
                document_title,
                end_char_index,
                start_char_index,
            } => Self::CharLocation {
                cited_text: cited_text.clone(),
                document_index: *document_index,
                document_title: document_title.clone(),
                end_char_index: *end_char_index,
                start_char_index: *start_char_index,
            },
            AnthropicCitation::PageLocation {
                cited_text,
                document_index,
                document_title,
                end_page_number,
                start_page_number,
            } => Self::PageLocation {
                cited_text: cited_text.clone(),
                document_index: *document_index,
                document_title: document_title.clone(),
                end_page_number: *end_page_number,
                start_page_number: *start_page_number,
            },
            AnthropicCitation::ContentBlockLocation {
                cited_text,
                document_index,
                document_title,
                end_block_index,
                start_block_index,
            } => Self::ContentBlockLocation {
                cited_text: cited_text.clone(),
                document_index: *document_index,
                document_title: document_title.clone(),
                end_block_index: *end_block_index,
                start_block_index: *start_block_index,
            },
            AnthropicCitation::WebSearchResultLocation {
                cited_text,
                encrypted_index,
                title,
                url,
            } => Self::WebSearchResultLocation {
                cited_text: cited_text.clone(),
                encrypted_index: encrypted_index.clone(),
                title: title.clone(),
                url: url.clone(),
            },
        }
    }
}

impl From<&WebSearchToolResultContent> for AnthropicWebSearchToolResultContent {
    fn from(content: &WebSearchToolResultContent) -> Self {
        match content {
            WebSearchToolResultContent::WebSearchResult {
                encrypted_content,
                title,
                url,
                page_age,
            } => Self::WebSearchResult {
                encrypted_content: encrypted_content.clone(),
                title: title.clone(),
                url: url.clone(),
                page_age: page_age.clone(),
            },
            WebSearchToolResultContent::WebSearchError { error_code } => Self::WebSearchError {
                error_code: error_code.clone(),
            },
        }
    }
}

impl From<&AnthropicWebSearchToolResultContent> for WebSearchToolResultContent {
    fn from(content: &AnthropicWebSearchToolResultContent) -> Self {
        match content {
            AnthropicWebSearchToolResultContent::WebSearchResult {
                encrypted_content,
                title,
                url,
                page_age,
            } => Self::WebSearchResult {
                encrypted_content: encrypted_content.clone(),
                title: title.clone(),
                url: url.clone(),
                page_age: page_age.clone(),
            },
            AnthropicWebSearchToolResultContent::WebSearchError { error_code } => {
                Self::WebSearchError {
                    error_code: error_code.clone(),
                }
            }
        }
    }
}

impl From<ThinkingConfig> for AnthropicThinking {
    fn from(thinking: ThinkingConfig) -> Self {
        match thinking {
            ThinkingConfig::Enabled { budget_tokens } => {
                AnthropicThinking::Enabled { budget_tokens }
            }
        }
    }
}

impl From<AnthropicMessage> for Message {
    fn from(message: AnthropicMessage) -> Self {
        let AnthropicMessage {
            id,
            r#type,
            role,
            content,
            model,
            stop_reason,
            stop_sequence,
            usage,
            container,
        } = message;

        Self {
            id,
            r#type: r#type.into(),
            role: role.into(),
            content: content.into_iter().map(Into::into).collect(),
            model,
            stop_reason: stop_reason.map(Into::into),
            stop_sequence,
            usage: usage.into(),
            container: container.map(Into::into),
        }
    }
}

impl From<AnthropicMessageType> for MessageType {
    fn from(message_type: AnthropicMessageType) -> Self {
        match message_type {
            AnthropicMessageType::Message => Self::Message,
        }
    }
}

impl From<AnthropicRole> for Role {
    fn from(role: AnthropicRole) -> Self {
        match role {
            AnthropicRole::User => Self::User,
            AnthropicRole::Assistant => Self::Assistant,
        }
    }
}

impl From<AnthropicStopReason> for StopReason {
    fn from(stop_reason: AnthropicStopReason) -> Self {
        match stop_reason {
            AnthropicStopReason::EndTurn => Self::EndTurn,
            AnthropicStopReason::MaxTokens => Self::MaxTokens,
            AnthropicStopReason::StopSequence => Self::StopSequence,
            AnthropicStopReason::ToolUse => Self::ToolUse,
            AnthropicStopReason::PauseTurn => Self::PauseTurn,
            AnthropicStopReason::Refusal => Self::Refusal,
        }
    }
}

impl From<AnthropicContainer> for Container {
    fn from(container: AnthropicContainer) -> Self {
        let AnthropicContainer { expires_at, id } = container;

        Self { expires_at, id }
    }
}

impl From<AnthropicContentBlock> for ContentBlock {
    fn from(content_block: AnthropicContentBlock) -> Self {
        match content_block {
            AnthropicContentBlock::TextBlock { text, citations } => Self::TextBlock {
                text: text.clone(),
                citations: citations.into_iter().map(|c| (&c).into()).collect(),
            },
            AnthropicContentBlock::ThinkingBlock {
                signature,
                thinking,
            } => Self::ThinkingBlock {
                signature: signature.clone(),
                thinking: thinking.clone(),
            },
            AnthropicContentBlock::RedactedThinkingBlock { data } => {
                Self::RedactedThinkingBlock { data: data.clone() }
            }
            AnthropicContentBlock::ToolUseBlock { id, input, name } => Self::ToolUseBlock {
                id: id.clone(),
                input: input.clone(),
                name: name.clone(),
            },
            AnthropicContentBlock::ServerToolUseBlock { id, input, name } => {
                Self::ServerToolUseBlock {
                    id: id.clone(),
                    input: input.clone(),
                    name: name.clone(),
                }
            }
            AnthropicContentBlock::WebSearchToolResultBlock {
                content,
                tool_use_id,
            } => Self::WebSearchToolResultBlock {
                content: content.iter().map(Into::into).collect(),
                tool_use_id: tool_use_id.clone(),
            },
            AnthropicContentBlock::CodeExecutionToolResultBlock {
                content,
                tool_use_id,
            } => Self::CodeExecutionToolResultBlock {
                content: content.clone(),
                tool_use_id: tool_use_id.clone(),
            },
            AnthropicContentBlock::MCPToolUseBlock {
                id,
                input,
                name,
                server_name,
            } => Self::MCPToolUseBlock {
                id: id.clone(),
                input: input.clone(),
                name: name.clone(),
                server_name: server_name.clone(),
            },
            AnthropicContentBlock::MCPToolResultBlock {
                tool_use_id,
                content,
                is_error,
            } => Self::MCPToolResultBlock {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error,
            },
            AnthropicContentBlock::ContainerUploadBlock { file_id } => Self::ContainerUploadBlock {
                file_id: file_id.clone(),
            },
        }
    }
}

impl From<AnthropicUsage> for Usage {
    fn from(usage: AnthropicUsage) -> Self {
        let AnthropicUsage {
            cache_creation,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            input_tokens,
            output_tokens,
            server_tool_use,
            service_tier,
        } = usage;

        Self {
            cache_creation: cache_creation.map(Into::into),
            cache_creation_input_tokens,
            cache_read_input_tokens,
            input_tokens,
            output_tokens,
            server_tool_use: server_tool_use.map(Into::into),
            service_tier: service_tier.map(Into::into),
        }
    }
}

impl From<AnthropicUsageDelta> for UsageDelta {
    fn from(usage: AnthropicUsageDelta) -> Self {
        let AnthropicUsageDelta {
            cache_creation,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            input_tokens,
            output_tokens,
            server_tool_use,
            service_tier,
        } = usage;

        Self {
            cache_creation: cache_creation.map(Into::into),
            cache_creation_input_tokens,
            cache_read_input_tokens,
            input_tokens,
            output_tokens,
            server_tool_use: server_tool_use.map(Into::into),
            service_tier: service_tier.map(Into::into),
        }
    }
}

impl From<AnthropicCacheCreation> for CacheCreation {
    fn from(cache_creation: AnthropicCacheCreation) -> Self {
        let AnthropicCacheCreation {
            ephemeral_1h_input_tokens,
            ephemeral_5m_input_tokens,
        } = cache_creation;

        Self {
            ephemeral_1h_input_tokens,
            ephemeral_5m_input_tokens,
        }
    }
}

impl From<AnthropicServerToolUsage> for ServerToolUsage {
    fn from(tool_usage: AnthropicServerToolUsage) -> Self {
        let AnthropicServerToolUsage {
            web_search_requests,
        } = tool_usage;

        Self {
            web_search_requests,
        }
    }
}

impl From<AnthropicServiceTier> for ServiceTier {
    fn from(service_tier: AnthropicServiceTier) -> Self {
        match service_tier {
            AnthropicServiceTier::Standard => Self::Standard,
            AnthropicServiceTier::Priority => Self::Priority,
            AnthropicServiceTier::Batch => Self::Batch,
        }
    }
}

impl From<AnthropicApiError> for ApiError {
    fn from(error: AnthropicApiError) -> Self {
        match error {
            AnthropicApiError::ApiError { message } => Self::ApiError { message },
            AnthropicApiError::AuthenticationError { message } => {
                Self::AuthenticationError { message }
            }
            AnthropicApiError::BillingError { message } => Self::BillingError { message },
            AnthropicApiError::InvalidRequestError { message } => {
                Self::InvalidRequestError { message }
            }
            AnthropicApiError::NotFoundError { message } => Self::NotFoundError { message },
            AnthropicApiError::OverloadedError { message } => Self::OverloadedError { message },
            AnthropicApiError::PermissionError { message } => Self::PermissionError { message },
            AnthropicApiError::RateLimitError { message } => Self::RateLimitError { message },
            AnthropicApiError::GatewayTimeoutError { message } => {
                Self::GatewayTimeoutError { message }
            }
        }
    }
}

impl From<AnthropicStreamingEvent> for StreamingEvent {
    fn from(event: AnthropicStreamingEvent) -> Self {
        match event {
            AnthropicStreamingEvent::MessageStart { message } => StreamingEvent::MessageStart {
                message: message.into(),
            },
            AnthropicStreamingEvent::UsageUpdate { usage } => StreamingEvent::UsageUpdate {
                usage: usage.into(),
            },
            AnthropicStreamingEvent::FinalizedMessage { message } => {
                StreamingEvent::FinalizedMessage {
                    message: message.into(),
                }
            }
            AnthropicStreamingEvent::Error { error } => StreamingEvent::Error {
                error: error.into(),
            },
            AnthropicStreamingEvent::TextStart { text, citations } => StreamingEvent::TextStart {
                text,
                citations: citations.iter().map(Into::into).collect(),
            },
            AnthropicStreamingEvent::TextUpdate { diff, snapshot } => {
                StreamingEvent::TextUpdate { diff, snapshot }
            }
            AnthropicStreamingEvent::TextStop { text } => StreamingEvent::TextStop { text },
            AnthropicStreamingEvent::ThinkingStart { thinking } => {
                StreamingEvent::ThinkingStart { thinking }
            }
            AnthropicStreamingEvent::ThinkingUpdate { diff, snapshot } => {
                StreamingEvent::ThinkingUpdate { diff, snapshot }
            }
            AnthropicStreamingEvent::ThinkingStop => StreamingEvent::ThinkingStop,
            AnthropicStreamingEvent::ParseError { error, raw_data } => {
                StreamingEvent::ParseError { error, raw_data }
            }
            AnthropicStreamingEvent::ProtocolError { error } => {
                StreamingEvent::ProtocolError { error }
            }
        }
    }
}
