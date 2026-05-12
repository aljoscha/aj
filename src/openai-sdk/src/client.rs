use std::pin::Pin;

use anyhow::{Context, Result, anyhow};
use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};
use reqwest::{Client as ReqwestClient, StatusCode};
use thiserror::Error;

use crate::types::responses::{
    CreateResponseRequest, Response as ResponsesResponse, ResponseStreamEvent,
};
use crate::types::{
    ApiError, ApiErrorResponse, CreateChatCompletionRequest, CreateChatCompletionResponse,
    CreateChatCompletionStreamResponse,
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
    ) -> Result<CreateChatCompletionResponse, anyhow::Error> {
        let url = format!("{}/chat/completions", self.base_url);
        debug_log_request(&url, &request);
        let request_builder = self.apply_extra_headers(
            self.client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&request),
        );

        let response = request_builder
            .send()
            .await
            .context("failed to send request")?;

        let status = response.status();
        if status == StatusCode::OK {
            let json_text = response
                .text()
                .await
                .context("failed to read response text")?;
            let response: CreateChatCompletionResponse = serde_json::from_str(&json_text)?;
            return Ok(response);
        }

        let error_text = response.text().await?;
        if status.is_client_error() || status.is_server_error() {
            match serde_json::from_str::<ApiErrorResponse>(&error_text) {
                Ok(error_response) => Err(anyhow!(error_response.error)),
                Err(_) => Err(anyhow!("request failed ({status}): {}", error_text)),
            }
        } else {
            Err(anyhow!("unexpected status code ({status}): {}", error_text))
        }
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
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let error_text = response.text().await?;
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
            Err(ClientError::ApiError {
                error,
                http_status,
                retry_after,
            })
        } else {
            Err(ClientError::InternalError(format!(
                "unexpected status code ({status}): {error_text}"
            )))
        }
    }

    pub async fn responses(
        &self,
        request: CreateResponseRequest,
    ) -> Result<ResponsesResponse, anyhow::Error> {
        let url = format!("{}/responses", self.base_url);
        debug_log_request(&url, &request);
        let request_builder = self.apply_extra_headers(
            self.client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&request),
        );

        let response = request_builder
            .send()
            .await
            .context("failed to send request")?;

        let status = response.status();
        if status == StatusCode::OK {
            let json_text = response
                .text()
                .await
                .context("failed to read response text")?;
            let response: ResponsesResponse = serde_json::from_str(&json_text)?;
            return Ok(response);
        }

        let error_text = response.text().await?;
        if status.is_client_error() || status.is_server_error() {
            match serde_json::from_str::<ApiErrorResponse>(&error_text) {
                Ok(error_response) => Err(anyhow!(error_response.error)),
                Err(_) => Err(anyhow!("request failed ({status}): {}", error_text)),
            }
        } else {
            Err(anyhow!("unexpected status code ({status}): {}", error_text))
        }
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
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let error_text = response.text().await?;
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
            Err(ClientError::ApiError {
                error,
                http_status,
                retry_after,
            })
        } else {
            Err(ClientError::InternalError(format!(
                "unexpected status code ({status}): {error_text}"
            )))
        }
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
