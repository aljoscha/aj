//! Shared `openai-sdk` error classification for the three OpenAI
//! adapters (Chat Completions, Responses, Codex Responses).
//!
//! `crate::errors` deliberately speaks in decomposed primitives
//! (`Option<&str>`, `u16`, `String`) so it stays free of any SDK type.
//! Turning an `openai_sdk::client::ClientError` into those primitives
//! is the adapter layer's job, so it lives here, next to the three
//! providers that share it.

use openai_sdk::client::ClientError;

use crate::errors::{classify_openai_error, parse_retry_after, transport_error};
use crate::types::AssistantError;

/// Classify an `openai-sdk` [`ClientError`] into the unified
/// [`AssistantError`] shape per `docs/models-spec.md` §10.3.
pub(super) fn classify_client_error(err: &ClientError) -> AssistantError {
    classify_client_error_with(err, |_code, _type, _status, message| message.to_string())
}

/// Like [`classify_client_error`], but lets the caller rewrite the
/// human-facing message of a typed `ApiError` before classification.
///
/// `message_for_api_error` receives `(code, type, http_status,
/// server_message)` and returns the message to classify. It affects
/// only the message, never the category. The Codex adapter uses this
/// to overlay its friendly usage-limit text on a 429 without
/// re-spelling the transport/parse/internal arms.
pub(super) fn classify_client_error_with(
    err: &ClientError,
    message_for_api_error: impl FnOnce(Option<&str>, Option<&str>, u16, &str) -> String,
) -> AssistantError {
    match err {
        ClientError::ApiError {
            error,
            http_status,
            retry_after,
        } => {
            let message = message_for_api_error(
                error.code.as_deref(),
                error.r#type.as_deref(),
                *http_status,
                &error.message,
            );
            classify_openai_error(
                error.code.as_deref(),
                error.r#type.as_deref(),
                Some(*http_status),
                parse_retry_after(retry_after.as_deref()),
                message,
            )
        }
        ClientError::TransportError(t) => transport_error(format!("transport: {t}")),
        ClientError::ParseError(s) => transport_error(format!("parse: {s}")),
        ClientError::InternalError(s) => transport_error(format!("internal: {s}")),
    }
}
