//! The legacy immediate-mode widgets drawn directly onto a [`crate::window`].
//!
//! Unlike the retained-mode [`crate::vxfw`] framework, these widgets carry no
//! `Widget` trait and no layout tree. The application owns their state, routes
//! input into them, and calls their `draw` method against a [`Window`] each
//! frame. The widget writes cells straight through that window.
//!
//! [`Window`]: crate::window::Window
//!
//! NOTE: [`scroll_view::ScrollView`] deliberately shares its name with
//! [`crate::vxfw::ScrollView`]. They are distinct widgets in distinct modules,
//! one immediate-mode and one retained-mode.

pub mod alignment;
pub mod code_view;
pub mod line_numbers;
pub mod scroll_view;
pub mod scrollbar;
pub mod text_input;
pub mod text_view;
pub mod view;

pub use crate::widgets::code_view::CodeView;
pub use crate::widgets::line_numbers::LineNumbers;
pub use crate::widgets::scroll_view::ScrollView;
pub use crate::widgets::scrollbar::Scrollbar;
pub use crate::widgets::text_input::TextInput;
pub use crate::widgets::text_view::{Buffer, BufferWriter, TextView};
pub use crate::widgets::view::View;
