//! Model-selector overlay (`/model`).
//!
//! Pairs a search box with a [`aj_tui::components::select_list::SelectList`]
//! that shows the matching entries from a snapshotted
//! [`aj_models::registry::ModelRegistry`]. The host opens this overlay
//! from `/model`; pressing Enter commits the highlighted entry, Esc
//! cancels.
//!
//! The shared [`FilterableSelect`] owns the search box and key routing.
//! Filtering can't be expressed as a [`SelectList`] filter mode here — the
//! model filter fuzzy-scores each entry's `provider`, `id`, and `name`
//! fields independently — so this component installs an `on_query` handler
//! that repopulates the list via [`SelectList::set_items`] on each
//! keystroke. The current model (the one already wired into the agent) is
//! pre-selected on open and tagged `(current)` so a no-op confirm is
//! obvious.

use std::sync::Arc;

use aj_models::registry::ModelInfo;
use aj_tui::component::Component;
use aj_tui::components::filterable_select::FilterableSelect;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use aj_tui::fuzzy::FuzzyMatcher;
use aj_tui::keys::InputEvent;

use crate::modes::interactive::components::outcome::OutcomeSlot;

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
pub type OutcomeHandle = OutcomeSlot<ModelSelectorOutcome>;

/// `(provider, id)` of the agent's current model, used to pre-select and
/// tag the active row.
type CurrentKey = Option<(String, String)>;

/// The overlay's top-level component: a [`FilterableSelect`] whose query
/// handler re-scores the catalog.
pub struct ModelSelectorComponent {
    inner: FilterableSelect,
    outcome: OutcomeHandle,
}

/// Initial visible-row budget for the result list, used before the
/// surrounding overlay reports its real inner height via
/// [`Component::set_available_height`]. The overlay then resizes the
/// list to fill its content area; this default just keeps the first
/// render sensible if a height is never pushed (e.g. in tests).
const DEFAULT_VISIBLE_ROWS: usize = 8;

impl ModelSelectorComponent {
    /// Build a fresh selector.
    ///
    /// `catalog` is the snapshotted model list to choose from (must
    /// already include any provider/override merging). `current` is
    /// the agent's active model — used to pre-select the matching
    /// row and mark it `(current)`. `initial_query`, when set,
    /// pre-fills the search box so the overlay opens already
    /// filtered; the host passes `None`, but the parameter is kept as a
    /// general capability. `theme` styles the underlying [`SelectList`].
    pub fn new(
        theme: SelectListTheme,
        catalog: Vec<ModelInfo>,
        current: Option<&dyn ModelIdentity>,
        initial_query: Option<String>,
    ) -> Self {
        let current_key: CurrentKey =
            current.map(|m| (m.provider().to_string(), m.id().to_string()));
        let catalog = Arc::new(catalog);
        let status_style = Arc::clone(&theme.description);

        let list = SelectList::new(
            Vec::new(),
            DEFAULT_VISIBLE_ROWS,
            theme,
            SelectListLayout::default(),
        );
        let mut inner = FilterableSelect::new("search: ", list, status_style);

        // Score policy: empty query returns the full catalog in stable
        // catalog order; non-empty query fuzzy-scores each entry's
        // `provider`, `id`, and `name` independently and sorts
        // highest-first. The matcher is reused across keystrokes (only
        // its scratch buffers are cleared), so it lives in the closure.
        let query_catalog = Arc::clone(&catalog);
        let query_current = current_key.clone();
        let mut matcher = FuzzyMatcher::new();
        inner.on_query = Some(Box::new(move |query, list| {
            let (items, selected) =
                score_items(&query_catalog, &query_current, &mut matcher, query);
            list.set_items(items);
            list.set_selected_index(selected);
        }));

        let outcome = OutcomeHandle::new();
        let confirm = outcome.clone();
        let confirm_catalog = Arc::clone(&catalog);
        inner.on_select = Some(Box::new(move |item| {
            if let Some(info) = lookup(&confirm_catalog, &item.value) {
                confirm.set(ModelSelectorOutcome::Confirmed(info));
            }
        }));
        let cancel = outcome.clone();
        inner.on_cancel = Some(Box::new(move || {
            cancel.set(ModelSelectorOutcome::Cancelled)
        }));

        // Populate the initial list (and pre-selection) through the same
        // scoring path the query handler uses, so the open state and every
        // subsequent keystroke agree.
        inner.set_query(&initial_query.unwrap_or_default());

        Self { inner, outcome }
    }

    /// Hand the host a clone of the outcome slot. After each input
    /// event the host calls `take()` on this handle; on `Some(_)` it
    /// hides the overlay and applies the result.
    pub fn outcome_handle(&self) -> OutcomeHandle {
        self.outcome.clone()
    }
}

/// Score `catalog` against `query` and return the row items plus the index
/// to pre-select (the current model's row, else `0`).
///
/// Scoring each field separately keeps a query like `gpt-5.5` from matching
/// a `gpt-5.1` entry by spanning its id and name (see
/// [`FuzzyMatcher::score_fields`]). An empty query returns the full catalog
/// in registry order.
fn score_items(
    catalog: &[ModelInfo],
    current_key: &CurrentKey,
    matcher: &mut FuzzyMatcher,
    query: &str,
) -> (Vec<SelectItem>, usize) {
    let query = query.trim();
    let mut scored: Vec<(usize, u32)> = Vec::new();
    if query.is_empty() {
        scored.extend((0..catalog.len()).map(|i| (i, 0u32)));
    } else {
        for (idx, info) in catalog.iter().enumerate() {
            let fields = [info.provider.as_str(), info.id.as_str(), info.name.as_str()];
            if let Some(score) = matcher.score_fields(query, &fields) {
                scored.push((idx, score));
            }
        }
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    }

    let mut selected_index = 0;
    let items: Vec<SelectItem> = scored
        .iter()
        .enumerate()
        .map(|(row, (idx, _))| {
            let info = &catalog[*idx];
            let is_current = current_key
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
            // The description column carries the wire-level identifier and
            // provider tag so the user can disambiguate same-name models
            // across providers.
            let description = format!("{} · {}", info.provider, info.id);
            SelectItem::new(&format!("{}/{}", info.provider, info.id), &label)
                .with_description(&description)
        })
        .collect();

    (items, selected_index)
}

