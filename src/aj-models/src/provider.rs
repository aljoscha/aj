//! Provider trait and top-level dispatch functions.
//!
//! Defines the [`Provider`] trait that each model API implementation conforms
//! to (`anthropic-messages`, `openai-completions`, `openai-responses`) and the
//! top-level [`stream`], [`stream_simple`], [`complete`], and
//! [`complete_simple`] convenience functions that look up the appropriate
//! provider based on [`ModelInfo::api`] and forward the call.
//!
//! Providers are intentionally stateless at this layer: per-call auth and
//! HTTP knobs flow in through [`StreamOptions`]. That keeps the dispatch
//! functions free to construct (or pool) provider instances however they
//! see fit without leaking lifecycle concerns to callers.
//!
//! See `docs/models-spec.md` §5 for the full design.

use futures::StreamExt;

use crate::registry::ModelInfo;
use crate::streaming::{AssistantMessageEvent, AssistantMessageEventStream, ErrorReason};
use crate::types::{
    AssistantError, AssistantMessage, Context, ErrorCategory, SimpleStreamOptions, StopReason,
    StreamOptions,
};

/// A provider knows how to stream inference for a specific API type.
///
/// Each variant of [`ModelInfo::api`] (e.g. `"anthropic-messages"`,
/// `"openai-completions"`, `"openai-responses"`,
/// `"openai-codex-responses"`) has exactly one [`Provider`]
/// implementation. Providers are responsible for translating
/// the unified [`Context`] / [`StreamOptions`] into the wire format their
/// SDK expects, driving the streaming HTTP request, and emitting
/// [`AssistantMessageEvent`]s onto the returned
/// [`AssistantMessageEventStream`].
pub trait Provider: Send + Sync {
    /// Low-level stream with provider-specific options already resolved.
    ///
    /// The returned [`AssistantMessageEventStream`] is live: events flow as
    /// the underlying HTTP response streams, terminating with exactly one
    /// of [`AssistantMessageEvent::Done`] or [`AssistantMessageEvent::Error`].
    /// Dropping the stream cancels the in-flight request.
    fn stream(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &StreamOptions,
    ) -> AssistantMessageEventStream;

    /// High-level stream that maps
    /// [`ThinkingLevel`](crate::types::ThinkingLevel) to provider-specific
    /// reasoning configuration before delegating to [`Self::stream`].
    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream;
}

/// Look up the provider implementation for a given API string.
///
/// Returns [`None`] when no provider has been registered for `api`.
/// Concrete providers (Anthropic, OpenAI Chat Completions, OpenAI
/// Responses, OpenAI Codex Responses) plug in here as they land in §6
/// and §7; until each one arrives the top-level dispatch functions
/// surface the missing provider as an [`AssistantMessageEvent::Error`]
/// on the resulting stream so callers always observe a uniform stream
/// shape.
///
/// Exposed `pub` so the binary can build a provider handle once at
/// startup and pass it to the agent through [`Provider`]-aware
/// constructors, rather than re-dispatching the API string on every
/// inference call.
pub fn provider_for(api: &str) -> Option<Box<dyn Provider>> {
    match api {
        "anthropic-messages" => Some(Box::new(crate::anthropic::AnthropicProvider)),
        "openai-completions" => Some(Box::new(crate::openai::OpenAiCompletionsProvider)),
        "openai-responses" => Some(Box::new(crate::openai::OpenAiResponsesProvider)),
        "openai-codex-responses" => Some(Box::new(crate::openai::OpenAiCodexResponsesProvider)),
        _ => None,
    }
}

/// Stream inference using the appropriate provider for the model.
///
/// Dispatches on [`ModelInfo::api`] (not [`ModelInfo::provider`]) so that
/// providers exposing multiple APIs (e.g. OpenAI's Chat Completions vs.
/// Responses) can be selected per-model.
pub fn stream(
    model: &ModelInfo,
    context: &Context,
    options: &StreamOptions,
) -> AssistantMessageEventStream {
    match provider_for(&model.api) {
        Some(provider) => provider.stream(model, context, options),
        None => unsupported_api_stream(model),
    }
}

/// Stream inference with simplified reasoning options.
///
/// Same dispatch rules as [`stream`]; only the options shape differs.
pub fn stream_simple(
    model: &ModelInfo,
    context: &Context,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    match provider_for(&model.api) {
        Some(provider) => provider.stream_simple(model, context, options),
        None => unsupported_api_stream(model),
    }
}

/// Non-streaming convenience: drive the stream to completion and return
/// the final [`AssistantMessage`].
///
/// Useful for batch / scripting contexts that don't care about
/// intermediate deltas. Equivalent to calling [`stream`] and awaiting
/// [`AssistantMessageEventStream::result`] after draining the stream.
pub async fn complete(
    model: &ModelInfo,
    context: &Context,
    options: &StreamOptions,
) -> AssistantMessage {
    drain(stream(model, context, options)).await
}

/// Non-streaming convenience for [`SimpleStreamOptions`].
pub async fn complete_simple(
    model: &ModelInfo,
    context: &Context,
    options: &SimpleStreamOptions,
) -> AssistantMessage {
    drain(stream_simple(model, context, options)).await
}

/// Drain a stream and return its final message.
///
/// We poll the stream to drive the producer (so events don't pile up
/// unbounded in the channel) and then await the side-channel
/// [`AssistantMessageEventStream::result`] for the canonical final
/// message. The stream's `result()` is populated from the same terminal
/// event that closes the channel, so this is well-defined regardless of
/// whether the producer pushes events synchronously or from a spawned
/// task.
async fn drain(mut stream: AssistantMessageEventStream) -> AssistantMessage {
    while stream.next().await.is_some() {}
    stream.result().await
}

