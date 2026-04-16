//! A simple vertical stacking container.
//!
//! Renders children sequentially, concatenating their output lines.

use crate::component::Component;
use crate::keys::InputEvent;

/// A container that stacks child components vertically.
///
/// `render()` concatenates all children's rendered lines into a single flat list.
/// The container itself has no visual representation (no padding, no borders).
///
/// `Container` does not propagate any [`crate::tui::RenderHandle`] to its
/// children. Components that need a handle (the editor's autocomplete
/// pump, the loader's animation pump) take one as a required
/// constructor argument — see
/// [`crate::components::editor::Editor::new`] and
/// [`crate::components::loader::Loader::new`]. This matches the
/// upstream framework's required-at-construction shape; before H6 the
/// Rust port instead had `Container` cache and forward a handle on
/// `add_child` / `insert_child`, but every component that needs a
/// handle now holds it from birth, so the forwarding pass is gone.
pub struct Container {
    children: Vec<Box<dyn Component>>,
}

impl Container {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    /// Add a child component to the end of the children list.
    pub fn add_child(&mut self, child: Box<dyn Component>) {
        self.children.push(child);
    }

    /// Insert a child component at the given index.
    pub fn insert_child(&mut self, index: usize, child: Box<dyn Component>) {
        self.children.insert(index, child);
    }

    /// Remove the first child whose identity matches `child` (data-pointer
    /// equality on the underlying component). Mirrors pi-tui's
    /// `Container.removeChild(component: Component)`
    /// (`packages/tui/src/tui.ts:185`), which uses `indexOf` on the
    /// children array.
    ///
    /// Identity is by [`std::ptr::addr_eq`] on the underlying
    /// `&dyn Component`, so the caller's reference must point to the
    /// same in-memory instance that was added via [`Container::add_child`]
    /// or [`Container::insert_child`]. Two distinct instances of the
    /// same type with byte-identical fields compare unequal.
    ///
    /// Returns the removed component, or `None` if `child` is not
    /// currently in the container. Pi-tui's `indexOf` returns `-1` for
    /// "not present" and `splice(-1, 1)` is a no-op; the Rust port
    /// returns `None` for the same case so a missing child doesn't
    /// tear an unrelated item out of the list.
    ///
    /// The Rust port returns the boxed component (rather than pi's
    /// `void`) so callers can re-attach it elsewhere. Callers that
    /// want pi's discard behavior write
    /// `let _ = c.remove_child_by_ref(child);`.
    pub fn remove_child_by_ref(&mut self, child: &dyn Component) -> Option<Box<dyn Component>> {
        let target: *const dyn Component = child;
        let index = self.children.iter().position(|c| {
            let ptr: *const dyn Component = c.as_ref();
            std::ptr::addr_eq(ptr, target)
        })?;
        Some(self.children.remove(index))
    }

    /// Remove all children.
    pub fn clear(&mut self) {
        self.children.clear();
    }

    /// Returns the number of children.
    pub fn len(&self) -> usize {
        self.children.len()
    }

    /// Returns true if there are no children.
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// Get a reference to a child by index.
    pub fn get(&self, index: usize) -> Option<&dyn Component> {
        self.children.get(index).map(|c| &**c)
    }

    /// Get a mutable reference to a child by index.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut Box<dyn Component>> {
        self.children.get_mut(index)
    }

    /// Get a reference to a child downcast to a concrete type, or `None`
    /// if the index is out of bounds or the child is not of type `T`.
    ///
    /// Sugar over `get(i)` + `as_any().downcast_ref::<T>()` that's
    /// useful when application code owns the layout (it added a
    /// specific widget at a known index) and needs to read that
    /// widget's concrete surface.
    pub fn get_as<T: Component>(&self, index: usize) -> Option<&T> {
        self.get(index).and_then(|c| c.as_any().downcast_ref::<T>())
    }

    /// Get a mutable reference to a child downcast to a concrete type,
    /// or `None` if the index is out of bounds or the child is not of
    /// type `T`.
    ///
    /// Sugar over `get_mut(i)` + `as_any_mut().downcast_mut::<T>()`.
    /// See [`Container::get_as`] for the idiomatic use case.
    pub fn get_mut_as<T: Component>(&mut self, index: usize) -> Option<&mut T> {
        self.get_mut(index)
            .and_then(|c| c.as_any_mut().downcast_mut::<T>())
    }

    /// Index of the last child, or `None` if the container is empty.
    /// Convenience for the common "focus the editor at the bottom of the
    /// stack" and "read the last appended child" patterns — more robust
    /// than `len().saturating_sub(1)` because it returns `None` on an
    /// empty container rather than a misleading `0`.
    pub fn last_index(&self) -> Option<usize> {
        if self.children.is_empty() {
            None
        } else {
            Some(self.children.len() - 1)
        }
    }
}

impl Default for Container {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Container {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for child in &mut self.children {
            lines.extend(child.render(width));
        }
        lines
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::impl_component_any;

    /// Tiny fixture so the unit tests don't need to pull in the test
    /// support layer.
    struct Lines(Vec<String>);

    impl Component for Lines {
        impl_component_any!();

        fn render(&mut self, _width: usize) -> Vec<String> {
            self.0.clone()
        }
    }

    struct Empty;

    impl Component for Empty {
        impl_component_any!();

