use std::pin::Pin;

use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};
use reqwest::{Client as ReqwestClient, StatusCode};
use thiserror::Error;

use crate::types::chat_completions::{
    CreateChatCompletionRequest, CreateChatCompletionResponse, CreateChatCompletionStreamResponse,
};
use crate::types::common::{ApiError, ApiErrorResponse};
use crate::types::responses::{
    CreateResponseRequest, Response as ResponsesResponse, ResponseStreamEvent,
};

const OPENAI_API_BASE_URL: &str = "https://api.openai.com/v1";

/// Log an outgoing request URL and JSON body at `DEBUG` level. Gated
/// on `tracing::enabled!` so callers don't pay the serialization cost
/// when debug logging is disabled.
fn debug_log_request<T: serde::Serialize>(url: &str, body: &T) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    match serde_json::to_string(body) {
        Ok(json) => tracing::debug!(
            url = %url,
            body = %json,
            "openai-sdk: outgoing request",
        ),
        Err(err) => tracing::debug!(
            url = %url,
            "openai-sdk: outgoing request (body serialization failed: {err})",
        ),
    }
}

pub struct Client {
    client: ReqwestClient,
    api_key: String,
    base_url: String,
    /// Extra request headers merged into every outgoing call.
    /// Used by the Responses provider for session-correlation
    /// headers (`session_id`, `x-client-request-id`).
    extra_headers: Vec<(String, String)>,
}

impl Client {
    pub fn new(base_url: Option<String>, api_key: String) -> Self {
        let base_url = base_url.unwrap_or_else(|| OPENAI_API_BASE_URL.to_string());

        Self {
            client: ReqwestClient::new(),
            api_key,
            base_url,
            extra_headers: Vec::new(),
        }
    }

    /// Add a header to be sent with every request.
    pub fn with_extra_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    fn apply_extra_headers(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (name, value) in &self.extra_headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder
    }

    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    pub async fn chat_completions(
        &self,
        request: CreateChatCompletionRequest,
    ) -> Result<CreateChatCompletionResponse, ClientError> {
        let url = format!("{}/chat/completions", self.base_url);
        debug_log_request(&url, &request);
        let request_builder = self.apply_extra_headers(
            self.client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&request),
        );

        let response = request_builder.send().await?;

