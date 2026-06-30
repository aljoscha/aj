//! Per-frame grapheme byte storage. See the D1 grapheme-ownership decision in
//! the port plan.
//!
//! NOTE: Under the D1 Option A model cells own their grapheme bytes inline, so
//! this cache is vestigial. We keep it for API parity with upstream and as a
//! building block we might reuse. It is a fixed 8 KiB ring with a footgun:
//! `put` overwrites from the start of the buffer once an entry would not fit in
//! the remaining space, so a slice returned by an earlier `put` can be
//! clobbered by a later one after the buffer wraps. Upstream relies on a single
//! frame never needing more than 8 KiB of distinct grapheme bytes.

const BUF_LEN: usize = 1024 * 8;

/// An 8 KiB ring buffer that hands out byte slices for grapheme clusters.
pub struct GraphemeCache {
    buf: [u8; BUF_LEN],
    /// Start index of the next grapheme.
    idx: usize,
}

impl Default for GraphemeCache {
    fn default() -> Self {
        Self {
            buf: [0; BUF_LEN],
            idx: 0,
        }
    }
}

impl GraphemeCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Copies `bytes` into the ring and returns the stored slice.
    ///
    /// Resets to the start of the buffer when `bytes` would not fit in the
    /// remaining space, overwriting earlier entries (see the module NOTE).
    pub fn put(&mut self, bytes: &[u8]) -> &[u8] {
        if self.idx + bytes.len() > self.buf.len() {
            self.idx = 0;
        }
        let start = self.idx;
        let end = start + bytes.len();
        self.buf[start..end].copy_from_slice(bytes);
        self.idx = end;
        &self.buf[start..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_round_trip() {
        let mut cache = GraphemeCache::new();
        assert_eq!(cache.put(b"hello"), b"hello");
        assert_eq!(cache.put(b"world"), b"world");
        assert_eq!(cache.idx, 10);
    }

    #[test]
    fn put_wraps_to_start_on_overflow() {
        let mut cache = GraphemeCache::new();
        let big = vec![b'x'; BUF_LEN - 2];
        let _ = cache.put(&big);
        assert_eq!(cache.idx, BUF_LEN - 2);

        // The next put does not fit in the remaining 2 bytes, so idx resets to
        // 0 and the entry lands at the start of the buffer.
        assert_eq!(cache.put(b"abc"), b"abc");
        assert_eq!(cache.idx, 3);
    }
}