        fn render(&mut self, _width: usize) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn get_as_returns_concrete_ref_when_types_match() {
        let mut c = Container::new();
        c.add_child(Box::new(Lines(vec!["hello".to_string()])));
        let concrete = c.get_as::<Lines>(0).expect("Lines at index 0");
        assert_eq!(concrete.0, vec!["hello".to_string()]);
    }

    #[test]
    fn get_as_returns_none_for_wrong_type() {
        let mut c = Container::new();
        c.add_child(Box::new(Empty));
        assert!(c.get_as::<Lines>(0).is_none());
    }

    #[test]
    fn get_as_returns_none_for_out_of_bounds_index() {
        let c = Container::new();
        assert!(c.get_as::<Lines>(0).is_none());
    }

    #[test]
    fn get_mut_as_allows_mutation_on_concrete_child() {
        let mut c = Container::new();
        c.add_child(Box::new(Lines(vec!["a".to_string()])));

        {
            let lines = c.get_mut_as::<Lines>(0).expect("Lines at index 0");
            lines.0.push("b".to_string());
        }

        let after = c.get_as::<Lines>(0).expect("still Lines at index 0");
        assert_eq!(after.0, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn get_mut_as_returns_none_for_wrong_type() {
        let mut c = Container::new();
        c.add_child(Box::new(Empty));
        assert!(c.get_mut_as::<Lines>(0).is_none());
    }

    #[test]
    fn last_index_is_none_on_empty_container() {
        let c = Container::new();
        assert_eq!(c.last_index(), None);
    }

    #[test]
    fn last_index_tracks_the_current_len() {
        let mut c = Container::new();
        c.add_child(Box::new(Empty));
        assert_eq!(c.last_index(), Some(0));
        c.add_child(Box::new(Empty));
        assert_eq!(c.last_index(), Some(1));

        // Detach via by-ref using the held heap address. (The
        // index-based shortcut is gone — only `remove_child_by_ref`
        // remains, mirroring pi-tui's `Container.removeChild` shape.)
        let first_ptr: *const dyn Component = c.get(0).unwrap();
        // SAFETY: heap allocation is owned by `c.children[0]`.
        c.remove_child_by_ref(unsafe { &*first_ptr })
            .expect("first child present");
        assert_eq!(c.last_index(), Some(0));

        let last_ptr: *const dyn Component = c.get(0).unwrap();
        c.remove_child_by_ref(unsafe { &*last_ptr })
            .expect("last child present");
        assert_eq!(c.last_index(), None);
    }

    /// F35 — `remove_child_by_ref` removes the first child whose data
    /// pointer matches and returns the boxed component. Verifies the
    /// happy path: caller has a borrow of an added child and asks the
    /// container to detach it without knowing its current index.
    #[test]
    fn remove_child_by_ref_removes_the_matching_child_and_returns_it() {
        let mut c = Container::new();
        c.add_child(Box::new(Lines(vec!["a".to_string()])));
        c.add_child(Box::new(Lines(vec!["b".to_string()])));
        c.add_child(Box::new(Lines(vec!["c".to_string()])));

        // Read the raw pointer for the middle child, then drop the
        // immutable borrow before the `&mut self` call below. The
        // pointer remains valid because the Box's heap allocation
        // doesn't move when we re-borrow `c`.
        // SAFETY: the pointer was acquired from a live `&dyn Component`
        // borrow; the underlying Box still owns the allocation when
        // we deref it for the call.
        let target_ptr: *const dyn Component = c.get(1).unwrap();
        let target: &dyn Component = unsafe { &*target_ptr };

        let removed = c
            .remove_child_by_ref(target)
            .expect("middle child to be present");
        let removed_lines = removed
            .as_any()
            .downcast_ref::<Lines>()
            .expect("downcast to Lines");
        assert_eq!(removed_lines.0, vec!["b".to_string()]);

        assert_eq!(c.len(), 2);
        assert_eq!(c.get_as::<Lines>(0).unwrap().0, vec!["a".to_string()]);
        assert_eq!(c.get_as::<Lines>(1).unwrap().0, vec!["c".to_string()]);
    }

    /// F35 — passing a `Component` reference that was never added to
    /// the container returns `None` and leaves the container untouched.
    /// Mirrors pi-tui's `indexOf === -1` no-op branch.
    #[test]
    fn remove_child_by_ref_returns_none_when_child_is_not_in_container() {
        let mut c = Container::new();
        c.add_child(Box::new(Lines(vec!["a".to_string()])));
        c.add_child(Box::new(Lines(vec!["b".to_string()])));

        // `stray` lives on the stack; its address is necessarily
        // distinct from any heap-allocated child in `c`.
        let stray = Lines(vec!["a".to_string()]);
        let removed = c.remove_child_by_ref(&stray);
        assert!(removed.is_none());
        assert_eq!(c.len(), 2);
    }

    /// F35 — identity (not equality) is the contract: a distinct
    /// instance of the same type with byte-identical fields is treated
    /// as a different child and skipped.
    #[test]
    fn remove_child_by_ref_does_not_match_a_distinct_instance_with_identical_fields() {
        let mut c = Container::new();
        c.add_child(Box::new(Lines(vec!["same".to_string()])));

        let twin = Lines(vec!["same".to_string()]);
        let removed = c.remove_child_by_ref(&twin);
        assert!(removed.is_none());
        assert_eq!(c.len(), 1);
    }
}
