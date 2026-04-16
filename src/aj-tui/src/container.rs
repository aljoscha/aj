//! A simple vertical stacking container.
//!
//! Renders children sequentially, concatenating their output lines.

use crate::component::Component;
use crate::keys::InputEvent;
use crate::tui::RenderHandle;

/// A container that stacks child components vertically.
///
/// `render()` concatenates all children's rendered lines into a single flat list.
/// The container itself has no visual representation (no padding, no borders).
pub struct Container {
    children: Vec<Box<dyn Component>>,
    /// Cached [`RenderHandle`], set by [`Container::set_render_handle`]
    /// (forwarded from [`crate::tui::Tui`] when the container enters
    /// the tree). Every child added after this point inherits the
    /// handle automatically via [`Container::install_handle_on`].
    render_handle: Option<RenderHandle>,
}

impl Container {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
            render_handle: None,
        }
    }

    /// Add a child component to the end of the children list.
    pub fn add_child(&mut self, mut child: Box<dyn Component>) {
        self.install_handle_on(&mut *child);
        self.children.push(child);
    }

    /// Insert a child component at the given index.
    pub fn insert_child(&mut self, index: usize, mut child: Box<dyn Component>) {
        self.install_handle_on(&mut *child);
        self.children.insert(index, child);
    }

    /// Forward the cached render handle, if any, to a freshly-admitted
    /// child. No-op when the container itself has not been wired to a
    /// `Tui` yet (e.g. tests that exercise `Container` standalone).
    fn install_handle_on(&self, child: &mut dyn Component) {
        if let Some(handle) = self.render_handle.as_ref() {
            child.set_render_handle(handle.clone());
        }
    }

    /// Remove the child at the given index. Panics if out of bounds.
    pub fn remove_child(&mut self, index: usize) -> Box<dyn Component> {
        self.children.remove(index)
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

    fn set_render_handle(&mut self, handle: RenderHandle) {
        // Cache so later-added children pick up the same handle.
        self.render_handle = Some(handle.clone());
        for child in &mut self.children {
            child.set_render_handle(handle.clone());
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
        c.remove_child(0);
        assert_eq!(c.last_index(), Some(0));
        c.remove_child(0);
        assert_eq!(c.last_index(), None);
    }
}
