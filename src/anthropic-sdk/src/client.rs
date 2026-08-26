use std::fmt::Display;
use std::pin::Pin;
use std::time::Duration;

use eventsource_stream::{Event, Eventsource};
use futures::{Stream, StreamExt, future};
use reqwest::Client as ReqwestClient;
use thiserror::Error;

use crate::messages::{ApiError, ApiErrorResponse, Message, Messages, ServerSentEvent};
use crate::stealth::{
    CLAUDE_CODE_VERSION, apply_request_transformations, collect_caller_tool_names,
    reverse_map_event, reverse_map_message,
};
use crate::usage::OAuthUsage;

const BASE_URL: &str = "https://api.anthropic.com";

/// Beta header for fine-grained tool input streaming. Always sent —
/// the API silently ignores it on models that don't support it.
const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";

/// Beta header for interleaved thinking. Sent only when the caller
/// opts in — adaptive-thinking models reject it.
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

/// Beta headers required for OAuth authentication.
const OAUTH_REQUIRED_BETAS: &[&str] = &["claude-code-20250219", "oauth-2025-04-20"];

/// Authentication mode for the Anthropic API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    /// Standard API key authentication (`x-api-key` header).
    ApiKey,
    /// OAuth token authentication (`Authorization: Bearer` header).
    OAuth,
}

pub struct Client {
    client: ReqwestClient,
    api_key: String,
    auth_mode: AuthMode,
    version: String,
    base_url: String,
    #[cfg(test)]
    stream_response_override: std::sync::Mutex<Option<reqwest::Response>>,
    /// Beta feature headers configured by the caller (e.g.
    /// `["mcp-client-2025-11-20"]`). The default
    /// `fine-grained-tool-streaming-2025-05-14` and the optional
    /// `interleaved-thinking-2025-05-14` and OAuth-required betas are
    /// added automatically and are not stored here.
    beta_headers: Vec<String>,
    /// Whether to send the `interleaved-thinking-2025-05-14` beta
    /// header. The provider sets this when reasoning is enabled and
    /// the model is non-adaptive (adaptive models reject the header).
    interleaved_thinking: bool,
}

impl Client {
    pub fn new(base_url: Option<String>, api_key: String) -> Self {
        let base_url = base_url.unwrap_or_else(|| BASE_URL.to_string());
        let auth_mode = if api_key.starts_with("sk-ant-oat") {
            AuthMode::OAuth
        } else {
            AuthMode::ApiKey
        };

        Self {
            client: ReqwestClient::new(),
            api_key,
            auth_mode,
            version: "2023-06-01".to_string(),
            base_url,
            #[cfg(test)]
            stream_response_override: std::sync::Mutex::new(None),
            beta_headers: Vec::new(),
            interleaved_thinking: false,
        }
    }

    /// Returns whether this client is using OAuth authentication.
    pub fn is_oauth(&self) -> bool {
        self.auth_mode == AuthMode::OAuth
    }

    /// Add a beta feature header (e.g. `"mcp-client-2025-11-20"`).
    pub fn with_beta(mut self, beta: impl Into<String>) -> Self {
        self.beta_headers.push(beta.into());
        self
    }

    /// Add multiple beta feature headers.
    pub fn with_betas(mut self, betas: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.beta_headers.extend(betas.into_iter().map(Into::into));
        self
    }

    /// Set beta headers, replacing any previously configured.
    pub fn set_betas(&mut self, betas: Vec<String>) {
        self.beta_headers = betas;
    }

    /// Enable the `interleaved-thinking-2025-05-14` beta. The provider
    /// should set this when the caller has reasoning enabled and the
    /// chosen model is non-adaptive — adaptive-thinking models (Opus
    /// 4.6+, Sonnet 4.6+) reject the header. Defaults to `false`.
    pub fn with_interleaved_thinking(mut self, enabled: bool) -> Self {
        self.interleaved_thinking = enabled;
        self
    }

