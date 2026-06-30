//! Window-positioning helpers: place a fixed-size child window at one of five
//! anchor points within a parent.
//!
//! Each function returns a child [`Window`] sized `cols` by `rows`, offset so
//! it sits at the requested anchor. The offsets use saturating subtraction, so
//! a child larger than the parent clamps to the parent's origin rather than
//! wrapping.

use crate::window::{ChildOptions, Window};

/// A child window of size `cols` by `rows`, centered within `parent`.
pub fn center(parent: Window<'_>, cols: u16, rows: u16) -> Window<'_> {
    let y_off = (parent.height / 2).saturating_sub(rows / 2);
    let x_off = (parent.width / 2).saturating_sub(cols / 2);
    parent.child(ChildOptions {
        x_off: i32::from(x_off),
        y_off: i32::from(y_off),
        width: Some(cols),
        height: Some(rows),
        ..ChildOptions::default()
    })
}

/// A child window of size `cols` by `rows`, anchored to the top-left corner.
pub fn top_left(parent: Window<'_>, cols: u16, rows: u16) -> Window<'_> {
    parent.child(ChildOptions {
        x_off: 0,
        y_off: 0,
        width: Some(cols),
        height: Some(rows),
        ..ChildOptions::default()
    })
}

/// A child window of size `cols` by `rows`, anchored to the top-right corner.
pub fn top_right(parent: Window<'_>, cols: u16, rows: u16) -> Window<'_> {
    let x_off = parent.width.saturating_sub(cols);
    parent.child(ChildOptions {
        x_off: i32::from(x_off),
        y_off: 0,
        width: Some(cols),
        height: Some(rows),
        ..ChildOptions::default()
    })
}

/// A child window of size `cols` by `rows`, anchored to the bottom-left corner.
pub fn bottom_left(parent: Window<'_>, cols: u16, rows: u16) -> Window<'_> {
    let y_off = parent.height.saturating_sub(rows);
    parent.child(ChildOptions {
        x_off: 0,
        y_off: i32::from(y_off),
        width: Some(cols),
        height: Some(rows),
        ..ChildOptions::default()
    })
}

/// A child window of size `cols` by `rows`, anchored to the bottom-right corner.
pub fn bottom_right(parent: Window<'_>, cols: u16, rows: u16) -> Window<'_> {
    let y_off = parent.height.saturating_sub(rows);
    let x_off = parent.width.saturating_sub(cols);
    parent.child(ChildOptions {
        x_off: i32::from(x_off),
        y_off: i32::from(y_off),
        width: Some(cols),
        height: Some(rows),
        ..ChildOptions::default()
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::screen::Screen;

    fn screen() -> RefCell<Screen> {
        RefCell::new(Screen::new(crate::Winsize {
            rows: 20,
            cols: 20,
            x_pixel: 0,
            y_pixel: 0,
        }))
    }

    fn root(screen: &RefCell<Screen>) -> Window<'_> {
        Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 20,
            height: 20,
            screen,
        }
    }

    #[test]
    fn center_offsets_and_size() {
        let screen = screen();
        let win = center(root(&screen), 4, 2);
        assert_eq!((win.x_off, win.y_off), (8, 9));
        assert_eq!((win.width, win.height), (4, 2));
    }

    #[test]
    fn corners_anchor_correctly() {
        let screen = screen();
        let parent = root(&screen);
        let tl = top_left(parent, 4, 2);
        assert_eq!((tl.x_off, tl.y_off), (0, 0));
        assert_eq!(top_right(parent, 4, 2).x_off, 16);
        assert_eq!(bottom_left(parent, 4, 2).y_off, 18);
        let br = bottom_right(parent, 4, 2);
        assert_eq!((br.x_off, br.y_off), (16, 18));
    }

    #[test]
    fn oversized_child_saturates_to_origin() {
        let screen = screen();
        let parent = root(&screen);
        // A child wider and taller than the parent clamps its offset to 0.
        let br = bottom_right(parent, 40, 40);
        assert_eq!((br.x_off, br.y_off), (0, 0));
    }
}
