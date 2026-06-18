//! Shared outcome slot for selector overlays.
//!
//! A selector overlay component reports its terminal result (a confirmed
//! selection, a cancellation, a close) by writing into a slot the host
//! holds a clone of. After each input event the host calls
//! [`OutcomeSlot::take`]; a `Some(_)` means "close the overlay and act on
//! this". This is the seam between an overlay's `on_select` / `on_cancel`
//! callbacks (which can't borrow the host) and the host's run loop.

use std::sync::{Arc, Mutex};

/// A cheap-to-clone handle to a single overlay outcome of type `T`.
///
/// Clones share one slot, so a closure captured into an overlay and the
/// host's tracking handle observe the same value.
pub struct OutcomeSlot<T>(Arc<Mutex<Option<T>>>);

impl<T> OutcomeSlot<T> {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// Take the current outcome (if any), leaving the slot empty.
    pub fn take(&self) -> Option<T> {
        self.0.lock().expect("outcome slot poisoned").take()
    }

    /// Record an outcome, overwriting any previous unconsumed value.
    pub fn set(&self, value: T) {
        *self.0.lock().expect("outcome slot poisoned") = Some(value);
    }
}

impl<T> Clone for OutcomeSlot<T> {
    fn clone(&self) -> Self {
        // Manual impl so `T` need not be `Clone`: we only clone the `Arc`.
        Self(Arc::clone(&self.0))
    }
}

impl<T> Default for OutcomeSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_slot() {
        let a = OutcomeSlot::<u32>::new();
        let b = a.clone();
        b.set(7);
        assert_eq!(a.take(), Some(7));
        // Taken once, the slot is empty for the other handle too.
        assert_eq!(b.take(), None);
    }
}