    /// Toggle the interleaved-thinking beta header on an existing
    /// client. See [`Self::with_interleaved_thinking`].
    pub fn set_interleaved_thinking(&mut self, enabled: bool) {
        self.interleaved_thinking = enabled;
    }

    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    /// Compute the full set of beta headers for this request, in
    /// declaration order: caller-configured first, then the always-on
    /// `fine-grained-tool-streaming` beta, then OAuth-required betas
    /// when in OAuth mode, then the optional `interleaved-thinking`
    /// beta. Duplicates are filtered.
    fn effective_beta_headers(&self) -> Vec<String> {
        let mut betas = self.beta_headers.clone();
        let push_unique = |betas: &mut Vec<String>, value: &str| {
            if !betas.iter().any(|b| b == value) {
                betas.push(value.to_string());
            }
        };
        push_unique(&mut betas, FINE_GRAINED_TOOL_STREAMING_BETA);
        if self.auth_mode == AuthMode::OAuth {
            for required in OAUTH_REQUIRED_BETAS {
                push_unique(&mut betas, required);
            }
        }
        if self.interleaved_thinking {
            push_unique(&mut betas, INTERLEAVED_THINKING_BETA);
        }
        betas
    }

    /// Build a request for `POST /v1/messages` with the common headers.
    fn build_request(&self) -> reqwest::RequestBuilder {
        let builder = self.client.post(format!("{}/v1/messages", self.base_url));
        self.apply_common_headers(builder)
    }

    /// Apply the headers shared by every endpoint: auth, version,
    /// content-type, and the configured + always-on beta headers.
    fn apply_common_headers(
        &self,
        mut builder: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        match self.auth_mode {
            AuthMode::ApiKey => {
                builder = builder.header("x-api-key", self.api_key.clone());
            }
            AuthMode::OAuth => {
                builder = builder
                    .header("authorization", format!("Bearer {}", self.api_key))
                    .header("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}"))
                    .header("x-app", "cli");
            }
        }

        builder = builder
            .header("anthropic-version", self.version.clone())
            .header("content-type", "application/json");

        let betas = self.effective_beta_headers();
        if !betas.is_empty() {
            builder = builder.header("anthropic-beta", betas.join(","));
        }

        builder
    }

    /// Log the outgoing request URL and JSON body at `DEBUG` level.
    /// Gated on `tracing::enabled!` so callers don't pay the
    /// serialization cost when debug logging is disabled.
    fn debug_log_request(&self, body: &Messages) {
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let url = format!("{}/v1/messages", self.base_url);
        match serde_json::to_string(body) {
            Ok(json) => tracing::debug!(
                url = %url,
                body = %json,
                "anthropic-sdk: outgoing request",
            ),
            Err(err) => tracing::debug!(
                url = %url,
                "anthropic-sdk: outgoing request (body serialization failed: {err})",
            ),
        }
    }

    /// Apply OAuth stealth-mode transformations to the outgoing request
    /// and return the caller's tool-name list to be used for reverse
    /// mapping on the response. Returns an empty list when stealth
    /// mode does not apply.
    fn prepare_request(&self, messages: &mut Messages) -> Vec<String> {
        if self.auth_mode != AuthMode::OAuth {
            return Vec::new();
        }
        let caller_tool_names = collect_caller_tool_names(messages);
        apply_request_transformations(messages);
        caller_tool_names
    }

    #[cfg(test)]
    fn with_stream_response(self, response: reqwest::Response) -> Self {
        *self
            .stream_response_override
            .lock()
            .expect("stream response override mutex poisoned") = Some(response);
        self
    }
}

impl Client {
    pub async fn messages(&self, mut messages: Messages) -> Result<Message, ClientError> {
        let caller_tool_names = self.prepare_request(&mut messages);

        self.debug_log_request(&messages);
        let request_builder = self.build_request().json(&messages);

        let response = request_builder.send().await?;

        let status = response.status();
        if status.is_success() {
            let json_text = response.text().await?;
            let mut response: Message = serde_json::from_str(&json_text).map_err(|err| {
                ClientError::ParseError(format!("could not parse response body: {err}"))
            })?;
            if !caller_tool_names.is_empty() {
                reverse_map_message(&mut response, &caller_tool_names);
            }
            return Ok(response);
        }

        let http_status = status.as_u16();
        let retry_after = retry_after_header(&response);
        let error_text = response.text().await?;
        Err(classify_error_response(
            status,
            http_status,
            retry_after,
            error_text,
        ))
    }

