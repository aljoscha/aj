//! Generic undo stack with clone-on-push semantics.
//!
//! Stores owned clones of state snapshots. Popped snapshots are returned
//! directly (no re-cloning) since they are already owned by the stack.
//!
//! Used by both the [`Input`](crate::components::text_input::Input) and
//! [`Editor`](crate::components::editor::Editor) components to track
//! pre-mutation snapshots for the Ctrl+- undo key. The snapshot shape
//! (`S`) is whatever state the component needs to restore — a
//! `(String, usize)` for the single-line input, a richer struct for the
//! multi-line editor — and the stack is agnostic.

/// A last-in-first-out stack of undo snapshots.
///
/// `push` stores a clone of the caller's state so subsequent mutations
/// don't invalidate the snapshot. `pop` returns the snapshot directly
/// (without re-cloning) because the stack no longer retains it.
#[derive(Debug)]
pub struct UndoStack<S> {
    stack: Vec<S>,
}

impl<S> UndoStack<S> {
    /// Create an empty stack.
    pub fn new() -> Self {
        Self { stack: Vec::new() }
    }

    /// Push a snapshot onto the stack, taking ownership. Callers that
    /// still need access to the underlying state should clone before
    /// calling.
    pub fn push(&mut self, state: S) {
        self.stack.push(state);
    }

    /// Pop and return the most recent snapshot, or `None` if the stack
    /// is empty.
    pub fn pop(&mut self) -> Option<S> {
        self.stack.pop()
    }

    /// Remove every snapshot. The stack is empty afterwards.
    pub fn clear(&mut self) {
        self.stack.clear();
    }

    /// Number of snapshots currently on the stack.
    pub fn len(&self) -> usize {
        self.stack.len()
    }

    /// Whether the stack has no snapshots.
    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

impl<S> Default for UndoStack<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_stack_is_empty() {
        let stack: UndoStack<String> = UndoStack::new();
        assert!(stack.is_empty());
        assert_eq!(stack.len(), 0);
    }

    #[test]
    fn push_stores_the_value_as_passed() {
        let mut stack: UndoStack<String> = UndoStack::new();
        let state = String::from("before");
        stack.push(state.clone());

        assert_eq!(stack.pop().as_deref(), Some("before"));
    }

    #[test]
    fn pop_returns_entries_in_lifo_order() {
        let mut stack: UndoStack<i32> = UndoStack::new();
        stack.push(1);
        stack.push(2);
        stack.push(3);
        assert_eq!(stack.pop(), Some(3));
        assert_eq!(stack.pop(), Some(2));
        assert_eq!(stack.pop(), Some(1));
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn clear_empties_the_stack() {
        let mut stack: UndoStack<i32> = UndoStack::new();
        stack.push(10);
        stack.push(20);
        assert_eq!(stack.len(), 2);
        stack.clear();
        assert!(stack.is_empty());
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn works_with_tuple_snapshot_shapes() {
        // Both Input and Editor use tuple- or struct-shaped S; verify
        // a (String, usize) compiles and round-trips.
        let mut stack: UndoStack<(String, usize)> = UndoStack::new();
        stack.push(("hello".to_string(), 5));
        stack.push(("hello world".to_string(), 11));
        assert_eq!(stack.pop(), Some(("hello world".to_string(), 11)));
        assert_eq!(stack.pop(), Some(("hello".to_string(), 5)));
    }
}
