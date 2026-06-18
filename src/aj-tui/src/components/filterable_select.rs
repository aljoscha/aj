//! Search box wrapped around a [`SelectList`].
//!
//! Most selector overlays in a host application are the same shape: a
//! one-line search input stacked above a navigable result list, where
//! typing filters the list, the arrow/page keys move the highlight, Enter
//! confirms the highlighted row and Escape cancels. `FilterableSelect`
//! owns that universal mechanics so each host selector only supplies its
//! items, its confirm mapping, and (optionally) a custom query handler.
//!
//! It is deliberately domain-agnostic: like [`SelectList`], it reports
//! outcomes through `on_select` / `on_cancel` callbacks rather than owning
//! a result slot, so the host wires the callbacks to whatever it polls.
//!
//! # Query handling
//!
//! By default a query change calls [`SelectList::set_filter`] (the list's
//! own fuzzy/substring filter). A host whose filtering can't be expressed
//! as a `SelectList` filter mode (e.g. custom per-field scoring) installs
//! an [`FilterableSelect::on_query`] handler instead, which is handed the
//! query and the list to repopulate via [`SelectList::set_items`].
//!
//! # Streaming and status
//!
//! Lists that fill incrementally push rows in through
//! [`FilterableSelect::list_mut`] and toggle [`FilterableSelect::set_loading`];
//! while loading with an empty list the body shows a loading message. A
//! host that reserves a status line (via [`FilterableSelect::with_status_line`])
//! gets one styled row between the search box and the list, whose text it
//! updates with [`FilterableSelect::set_status_line`].

use std::sync::Arc;

use crate::component::Component;
use crate::components::select_list::{SelectItem, SelectList};
use crate::components::text_input::TextInput;
use crate::keybindings;
use crate::keys::InputEvent;

/// A search input stacked above a [`SelectList`], with the standard
/// filter/navigate/confirm/cancel key routing handled internally.
pub struct FilterableSelect {
    search: TextInput,
    list: SelectList,
    /// Styles the optional status line and the loading message. Supplied
    /// by the host from the same palette its list uses (typically the
    /// list theme's `description` closure).
    status_style: Arc<dyn Fn(&str) -> String>,
    /// Whether a status row is reserved between the search box and the
    /// list. Fixed for the life of the instance so the chrome-row
    /// accounting in `set_available_height` stays stable; the text is
    /// dynamic via `set_status_line`.
    has_status_line: bool,
    status_line: Option<String>,
    /// While `true` and the list is empty, the body renders
    /// `loading_message` instead of the list.
    loading: bool,
    loading_message: String,
    /// Called when an item is confirmed (Enter) with the highlighted row.
    pub on_select: Option<Box<dyn FnMut(&SelectItem)>>,
    /// Called when the overlay is cancelled (Escape).
    pub on_cancel: Option<Box<dyn FnMut()>>,
    /// Optional custom query handler. When set, a search-text change calls
    /// this with the query and the list to repopulate; when unset, a
    /// change calls [`SelectList::set_filter`].
    pub on_query: Option<Box<dyn FnMut(&str, &mut SelectList)>>,
}

impl FilterableSelect {
    /// Build a selector around a pre-constructed `list`.
    ///
    /// `search_prompt` labels the embedded search input. `status_style`
    /// styles the optional status line and the loading message; pass the
    /// same palette the list uses. Both the search input and the list
    /// start focused so a host that forwards focus down sees the cursor in
    /// the search box.
    pub fn new(
        search_prompt: &str,
        mut list: SelectList,
        status_style: Arc<dyn Fn(&str) -> String>,
    ) -> Self {
        let mut search = TextInput::new(search_prompt);
        search.set_focused(true);
        list.set_focused(true);
        Self {
            search,
            list,
            status_style,
            has_status_line: false,
            status_line: None,
            loading: false,
            loading_message: String::new(),
            on_select: None,
            on_cancel: None,
            on_query: None,
        }
    }

    /// Reserve a status row between the search box and the list. The row
    /// is always rendered (blank until [`Self::set_status_line`] sets it)
    /// so the layout doesn't jump when the text appears.
    pub fn with_status_line(mut self) -> Self {
        self.has_status_line = true;
        self
    }

    /// Set the loading-body message shown while [`Self::set_loading`] is on
    /// and the list is empty.
    pub fn set_loading_message(&mut self, message: &str) {
        self.loading_message = message.to_string();
    }

    /// Update the status-line text. No effect unless the row was reserved
    /// with [`Self::with_status_line`].
    pub fn set_status_line(&mut self, text: Option<String>) {
        self.status_line = text;
    }

