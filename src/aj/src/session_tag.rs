//! The session-tag overlay: name the focused session (spec 6.8).
//!
//! A one-line editor prefilled with the label the session carries, so an edit
//! starts from what is on screen rather than from nothing. Submitting parks a
//! [`TagEdit`] for the drive loop, which is the only place that can reach the
//! host, so a local session and a connected one take the same path.
//!
//! Validation is [`normalize_tag`], the function the wire boundary and the
//! launch flag also apply, run here rather than at the peer: a refusal has to
//! read as something the person typed, and a round trip for a label the store
//! would never keep buys nothing.

use std::cell::RefCell;
use std::rc::Rc;

use aj_session::normalize_tag;
use vaxis::vxfw::to_widget_ref;

use crate::interactive::OverlayHandles;
use crate::overlay::{OverlayPlacement, close_all, close_key_label, close_top, confirm_key_label};
use crate::settings_ui::{TextEditOverlay, push_window};
use crate::toasts::show_toast;

/// A confirmed tag edit parked for the drive loop: the normalized label, or
/// `None` to clear it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TagEdit {
    pub(crate) tag: Option<String>,
}

/// Open the tag editor for the focused session, prefilled with `current`.
///
/// A confirmed edit is terminal, so it tears the whole stack down (the palette
/// included) rather than popping back to it. A refusal keeps the overlay open
/// and raises a toast: the label the store would not keep never becomes a
/// silent no-op, and the editor is still there to fix it in.
pub(crate) fn open_session_tag(handles: &OverlayHandles, current: Option<&str>) {
    let overlay = Rc::new(RefCell::new(TextEditOverlay::new(current.unwrap_or(""))));
    let focus = overlay.borrow().focus_target();
    {
        let stack = Rc::clone(&handles.stack);
        let editor = Rc::clone(&handles.editor);
        let slot = Rc::clone(&handles.tag_edit);
        let toasts = Rc::clone(&handles.toasts);
        overlay
            .borrow()
            .set_on_submit(Box::new(move |ctx, text| match normalize_tag(text) {
                Ok(tag) => {
                    *slot.borrow_mut() = Some(TagEdit { tag });
                    close_all(&stack, ctx, &editor);
                }
                // The store's own sentence, so one refusal reads the same
                // whether it is met here, on the wire, or at the launch flag.
                Err(err) => {
                    show_toast(&toasts, format!("Tag not set: {err}"));
                    ctx.redraw = true;
                }
            }));
        let stack_cancel = Rc::clone(&handles.stack);
        let editor_cancel = Rc::clone(&handles.editor);
        overlay.borrow_mut().on_cancel = Some(Box::new(move |ctx| {
            close_top(&stack_cancel, ctx, &editor_cancel)
        }));
    }
    push_window(
        &handles.stack,
        &handles.chrome,
        "Session tag",
        subtitle(),
        to_widget_ref(overlay),
        focus,
        OverlayPlacement::Small,
    );
}

/// The overlay's key-hint subtitle. Enter and Esc are the widget's fixed
/// conventions, so only the labels resolve through the keybinding data.
fn subtitle() -> String {
    format!(
        "{} to set (empty clears)  \u{2022}  {} to close",
        confirm_key_label(),
        close_key_label(),
    )
}
