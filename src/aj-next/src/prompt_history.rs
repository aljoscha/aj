//! The prompt-history search overlay: recall a previously submitted
//! prompt into the editor.
//!
//! A [`FilterableSelect`] over the prompts scanned from on-disk session
//! logs. Confirming a row parks the chosen prompt text for the host to
//! recall into the editor (it is not submitted, so the user can edit
//! first). Esc cancels.
//!
//! The scan is off the drive loop: the overlay opens showing a loading
//! placeholder and the host streams rows in as the scan (run on a
//! blocking thread) emits per-file batches, so the list fills
//! progressively rather than blocking on the whole walk. The
//! overlay-local `Ctrl+T` ([`ACTION_HISTORY_TOGGLE_SCOPE`]) flips the
//! scope between the current workspace and all workspaces, re-parking a
//! fetch for the host.
//!
//! Filter key: a row's filter key is the full prompt text, which is both
//! what the fuzzy filter matches and the value recalled on confirm. The
//! project label (all-workspaces scope) shows in the description column.

use std::cell::RefCell;
use std::rc::Rc;

use aj_app::keybindings::{ACTION_HISTORY_TOGGLE_SCOPE, default_action_shortcut};
use aj_session::PromptEntry;
use vaxis::vxfw::{
    DrawContext, Event, EventContext, FilterableSelect, OverlayWindow, RelativePoint, SelectItem,
    SubSurface, Surface, Widget, WidgetRef, draw_widget, to_widget_ref,
};

use crate::keymap::action_matches;
use crate::overlay::{
    OverlayChrome, OverlayPlacement, OverlayStack, close_all, close_key_label, close_top,
    confirm_key_label,
};
use crate::settings_ui::push_window;

/// Cap on how many prompts a scope retains. Generous enough to cover any
/// realistic history while bounding the scan and the in-memory list.
pub(crate) const MAX_ENTRIES: usize = 2000;

/// How much of a prompt's first line the row label shows.
const LABEL_MAX_CHARS: usize = 120;

/// Which history scope the overlay is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoryScope {
    Workspace,
    All,
}

/// A parked request for the host to scan `scope` and fill `select`. The
/// select handle is `!Send`, so it stays on the host side; the spawned
/// scan produces only the (Send) prompt entries.
pub(crate) struct HistoryFetch {
    pub(crate) scope: HistoryScope,
    pub(crate) select: Rc<RefCell<FilterableSelect>>,
}

/// The prompt-history widget: a [`FilterableSelect`] plus the scope and
/// the host slots it parks into.
pub(crate) struct PromptHistoryView {
    select: Rc<RefCell<FilterableSelect>>,
    scope: HistoryScope,
    /// The window frame, kept so a scope toggle can refresh its dynamic
    /// subtitle. `None` until wired after the push.
    window: Option<Rc<RefCell<OverlayWindow>>>,
    fetch_slot: Rc<RefCell<Option<HistoryFetch>>>,
}

impl PromptHistoryView {
    fn set_window(&mut self, window: Rc<RefCell<OverlayWindow>>) {
        self.window = Some(window);
    }

    /// Park a scan request for the current scope and show the loading
    /// placeholder until it lands.
    fn request_scan(&self) {
        self.select.borrow().set_items(loading_items());
        *self.fetch_slot.borrow_mut() = Some(HistoryFetch {
            scope: self.scope,
            select: Rc::clone(&self.select),
        });
    }
}

impl Widget for PromptHistoryView {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // Wrap the select's surface as a child so both identities stay on
        // the focus path (see the agent picker's draw for the rationale).
        let size = ctx.max.size();
        let mut surface = Surface::with_size(size);
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: draw_widget(&to_widget_ref(Rc::clone(&self.select)), ctx),
            z_index: 0,
        });
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else {
            return;
        };
        // Overlay-local scope toggle (Spec F): flip, re-parks a fetch,
        // and refresh the subtitle to name the scope it would switch to.
        if action_matches(key, ACTION_HISTORY_TOGGLE_SCOPE) {
            self.scope = match self.scope {
                HistoryScope::Workspace => HistoryScope::All,
                HistoryScope::All => HistoryScope::Workspace,
            };
            self.request_scan();
            if let Some(window) = &self.window {
                window.borrow_mut().subtitle = subtitle(self.scope);
            }
            ctx.consume_and_redraw();
        }
        // Everything else (Enter/Esc/nav/typing) falls through to the
        // inner select and the filter field below it.
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// The single loading placeholder shown until a scan lands. Its filter
/// key is empty so the confirm guard treats it as "no selection".
fn loading_items() -> Vec<SelectItem> {
    vec![SelectItem::new("Loading\u{2026}", "")]
}

