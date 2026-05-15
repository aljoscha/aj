//! Model-selector overlay (`/model`).
//!
//! Pairs a [`aj_tui::components::text_input::TextInput`] for live
//! filtering with a [`aj_tui::components::select_list::SelectList`]
//! that shows the matching entries from a snapshotted
//! [`aj_models::registry::ModelRegistry`]. The host opens this
//! overlay from `/model` (no args) or `/model <query>` (initial
//! search pre-filled); pressing Enter commits the highlighted entry,
//! Esc cancels.
//!
//! The component owns the catalog and rebuilds the inner
//! [`SelectList`] on every text change so the visible rows track the
//! current query through a fuzzy matcher
//! ([`aj_tui::fuzzy::FuzzyMatcher`]). The current model (the one
//! already wired into the agent) is pre-selected on open and tagged
//! `(current)` so a no-op confirm is obvious.
//!
//! See `docs/aj-next-plan.md` Phase 1 "Selectors and theming".

use std::sync::{Arc, Mutex};

use aj_models::registry::ModelInfo;
use aj_tui::component::Component;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use aj_tui::components::text_input::TextInput;
use aj_tui::fuzzy::FuzzyMatcher;
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;
use aj_tui::style;

/// Maximum visible rows in the result list. Eight matches the
/// thinking selector's screen footprint at a 60-column overlay;
/// taller terminals see a scrolled view via [`SelectList`]'s own
/// scroll model.
const MAX_VISIBLE_ROWS: usize = 8;

/// Outcome of a single overlay session.
///
/// `Confirmed(info)` carries the chosen [`ModelInfo`] (cloned so the
/// host can construct a new model handle without borrowing the
/// catalog); `Cancelled` is the user pressing `Esc`. The host treats
/// both as "close the overlay"; only the former mutates agent state.
#[derive(Debug, Clone)]
pub enum ModelSelectorOutcome {
    Confirmed(ModelInfo),
    Cancelled,
}

/// Cheap-to-clone handle pointing at the same outcome slot the
/// overlay component writes into.
pub type OutcomeHandle = Arc<Mutex<Option<ModelSelectorOutcome>>>;

/// The overlay's top-level component.
///
/// Owns the search input (`search`), the inner [`SelectList`]
/// (`list`), the cached catalog (`catalog`), and the outcome slot
/// (`outcome`). The host keeps another clone of `outcome` and polls
/// it after each input event to decide whether to close the overlay.
pub struct ModelSelectorComponent {
    /// Search box at the top of the overlay. Typing into it
    /// rebuilds `list`; Enter on this field is intercepted at the
    /// component level so it commits the highlighted list item
    /// instead of firing `TextInput::on_submit`.
    search: TextInput,
    /// Result list. Rebuilt every time `search` changes so the
    /// fuzzy-filtered entries reflect the current query.
    list: SelectList,
    /// Full unfiltered catalog. The component clones each entry it
    /// emits on confirm; keeping the source of truth here avoids
    /// any chance of drifting between filter and confirm.
    catalog: Vec<ModelInfo>,
    /// `(provider, id)` of the model the agent is currently using.
    /// Used to pre-select the active row on open and mark it
    /// `(current)` so a no-op confirm is obvious.
    current_key: Option<(String, String)>,
    /// Shared outcome slot. The host clones this handle once at
    /// construction and polls it after every input event.
    outcome: OutcomeHandle,
    /// Theme used to build the inner [`SelectList`]. Stored so a
    /// rebuild (after a search-text change) can reuse the same
    /// palette without the host having to pass it back in.
    theme: SelectListTheme,
    /// Reusable fuzzy matcher. Pulled out as a field so we don't
    /// reconstruct the underlying nucleo state on every keystroke
    /// (it allocates ~135 KB up front per `FuzzyMatcher::new`).
    matcher: FuzzyMatcher,
    /// One-line title rendered above the search input.
    title: String,
}

impl ModelSelectorComponent {
    /// Build a fresh selector.
    ///
    /// `catalog` is the snapshotted model list to choose from (must
    /// already include any provider/override merging). `current` is
    /// the agent's active model — used to pre-select the matching
    /// row and mark it `(current)`. `initial_query` pre-fills the
    /// search box (used by `/model <query>` invocations) so the
    /// overlay opens already filtered. `theme` styles the underlying
    /// [`SelectList`].
    pub fn new(
        theme: SelectListTheme,
        catalog: Vec<ModelInfo>,
        current: Option<&dyn ModelIdentity>,
        initial_query: Option<String>,
    ) -> Self {
        let current_key = current.map(|m| (m.provider().to_string(), m.id().to_string()));

        let mut search = TextInput::new("search: ");
        if let Some(q) = initial_query {
            search.set_value(&q);
        }
        search.set_focused(true);

        // Placeholder list — rebuilt by `rebuild_list` below to apply
        // the initial filter and pre-selection.
        let list = SelectList::new(
            Vec::new(),
            MAX_VISIBLE_ROWS,
            theme.clone(),
            SelectListLayout::default(),
        );

        let outcome: OutcomeHandle = Arc::new(Mutex::new(None));
        let mut component = Self {
            search,
            list,
            catalog,
            current_key,
            outcome,
            theme,
            matcher: FuzzyMatcher::new(),
            title: "Select model — Enter to apply, Esc to cancel".to_string(),
        };
        component.rebuild_list();
        component
    }