    /// Toggle the loading state. While on and the list is empty, the body
    /// renders the loading message instead of the list.
    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    /// Pre-fill the search box and apply it as though the user typed it.
    /// Used when an overlay opens already filtered.
    pub fn set_query(&mut self, query: &str) {
        self.search.set_value(query);
        self.apply_query();
    }

    /// Mutable access to the inner list, for hosts that stream rows in
    /// ([`SelectList::extend_items`]) or chase a row
    /// ([`SelectList::select_by_value`]).
    pub fn list_mut(&mut self) -> &mut SelectList {
        &mut self.list
    }

    /// The currently-highlighted item, if any.
    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.list.selected_item()
    }

    /// Whether `event` is one of the list-navigation keys this selector
    /// routes into the list (up / down / pageUp / pageDown).
    ///
    /// Exposed so a host that reacts to user navigation (e.g. a streaming
    /// selector that stops auto-selecting a row once the user takes over)
    /// can detect the intent without re-deriving the bindings. This fires
    /// even when the key doesn't move the highlight (Up at the top), which
    /// a before/after selection comparison would miss.
    pub fn is_navigation_key(event: &InputEvent) -> bool {
        let kb = keybindings::get();
        kb.matches(event, "tui.select.up")
            || kb.matches(event, "tui.select.down")
            || kb.matches(event, "tui.select.pageUp")
            || kb.matches(event, "tui.select.pageDown")
    }

    /// Current search query (trimmed by the list, not here).
    pub fn query(&self) -> &str {
        self.search.value()
    }

    /// Re-derive the visible set from the current search text: a custom
    /// `on_query` handler if installed, else the list's own filter.
    fn apply_query(&mut self) {
        let query = self.search.value().to_string();
        // Take the handler out so it can borrow `&mut self.list` without
        // overlapping the borrow of `self.on_query`; it doesn't re-enter
        // `FilterableSelect`, so a one-shot swap is safe.
        match self.on_query.take() {
            Some(mut handler) => {
                handler(&query, &mut self.list);
                self.on_query = Some(handler);
            }
            None => self.list.set_filter(&query),
        }
    }

    fn fire_select(&mut self) {
        if let Some(item) = self.list.selected_item().cloned()
            && let Some(on_select) = self.on_select.as_mut()
        {
            on_select(&item);
        }
    }
}

impl Component for FilterableSelect {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = self.search.render(width);
        if self.has_status_line {
            let text = self.status_line.clone().unwrap_or_default();
            lines.push((self.status_style)(&text));
        }
        lines.push(String::new());
        if self.loading && self.list.items().is_empty() {
            lines.push((self.status_style)(&self.loading_message));
        } else {
            lines.extend(self.list.render(width));
        }
        lines
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();

        // Cancel and confirm are intercepted here so exactly one outcome
        // fires regardless of which child the keys would otherwise reach.
        if kb.matches(event, "tui.select.cancel") {
            drop(kb);
            if let Some(on_cancel) = self.on_cancel.as_mut() {
                on_cancel();
            }
            return true;
        }
        if kb.matches(event, "tui.input.submit") {
            drop(kb);
            self.fire_select();
            return true;
        }

        // Navigation belongs to the list; the search text is untouched.
        if kb.matches(event, "tui.select.up")
            || kb.matches(event, "tui.select.down")
            || kb.matches(event, "tui.select.pageUp")
            || kb.matches(event, "tui.select.pageDown")
        {
            drop(kb);
            return self.list.handle_input(event);
        }

