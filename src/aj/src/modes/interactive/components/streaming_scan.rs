//! Background scan that streams batches into a live selector.
//!
//! Selector overlays whose rows come from disk (the session list, prompt
//! history) open immediately and fill in as a scan walks the on-disk logs.
//! `StreamingScan<T>` owns that machinery: it drives the scan off the TUI
//! event loop, buffers the batches it emits, and lets the component drain
//! them at render time.
//!
//! The scan runs on a blocking task when a Tokio runtime is present and
//! inline otherwise (unit tests), so results are delivered synchronously
//! when there's no runtime to offload to.

use aj_tui::tui::RenderHandle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// One message from the scan thread to the UI thread.
enum ScanMessage<T> {
    /// A batch of items, delivered in emission order.
    Batch(Vec<T>),
    /// The scan finished; flips `loading` false once drained.
    Done,
}

/// A running background scan whose batches the owner drains on the UI
/// thread. Generic over the streamed item type.
pub struct StreamingScan<T> {
    rx: UnboundedReceiver<ScanMessage<T>>,
    loading: bool,
}

impl<T: Send + 'static> StreamingScan<T> {
    /// Spawn `scan`, forwarding each batch it emits to the buffer and
    /// waking the TUI via `render_handle`. A `Done` marker follows the
    /// last batch so [`Self::is_loading`] flips false once the owner
    /// drains it.
    ///
    /// Empty batches are dropped (no render wake), matching the contract
    /// the scanners rely on. Inside a Tokio runtime the scan runs on a
    /// blocking task; outside one it runs inline so a non-async caller
    /// (tests) gets its results synchronously before this returns.
    pub fn spawn(
        scan: impl FnOnce(&mut dyn FnMut(Vec<T>)) + Send + 'static,
        render_handle: RenderHandle,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_scan(scan, tx, render_handle);
        Self { rx, loading: true }
    }

    /// Drain every batch delivered since the last call, coalesced into one
    /// vector so a burst of batches in a single frame costs one append.
    /// Seeing the `Done` marker flips `loading` false.
    pub fn drain(&mut self) -> Vec<T> {
        let mut items = Vec::new();
        while let Ok(message) = self.rx.try_recv() {
            match message {
                ScanMessage::Batch(batch) => items.extend(batch),
                ScanMessage::Done => self.loading = false,
            }
        }
        items
    }

    /// Whether the scan is still running (its `Done` marker hasn't been
    /// drained yet).
    pub fn is_loading(&self) -> bool {
        self.loading
    }
}

/// Run `scan` on a blocking thread (or inline outside a runtime),
/// forwarding batches over `tx` and waking the TUI after each.
fn spawn_scan<T: Send + 'static>(
    scan: impl FnOnce(&mut dyn FnMut(Vec<T>)) + Send + 'static,
    tx: UnboundedSender<ScanMessage<T>>,
    render_handle: RenderHandle,
) {
    let run = move || {
        let mut emit = |batch: Vec<T>| {
            if batch.is_empty() {
                return;
            }
            let _ = tx.send(ScanMessage::Batch(batch));
            render_handle.request_render();
        };
        scan(&mut emit);
        let _ = tx.send(ScanMessage::Done);
        render_handle.request_render();
    };
    match tokio::runtime::Handle::try_current() {
        Ok(_) => {
            tokio::task::spawn_blocking(run);
        }
        Err(_) => run(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_batches_in_order_and_clears_loading_on_done() {
        // No runtime here, so the scan runs inline and everything is in
        // the channel before `spawn` returns.
        let mut scan: StreamingScan<i32> = StreamingScan::spawn(
            |emit| {
                emit(vec![1, 2]);
                emit(vec![]); // dropped: empty batches don't wake/queue
                emit(vec![3]);
            },
            RenderHandle::detached(),
        );
        assert!(scan.is_loading());
        assert_eq!(scan.drain(), vec![1, 2, 3]);
        // The `Done` marker was drained alongside the batches.
        assert!(!scan.is_loading());
        assert!(scan.drain().is_empty());
    }
}