    /// Stream the raw server-sent events from `POST /v1/messages`.
    ///
    /// The returned stream yields one [`ServerSentEvent`] per SSE frame
    /// whose JSON payload parses. Unparseable JSON payloads are logged at
    /// `warn` and dropped so a future API version that adds new event types
    /// does not crash the stream. OAuth stealth-mode reverse-mapping of tool
    /// names is applied per-event before yielding.
    ///
    /// A mid-stream transport error (a dropped connection, a read error
    /// from the event source) is logged at `error` and surfaced as
    /// end-of-stream: the stream simply ends. This layer does not
    /// distinguish an abnormal termination from a clean close, so a
    /// consumer that needs to detect a truncated turn must check for a
    /// terminal frame itself.
    pub async fn messages_stream(
        &self,
        mut messages: Messages,
    ) -> Result<Pin<Box<dyn Stream<Item = ServerSentEvent> + Send>>, ClientError> {
        messages.stream = Some(true);
        let caller_tool_names = self.prepare_request(&mut messages);

        self.debug_log_request(&messages);
        let request_builder = self.build_request().json(&messages);

        #[cfg(test)]
        let response = if let Some(response) = self
            .stream_response_override
            .lock()
            .expect("stream response override mutex poisoned")
            .take()
        {
            response
        } else {
            request_builder.send().await?
        };
        #[cfg(not(test))]
        let response = request_builder.send().await?;

        let status = response.status();
        if status.is_success() {
            let stream = adapt_sse_events(response.bytes_stream().eventsource(), caller_tool_names);
            return Ok(stream.boxed());
        }

        // Capture status + Retry-After before consuming the response
        // body — `text()` takes the response by value.
        let http_status = status.as_u16();
        let retry_after = retry_after_header(&response);
        let error_text = response.text().await?;
        Err(classify_error_response(
            status,
            http_status,
            retry_after,
            error_text,
        ))
    }

    /// Fetch plan rate-limit utilization from `GET /api/oauth/usage`.
    ///
    /// Only meaningful in OAuth mode — the endpoint rejects plain API
    /// keys, so we fail fast with a clear error instead of sending a
    /// request that can only 401.
    pub async fn oauth_usage(&self) -> Result<OAuthUsage, ClientError> {
        if self.auth_mode != AuthMode::OAuth {
            return Err(ClientError::InternalError(
                "usage reporting requires OAuth authentication".to_string(),
            ));
        }

        let builder = self
            .client
            .get(format!("{}/api/oauth/usage", self.base_url))
            // The endpoint is a quick lookup; a tight timeout keeps a
            // stalled request from hanging a UI that's waiting on it.
            .timeout(Duration::from_secs(5));
        let response = self.apply_common_headers(builder).send().await?;

        let status = response.status();
        if status.is_success() {
            let text = response.text().await?;
            return serde_json::from_str(&text).map_err(|err| {
                ClientError::ParseError(format!("could not parse usage response: {err}"))
            });
        }

        let http_status = status.as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let error_text = response.text().await?;
        let error = match serde_json::from_str::<ApiErrorResponse>(&error_text) {
            Ok(error_response) => error_response.error,
            Err(_) => ApiError::ApiError {
                message: format!("request failed ({status}): {error_text}"),
            },
        };
        Err(ClientError::ApiError {
            error,
            http_status,
            retry_after,
        })
    }
}

