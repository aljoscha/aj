use std::pin::Pin;

use anyhow::{Context, Result, anyhow};
use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};
use reqwest;
use reqwest::{Client as ReqwestClient, StatusCode};
use thiserror::Error;

use crate::messages::{ApiError, ApiErrorResponse, Message, Messages, ServerSentEvent};
use crate::streaming::{StreamProcessor, StreamingEvent};

const BASE_URL: &str = "https://api.anthropic.com";

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
    /// Beta feature headers (e.g. `["mcp-client-2025-11-20"]`).
    beta_headers: Vec<String>,
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
            beta_headers: Vec::new(),
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

    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    /// Build a request with common headers (auth, version, content-type,
    /// and any configured beta headers).
    fn build_request(&self) -> reqwest::RequestBuilder {
        let mut builder = self.client.post(format!("{}/v1/messages", self.base_url));

        match self.auth_mode {
            AuthMode::ApiKey => {
                builder = builder.header("x-api-key", self.api_key.clone());
            }
            AuthMode::OAuth => {
                builder = builder
                    .header("authorization", format!("Bearer {}", self.api_key))
                    .header("user-agent", "claude-cli/2.1.0")
                    .header("x-app", "cli");
            }
        }

        builder = builder
            .header("anthropic-version", self.version.clone())
            .header("content-type", "application/json");

        let mut betas = self.beta_headers.clone();
        if self.auth_mode == AuthMode::OAuth {
            for required in [
                "claude-code-20250219",
                "oauth-2025-04-20",
                "fine-grained-tool-streaming-2025-05-14",
            ] {
                if !betas.iter().any(|b| b == required) {
                    betas.push(required.to_string());
                }
            }
        }

        if !betas.is_empty() {
            builder = builder.header("anthropic-beta", betas.join(","));
        }

        builder
    }
}

impl Client {
    pub async fn messages(&self, messages: Messages) -> Result<Message, anyhow::Error> {
        let request_builder = self.build_request().json(&messages);

        let response = request_builder
            .send()
            .await
            .context("failed to send request");

        let response = response?;

        match response.status() {
            StatusCode::OK => {
                let json_text = response
                    .text()
                    .await
                    .context("failed to read response text")?;
                let response: Message = serde_json::from_str(&json_text)?;
                Ok(response)
            }
            StatusCode::BAD_REQUEST
            | StatusCode::UNAUTHORIZED
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::INTERNAL_SERVER_ERROR => {
                let error_text = response.text().await?;
                match serde_json::from_str::<ApiErrorResponse>(&error_text) {
                    Ok(error_response) => Err(anyhow!(error_response.error)),
                    Err(_) => Err(anyhow!("request failed: {}", error_text)),
                }
            }
            _ => {
                let error_message = format!("unexpected status code: {:?}", response.text().await?);
                Err(anyhow!(error_message))
            }
        }
    }

    pub async fn messages_stream_raw(
        &self,
        mut messages: Messages,
    ) -> Result<Pin<Box<dyn Stream<Item = ServerSentEvent> + Send>>, ClientError> {
        messages.stream = Some(true);

        let request_builder = self.build_request().json(&messages);

        let response = request_builder.send().await?;

        match response.status() {
            StatusCode::OK => {
                let stream = response.bytes_stream().eventsource();

                let stream = stream.filter_map(|event| async move {
                    match event {
                        Ok(event) => match serde_json::from_str::<ServerSentEvent>(&event.data) {
                            Ok(json_event) => Some(json_event),
                            Err(err) => {
                                // The API versioning policy reserves the right
                                // to add new event types; skip anything we
                                // can't parse rather than crashing the stream.
                                tracing::warn!(
                                    "could not parse server-sent event, skipping: \
                                     error={err}, data={}",
                                    event.data
                                );
                                None
                            }
                        },
                        Err(e) => {
                            tracing::error!("event-stream error: {e}");
                            None
                        }
                    }
                });
                Ok(stream.boxed())
            }
            StatusCode::BAD_REQUEST
            | StatusCode::UNAUTHORIZED
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::INTERNAL_SERVER_ERROR => {
                let error_text = response.text().await?;
                match serde_json::from_str::<ApiErrorResponse>(&error_text) {
                    Ok(error_response) => Err(error_response.error.into()),
                    Err(_) => Err(ApiError::ApiError {
                        message: error_text,
                    }
                    .into()),
                }
            }
            status_code => {
                let error_message = format!(
                    "unexpected status code ({status_code}), response text: {:?}",
                    response.text().await?
                );
                Err(ClientError::InternalError(error_message))
            }
        }
    }

    pub async fn messages_stream(
        &self,
        messages: Messages,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamingEvent> + Send>>, ClientError> {
        let stream = self.messages_stream_raw(messages).await?;
        Ok(create_high_level_stream(stream))
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("{0}")]
    ApiError(#[from] ApiError),
    #[error("{0}")]
    TransportError(#[from] reqwest::Error),
    #[error("{0}")]
    ParseError(String),
    #[error("{0}")]
    InternalError(String),
}

fn create_high_level_stream<S>(stream: S) -> Pin<Box<dyn Stream<Item = StreamingEvent> + Send>>
where
    S: Stream<Item = ServerSentEvent> + Send + 'static,
{
    let processor = StreamProcessor::new();
    processor.process_stream(stream)
}