    /// Hand the host a clone of the outcome slot. After each input
    /// event the host calls `lock().take()` on this handle; on
    /// `Some(_)` it hides the overlay and applies the result.
    pub fn outcome_handle(&self) -> OutcomeHandle {
        Arc::clone(&self.outcome)
    }

    /// Rebuild `list` from `catalog` filtered by the current search
    /// value.
    ///
    /// Score policy: empty query returns the full catalog in stable
    /// catalog order; non-empty query fuzzy-scores against a
    /// `"provider id name"` blob and sorts highest-score-first with
    /// a catalog-order tiebreak (so equally strong matches stay in
    /// the registry's intentional sequence). The matcher
    /// (`self.matcher`) is reused across calls — only its scratch
    /// buffers are cleared.
    fn rebuild_list(&mut self) {
        let query = self.search.value().trim().to_string();
        let mut scored: Vec<(usize, u32)> = Vec::new();
        if query.is_empty() {
            scored.extend((0..self.catalog.len()).map(|i| (i, 0u32)));
        } else {
            for (idx, info) in self.catalog.iter().enumerate() {
                let haystack = format!("{} {} {}", info.provider, info.id, info.name);
                if let Some(score) = self.matcher.score(&query, &haystack) {
                    scored.push((idx, u32::from(score)));
                }
            }
            scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        }

        let mut selected_index = 0;
        let items: Vec<SelectItem> = scored
            .iter()
            .enumerate()
            .map(|(row, (idx, _))| {
                let info = &self.catalog[*idx];
                let is_current = self
                    .current_key
                    .as_ref()
                    .is_some_and(|(p, id)| p == &info.provider && id == &info.id);
                if is_current {
                    selected_index = row;
                }
                let label = if is_current {
                    format!("{} (current)", info.name)
                } else {
                    info.name.clone()
                };
                // The description column carries the wire-level
                // identifier and provider tag so the user can
                // disambiguate same-name models across providers.
                let description = format!("{} · {}", info.provider, info.id);
                SelectItem::new(&format!("{}/{}", info.provider, info.id), &label)
                    .with_description(&description)
            })
            .collect();

        // SelectList isn't mutator-friendly for items / layout — the
        // documented path is to rebuild on change. New layout + theme
        // mirror the construction in `new()` so the visual presentation
        // stays consistent across rebuilds.
        let mut list = SelectList::new(
            items,
            MAX_VISIBLE_ROWS,
            self.theme.clone(),
            SelectListLayout::default(),
        );
        list.set_focused(true);
        list.set_selected_index(selected_index);
        self.list = list;
    }

    /// Commit the currently-highlighted list entry into the outcome
    /// slot. Looks the entry up in `catalog` by its `(provider, id)`
    /// key to recover the full [`ModelInfo`].
    fn commit_selection(&self) {
        let Some(item) = self.list.selected_item().cloned() else {
            return;
        };
        // `item.value` was constructed as "{provider}/{id}". Split
        // once on the first '/' so model ids containing slashes
        // (rare but possible) stay intact.
        let Some((provider, id)) = item.value.split_once('/') else {
            return;
        };
        let Some(info) = self
            .catalog
            .iter()
            .find(|m| m.provider == provider && m.id == id)
            .cloned()
        else {
            return;
        };
        *self.outcome.lock().expect("outcome mutex poisoned") =
            Some(ModelSelectorOutcome::Confirmed(info));
    }

    /// Record a cancellation in the outcome slot.
    fn commit_cancel(&self) {
        *self.outcome.lock().expect("outcome mutex poisoned") =
            Some(ModelSelectorOutcome::Cancelled);
    }
}

/// Minimal trait the host uses to identify the agent's current
/// model when constructing the selector. Implemented inline by the
/// host wrapper around `aj_models::Model` (which only exposes
/// `model_name`/`model_url`); the host knows the provider + id from
/// the registry lookup it did when constructing `Agent`, so it can
/// build the identity blob without us depending on the wire-level
/// `Model` trait here.
///
/// Kept narrow on purpose: the selector doesn't need any inference
/// surface, just the two identifiers that key a row.
pub trait ModelIdentity {
    fn provider(&self) -> &str;
    fn id(&self) -> &str;
}

