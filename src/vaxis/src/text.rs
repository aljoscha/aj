//! Reusable, widget-free text-editing primitives.
//!
//! These building blocks carry no rendering and no input handling. They depend
//! on nothing in [`vxfw`](crate::vxfw) and are usable on their own. The crate's
//! text widgets build on them so the editing logic has one home and one set of
//! tests.
//!
//! - [`KillRing`] is an emacs-style kill ring for cut-and-yank.
//! - [`UndoStack`] is a generic last-in-first-out stack of state snapshots.
//! - The word-motion engine ([`word_left`], [`word_right`] and the two-phase
//!   [`skip_separators`] / [`skip_class`] helpers) moves and deletes by word
//!   under a pluggable [`WordClassifier`]. Two built-in classifiers,
//!   [`ReadlineWords`] and [`EmacsWords`], give the two common word feels.

pub mod kill_ring;
pub mod undo_stack;
pub mod word_motion;

pub use crate::text::kill_ring::KillRing;
pub use crate::text::undo_stack::UndoStack;
pub use crate::text::word_motion::{
    CharClass, EmacsWords, ReadlineWords, WordClassifier, is_punctuation_grapheme,
    is_whitespace_grapheme, skip_class, skip_separators, word_left, word_right,
};
