use core::panic;

use anyhow::{Context, Result, anyhow};
use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};
use reqwest;
use reqwest::{Client as ReqwestClient, StatusCode};

use crate::messages::{ApiErrorResponse, Message, Messages, StreamingEvent};

pub struct Client {
    client: ReqwestClient,
    api_key: String,
    version: String,
}

impl Client {
    pub fn new(api_key: String) -> Self {
        Self {
            client: ReqwestClient::new(),
            api_key,
            version: "2023-06-01".to_string(),
        }
    }
}

impl Client {
    pub async fn messages(&self, messages: Messages) -> Result<Message, anyhow::Error> {
        let request_builder = self
            .client
            .post("https://api.anthropic.com/v1/messages")
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
            StatusCode::BAD_REQUEST => {
                let error_text = response.text().await?;
                match serde_json::from_str::<ApiErrorResponse>(&error_text) {
                    Ok(error_response) => Err(anyhow!(
                        "API Error: {} - {}",
                        error_response.error.r#type,
                        error_response.error.message
                    )),
                    Err(_) => Err(anyhow!("bad request: {}", error_text)),
                }
            }
            StatusCode::UNAUTHORIZED => Err(anyhow!("unauthorized")),
            _ => {
                let error_message = format!("unexpected status code: {:?}", response.text().await?);
                Err(anyhow!(error_message))
            }
        }
    }

    pub async fn messages_stream(
        &self,
        mut messages: Messages,
    ) -> Result<impl Stream<Item = StreamingEvent>, anyhow::Error> {
        messages.stream = Some(true);

        let request_builder = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", self.api_key.clone())
            .header("anthropic-version", self.version.clone())
            .header("content-type", "application/json")
            .json(&messages);

        let response = request_builder
            .send()
            .await
            .context("failed to send request")?;

        match response.status() {
            StatusCode::OK => {
                let stream = response.bytes_stream().eventsource();

                let stream = stream.map(|event| match event {
                    Ok(event) => match serde_json::from_str::<StreamingEvent>(&event.data) {
                        Ok(json_event) => json_event,
                        Err(err) => {
                            panic!("could not parse event {}: {}", event.data, err);
                        }
                    },
                    Err(e) => {
                        panic!("event-stream error: {e}");
                    }
                });
                Ok(stream)
            }
            StatusCode::BAD_REQUEST => {
                let error_text = response.text().await?;
                match serde_json::from_str::<ApiErrorResponse>(&error_text) {
                    Ok(error_response) => Err(anyhow!(
                        "API Error: {} - {}",
                        error_response.error.r#type,
                        error_response.error.message
                    )),
                    Err(_) => Err(anyhow!("bad request: {}", error_text)),
                }
            }
            StatusCode::UNAUTHORIZED => Err(anyhow!("unauthorized")),
            _ => {
                let error_message = format!("unexpected status code: {:?}", response.text().await?);
                Err(anyhow!(error_message))
            }
        }
    }
}
