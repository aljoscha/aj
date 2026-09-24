use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use aj_models::provider::stream;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::types::{ApiKeyResolver, Context, StopReason, StreamOptions};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn cancellation_during_credentials_aborts_and_releases_the_resolver() {
    for api in [
        "anthropic-messages",
        "openai-completions",
        "openai-responses",
        "openai-codex-responses",
    ] {
        let model = ModelInfo {
            id: "fixture".into(),
            name: "fixture".into(),
            family: None,
            api: api.into(),
            provider: "fixture".into(),
            base_url: "http://127.0.0.1:1".into(),
            reasoning: false,
            reasoning_options: vec![],
            supports_verbosity: false,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 4096,
            max_tokens: 1024,
        };
        let cancel = CancellationToken::new();
        let pending = Arc::new(Notify::new());
        let resource = Arc::new(Mutex::new(()));
        let options = StreamOptions {
            cancel: Some(cancel.clone()),
            api_key_resolver: Some(ApiKeyResolver::new({
                let pending = Arc::clone(&pending);
                let resource = Arc::clone(&resource);
                move || {
                    let pending = Arc::clone(&pending);
                    let resource = Arc::clone(&resource);
                    async move {
                        let _guard = resource.lock().await;
                        // Signal from the poll that suspends resolution, not merely
                        // from construction of the resolver future.
                        std::future::poll_fn(|_| {
                            pending.notify_one();
                            Poll::Pending
                        })
                        .await
                    }
                }
            })),
            ..StreamOptions::default()
        };
        let stream = stream(&model, &Context::new(""), &options);
        tokio::time::timeout(Duration::from_secs(5), pending.notified())
            .await
            .unwrap_or_else(|_| panic!("{api}: resolver was not polled"));
        assert!(
            resource.try_lock().is_err(),
            "{api}: resolver holds resource"
        );

        cancel.cancel();
        let terminal = tokio::time::timeout(Duration::from_secs(5), stream.result())
            .await
            .unwrap_or_else(|_| panic!("{api}: cancellation waited for credentials"));
        assert_eq!(terminal.stop_reason, StopReason::Aborted, "{api}");
        assert_eq!(terminal.account, None, "{api}");
        assert!(terminal.content.is_empty(), "{api}");
        assert!(
            resource.try_lock().is_ok(),
            "{api}: resolver retained resource"
        );
    }
}