/// Build an immediately-terminated stream carrying an
/// [`AssistantMessageEvent::Error`] event for an unrecognized API.
///
/// We populate the synthetic message's `api` / `provider` / `model`
/// fields from the requested [`ModelInfo`] so that callers logging the
/// error can still attribute it to the model they asked for.
fn unsupported_api_stream(model: &ModelInfo) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let mut error = AssistantMessage::empty();
    error.api = model.api.clone();
    error.provider = model.provider.clone();
    error.model = model.id.clone();
    error.stop_reason = StopReason::Error;
    error.error = Some(AssistantError::new(
        ErrorCategory::InvalidRequest,
        format!("no provider registered for api {:?}", model.api),
    ));
    stream.push(AssistantMessageEvent::Error {
        reason: ErrorReason::Error,
        error,
    });
    stream
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{InputModality, ModelCost, ModelInfo};
    use crate::types::{Context, SimpleStreamOptions, StreamOptions, ThinkingLevel};

    fn fake_model(api: &str) -> ModelInfo {
        ModelInfo {
            id: "fake-model-1".into(),
            name: "Fake".into(),
            api: api.into(),
            provider: "fake".into(),
            base_url: "https://example.invalid".into(),
            reasoning: false,
            supports_adaptive_thinking: false,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 1024,
            max_tokens: 256,
            headers: None,
        }
    }

    /// A trivial provider impl exists in the test module to make sure the
    /// trait shape compiles for downstream impls. It pushes a single
    /// `Done` event so we can verify the dispatch path end-to-end once a
    /// provider is wired in.
    struct EchoProvider;

    impl Provider for EchoProvider {
        fn stream(
            &self,
            model: &ModelInfo,
            _context: &Context,
            _options: &StreamOptions,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            let mut msg = AssistantMessage::empty();
            msg.api = model.api.clone();
            msg.provider = model.provider.clone();
            msg.model = model.id.clone();
            stream.push(AssistantMessageEvent::Done {
                reason: crate::streaming::DoneReason::Stop,
                message: msg,
            });
            stream
        }

        fn stream_simple(
            &self,
            model: &ModelInfo,
            context: &Context,
            options: &SimpleStreamOptions,
        ) -> AssistantMessageEventStream {
            self.stream(model, context, &options.base)
        }
    }

    #[tokio::test]
    async fn stream_with_unknown_api_emits_error() {
        let model = fake_model("definitely-not-real");
        let ctx = Context::new("you are a test");
        let result = complete(&model, &ctx, &StreamOptions::default()).await;
        assert_eq!(result.stop_reason, StopReason::Error);
        let err = result.error.expect("error populated");
        assert_eq!(err.category, ErrorCategory::InvalidRequest);
        assert!(
            err.message.contains("definitely-not-real"),
            "error message should mention the unknown api, got: {}",
            err.message
        );
        // Synthetic error inherits identity fields from the requested model.
        assert_eq!(result.api, "definitely-not-real");
        assert_eq!(result.provider, "fake");
        assert_eq!(result.model, "fake-model-1");
    }

    #[tokio::test]
    async fn complete_simple_with_unknown_api_emits_error() {
        let model = fake_model("nope");
        let ctx = Context::new("system");
        let opts = SimpleStreamOptions {
            base: StreamOptions::default(),
            reasoning: Some(ThinkingLevel::Low),
        };
        let result = complete_simple(&model, &ctx, &opts).await;
        assert_eq!(result.stop_reason, StopReason::Error);
    }

    #[tokio::test]
    async fn provider_trait_drives_dispatch_when_implemented() {
        // Sanity-check that a Provider impl satisfies the trait bounds and
        // produces a usable stream — guards against drift in the trait
        // signature once real providers land.
        let provider: Box<dyn Provider> = Box::new(EchoProvider);
        let model = fake_model("anthropic-messages");
        let ctx = Context::new("system");
        let mut s = provider.stream(&model, &ctx, &StreamOptions::default());
        let event = s.next().await.expect("at least one event");
        assert!(event.is_terminal());
        assert!(matches!(event, AssistantMessageEvent::Done { .. }));
    }

    /// `openai-codex-responses` is dispatched through `provider_for` to the
    /// Codex provider rather than falling through to `unsupported_api_stream`.
    ///
    /// We verify this without a network call by calling [`complete`] with no
    /// `api_key` set: the Codex provider's auth check fails fast with an
    /// `Auth`-category error, whereas the unknown-API path would surface an
    /// `InvalidRequest`-category error mentioning `"no provider registered"`.
    /// The category discriminator is enough to tell the two paths apart.
    #[tokio::test]
    async fn openai_codex_responses_api_is_dispatched_to_codex_provider() {
        let model = fake_model("openai-codex-responses");
        let ctx = Context::new("system");
        let result = complete(&model, &ctx, &StreamOptions::default()).await;
        assert_eq!(result.stop_reason, StopReason::Error);
        let err = result.error.expect("error populated");
        assert_eq!(
            err.category,
            ErrorCategory::Auth,
            "codex provider should surface Auth error (no api_key); \
             got {:?} with message {:?}",
            err.category,
            err.message,
        );
        // The synthetic error still carries the requested api / provider /
        // model identifiers so log lines attribute correctly.
        assert_eq!(result.api, "openai-codex-responses");
    }
}
