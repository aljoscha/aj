//! Layout slots for the interactive mode.
//!
//! Per `docs/aj-next-plan.md` §4 the TUI is laid out as a fixed
//! sequence of named slots, each holding either a single component
//! or a [`Container`] of dynamically-added components. The event
//! pump and the input-handling code address slots by index via
//! the [`SlotIndex`] enum so changes to the layout don't ripple
//! through every call site.
//!
//! Today's slots, top to bottom:
//!
//! | Slot | Content |
//! |---|---|
//! | `Header` | one-line dim banner with session id + transient notices |
//! | `Chat` | [`super::components::chat_view::ChatView`] — main transcript + sub-agent boxes |
//! | `Status` | `Container` holding the [`super::components::loader_status::LoaderStatus`] (idle = empty) |
//! | `Pending` | [`super::components::pending_message::PendingMessage`] — the queued message (empty when none) |
//! | `Editor` | the prompt editor (focused) |
//! | `Footer` | one-line dim banner with model / cwd / usage |
//!
//! [`Container`]: aj_tui::container::Container

use aj_tui::components::editor::Editor;
use aj_tui::container::Container;
use aj_tui::tui::Tui;

use crate::config::theme::{ThemeHandle, chat_theme, editor_theme};
use crate::modes::interactive::components::chat_view::ChatView;
use crate::modes::interactive::components::footer::Footer;
use crate::modes::interactive::components::header::Header;
use crate::modes::interactive::components::loader_status::LoaderStatus;
use crate::modes::interactive::components::pending_message::PendingMessage;

/// Stable index of each layout slot in the TUI's root container.
///
/// Mapping is enforced by [`build_layout`]; downstream code that
/// pulls a slot back out should always go through the
/// `slot_index_of_*` helpers below rather than hard-coding the
/// numeric value, so a future reshape (insert a slot, swap two
/// slots) only needs to update one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotIndex {
    Header,
    Chat,
    Status,
    Pending,
    Editor,
    Footer,
}

impl SlotIndex {
    /// Numeric index inside [`Tui::root`]'s [`Container`]. Use
    /// this when calling [`Tui::get_mut_as`] to look up a slot's
    /// concrete type.
    ///
    /// The mapping lives here (not on a `repr(usize)` cast) so
    /// reordering slots only touches the match arms below; nothing
    /// else in the file mentions a numeric index.
    pub fn idx(self) -> usize {
        match self {
            SlotIndex::Header => 0,
            SlotIndex::Chat => 1,
            SlotIndex::Status => 2,
            SlotIndex::Pending => 3,
            SlotIndex::Editor => 4,
            SlotIndex::Footer => 5,
        }
    }
}

/// Construct and attach the interactive-mode layout to `tui`.
///
/// Adds the five slots in the order documented above. The editor
/// is constructed with the shared [`editor_theme`] and immediately
/// focused so a freshly-launched session has the cursor in the
/// prompt.
///
/// `tui.start()` is *not* called here — the caller drives the TUI
/// lifecycle, and tests using a virtual terminal want full control
/// over when the terminal is initialised.
///
/// `syntax_highlight` toggles syntect highlighting of fenced code
/// blocks in the chat scrollback (the `config.toml`
/// `syntax_highlighting` option).
pub fn build_layout(tui: &mut Tui, theme: &ThemeHandle, syntax_highlight: bool) {
    // Header slot.
    tui.add_child(Box::new(Header::new()));

    // Chat scrollback. A `ChatView` owns the main transcript and
    // the per-sub-agent boxes; the event pump routes each event to
    // the owning agent's transcript and the view switches which
    // agent's transcript is shown (main, or a sub-agent in full).
    tui.add_child(Box::new(ChatView::new(chat_theme(theme, syntax_highlight))));

    // Status slot. Always present; the loader inside it toggles
    // its own visibility based on whether the agent is mid-turn.
    let status = {
        let mut c = Container::new();
        c.add_child(Box::new(LoaderStatus::new(tui.handle())));
        c
    };
    tui.add_child(Box::new(status));

    // Pending-message box. Sits directly above the editor and renders
    // the queued message (steering / follow-up) for the viewed agent;
    // empty (zero height) when nothing is queued.
    tui.add_child(Box::new(PendingMessage::new()));

    // Editor. Themed via the shared `editor_theme`; the event
    // pump installs an autocomplete provider once selectors land.
    let editor = Editor::new(tui.handle(), editor_theme(theme));
    tui.add_child(Box::new(editor));

    // Footer slot.
    tui.add_child(Box::new(Footer::new()));

    // Focus the editor. After `build_layout` returns, the host's
    // main loop drives `tui.next_event()` and the editor receives
    // every key.
    tui.set_focus(Some(SlotIndex::Editor.idx()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_tui::terminal::ProcessTerminal;

    #[test]
    fn build_layout_attaches_six_slots() {
        // Construct against a `ProcessTerminal` without calling
        // `start()` — the layout function only ever needs the
        // container surface, not the running terminal. This keeps
        // the test from touching real stdin/stdout.
        let mut tui = Tui::new(Box::new(ProcessTerminal::new()));
        let theme = ThemeHandle::new(crate::config::theme::Theme::bundled_dark());
        build_layout(&mut tui, &theme, true);
        assert_eq!(tui.len(), 6, "expected the six layout slots");
    }
}
