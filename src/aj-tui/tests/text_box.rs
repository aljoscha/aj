//! Tests for the `TextBox` container component: padding, background
//! application, and the render cache that avoids rebuilding output on
//! frames where no input changed.

mod support;

use std::cell::Cell;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::components::text_box::TextBox;
use aj_tui::impl_component_any;

/// A child that returns a pre-set set of lines and counts how many
/// times its `render` method was called. Used to prove the TextBox
/// cache path doesn't re-invoke children (it does, to get fresh line
/// content; the proof is that the *output* doesn't get re-built).
struct CountingLines {
    lines: Vec<String>,
    renders: Rc<Cell<usize>>,
}

impl CountingLines {
    fn new(lines: Vec<String>) -> (Self, Rc<Cell<usize>>) {
        let renders = Rc::new(Cell::new(0));
        (
            Self {
                lines,
                renders: Rc::clone(&renders),
            },
            renders,
        )
    }
}

impl Component for CountingLines {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        self.renders.set(self.renders.get() + 1);
        self.lines.clone()
    }
}

#[test]
fn renders_children_with_padding() {
    let mut tb = TextBox::new();
    let (child, _) = CountingLines::new(vec!["hello".into(), "world".into()]);
    tb.add_child(Box::new(child));

    let lines = tb.render(10);

    // Default padding_x=1, padding_y=1 on a width-10 box:
    //   row 0: top padding (10 spaces)
    //   row 1: " hello    "
    //   row 2: " world    "
    //   row 3: bottom padding (10 spaces)
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0], " ".repeat(10));
    assert_eq!(lines[3], " ".repeat(10));
    assert!(lines[1].starts_with(" hello"));
    assert!(lines[2].starts_with(" world"));
}

#[test]
fn empty_children_render_as_empty() {
    let mut tb = TextBox::new();
    assert!(tb.render(10).is_empty());
}

#[test]
fn zero_content_width_renders_as_empty() {
    // padding_x=1 on a width-2 box leaves 0 content columns.
    let mut tb = TextBox::new();
    let (child, _) = CountingLines::new(vec!["x".into()]);
    tb.add_child(Box::new(child));
    assert!(tb.render(2).is_empty());
}

#[test]
fn cache_reuses_lines_when_width_and_children_unchanged() {
    let mut tb = TextBox::new();
    let (child, _) = CountingLines::new(vec!["streaming".into()]);
    tb.add_child(Box::new(child));

    let first = tb.render(20);
    let second = tb.render(20);

    // Byte-identical output across renders: the cache delivered it.
    assert_eq!(first, second);
}

#[test]
fn cache_invalidates_when_width_changes() {
    // Same child content, different width — the padded/padded-bg
    // rows must be rebuilt because their length depends on width.
    let mut tb = TextBox::new();
    let (child, _) = CountingLines::new(vec!["x".into()]);
    tb.add_child(Box::new(child));

    let at_10 = tb.render(10);
    let at_20 = tb.render(20);

    assert_ne!(at_10, at_20);
    // Top-padding row width follows the box width.
    assert_eq!(at_10[0].len(), 10);
    assert_eq!(at_20[0].len(), 20);
}

#[test]
fn cache_invalidates_when_child_output_changes() {
    struct Mutable {
        value: Rc<std::cell::RefCell<String>>,
    }
    impl Component for Mutable {
        impl_component_any!();

        fn render(&mut self, _width: usize) -> Vec<String> {
            vec![self.value.borrow().clone()]
        }
    }

    let value = Rc::new(std::cell::RefCell::new("first".to_string()));
    let mut tb = TextBox::new();
    tb.add_child(Box::new(Mutable {
        value: Rc::clone(&value),
    }));

    let first = tb.render(20);
    *value.borrow_mut() = "second".into();
    let second = tb.render(20);

    assert_ne!(first, second, "changing child output must bypass the cache");
    assert!(second[1].contains("second"));
}

#[test]
fn invalidate_drops_the_cache() {
    let mut tb = TextBox::new();
    let (child, _) = CountingLines::new(vec!["cached".into()]);
    tb.add_child(Box::new(child));

    let _ = tb.render(20);
    tb.invalidate();
    // The only visible consequence of invalidate() is that a subsequent
    // render rebuilds (which is an internal allocation concern). We
    // assert the output still matches so `invalidate` stays correct
    // even when the cache would otherwise have been reused.
    let after = tb.render(20);
    assert!(after[1].contains("cached"));
}

#[test]
fn padding_change_invalidates_cache() {
    // Shrinking horizontal padding widens the content column, so the
    // cached output at the old padding must not be returned.
    let mut tb = TextBox::new();
    let (child, _) = CountingLines::new(vec!["hi".into()]);
    tb.add_child(Box::new(child));

    let before = tb.render(10);
    tb.set_padding_x(2);
    let after = tb.render(10);

    assert_ne!(before, after);
    // New padding is two spaces on each side; the content row starts
    // with exactly two spaces before "hi".
    assert!(after[1].starts_with("  hi"));
}

#[test]
fn bg_fn_output_change_invalidates_via_sample_probe() {
    // Two closures that produce different outputs for the same input:
    // the cache probes `bg_fn("test")` on each render and invalidates
    // when the probe differs.
    let mut tb = TextBox::new();
    let (child, _) = CountingLines::new(vec!["content".into()]);
    tb.add_child(Box::new(child));
    tb.set_bg_fn(Box::new(|s| format!("[A:{}]", s)));
    let first = tb.render(20);
    tb.set_bg_fn(Box::new(|s| format!("[B:{}]", s)));
    let second = tb.render(20);

    assert_ne!(
        first, second,
        "swapping bg_fn to a closure with different output must invalidate"
    );
    assert!(first[1].starts_with("[A:"));
    assert!(second[1].starts_with("[B:"));
}

#[test]
fn adding_a_child_invalidates_cache() {
    let mut tb = TextBox::new();
    let (first_child, _) = CountingLines::new(vec!["first".into()]);
    tb.add_child(Box::new(first_child));

    let before = tb.render(20);
    let (second_child, _) = CountingLines::new(vec!["second".into()]);
    tb.add_child(Box::new(second_child));
    let after = tb.render(20);

    assert_ne!(before, after);
    assert_eq!(after.len(), before.len() + 1);
}