/// Plain-struct implementation of [`ModelIdentity`] for callers that
/// want to materialize the identity from two strings without
/// implementing a trait. Useful for the host's startup path where
/// the identity is held alongside the `Arc<dyn Model>` and a
/// dedicated wrapper would be noise.
pub struct ModelIdentityRef<'a> {
    pub provider: &'a str,
    pub id: &'a str,
}

impl ModelIdentity for ModelIdentityRef<'_> {
    fn provider(&self) -> &str {
        self.provider
    }
    fn id(&self) -> &str {
        self.id
    }
}

impl Component for ModelSelectorComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Stack: title, search input, dim separator, list.
        let mut lines = Vec::with_capacity(MAX_VISIBLE_ROWS + 4);
        lines.push(style::dim(&self.title));
        lines.extend(self.search.render(width));
        lines.push(style::dim(&"─".repeat(width.min(60))));
        lines.extend(self.list.render(width));
        lines
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();

        // Esc cancels regardless of where focus appears to be (the
        // search input and the list both bind Esc to cancel under
        // `tui.select.cancel`; we intercept here so we fire exactly
        // one Cancelled outcome and don't rely on either component's
        // callbacks).
        if kb.matches(event, "tui.select.cancel") {
            self.commit_cancel();
            return true;
        }

        // Enter commits the highlighted list row. We deliberately
        // do NOT route Enter into `TextInput::handle_input` — its
        // `on_submit` callback path isn't wired (we own the outcome
        // slot directly) and routing it there would just swallow the
        // event without committing.
        if kb.matches(event, "tui.input.submit") {
            self.commit_selection();
            return true;
        }

        // Navigation keys belong to the list: up/down/page-up/page-
        // down move the highlight without disturbing the search
        // text. We route through `SelectList::handle_input` so its
        // wraparound / scroll-window logic stays the single source
        // of truth.
        if kb.matches(event, "tui.select.up")
            || kb.matches(event, "tui.select.down")
            || kb.matches(event, "tui.select.pageUp")
            || kb.matches(event, "tui.select.pageDown")
        {
            drop(kb);
            return self.list.handle_input(event);
        }

        // Everything else goes to the search box. Drop the
        // keybinding registry guard first so the rebuild below can
        // re-acquire it without contention.
        drop(kb);

        let before = self.search.value().to_string();
        let handled = self.search.handle_input(event);
        if handled && self.search.value() != before {
            self.rebuild_list();
        }
        handled
    }

    fn set_focused(&mut self, focused: bool) {
        self.search.set_focused(focused);
        self.list.set_focused(focused);
    }

    fn is_focused(&self) -> bool {
        self.search.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_models::registry::ModelCost;
    use aj_tui::components::select_list::SelectListTheme;
    use aj_tui::keys::{InputEvent, Key};

    use super::*;

    /// Identity theme for tests — passes every closure through
    /// verbatim so renders show the structural text rather than ANSI
    /// escape sequences.
    fn identity_theme() -> SelectListTheme {
        SelectListTheme {
            selected_prefix: Arc::new(|s| s.to_string()),
            selected_text: Arc::new(|s| s.to_string()),
            description: Arc::new(|s| s.to_string()),
            scroll_info: Arc::new(|s| s.to_string()),
            no_match: Arc::new(|s| s.to_string()),
        }
    }

    /// Helper: build a minimal [`ModelInfo`] for tests. Most fields
    /// are filler — the selector only cares about `id`, `name`, and
    /// `provider` for matching + display.
    fn make_model(provider: &str, id: &str, name: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: name.into(),
            api: format!("{provider}-messages"),
            provider: provider.into(),
            base_url: format!("https://api.{provider}.com"),
            reasoning: false,
            supports_xhigh: false,
            supports_adaptive_thinking: false,
            input: vec![],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 8_192,
            headers: None,
        }
    }

    fn sample_catalog() -> Vec<ModelInfo> {
        vec![
            make_model("anthropic", "claude-sonnet-4-20250514", "Claude Sonnet 4"),
            make_model("anthropic", "claude-opus-4-20250514", "Claude Opus 4"),
            make_model("openai", "gpt-5", "GPT-5"),
            make_model("openai", "gpt-5-mini", "GPT-5 Mini"),
        ]
    }

    fn enter_event() -> InputEvent {
        Key::enter()
    }
    fn escape_event() -> InputEvent {
        Key::escape()
    }
    fn down_event() -> InputEvent {
        Key::down()
    }

    #[test]
    fn highlights_current_model_on_open() {
        let catalog = sample_catalog();
        let current = ModelIdentityRef {
            provider: "anthropic",
            id: "claude-opus-4-20250514",
        };
        let mut sel = ModelSelectorComponent::new(identity_theme(), catalog, Some(&current), None);
        let body = sel.render(60).join("\n");
        // The opus row should be marked "(current)" since it's the
        // active model on open.
        assert!(body.contains("Claude Opus 4 (current)"), "got: {body}");
    }

    #[test]
    fn enter_commits_highlighted_entry() {
        let catalog = sample_catalog();
        let current = ModelIdentityRef {
            provider: "anthropic",
            id: "claude-sonnet-4-20250514",
        };
        let mut sel = ModelSelectorComponent::new(identity_theme(), catalog, Some(&current), None);
        let outcome = sel.outcome_handle();
        // Sonnet is pre-selected (it's the "current" model); Enter
        // should commit it.
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take().expect("outcome was set");
        match result {
            ModelSelectorOutcome::Confirmed(info) => {
                assert_eq!(info.provider, "anthropic");
                assert_eq!(info.id, "claude-sonnet-4-20250514");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn esc_emits_cancelled_outcome() {
        let catalog = sample_catalog();
        let mut sel = ModelSelectorComponent::new(identity_theme(), catalog, None, None);
        let outcome = sel.outcome_handle();
        sel.handle_input(&escape_event());
        let result = outcome.lock().unwrap().take().expect("outcome was set");
        assert!(
            matches!(result, ModelSelectorOutcome::Cancelled),
            "got {result:?}"
        );
    }

    #[test]
    fn down_arrow_moves_to_next_match_then_enter_confirms_it() {
        let catalog = sample_catalog();
        // Pre-select the first row by passing no `current`.
        let mut sel = ModelSelectorComponent::new(identity_theme(), catalog, None, None);
        let outcome = sel.outcome_handle();
        sel.handle_input(&down_event());
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take().expect("outcome was set");
        match result {
            ModelSelectorOutcome::Confirmed(info) => {
                // First row was anthropic/claude-sonnet-4; one down
                // lands on anthropic/claude-opus-4.
                assert_eq!(info.provider, "anthropic");
                assert_eq!(info.id, "claude-opus-4-20250514");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn typing_filters_the_list_and_enter_commits_top_match() {
        let catalog = sample_catalog();
        let mut sel = ModelSelectorComponent::new(identity_theme(), catalog, None, None);
        let outcome = sel.outcome_handle();
        // Type "opus" — only "Claude Opus 4" should remain.
        for c in "opus".chars() {
            sel.handle_input(&Key::char(c));
        }
        let body = sel.render(60).join("\n");
        assert!(body.contains("Claude Opus 4"), "got: {body}");
        assert!(!body.contains("GPT-5"), "got: {body}");
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take().expect("outcome was set");
        match result {
            ModelSelectorOutcome::Confirmed(info) => {
                assert_eq!(info.id, "claude-opus-4-20250514");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn initial_query_pre_fills_search_and_filters_immediately() {
        let catalog = sample_catalog();
        let mut sel =
            ModelSelectorComponent::new(identity_theme(), catalog, None, Some("gpt".to_string()));
        let body = sel.render(60).join("\n");
        // Only the two GPT-5 rows should appear.
        assert!(body.contains("GPT-5"), "got: {body}");
        assert!(!body.contains("Claude"), "got: {body}");
    }

    #[test]
    fn empty_catalog_renders_no_match_placeholder() {
        let mut sel = ModelSelectorComponent::new(identity_theme(), vec![], None, None);
        let body = sel.render(60).join("\n");
        // SelectList renders "No matching commands" when filtered
        // indices is empty; the selector inherits that placeholder
        // verbatim.
        assert!(body.contains("No matching"), "got: {body}");
    }

    #[test]
    fn unknown_current_pre_selects_first_entry() {
        let catalog = sample_catalog();
        // current = some provider/id that's not in the catalog.
        let current = ModelIdentityRef {
            provider: "anthropic",
            id: "not-a-real-model",
        };
        let mut sel = ModelSelectorComponent::new(identity_theme(), catalog, Some(&current), None);
        let outcome = sel.outcome_handle();
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take().expect("outcome was set");
        match result {
            ModelSelectorOutcome::Confirmed(info) => {
                // First catalog entry is anthropic/claude-sonnet-4-...
                assert_eq!(info.provider, "anthropic");
                assert_eq!(info.id, "claude-sonnet-4-20250514");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }
}