/// Build one row per entry: the label is the truncated first line, the
/// filter key is the full prompt (matched and recalled verbatim), and
/// the project label (all-workspaces scope) rides in the description.
pub(crate) fn build_items(entries: &[PromptEntry]) -> Vec<SelectItem> {
    entries
        .iter()
        .map(|e| {
            let label = truncate_chars(first_line(&e.text), LABEL_MAX_CHARS);
            let mut item = SelectItem::new(label, e.text.clone());
            if let Some(project) = &e.project {
                item = item.with_description(project.clone());
            }
            item
        })
        .collect()
}

/// First line of `text` for the label. Prompts are trimmed at scan time,
/// so this is non-blank in practice.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// Truncate to `max` characters (not bytes), appending an ellipsis when
/// cut.
fn truncate_chars(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let cut = max.saturating_sub(1).min(chars.len());
    let mut s: String = chars[..cut].iter().collect();
    s.push('\u{2026}');
    s
}

/// The scope-toggle subtitle, resolved from keybinding data: the toggle
/// hint names the scope it would switch *to*.
fn subtitle(scope: HistoryScope) -> String {
    let toggle = default_action_shortcut(ACTION_HISTORY_TOGGLE_SCOPE)
        .expect("aj.history.toggle_scope has a default chord");
    let scope_target = match scope {
        HistoryScope::All => "this workspace",
        HistoryScope::Workspace => "all workspaces",
    };
    let confirm = confirm_key_label();
    let close = close_key_label();
    format!("{confirm} to recall  \u{2022}  {toggle} {scope_target}  \u{2022}  {close} to close")
}

/// Open the prompt-history overlay, showing a loading placeholder and
/// parking the initial (current-workspace) scan for the host. Confirmed
/// prompts land in `recall_slot` for the host to recall into the editor.
/// Does not move focus: the caller (host) posts the refocus event.
pub(crate) fn open_prompt_history(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    fetch_slot: &Rc<RefCell<Option<HistoryFetch>>>,
    recall_slot: &Rc<RefCell<Option<String>>>,
) {
    let scope = HistoryScope::Workspace;
    let select = Rc::new(RefCell::new(FilterableSelect::new(
        loading_items(),
        chrome.select.clone(),
    )));
    let focus = select.borrow().focus_target();
    {
        let mut sel = select.borrow_mut();
        // The history list can run long, so show the vertical scroll bar.
        sel.set_show_scrollbar(true);
        let recall_c = Rc::clone(recall_slot);
        let stack_c = Rc::clone(stack);
        let editor_c = Rc::clone(editor);
        sel.on_confirm = Some(Box::new(move |ctx, item| {
            // Empty filter key is the loading placeholder: nothing to
            // recall, so leave the overlay open.
            if item.filter_key.is_empty() {
                return;
            }
            *recall_c.borrow_mut() = Some(item.filter_key.clone());
            // A confirmed pick is terminal: tear the whole stack down
            // (palette included) back to the transcript. Cancel below uses
            // `close_top`, which returns to the palette underneath.
            close_all(&stack_c, ctx, &editor_c);
        }));
        let stack_cancel = Rc::clone(stack);
        let editor_cancel = Rc::clone(editor);
        sel.on_cancel = Some(Box::new(move |ctx| {
            close_top(&stack_cancel, ctx, &editor_cancel)
        }));
    }
    let view = Rc::new(RefCell::new(PromptHistoryView {
        select: Rc::clone(&select),
        scope,
        window: None,
        fetch_slot: Rc::clone(fetch_slot),
    }));
    let window = push_window(
        stack,
        chrome,
        "Prompt history",
        subtitle(scope),
        to_widget_ref(Rc::clone(&view)),
        focus,
        OverlayPlacement::Large,
    );
    view.borrow_mut().set_window(window);
    // Park the current-workspace scan for the host to run and fill.
    view.borrow().request_scan();
}

