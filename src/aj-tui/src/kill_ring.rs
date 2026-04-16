//! Emacs-style kill ring for cut-and-yank operations.
//!
//! A [`KillRing`] is a FIFO/LIFO hybrid: [`KillRing::push`] appends to
//! the ring, [`KillRing::peek`] returns the most recent entry without
//! mutating, and [`KillRing::rotate`] cycles the tail to the head so
//! `yank-pop` style UI can walk backward through history.
//!
//! Consecutive kills can accumulate into one entry. Backward-delete
//! kills prepend the new text; forward-delete kills append. Callers
//! that don't want accumulation pass `accumulate = false`.

/// A ring buffer for killed text. See the module docs for semantics.
#[derive(Debug, Default, Clone)]
pub struct KillRing {
    ring: Vec<String>,
}

impl KillRing {
    /// Create an empty ring.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push text onto the kill ring.
    ///
    /// Empty strings are ignored so callers don't have to guard the
    /// boundary case of "delete zero characters" themselves.
    ///
    /// When `accumulate` is true and the ring is non-empty, merge with
    /// the most recent entry: `prepend = true` (backward kill) places
    /// `text` before the entry's contents; `prepend = false` (forward
    /// kill) appends.
    pub fn push(&mut self, text: &str, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        if accumulate && !self.ring.is_empty() {
            let last = self.ring.last_mut().expect("non-empty by check above");
            if prepend {
                *last = format!("{}{}", text, last);
            } else {
                last.push_str(text);
            }
        } else {
            self.ring.push(text.to_string());
        }
    }

    /// Look at the most recent entry without modifying the ring.
    pub fn peek(&self) -> Option<&str> {
        self.ring.last().map(String::as_str)
    }

    /// Move the last entry to the front. Used to cycle `yank-pop` style.
    pub fn rotate(&mut self) {
        if self.ring.len() > 1 {
            let last = self.ring.pop().expect("len > 1");
            self.ring.insert(0, last);
        }
    }

    /// Number of entries in the ring.
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// Whether the ring has any entries.
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}
