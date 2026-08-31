//! Cancellation helper shared by the streaming providers.
//!
//! A small `tokio::select!` wrapper that races an arbitrary future against
//! a [`CancellationToken`](tokio_util::sync::CancellationToken). It has no
//! tie to any particular message type, so it lives apart from the event
//! protocol it happens to be used with.

/// Outcome of [`select_cancel`].
pub(crate) enum SelectOutcome<T> {
    /// The future completed with `T` before the cancellation token fired.
    Ready(T),
    /// The cancellation token fired before the future completed. The
    /// future has been dropped.
    Cancelled,
}

/// Await `fut` concurrently with `token.cancelled()`. When `token` is
/// `None` this just awaits `fut` (the cancellation path is unreachable),
/// matching the "no cancel installed" case providers see when the
/// caller doesn't set [`StreamOptions::cancel`](crate::types::StreamOptions).
///
/// Used by every provider's `run_stream_inner` to drive the streaming
/// HTTP request inside a `select!` against the per-call cancellation
/// token so a `cancel()` rapidly tears down both the HTTP connection
/// (via dropping the SSE handle) and the polling task.
pub(crate) async fn select_cancel<T, F>(
    token: Option<&tokio_util::sync::CancellationToken>,
    fut: F,
) -> SelectOutcome<T>
where
    F: std::future::Future<Output = T>,
{
    let Some(token) = token else {
        return SelectOutcome::Ready(fut.await);
    };
    tokio::pin!(fut);
    tokio::select! {
        biased;
        _ = token.cancelled() => SelectOutcome::Cancelled,
        value = &mut fut => SelectOutcome::Ready(value),
    }
}

/// Outcome of [`select_request`].
pub(crate) enum RequestSelectOutcome<T> {
    /// The request future completed with `T` before cancellation.
    Ready(T),
    /// Cancellation won before the request future received its first poll.
    CancelledBeforePoll,
    /// Cancellation won after the request future had been polled.
    CancelledAfterPoll,
}

/// Await an HTTP request future while observing its issuance boundary.
///
/// Cancellation has priority whenever both branches are ready. A cancellation
/// before the first `fut` poll cannot have issued the request. Once `fut` has
/// been polled, it may have reached upstream, so cancellation must retain
/// partial accounting even if the HTTP handshake did not complete.
pub(crate) async fn select_request<T, F>(
    token: Option<&tokio_util::sync::CancellationToken>,
    fut: F,
) -> RequestSelectOutcome<T>
where
    F: std::future::Future<Output = T>,
{
    let Some(token) = token else {
        return RequestSelectOutcome::Ready(fut.await);
    };
    tokio::pin!(fut);
    let mut polled = false;
    let result = {
        let tracked = std::future::poll_fn(|cx| {
            polled = true;
            fut.as_mut().poll(cx)
        });
        tokio::pin!(tracked);
        tokio::select! {
            biased;
            _ = token.cancelled() => None,
            value = &mut tracked => Some(value),
        }
    };
    match result {
        Some(value) => RequestSelectOutcome::Ready(value),
        None => {
            if polled {
                RequestSelectOutcome::CancelledAfterPoll
            } else {
                RequestSelectOutcome::CancelledBeforePoll
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;

    use tokio_util::sync::CancellationToken;

    use super::*;

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn an_already_cancelled_request_is_not_polled() {
        let token = CancellationToken::new();
        token.cancel();
        let polled = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&polled);
        let drop_probe = DropProbe(Arc::clone(&dropped));
        let request = async move {
            let _drop_probe = drop_probe;
            std::future::poll_fn(move |_| {
                observed.store(true, Ordering::SeqCst);
                Poll::<()>::Pending
            })
            .await
        };

        assert!(matches!(
            select_request(Some(&token), request).await,
            RequestSelectOutcome::CancelledBeforePoll
        ));
        assert!(!polled.load(Ordering::SeqCst));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancellation_after_the_request_poll_is_reported_as_issued() {
        let token = CancellationToken::new();
        let cancel = token.clone();
        let polled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&polled);
        let request = std::future::poll_fn(move |_| {
            observed.store(true, Ordering::SeqCst);
            cancel.cancel();
            Poll::<()>::Pending
        });

        assert!(matches!(
            select_request(Some(&token), request).await,
            RequestSelectOutcome::CancelledAfterPoll
        ));
        assert!(polled.load(Ordering::SeqCst));
    }
}