#[cfg(test)]
mod tests {
    use vaxis::key::{Key, Modifiers};
    use vaxis::vxfw::{Phase, SelectStyles};

    use super::*;

    fn entry(text: &str, project: Option<&str>) -> PromptEntry {
        PromptEntry {
            text: text.to_string(),
            project: project.map(|s| s.to_string()),
        }
    }

    #[test]
    fn build_items_labels_first_line_and_recalls_full_text() {
        let items = build_items(&[entry("line one\nline two", Some("proj-a"))]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "line one");
        // The filter key is the full text, recalled verbatim on confirm.
        assert_eq!(items[0].filter_key, "line one\nline two");
        assert_eq!(items[0].description.as_deref(), Some("proj-a"));
    }

    #[test]
    fn truncate_keeps_first_line_within_the_cap() {
        let long = "x".repeat(200);
        let out = truncate_chars(&long, LABEL_MAX_CHARS);
        assert!(out.chars().count() <= LABEL_MAX_CHARS);
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn subtitle_names_the_scope_to_switch_to() {
        let toggle = default_action_shortcut(ACTION_HISTORY_TOGGLE_SCOPE).unwrap();
        let ws = subtitle(HistoryScope::Workspace);
        assert!(ws.contains(&toggle), "toggle hint resolved from data: {ws}");
        assert!(ws.contains("all workspaces"), "{ws}");
        assert!(subtitle(HistoryScope::All).contains("this workspace"));
        // The confirm/close labels track the keybinding data, not a literal.
        assert!(ws.contains(&confirm_key_label()), "{ws}");
        assert!(ws.contains(&close_key_label()), "{ws}");
    }

    /// Ctrl+T flips the scope, shows the loading placeholder, and parks a
    /// fetch for the new scope.
    #[test]
    fn ctrl_t_toggles_scope_and_parks_a_fetch() {
        let fetch_slot = Rc::new(RefCell::new(None));
        let select = Rc::new(RefCell::new(FilterableSelect::new(
            build_items(&[entry("prompt", None)]),
            SelectStyles::default(),
        )));
        let mut view = PromptHistoryView {
            select: Rc::clone(&select),
            scope: HistoryScope::Workspace,
            window: None,
            fetch_slot: Rc::clone(&fetch_slot),
        };
        let ctrl_t = Event::KeyPress(Key {
            codepoint: u32::from('t'),
            mods: Modifiers::CTRL,
            ..Key::default()
        });
        let mut ctx = EventContext::new();
        ctx.phase = Phase::Capturing;
        view.capture_event(&mut ctx, &ctrl_t);
        assert_eq!(view.scope, HistoryScope::All);
        let fetch = fetch_slot.borrow();
        assert!(
            matches!(
                fetch.as_ref(),
                Some(HistoryFetch {
                    scope: HistoryScope::All,
                    ..
                })
            ),
            "toggle parked an all-workspaces fetch"
        );
        // The list shows the loading placeholder while the scan runs.
        assert_eq!(select.borrow().visible_labels(), vec!["Loading\u{2026}"]);
    }

    /// The loading placeholder is inert on confirm: its empty filter key
    /// recalls nothing.
    #[test]
    fn confirm_ignores_the_loading_placeholder() {
        let recall: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let recall_c = Rc::clone(&recall);
        let mut select = FilterableSelect::new(loading_items(), SelectStyles::default());
        select.on_confirm = Some(Box::new(move |_ctx, item| {
            if item.filter_key.is_empty() {
                return;
            }
            *recall_c.borrow_mut() = Some(item.filter_key.clone());
        }));
        let enter = Event::KeyPress(Key {
            codepoint: Key::ENTER,
            ..Key::default()
        });
        let mut ctx = EventContext::new();
        ctx.phase = Phase::Capturing;
        select.capture_event(&mut ctx, &enter);
        assert!(recall.borrow().is_none(), "placeholder recalled nothing");
    }
}
