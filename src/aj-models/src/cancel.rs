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
