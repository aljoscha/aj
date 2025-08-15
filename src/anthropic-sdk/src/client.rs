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

pub struct Client {
    client: ReqwestClient,
    api_key: String,
    version: String,
    base_url: String,
}

impl Client {
    pub fn new(base_url: Option<String>, api_key: String) -> Self {
        let base_url = base_url.unwrap_or_else(|| BASE_URL.to_string());

        Self {
            client: ReqwestClient::new(),
            api_key,
            version: "2023-06-01".to_string(),
            base_url,
        }
    }

    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }
}

impl Client {
    pub async fn messages(&self, messages: Messages) -> Result<Message, anyhow::Error> {
        let request_builder = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", self.api_key.clone())
            .header("anthropic-version", self.version.clone())
            .header("content-type", "application/json")
            .json(&messages);

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

        let request_builder = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", self.api_key.clone())
            .header("anthropic-version", self.version.clone())
            // .header("anthropic-beta", "interleaved-thinking-2025-05-14")
            .header("content-type", "application/json")
            .json(&messages);

        let response = request_builder.send().await?;

        match response.status() {
            StatusCode::OK => {
                let stream = response.bytes_stream().eventsource();

                let stream = stream.filter_map(|event| async move {
                    match event {
                        Ok(event) => match serde_json::from_str::<ServerSentEvent>(&event.data) {
                            Ok(json_event) => Some(json_event),
                            Err(err) => {
                                panic!("could not parse server-sent event {}: {}", event.data, err);
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

// Helper function to create a high-level stream
fn create_high_level_stream<S>(stream: S) -> Pin<Box<dyn Stream<Item = StreamingEvent> + Send>>
where
    S: Stream<Item = ServerSentEvent> + Send + 'static,
{
    let processor = StreamProcessor::new();
    processor.process_stream(stream)
}
