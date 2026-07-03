//! Emacs-style kill ring for cut-and-yank operations.
//!
//! A [`KillRing`] is a FIFO/LIFO hybrid: [`KillRing::push`] appends to the
//! ring, [`KillRing::peek`] returns the most recent entry without mutating, and
//! [`KillRing::rotate`] cycles the tail to the head so a `yank-pop` style UI can
//! walk backward through history.
//!
//! Consecutive kills can accumulate into one entry. Backward-delete kills
//! prepend the new text, forward-delete kills append. Callers that don't want
//! accumulation pass `accumulate = false`.

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
    /// Empty strings are ignored so callers don't have to guard the boundary
    /// case of "delete zero characters" themselves.
    ///
    /// When `accumulate` is true and the ring is non-empty, merge with the most
    /// recent entry: `prepend = true` (backward kill) places `text` before the
    /// entry's contents, `prepend = false` (forward kill) appends.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ring_is_empty() {
        let ring = KillRing::new();
        assert!(ring.is_empty());
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.peek(), None);
    }

    #[test]
    fn push_ignores_empty_text() {
        let mut ring = KillRing::new();
        ring.push("", false, false);
        assert!(ring.is_empty());
    }

    #[test]
    fn push_without_accumulate_adds_distinct_entries() {
        let mut ring = KillRing::new();
        ring.push("a", false, false);
        ring.push("b", false, false);
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.peek(), Some("b"));
    }

    #[test]
    fn accumulate_appends_on_forward_kill() {
        let mut ring = KillRing::new();
        ring.push("foo", false, false);
        ring.push("bar", false, true);
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.peek(), Some("foobar"));
    }

    #[test]
    fn accumulate_prepends_on_backward_kill() {
        let mut ring = KillRing::new();
        ring.push("bar", false, false);
        ring.push("foo", true, true);
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.peek(), Some("foobar"));
    }

    #[test]
    fn accumulate_on_empty_ring_pushes_a_new_entry() {
        let mut ring = KillRing::new();
        ring.push("foo", true, true);
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.peek(), Some("foo"));
    }

    #[test]
    fn rotate_cycles_the_tail_to_the_head() {
        let mut ring = KillRing::new();
        ring.push("a", false, false);
        ring.push("b", false, false);
        ring.push("c", false, false);
        // Most recent is "c". Rotating brings the older tail forward.
        assert_eq!(ring.peek(), Some("c"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("b"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("a"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("c"));
    }

    #[test]
    fn rotate_is_a_noop_with_fewer_than_two_entries() {
        let mut ring = KillRing::new();
        ring.rotate();
        assert!(ring.is_empty());
        ring.push("only", false, false);
        ring.rotate();
        assert_eq!(ring.peek(), Some("only"));
    }
}
