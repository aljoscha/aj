//! Read-only list overlay.
//!
//! Some overlays (help, auth status) present a non-interactive
//! [`SelectList`] purely to be read: there is nothing to select, both Esc
//! and Enter close the view, and every other key is swallowed so it can't
//! leak to the background. `ReadOnlyListOverlay` is that shape, shared by
//! the host's read-only overlays.
//!
//! The host builds the [`SelectList`] (with its selection indicator
//! suppressed) and wraps it here, then polls [`Self::outcome_handle`]:
//! a `Some(())` means "close me".

use aj_tui::component::Component;
use aj_tui::components::select_list::SelectList;
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;

use crate::modes::interactive::components::outcome::OutcomeSlot;

/// Handle the host polls to learn the overlay was closed. The unit payload
/// carries no data: the only outcome of a read-only view is "closed".
pub type ReadOnlyCloseHandle = OutcomeSlot<()>;

/// A non-interactive [`SelectList`] that closes on Esc/Enter and swallows
/// every other key.
pub struct ReadOnlyListOverlay {
    list: SelectList,
    outcome: ReadOnlyCloseHandle,
    focused: bool,
}

impl ReadOnlyListOverlay {
    /// Wrap a read-only `list`. The caller is responsible for building it
    /// with `show_selection_indicator: false` so no row reads as focused.
    pub fn new(list: SelectList) -> Self {
        Self {
            list,
            outcome: ReadOnlyCloseHandle::new(),
            focused: true,
        }
    }

    /// Hand the host a clone of the close slot.
    pub fn outcome_handle(&self) -> ReadOnlyCloseHandle {
        self.outcome.clone()
    }
}

impl Component for ReadOnlyListOverlay {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Chrome (title + border) comes from the surrounding overlay frame.
        self.list.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();
        if kb.matches(event, "tui.select.cancel") || kb.matches(event, "tui.input.submit") {
            self.outcome.set(());
            return true;
        }
        // Swallow every other key: the list is read-only, so nothing
        // should reach the components behind it.
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}
