//! A minimal SDK for the OpenAI API. Covers the Chat Completions,
//! Responses, and Codex Responses endpoints, with both blocking and
//! streaming variants.
//!
//! Unknown wire shapes are preserved via `Other(Value)` catch-alls on the
//! streaming-event and output-item enums so schema evolution doesn't break
//! the parser.
//!
//! The public surface tracks the OpenAI wire API, not only what AJ calls
//! today. AJ currently drives the three streaming endpoints and builds the
//! request structs by field literal, so the non-streaming
//! `Client::chat_completions`/`responses`, `Client::base_url`,
//! `Response::output_text`, and the request convenience constructors
//! (`CreateResponseRequest::new`, `CreateChatCompletionRequest::new`,
//! `ResponseInputItem::user_text`/`function_call_output`,
//! `ResponseTool::function`, `ChatCompletionUserContent::text`/`with_image`,
//! and the `ResponseInstructions` `From` impls) are public on purpose for
//! other consumers rather than leaked.

pub mod client;
pub mod types;