        // Everything else edits the search box. Drop the registry guard
        // so `apply_query` can re-acquire it without contention.
        drop(kb);
        let before = self.search.value().to_string();
        let handled = self.search.handle_input(event);
        if handled && self.search.value() != before {
            self.apply_query();
        }
        handled
    }

    fn set_available_height(&mut self, rows: usize) {
        // Chrome above the list, mirrored in `render`: search input + a
        // blank separator + the list's own scroll-info line, plus the
        // status row when one is reserved.
        let chrome = 3 + usize::from(self.has_status_line);
        self.list
            .set_max_visible(rows.saturating_sub(chrome).max(1));
    }

    fn set_focused(&mut self, focused: bool) {
        self.search.set_focused(focused);
        self.list.set_focused(focused);
    }

    fn is_focused(&self) -> bool {
        self.search.is_focused()
    }

    fn invalidate(&mut self) {
        self.search.invalidate();
        self.list.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::select_list::{SelectListLayout, SelectListTheme};
    use crate::keys::Key;
    use std::sync::{Arc, Mutex};

    fn identity_theme() -> SelectListTheme {
        SelectListTheme {
            selected_prefix: Arc::new(|s| s.to_string()),
            selected_text: Arc::new(|s| s.to_string()),
            description: Arc::new(|s| s.to_string()),
            scroll_info: Arc::new(|s| s.to_string()),
            no_match: Arc::new(|s| s.to_string()),
            prefix: Arc::new(|s| s.to_string()),
            shortcut: Arc::new(|s| s.to_string()),
        }
    }

    fn list_with(items: &[(&str, &str)]) -> SelectList {
        let items = items
            .iter()
            .map(|(v, l)| SelectItem::new(v, l))
            .collect::<Vec<_>>();
        SelectList::new(items, 10, identity_theme(), SelectListLayout::default())
    }

    fn build(items: &[(&str, &str)]) -> FilterableSelect {
        FilterableSelect::new("search: ", list_with(items), Arc::new(|s| s.to_string()))
    }

    #[test]
    fn enter_confirms_highlighted_row() {
        let mut fs = build(&[("a", "alpha"), ("b", "bravo")]);
        let picked = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&picked);
        fs.on_select = Some(Box::new(move |item| {
            *sink.lock().unwrap() = Some(item.value.clone());
        }));
        fs.handle_input(&Key::down());
        fs.handle_input(&Key::enter());
        assert_eq!(picked.lock().unwrap().as_deref(), Some("b"));
    }

    #[test]
    fn escape_cancels() {
        let mut fs = build(&[("a", "alpha")]);
        let cancelled = Arc::new(Mutex::new(false));
        let sink = Arc::clone(&cancelled);
        fs.on_cancel = Some(Box::new(move || *sink.lock().unwrap() = true));
        fs.handle_input(&Key::escape());
        assert!(*cancelled.lock().unwrap());
    }

    #[test]
    fn typing_filters_via_set_filter_by_default() {
        let mut fs = build(&[("a", "alpha"), ("b", "bravo")]);
        for c in "alp".chars() {
            fs.handle_input(&Key::char(c));
        }
        let body = fs.render(40).join("\n");
        assert!(body.contains("alpha"), "got: {body}");
        assert!(!body.contains("bravo"), "got: {body}");
    }

    #[test]
    fn on_query_handler_overrides_default_filter() {
        let mut fs = build(&[("a", "alpha")]);
        // A handler that replaces the items wholesale, ignoring the
        // default `set_filter` path entirely.
        fs.on_query = Some(Box::new(|query, list| {
            list.set_items(vec![SelectItem::new("q", &format!("query:{query}"))]);
        }));
        fs.handle_input(&Key::char('z'));
        let body = fs.render(40).join("\n");
        assert!(body.contains("query:z"), "got: {body}");
    }

    #[test]
    fn loading_body_shows_message_until_rows_arrive() {
        let mut fs = FilterableSelect::new("search: ", list_with(&[]), Arc::new(|s| s.to_string()));
        fs.set_loading_message("Loading…");
        fs.set_loading(true);
        assert!(fs.render(40).join("\n").contains("Loading…"));

        fs.list_mut()
            .extend_items(vec![SelectItem::new("a", "alpha")]);
        let body = fs.render(40).join("\n");
        assert!(body.contains("alpha"), "got: {body}");
        assert!(!body.contains("Loading…"), "got: {body}");
    }

    #[test]
    fn status_line_renders_between_search_and_list() {
        let mut fs = build(&[("a", "alpha")]).with_status_line();
        fs.set_status_line(Some("Showing: everything".to_string()));
        let lines = fs.render(60);
        // Layout: search(0), status(1), blank(2), list(3+).
        assert!(lines[1].contains("Showing: everything"), "got: {lines:?}");
        assert_eq!(lines[2], "", "blank separator after status: {lines:?}");
        assert!(lines[3].contains("alpha"), "list follows: {lines:?}");
    }

    #[test]
    fn status_row_costs_one_extra_visible_row_of_chrome() {
        // A reserved status row consumes one more line of the available
        // height than a plain selector, so the visible list window is one
        // shorter for the same budget.
        let items: Vec<(String, String)> = (0..20)
            .map(|i| (i.to_string(), format!("row {i}")))
            .collect();
        let refs: Vec<(&str, &str)> = items
            .iter()
            .map(|(v, l)| (v.as_str(), l.as_str()))
            .collect();

        let mut plain = build(&refs);
        plain.set_available_height(12);
        let plain_rows = plain.render(40).len();

        let mut with_status = build(&refs).with_status_line();
        with_status.set_status_line(Some("status".to_string()));
        with_status.set_available_height(12);
        let status_rows = with_status.render(40).len();

        // Same total rendered height for the same budget, but the status
        // variant spends one of those rows on the status line instead of a
        // list row.
        assert_eq!(plain_rows, status_rows);
        assert!(
            with_status.list_mut().items().len() == plain.list_mut().items().len(),
            "same backing items"
        );
    }
}