/// Decode SSE payloads, ending the stream on a source error while skipping
/// payloads this SDK does not understand.
fn adapt_sse_events<S, E>(
    stream: S,
    caller_tool_names: Vec<String>,
) -> impl Stream<Item = ServerSentEvent>
where
    S: Stream<Item = Result<Event, E>>,
    E: Display,
{
    stream
        .scan(caller_tool_names, |caller_tool_names, event| {
            let decoded = match event {
                Err(err) => {
                    tracing::error!("event-stream error: {err}");
                    return future::ready(None);
                }
                Ok(event) => match serde_json::from_str::<ServerSentEvent>(&event.data) {
                    Ok(mut json_event) => {
                        if !caller_tool_names.is_empty() {
                            reverse_map_event(&mut json_event, caller_tool_names);
                        }
                        Some(json_event)
                    }
                    Err(err) => {
                        // The API versioning policy reserves the right to add
                        // new event types. Skip anything we cannot parse rather
                        // than crashing or terminating a healthy stream.
                        tracing::warn!(
                            "could not parse server-sent event, skipping: \
                             error={err}, data={}",
                            event.data
                        );
                        None
                    }
                },
            };
            future::ready(Some(decoded))
        })
        .filter_map(future::ready)
}

/// Extract the raw `Retry-After` header value, if present and printable.
fn retry_after_header(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Map a non-success HTTP response into a [`ClientError`].
///
/// `error_text` is the already-read response body. A client/server
/// error becomes a structured [`ClientError::ApiError`] (with a
/// synthetic [`ApiError`] when the body isn't the expected shape) and
/// carries the status plus `Retry-After` for the retry layer. Any other
/// non-success status maps to [`ClientError::InternalError`].
fn classify_error_response(
    status: reqwest::StatusCode,
    http_status: u16,
    retry_after: Option<String>,
    error_text: String,
) -> ClientError {
    if status.is_client_error() || status.is_server_error() {
        let error = match serde_json::from_str::<ApiErrorResponse>(&error_text) {
            Ok(error_response) => error_response.error,
            Err(_) => ApiError::ApiError {
                message: format!("request failed ({status}): {error_text}"),
            },
        };
        ClientError::ApiError {
            error,
            http_status,
            retry_after,
        }
    } else {
        ClientError::InternalError(format!("unexpected status code ({status}): {error_text}"))
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("{error} (http {http_status})")]
    ApiError {
        #[source]
        error: ApiError,
        http_status: u16,
        /// Raw `Retry-After` header value, if present. Callers parse
        /// this with `aj-models::errors::parse_retry_after`.
        retry_after: Option<String>,
    },
    #[error("{0}")]
    TransportError(#[from] reqwest::Error),
    #[error("{0}")]
    ParseError(String),
    #[error("{0}")]
    InternalError(String),
}

impl ClientError {
    /// HTTP status of the originating response, when this error came
    /// from a server response (i.e. [`Self::ApiError`]). Returns
    /// `None` for transport / parse / internal failures.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::ApiError { http_status, .. } => Some(*http_status),
            _ => None,
        }
    }

    /// Raw `Retry-After` header value from the originating response.
    pub fn retry_after(&self) -> Option<&str> {
        match self {
            Self::ApiError { retry_after, .. } => retry_after.as_deref(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;

    use super::*;
    use crate::stealth::CLAUDE_CODE_IDENTITY_PROMPT;

    #[derive(Clone, Copy, Debug)]
    struct SyntheticStreamError;

    impl std::fmt::Display for SyntheticStreamError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("synthetic stream failure")
        }
    }

    impl std::error::Error for SyntheticStreamError {}

    fn event(data: &str) -> Event {
        Event {
            data: data.to_string(),
            ..Event::default()
        }
    }

    #[tokio::test]
    async fn source_error_terminates_without_polling_the_source_again() {
        let polls = Arc::new(AtomicUsize::new(0));
        let source = futures::stream::poll_fn({
            let polls = Arc::clone(&polls);
            move |_| {
                let previous = polls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(previous, 0, "the source was polled again after its error");
                Poll::Ready(Some(Result::<Event, SyntheticStreamError>::Err(
                    SyntheticStreamError,
                )))
            }
        });
        let mut stream = Box::pin(adapt_sse_events(source, Vec::new()));

        assert!(
            stream.next().await.is_none(),
            "the source error ends the stream"
        );
        assert_eq!(polls.load(Ordering::SeqCst), 1, "exactly one source poll");
    }

    #[tokio::test]
    async fn source_error_hides_every_ready_item_after_it() {
        let source = futures::stream::iter([
            Err(SyntheticStreamError),
            Ok(event(r#"{"type":"ping"}"#)),
            Ok(event(r#"{"type":"ping"}"#)),
        ]);
        let mut stream = Box::pin(adapt_sse_events(source, Vec::new()));

        for poll in 1..=3 {
            assert!(
                stream.next().await.is_none(),
                "poll {poll} after the terminal error must not resume the adapter"
            );
        }
    }

    #[tokio::test]
    async fn unparseable_json_payloads_are_skipped_without_ending_the_stream() {
        let source = futures::stream::iter([
            Ok::<_, SyntheticStreamError>(event(r#"{"type":"future_event"}"#)),
            Ok(event(r#"{"type":"ping"#)),
            Ok(event(r#"{"type":"ping"}"#)),
        ]);
        let mut stream = Box::pin(adapt_sse_events(source, Vec::new()));

        assert!(
            matches!(stream.next().await, Some(ServerSentEvent::Ping)),
            "the valid event after unknown and malformed JSON must still be delivered"
        );
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn adapter_restores_the_callers_tool_name() {
        let source = futures::stream::once(future::ready(Ok::<_, SyntheticStreamError>(event(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","input":{},"name":"Edit"}}"#,
        ))));
        let mut stream = Box::pin(adapt_sse_events(source, vec!["edit".to_string()]));

        match stream.next().await {
            Some(ServerSentEvent::ContentBlockStart {
                content_block: crate::messages::ContentBlock::ToolUseBlock { name, .. },
                ..
            }) => assert_eq!(name, "edit"),
            other => panic!("expected a tool-use start, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn messages_stream_carries_oauth_tool_names_into_response_mapping() {
        let body = reqwest::Body::from(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"input\":{},\"name\":\"Edit\"}}\n\n",
        );
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(reqwest::StatusCode::OK)
                .body(body)
                .expect("build synthetic response"),
        );
        let client = Client::new(
            Some("http://127.0.0.1:1".to_string()),
            "sk-ant-oat-test".to_string(),
        )
        .with_stream_response(response);
        let mut messages = Messages::default();
        messages.tools.push(crate::messages::ToolUnion::Custom {
            name: "edit".to_string(),
            description: None,
            input_schema: serde_json::json!({}),
            cache_control: None,
            allowed_callers: Vec::new(),
            defer_loading: None,
            eager_input_streaming: None,
            input_examples: Vec::new(),
            strict: None,
        });

        let mut stream = client
            .messages_stream(messages)
            .await
            .expect("build messages stream");
        match stream.next().await {
            Some(ServerSentEvent::ContentBlockStart {
                content_block: crate::messages::ContentBlock::ToolUseBlock { name, .. },
                ..
            }) => assert_eq!(name, "edit"),
            other => panic!("expected a reverse-mapped tool-use start, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn messages_stream_terminates_on_its_response_source_error() {
        let polls = Arc::new(AtomicUsize::new(0));
        let body = reqwest::Body::wrap_stream(futures::stream::poll_fn({
            let polls = Arc::clone(&polls);
            move |_| {
                let previous = polls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(previous, 0, "the response body was polled after its error");
                Poll::Ready(Some(Result::<Vec<u8>, SyntheticStreamError>::Err(
                    SyntheticStreamError,
                )))
            }
        }));
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(reqwest::StatusCode::OK)
                .body(body)
                .expect("build synthetic response"),
        );
        let client = Client::new(Some("http://127.0.0.1:1".to_string()), "key".to_string())
            .with_stream_response(response);

        let mut stream = client
            .messages_stream(Messages::default())
            .await
            .expect("build messages stream");

        assert!(
            stream.next().await.is_none(),
            "the response source error must terminate the public stream"
        );
        assert_eq!(polls.load(Ordering::SeqCst), 1, "exactly one body poll");
    }

    #[test]
    fn api_key_mode_includes_default_beta() {
        let client = Client::new(None, "sk-ant-12345".to_string());
        let betas = client.effective_beta_headers();
        assert!(
            betas.iter().any(|b| b == FINE_GRAINED_TOOL_STREAMING_BETA),
            "expected fine-grained-tool-streaming beta in API key mode, got {betas:?}"
        );
        assert!(
            !betas.iter().any(|b| b == INTERLEAVED_THINKING_BETA),
            "interleaved-thinking should be off by default"
        );
        for required in OAUTH_REQUIRED_BETAS {
            assert!(
                !betas.iter().any(|b| b == required),
                "OAuth-only beta {required} leaked into API key mode"
            );
        }
    }

    #[test]
    fn oauth_mode_includes_required_and_default_betas() {
        let client = Client::new(None, "sk-ant-oat-test".to_string());
        assert!(client.is_oauth());
        let betas = client.effective_beta_headers();
        assert!(betas.iter().any(|b| b == FINE_GRAINED_TOOL_STREAMING_BETA));
        for required in OAUTH_REQUIRED_BETAS {
            assert!(
                betas.iter().any(|b| b == required),
                "missing OAuth-required beta {required}"
            );
        }
    }

    #[test]
    fn interleaved_thinking_opt_in() {
        let client = Client::new(None, "sk-ant-12345".to_string()).with_interleaved_thinking(true);
        let betas = client.effective_beta_headers();
        assert!(betas.iter().any(|b| b == INTERLEAVED_THINKING_BETA));
    }

    #[test]
    fn caller_betas_preserved_and_deduped() {
        let client = Client::new(None, "sk-ant-12345".to_string())
            .with_beta("custom-beta-1")
            .with_beta(FINE_GRAINED_TOOL_STREAMING_BETA);
        let betas = client.effective_beta_headers();
        // No duplicate of the always-on beta.
        let count = betas
            .iter()
            .filter(|b| *b == FINE_GRAINED_TOOL_STREAMING_BETA)
            .count();
        assert_eq!(count, 1);
        assert!(betas.iter().any(|b| b == "custom-beta-1"));
    }

    #[test]
    fn prepare_request_no_op_for_api_key() {
        let client = Client::new(None, "sk-ant-12345".to_string());
        let mut messages = Messages::default();
        let caller_tools = client.prepare_request(&mut messages);
        assert!(caller_tools.is_empty());
        // System prompt untouched.
        assert!(messages.system.is_none());
    }

    #[test]
    fn prepare_request_applies_stealth_for_oauth() {
        let client = Client::new(None, "sk-ant-oat-test".to_string());
        let mut messages = Messages::default();
        let caller_tools = client.prepare_request(&mut messages);
        assert!(caller_tools.is_empty());
        // System prompt now has the identity block.
        let system = messages.system.as_ref().unwrap();
        assert_eq!(system.len(), 1);
        match &system[0] {
            crate::messages::ContentBlockParam::TextBlock { text, .. } => {
                assert_eq!(text, CLAUDE_CODE_IDENTITY_PROMPT);
            }
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn classify_error_response_structures_a_client_error() {
        // The non-streaming `messages()` and the streaming path both
        // route non-2xx responses through this helper, so a structured
        // error with status + Retry-After is the shared contract.
        let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
        let err = classify_error_response(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            429,
            Some("5".to_string()),
            body.to_string(),
        );
        assert_eq!(err.http_status(), Some(429));
        assert_eq!(err.retry_after(), Some("5"));
        assert!(err.to_string().contains("slow down"), "got: {err}");
    }

    #[test]
    fn classify_error_response_synthesizes_unparseable_body() {
        let err = classify_error_response(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            500,
            None,
            "upstream boom".to_string(),
        );
        assert_eq!(err.http_status(), Some(500));
        assert!(err.to_string().contains("upstream boom"), "got: {err}");
    }
}