/// Recover the full [`ModelInfo`] for a row `value` of the form
/// `"{provider}/{id}"`. Splits on the first `/` so ids containing slashes
/// (rare but possible) stay intact.
fn lookup(catalog: &[ModelInfo], value: &str) -> Option<ModelInfo> {
    let (provider, id) = value.split_once('/')?;
    catalog
        .iter()
        .find(|m| m.provider == provider && m.id == id)
        .cloned()
}

/// Minimal trait the host uses to identify the agent's current
/// model when constructing the selector. Implemented inline by the
/// host's wrapper around the agent's current [`ModelInfo`]: the
/// host knows the provider + id from the registry lookup it did
/// when constructing the `Agent`, so it can build the identity
/// blob without the selector depending on the wire layer.
///
/// Kept narrow on purpose: the selector doesn't need any inference
/// surface, just the two identifiers that key a row.
///
/// [`ModelInfo`]: aj_models::registry::ModelInfo
pub trait ModelIdentity {
    fn provider(&self) -> &str;
    fn id(&self) -> &str;
}

/// Plain-struct implementation of [`ModelIdentity`] for callers that
/// want to materialize the identity from two strings without
/// implementing a trait. Useful for the host's startup path where
/// the identity is held alongside the agent's
/// [`ModelInfo`](aj_models::registry::ModelInfo) and a dedicated
/// wrapper would be noise.
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

    fn render(&mut self, width: usize) -> Vec<aj_tui::Line> {
        self.inner.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        self.inner.handle_input(event)
    }

    fn set_focused(&mut self, focused: bool) {
        self.inner.set_focused(focused);
    }

    fn set_available_height(&mut self, rows: usize) {
        self.inner.set_available_height(rows);
    }

    fn is_focused(&self) -> bool {
        self.inner.is_focused()
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
            prefix: Arc::new(|s| s.to_string()),
            shortcut: Arc::new(|s| s.to_string()),
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
            supports_adaptive_thinking: false,
            supports_verbosity: false,
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
        let body = sel
            .render(60)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
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
        let result = outcome.take().expect("outcome was set");
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
        let result = outcome.take().expect("outcome was set");
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
        let result = outcome.take().expect("outcome was set");
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
        let body = sel
            .render(60)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("Claude Opus 4"), "got: {body}");
        assert!(!body.contains("GPT-5"), "got: {body}");
        sel.handle_input(&enter_event());
        let result = outcome.take().expect("outcome was set");
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
        let body = sel
            .render(60)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // Only the two GPT-5 rows should appear.
        assert!(body.contains("GPT-5"), "got: {body}");
        assert!(!body.contains("Claude"), "got: {body}");
    }

    #[test]
    fn empty_catalog_renders_no_match_placeholder() {
        let mut sel = ModelSelectorComponent::new(identity_theme(), vec![], None, None);
        let body = sel
            .render(60)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // SelectList renders "No matching commands" when filtered
        // indices is empty; the selector inherits that placeholder
        // verbatim.
        assert!(body.contains("No matching"), "got: {body}");
    }

    #[test]
    fn version_query_excludes_other_minor_versions() {
        // Reproduces the reported bug: filtering "gpt-5.5" must not
        // surface "gpt-5.1" / "gpt-5.2" rows. With a concatenated
        // "id name" haystack the second `5` of the query could be
        // borrowed from the repeated version in the name; scoring each
        // field independently prevents that.
        let catalog = vec![
            make_model("openai", "gpt-5.5", "GPT-5.5"),
            make_model("openai", "gpt-5.5-pro", "GPT-5.5 Pro"),
            make_model("openai", "gpt-5.1", "GPT-5.1"),
            make_model("openai", "gpt-5.2", "GPT-5.2"),
            make_model("openai", "gpt-5.4", "GPT-5.4"),
        ];
        let mut sel = ModelSelectorComponent::new(
            identity_theme(),
            catalog,
            None,
            Some("gpt-5.5".to_string()),
        );
        let body = sel
            .render(60)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("GPT-5.5"), "got: {body}");
        assert!(body.contains("GPT-5.5 Pro"), "got: {body}");
        assert!(!body.contains("GPT-5.1"), "got: {body}");
        assert!(!body.contains("GPT-5.2"), "got: {body}");
        assert!(!body.contains("GPT-5.4"), "got: {body}");
    }

    #[test]
    fn set_available_height_grows_the_visible_list() {
        // Catalog larger than the default visible budget so the window
        // is what bounds the row count.
        let catalog: Vec<ModelInfo> = (0..20)
            .map(|i| make_model("openai", &format!("gpt-{i}"), &format!("GPT {i}")))
            .collect();
        let mut sel = ModelSelectorComponent::new(identity_theme(), catalog, None, None);

        let default_rows = sel.render(60).len();
        // Report a tall overlay: the list should fill it (minus the
        // search box, blank separator, and scroll-info chrome).
        sel.set_available_height(20);
        let tall_rows = sel.render(60).len();
        assert!(
            tall_rows > default_rows,
            "expected the list to grow with available height: {default_rows} -> {tall_rows}"
        );
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
        let result = outcome.take().expect("outcome was set");
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
