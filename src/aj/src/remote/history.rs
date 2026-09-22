//! Finite history reads share one snapshot cadence and response-owned lifetime.

use std::future::Future;
use std::time::Duration;

use aj_wire::{ErrorResponse, PromptHistory};
use axum::response::{IntoResponse, Response, Sse, sse::Event};
use futures::{Stream, StreamExt};
use tokio::sync::watch;

type Update = Result<(PromptHistory, bool), (PromptHistory, ErrorResponse)>;

/// Poll a read alongside its coalesced snapshots. Dropping the stream drops
/// the read future, including any upstream requests or blocking-scan guard.
/// Completion is explicit, even when the final history is empty or unchanged.
pub(crate) fn snapshots<F, Fut>(read: F) -> impl Stream<Item = Update> + Send
where
    F: FnOnce(watch::Sender<PromptHistory>) -> Fut,
    Fut: Future<Output = Result<PromptHistory, ErrorResponse>> + Send,
{
    let (tx, rx) = watch::channel(PromptHistory::default());
    let state = (Box::pin(read(tx)), rx, tokio::time::Instant::now());
    futures::stream::unfold(Some(state), |state| async move {
        let (mut read, mut rx, next_update) = state?;
        tokio::select! {
            biased;
            result = &mut read => Some((result
                .map(|history| (history, true))
                .map_err(|error| (rx.borrow_and_update().clone(), error)), None)),
            _ = async {
                tokio::time::sleep_until(next_update).await;
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            } => {
                let history = rx.borrow_and_update().clone();
                let next_update = tokio::time::Instant::now() + Duration::from_millis(100);
                Some((Ok((history, false)), Some((read, rx, next_update))))
            }
        }
    })
}

/// SSE uses the ordinary history and error payloads. An EOF without a terminal
/// event is an interrupted scan, not a successful empty or partial history.
pub(crate) fn response(snapshots: impl Stream<Item = Update> + Send + 'static) -> Response {
    Sse::new(snapshots.flat_map(|result| {
        let events = match result {
            Ok((history, complete)) => vec![
                Event::default()
                    .event(if complete { "complete" } else { "snapshot" })
                    .json_data(history),
            ],
            // A failure can win the poll before a coalesced snapshot is sent.
            // Deliver that last useful history before the terminal error.
            Err((history, error)) => vec![
                Event::default().event("snapshot").json_data(history),
                Event::default().event("error").json_data(error),
            ],
        };
        futures::stream::iter(
            events
                .into_iter()
                .map(|event| event.map_err(aj_agent::BoxError::from)),
        )
    }))
    .into_response()
}
