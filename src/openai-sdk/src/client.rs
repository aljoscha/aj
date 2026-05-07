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

pub struct Client {
    client: ReqwestClient,
    api_key: String,
    base_url: String,
}

impl Client {
    pub fn new(base_url: Option<String>, api_key: String) -> Self {
        let base_url = base_url.unwrap_or_else(|| OPENAI_API_BASE_URL.to_string());

        Self {
            client: ReqwestClient::new(),
            api_key,
            base_url,
        }
    }

    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    pub async fn chat_completions(
        &self,
        request: CreateChatCompletionRequest,
    ) -> Result<CreateChatCompletionResponse, anyhow::Error> {
        let request_builder = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request);

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

        let request_builder = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request);

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

        let error_text = response.text().await?;
        if status.is_client_error() || status.is_server_error() {
            match serde_json::from_str::<ApiErrorResponse>(&error_text) {
                Ok(error_response) => Err(error_response.error.into()),
                Err(_) => Err(ApiError {
                    message: format!("request failed ({status}): {error_text}"),
                    r#type: None,
                    param: None,
                    code: None,
                }
                .into()),
            }
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
        let request_builder = self
            .client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request);

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
        mut request: CreateResponseRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ResponseStreamEvent, ClientError>> + Send>>,
        ClientError,
    > {
        request.stream = Some(true);

        let request_builder = self
            .client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request);

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

        let error_text = response.text().await?;
        if status.is_client_error() || status.is_server_error() {
            match serde_json::from_str::<ApiErrorResponse>(&error_text) {
                Ok(error_response) => Err(error_response.error.into()),
                Err(_) => Err(ApiError {
                    message: format!("request failed ({status}): {error_text}"),
                    r#type: None,
                    param: None,
                    code: None,
                }
                .into()),
            }
        } else {
            Err(ClientError::InternalError(format!(
                "unexpected status code ({status}): {error_text}"
            )))
        }
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
