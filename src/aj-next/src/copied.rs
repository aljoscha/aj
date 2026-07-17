//! The select-to-copy record the transcript shares with the host.
//!
//! The transcript is the single writer: a mouse select-to-copy landing
//! overwrites the record. The drive loop edge-detects fresh records (by
//! their timestamp) and folds each into the unified toast stack
//! ([`crate::toasts`]), which owns the display and expiry from there.

use std::time::Instant;

/// A record of the last select-to-copy: how many characters were copied, and
/// when. The `at` timestamp doubles as the edge the drive loop detects fresh
/// records by.
#[derive(Clone, Copy)]
pub(crate) struct Copied {
    pub(crate) chars: usize,
    pub(crate) at: Instant,
}