        let status = response.status();
        if status == StatusCode::OK {
            let json_text = response.text().await?;
            let response = serde_json::from_str(&json_text).map_err(|err| {
                ClientError::ParseError(format!("could not parse response body: {err}"))
            })?;
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

    pub async fn chat_completions_stream(
        &self,
        mut request: CreateChatCompletionRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<CreateChatCompletionStreamResponse, ClientError>> + Send>>,
        ClientError,
    > {
        request.stream = Some(true);

        let url = format!("{}/chat/completions", self.base_url);
        debug_log_request(&url, &request);
        let request_builder = self.apply_extra_headers(
            self.client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&request),
        );

        let response = request_builder.send().await?;

        let status = response.status();
        if status == StatusCode::OK {
            let stream = response.bytes_stream().eventsource();

            let stream = stream.filter_map(|event| async move {
                match event {
                    Ok(event) => {
                        if event.data == "[DONE]" {
                            return None;
                        }
                        match serde_json::from_str::<CreateChatCompletionStreamResponse>(
                            &event.data,
                        ) {
                            Ok(json_event) => Some(Ok(json_event)),
                            Err(err) => {
                                let err = format!(
                                    "could not parse server-sent event {}: {}",
                                    event.data, err
                                );
                                Some(Err(ClientError::ParseError(err)))
                            }
                        }
                    }
                    Err(e) => Some(Err(ClientError::InternalError(e.to_string()))),
                }
            });
            return Ok(stream.boxed());
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

    pub async fn responses(
        &self,
        request: CreateResponseRequest,
    ) -> Result<ResponsesResponse, ClientError> {
        let url = format!("{}/responses", self.base_url);
        debug_log_request(&url, &request);
        let request_builder = self.apply_extra_headers(
            self.client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&request),
        );

        let response = request_builder.send().await?;

        let status = response.status();
        if status == StatusCode::OK {
            let json_text = response.text().await?;
            let response = serde_json::from_str(&json_text).map_err(|err| {
                ClientError::ParseError(format!("could not parse response body: {err}"))
            })?;
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

    pub async fn responses_stream(
        &self,
        request: CreateResponseRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ResponseStreamEvent, ClientError>> + Send>>,
        ClientError,
    > {
        self.responses_stream_at_path("/responses", request).await
    }

    /// Stream the Codex Responses endpoint at `{base_url}/codex/responses`.
    ///
    /// The wire format and SSE event shape are identical to
    /// [`responses_stream`](Self::responses_stream); only the URL path
    /// differs. Codex-specific headers (`chatgpt-account-id`,
    /// `originator`, `User-Agent`, `OpenAI-Beta`) are expected to be
    /// installed on the [`Client`] via
    /// [`with_extra_header`](Self::with_extra_header) before the call.
    pub async fn codex_responses_stream(
        &self,
        request: CreateResponseRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ResponseStreamEvent, ClientError>> + Send>>,
        ClientError,
    > {
        self.responses_stream_at_path("/codex/responses", request)
            .await
    }

    /// Shared implementation behind [`responses_stream`] and
    /// [`codex_responses_stream`]: the two endpoints differ only in
    /// URL path; the request body, header machinery, and SSE parser
    /// are identical.
    async fn responses_stream_at_path(
        &self,
        path: &str,
        mut request: CreateResponseRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ResponseStreamEvent, ClientError>> + Send>>,
        ClientError,
    > {
        request.stream = Some(true);

        let url = format!("{}{}", self.base_url, path);
        debug_log_request(&url, &request);
        let request_builder = self.apply_extra_headers(
            self.client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&request),
        );

        let response = request_builder.send().await?;

        let status = response.status();
        if status == StatusCode::OK {
            let stream = response.bytes_stream().eventsource();

            let stream = stream.filter_map(|event| async move {
                match event {
                    Ok(event) => {
                        if event.data == "[DONE]" {
                            return None;
                        }
                        match serde_json::from_str::<ResponseStreamEvent>(&event.data) {
                            Ok(json_event) => Some(Ok(json_event)),
                            Err(err) => {
                                let err = format!(
                                    "could not parse server-sent event {}: {}",
                                    event.data, err
                                );
                                Some(Err(ClientError::ParseError(err)))
                            }
                        }
                    }
                    Err(e) => Some(Err(ClientError::InternalError(e.to_string()))),
                }
            });
            return Ok(stream.boxed());
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
    status: StatusCode,
    http_status: u16,
    retry_after: Option<String>,
    error_text: String,
) -> ClientError {
    if status.is_client_error() || status.is_server_error() {
        let error = match serde_json::from_str::<ApiErrorResponse>(&error_text) {
            Ok(error_response) => error_response.error,
            Err(_) => ApiError {
                message: format!("request failed ({status}): {error_text}"),
                r#type: None,
                param: None,
                code: None,
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
        /// Raw `Retry-After` header value, if present.
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
    /// from a server response. `None` for transport / parse / internal
    /// failures.
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
    use super::*;

    #[test]
    fn classify_error_response_structures_a_client_error() {
        // Both the non-streaming and streaming entry points route
        // non-2xx responses through this helper, so a structured error
        // with status + Retry-After is the shared contract.
        let body = r#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#;
        let err = classify_error_response(
            StatusCode::TOO_MANY_REQUESTS,
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
            StatusCode::INTERNAL_SERVER_ERROR,
            500,
            None,
            "upstream boom".to_string(),
        );
        assert_eq!(err.http_status(), Some(500));
        assert!(err.to_string().contains("upstream boom"), "got: {err}");
    }
}
