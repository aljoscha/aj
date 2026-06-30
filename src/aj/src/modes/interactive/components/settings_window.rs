//! Settings window overlay (`/settings`).
//!
//! Wraps an [`aj_tui::components::settings_list::SettingsList`] whose
//! rows are generated from [`Config::OPTIONS`] — the same schema table
//! the config parser walks — so a new config option shows up here (or
//! fails the drift test below) instead of being silently forgotten.
//!
//! Unlike the confirm-and-close selectors, the window stays open
//! across changes: every change the user makes is pushed onto a
//! shared queue ([`ChangesHandle`]) that the host drains after each
//! input event, applying and persisting each entry. `Esc` closes the
//! window via the usual outcome slot. Because the list updates its
//! displayed value optimistically, the host can push a display fix
//! back through [`CorrectionsHandle`] when an apply fails (e.g. a
//! speed change whose bundle rebuild errors), keeping the visible
//! value honest.
//!
//! Row ids are the `config.toml` keys, with one exception: the
//! `model_api` + `model_name` pair is folded into a single
//! [`MODEL_SETTING_ID`] row whose submenu embeds the `/model` picker
//! and commits a `provider/id` string.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use aj_conf::{Config, ConfigOption, ValueKind};
use aj_models::registry::ModelInfo;
use aj_tui::component::Component;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use aj_tui::components::settings_list::{
    SettingItem, SettingsList, SettingsListOptions, SettingsListTheme, SubmenuDoneCallback,
    SubmenuFactory,
};
use aj_tui::components::text_input::TextInput;
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;

use crate::config::commands::THINKING_LEVELS;
use crate::modes::interactive::components::model_selector::{
    ModelIdentity, ModelIdentityRef, ModelSelectorComponent, ModelSelectorOutcome,
};

/// Row id of the synthetic model row that folds `model_api` +
/// `model_name` into one picker-backed entry. Its change value is a
/// `provider/id` string.
pub const MODEL_SETTING_ID: &str = "model";

/// Cycle value representing "leave the option unset" for options
/// whose absence has its own meaning (today: `thinking_display`,
/// where unset keeps the provider's stock behavior). The host maps
/// it back to `None` / key removal on persist.
pub const UNSET_VALUE: &str = "default";

/// Outcome of a single window session. The window only ever closes;
/// individual changes flow through [`ChangesHandle`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsWindowOutcome {
    Closed,
}

/// Which kind of submenu is currently active inside the window. The
/// host uses this to render a matching key-hint on the overlay border
/// (the keys mean different things while a submenu is open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSubmenu {
    /// No submenu open; the main settings list has the input.
    None,
    /// A pick-one list (thinking level, theme, model picker).
    Picker,
    /// The one-line free-form text editor (`model_url`).
    TextEdit,
    /// The nested enabled/disabled toggle list (tools, skills).
    Toggles,
}

/// Cheap-to-clone handle pointing at the same outcome slot the
/// overlay component writes into.
pub type OutcomeHandle = Arc<Mutex<Option<SettingsWindowOutcome>>>;

/// Queue of `(row id, new value)` changes, in the order the user
/// made them. The host drains it after every input event.
pub type ChangesHandle = Arc<Mutex<Vec<(String, String)>>>;

/// Reverse channel: `(row id, display value)` fixes the host pushes
/// when an apply fails, drained by the component at render time.
pub type CorrectionsHandle = Arc<Mutex<Vec<(String, String)>>>;

/// Queue of `(row id, inherited value)` clears from the project
/// settings window: the user asked to drop a project override, and the
/// inherited value is what the live effect should revert to. Empty for
/// the user settings window.
pub type ClearsHandle = Arc<Mutex<Vec<(String, String)>>>;

/// Live values the window opens with. Strings use the same canonical
/// vocabulary the host's apply path parses (`thinking_level_name`,
/// `speed_name`, theme names from the loader catalog).
pub struct SettingsCurrentValues {
    /// `(provider, id)` of the main agent's next-turn model.
    pub model_key: (String, String),
    pub model_url: Option<String>,
    /// Canonical thinking level name (`"off"` … `"max"`).
    pub thinking: String,
    /// Canonical display mode name, `None` when unset (provider
    /// default).
    pub thinking_display: Option<String>,
    /// `"standard"` or `"fast"`.
    pub speed: String,
    /// Canonical verbosity name, `None` when unset (server default).
    pub verbosity: Option<String>,
    /// Configured theme name (the `config.toml` vocabulary, not a
    /// loaded theme's display label).
    pub theme: String,
    pub disabled_tools: Vec<String>,
    pub disabled_skills: Vec<String>,
    pub hide_thinking_block: bool,
    pub image_auto_resize: bool,
    pub image_show_in_terminal: bool,
    pub image_block: bool,
    pub syntax_highlighting: bool,
    pub auto_compact: bool,
    /// Compaction threshold fraction, formatted for display/editing
    /// (e.g. `"0.85"`).
    pub compact_threshold: String,
    /// Recent-tail token budget kept after compaction, formatted for
    /// display/editing (e.g. `"20000"`).
    pub compact_keep_recent: String,
}

/// The overlay's top-level component. See the module docs for the
/// changes/corrections flow.
///
/// In project mode (the per-project settings window) two extra
/// behaviors apply: the list renders layered (rows the project sets
/// carry an override marker, the rest show the inherited user value),
/// and the clear chord drops a project override via [`Self::clears`].
pub struct SettingsWindowComponent {
    inner: SettingsList,
    outcome: OutcomeHandle,
    changes: ChangesHandle,
    corrections: CorrectionsHandle,
    clears: ClearsHandle,
    /// Whether this is the per-project window (enables the clear chord
    /// and inherited-row rendering).
    project_mode: bool,
    /// Per-row value a clear reverts to (the inherited user value),
    /// keyed by row id. Empty in the user window.
    inherited: HashMap<String, String>,
}

impl SettingsWindowComponent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        settings_theme: SettingsListTheme,
        select_theme: SelectListTheme,
        model_catalog: Vec<ModelInfo>,
        theme_names: Vec<String>,
        tool_names: Vec<String>,
        skill_names: Vec<String>,
        current: SettingsCurrentValues,
    ) -> Self {
        let items = build_items(
            &settings_theme,
            &select_theme,
            model_catalog,
            theme_names,
            tool_names,
            skill_names,
            &current,
        );
        Self::build(items, settings_theme, false, HashMap::new())
    }

    /// Build the per-project settings window.
    ///
    /// `current` is the effective (project-over-user) snapshot, so a
    /// project-set row shows the project's value and an unset row shows
    /// the inherited user value. `inherited` is the user-only snapshot,
    /// the value a clear reverts to. `set_keys` are the option names the
    /// project layer currently sets, used to mark the unset rows as
    /// inherited (muted) and to gate the clear chord.
    #[allow(clippy::too_many_arguments)]
    pub fn new_project(
        settings_theme: SettingsListTheme,
        select_theme: SelectListTheme,
        model_catalog: Vec<ModelInfo>,
        theme_names: Vec<String>,
        tool_names: Vec<String>,
        skill_names: Vec<String>,
        current: SettingsCurrentValues,
        inherited: SettingsCurrentValues,
        set_keys: BTreeSet<String>,
    ) -> Self {
        let mut items = build_items(
            &settings_theme,
            &select_theme,
            model_catalog.clone(),
            theme_names.clone(),
            tool_names.clone(),
            skill_names.clone(),
            &current,
        );
        // Reuse the same row mapping to derive each row's inherited
        // value, so the clear path commits exactly what the row would
        // display once reverted.
        let inherited_map: HashMap<String, String> = build_items(
            &settings_theme,
            &select_theme,
            model_catalog,
            theme_names,
            tool_names,
            skill_names,
            &inherited,
        )
        .into_iter()
        .map(|item| (item.id, item.current_value))
        .collect();
        // Rows the project doesn't set render as inherited.
        for item in &mut items {
            item.inherited = !row_is_project_set(&item.id, &set_keys);
        }
        Self::build(items, settings_theme, true, inherited_map)
    }

    /// Shared construction: wire the list's change/cancel callbacks to
    /// the component's queues and stash the project-mode state.
    fn build(
        items: Vec<SettingItem>,
        settings_theme: SettingsListTheme,
        project_mode: bool,
        inherited: HashMap<String, String>,
    ) -> Self {
        let outcome: OutcomeHandle = Arc::new(Mutex::new(None));
        let changes: ChangesHandle = Arc::new(Mutex::new(Vec::new()));
        let corrections: CorrectionsHandle = Arc::new(Mutex::new(Vec::new()));
        let clears: ClearsHandle = Arc::new(Mutex::new(Vec::new()));

        let changes_for_cb = Arc::clone(&changes);
        let outcome_for_cb = Arc::clone(&outcome);
        let mut inner = SettingsList::new(
            items,
            // Pre-push default; the surrounding overlay window pushes
            // its real budget via `set_available_height`.
            Config::OPTIONS.len(),
            settings_theme,
            move |id: &str, value: &str| {
                changes_for_cb
                    .lock()
                    .expect("changes mutex poisoned")
                    .push((id.to_string(), value.to_string()));
            },
            move || {
                *outcome_for_cb.lock().expect("outcome mutex poisoned") =
                    Some(SettingsWindowOutcome::Closed);
            },
            SettingsListOptions {
                enable_search: true,
            },
        );
        // The project window renders layered: set rows get the override
        // marker, inherited rows render plain.
        inner.set_layered(project_mode);

        Self {
            inner,
            outcome,
            changes,
            corrections,
            clears,
            project_mode,
            inherited,
        }
    }

    /// Hand the host a clone of the outcome slot, polled after each
    /// input event; `Some(Closed)` means hide the overlay.
    pub fn outcome_handle(&self) -> OutcomeHandle {
        Arc::clone(&self.outcome)
    }

    /// Hand the host a clone of the changes queue to drain after
    /// each input event.
    pub fn changes_handle(&self) -> ChangesHandle {
        Arc::clone(&self.changes)
    }

    /// Hand the host a clone of the corrections queue it can push
    /// display fixes into.
    pub fn corrections_handle(&self) -> CorrectionsHandle {
        Arc::clone(&self.corrections)
    }

    /// Hand the host a clone of the clears queue to drain after each
    /// input event. Only ever populated in project mode.
    pub fn clears_handle(&self) -> ClearsHandle {
        Arc::clone(&self.clears)
    }

    /// Which submenu kind is currently open, for border key-hints.
    pub fn active_submenu(&self) -> SettingsSubmenu {
        match self.inner.active_submenu() {
            None => SettingsSubmenu::None,
            Some(c) if c.as_any().is::<TextEditSubmenu>() => SettingsSubmenu::TextEdit,
            Some(c) if c.as_any().is::<SettingsList>() => SettingsSubmenu::Toggles,
            // Everything else picks one value: a plain `SelectList`
            // or the embedded model picker.
            Some(_) => SettingsSubmenu::Picker,
        }
    }
}

impl Component for SettingsWindowComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<aj_tui::Line> {
        // Apply queued display corrections before painting so a
        // failed apply never leaves a stale value on screen longer
        // than one host round-trip.
        let pending: Vec<(String, String)> =
            std::mem::take(&mut *self.corrections.lock().expect("corrections mutex poisoned"));
        for (id, value) in pending {
            self.inner.update_value(&id, value);
        }
        self.inner.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        // In project mode, the clear chord on the main list drops the
        // selected row's project override. Intercepted before the list
        // sees it (so it never reaches the row search box), and only
        // when no submenu is open and the row is actually project-set.
        if self.project_mode && !self.inner.has_active_submenu() {
            let is_clear = {
                let kb = keybindings::get();
                kb.matches(event, crate::config::keybindings::ACTION_SETTINGS_CLEAR)
            };
            if is_clear {
                if let Some(id) = self.inner.selected_id().map(str::to_string) {
                    if !self.inner.is_inherited(&id) {
                        let inherited_value = self.inherited.get(&id).cloned().unwrap_or_default();
                        self.clears
                            .lock()
                            .expect("clears mutex poisoned")
                            .push((id.clone(), inherited_value.clone()));
                        // Optimistically revert the row to the inherited
                        // value, shown muted, until the host confirms.
                        self.inner.update_value(&id, inherited_value);
                        self.inner.set_inherited(&id, true);
                    }
                }
                return true;
            }
        }
        self.inner.handle_input(event)
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
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

/// Build one row per [`Config::OPTIONS`] entry (with `model_api` +
/// `model_name` folded into the [`MODEL_SETTING_ID`] row). An entry
/// without a mapping is skipped at runtime with a warning; the
/// `every_config_option_has_a_row` test turns that drift into a CI
/// failure.
fn build_items(
    settings_theme: &SettingsListTheme,
    select_theme: &SelectListTheme,
    model_catalog: Vec<ModelInfo>,
    theme_names: Vec<String>,
    tool_names: Vec<String>,
    skill_names: Vec<String>,
    current: &SettingsCurrentValues,
) -> Vec<SettingItem> {
    let mut items = Vec::with_capacity(Config::OPTIONS.len());
    for option in Config::OPTIONS {
        match option.name {
            "model_api" => {
                let mut item = SettingItem::with_submenu(
                    MODEL_SETTING_ID,
                    MODEL_SETTING_ID,
                    format!("{}/{}", current.model_key.0, current.model_key.1),
                    model_submenu_factory(select_theme.clone(), model_catalog.clone()),
                );
                item.description = Some(
                    "Model the main agent uses, applied from the next turn. Persisted as \
                     model_api + model_name."
                        .to_string(),
                );
                items.push(item);
            }
            // Folded into the model row above.
            "model_name" => {}
            "model_url" => {
                let mut item = SettingItem::with_submenu(
                    option.name,
                    option.name,
                    current.model_url.clone().unwrap_or_default(),
                    text_submenu_factory(),
                );
                // Empty means "use the provider's default endpoint".
                item.empty_placeholder = Some("(default)".to_string());
                item.description = Some(describe(
                    option,
                    "Takes effect on restart. Submit an empty value to unset.",
                ));
                items.push(item);
            }
            "thinking" => {
                let mut item = SettingItem::with_submenu(
                    option.name,
                    option.name,
                    current.thinking.clone(),
                    thinking_submenu_factory(select_theme.clone()),
                );
                item.description = Some(option.description.to_string());
                items.push(item);
            }
            "thinking_display" => {
                let mut values = vec![UNSET_VALUE.to_string()];
                values.extend(enum_values(option));
                let mut item = SettingItem::cycleable(
                    option.name,
                    option.name,
                    current
                        .thinking_display
                        .clone()
                        .unwrap_or_else(|| UNSET_VALUE.to_string()),
                    values,
                );
                item.description = Some(describe(
                    option,
                    "\"default\" keeps the provider's stock behavior. Takes effect next turn.",
                ));
                items.push(item);
            }
            "speed" => {
                let mut item = SettingItem::cycleable(
                    option.name,
                    option.name,
                    current.speed.clone(),
                    enum_values(option),
                );
                item.description = Some(describe(option, "Takes effect next turn."));
                items.push(item);
            }
            "verbosity" => {
                let mut values = vec![UNSET_VALUE.to_string()];
                values.extend(enum_values(option));
                let mut item = SettingItem::cycleable(
                    option.name,
                    option.name,
                    current
                        .verbosity
                        .clone()
                        .unwrap_or_else(|| UNSET_VALUE.to_string()),
                    values,
                );
                item.description = Some(describe(
                    option,
                    "\"default\" leaves the server default. Takes effect next turn.",
                ));
                items.push(item);
            }
            "theme" => {
                let mut item = SettingItem::with_submenu(
                    option.name,
                    option.name,
                    current.theme.clone(),
                    theme_submenu_factory(select_theme.clone(), theme_names.clone()),
                );
                item.description = Some(option.description.to_string());
                items.push(item);
            }
            "disabled_tools" => {
                let initial: BTreeSet<String> = current.disabled_tools.iter().cloned().collect();
                let mut item = SettingItem::with_submenu(
                    option.name,
                    option.name,
                    join_names(&initial),
                    name_toggle_submenu_factory(settings_theme.clone(), tool_names.clone()),
                );
                item.description = Some(describe(
                    option,
                    "Toggles apply when the picker closes; takes effect for new sessions.",
                ));
                items.push(item);
            }
            "disabled_skills" => {
                let initial: BTreeSet<String> = current.disabled_skills.iter().cloned().collect();
                let mut item = SettingItem::with_submenu(
                    option.name,
                    option.name,
                    join_names(&initial),
                    name_toggle_submenu_factory(settings_theme.clone(), skill_names.clone()),
                );
                item.description = Some(describe(
                    option,
                    "Toggles apply when the picker closes; takes effect for new sessions.",
                ));
                items.push(item);
            }
            "hide_thinking_block" => {
                items.push(bool_item(option, current.hide_thinking_block, None));
            }
            "image_auto_resize" => {
                items.push(bool_item(
                    option,
                    current.image_auto_resize,
                    Some("Takes effect for new sessions."),
                ));
            }
            "image_show_in_terminal" => {
                items.push(bool_item(option, current.image_show_in_terminal, None));
            }
            "image_block" => {
                items.push(bool_item(
                    option,
                    current.image_block,
                    Some("Takes effect for new sessions."),
                ));
            }
            "syntax_highlighting" => {
                items.push(bool_item(
                    option,
                    current.syntax_highlighting,
                    Some("Takes effect for new sessions."),
                ));
            }
            "auto_compact" => {
                items.push(bool_item(option, current.auto_compact, None));
            }
            "compact_threshold" => {
                let mut item = SettingItem::with_submenu(
                    option.name,
                    option.name,
                    current.compact_threshold.clone(),
                    text_submenu_factory(),
                );
                item.description = Some(describe(option, "A fraction between 0.0 and 1.0."));
                items.push(item);
            }
            "compact_keep_recent" => {
                let mut item = SettingItem::with_submenu(
                    option.name,
                    option.name,
                    current.compact_keep_recent.clone(),
                    text_submenu_factory(),
                );
                item.description = Some(describe(option, "A positive number of tokens."));
                items.push(item);
            }
            other => {
                tracing::warn!(option = other, "config option has no settings-window row");
            }
        }
    }
    items
}

/// Cycleable true/false row for a [`ValueKind::Bool`] option.
fn bool_item(option: &ConfigOption, value: bool, note: Option<&str>) -> SettingItem {
    let mut item = SettingItem::cycleable(
        option.name,
        option.name,
        value.to_string(),
        vec!["true".to_string(), "false".to_string()],
    );
    item.description = Some(match note {
        Some(n) => describe(option, n),
        None => option.description.to_string(),
    });
    item
}

/// Schema description plus a settings-window-specific note.
fn describe(option: &ConfigOption, note: &str) -> String {
    format!("{} {}", option.description, note)
}

/// Variant list of a [`ValueKind::Enum`] option, in schema order.
fn enum_values(option: &ConfigOption) -> Vec<String> {
    match option.kind {
        ValueKind::Enum(variants) => variants.iter().map(|v| v.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Whether the project layer sets the option(s) a settings row stands
/// for. The model row folds `model_api` + `model_name`, so it's set
/// when either is.
fn row_is_project_set(row_id: &str, set_keys: &BTreeSet<String>) -> bool {
    if row_id == MODEL_SETTING_ID {
        set_keys.contains("model_api") || set_keys.contains("model_name")
    } else {
        set_keys.contains(row_id)
    }
}

/// Wire a [`SelectList`] as a submenu: `Enter` commits the
/// highlighted value through `done(Some(..))`, `Esc` cancels with
/// `done(None)`. The `done` callback is shared by both paths via an
/// `Rc<RefCell<Option<..>>>`; whichever fires first consumes it.
fn select_submenu(
    items: Vec<SelectItem>,
    theme: SelectListTheme,
    current: &str,
    done: SubmenuDoneCallback,
) -> Box<dyn Component> {
    let slot: Rc<RefCell<Option<SubmenuDoneCallback>>> = Rc::new(RefCell::new(Some(done)));

    let max_visible = items.len().max(1);
    let mut list = SelectList::new(items, max_visible, theme, SelectListLayout::default());
    if let Some(pos) = list.items().iter().position(|i| i.value == current) {
        list.set_selected_index(pos);
    }
    list.set_focused(true);

    let slot_for_select = Rc::clone(&slot);
    list.on_select = Some(Box::new(move |item: &SelectItem| {
        if let Some(done) = slot_for_select.borrow_mut().take() {
            done(Some(item.value.clone()));
        }
    }));
    let slot_for_cancel = Rc::clone(&slot);
    list.on_cancel = Some(Box::new(move || {
        if let Some(done) = slot_for_cancel.borrow_mut().take() {
            done(None);
        }
    }));

    Box::new(list)
}

/// Submenu factory for the `thinking` row: the level catalog with
/// descriptions, current level pre-selected.
fn thinking_submenu_factory(theme: SelectListTheme) -> SubmenuFactory {
    Box::new(move |current: &str, done: SubmenuDoneCallback| {
        let items: Vec<SelectItem> = THINKING_LEVELS
            .iter()
            .map(|l| {
                let label = if l.name == current {
                    format!("{} (current)", l.name)
                } else {
                    l.name.to_string()
                };
                SelectItem::new(l.name, &label).with_description(l.description)
            })
            .collect();
        select_submenu(items, theme.clone(), current, done)
    })
}

/// Submenu factory for the `theme` row: every loader-known theme
/// name, current one pre-selected.
fn theme_submenu_factory(theme: SelectListTheme, names: Vec<String>) -> SubmenuFactory {
    Box::new(move |current: &str, done: SubmenuDoneCallback| {
        let items: Vec<SelectItem> = names
            .iter()
            .map(|name| {
                let label = if name == current {
                    format!("{name} (current)")
                } else {
                    name.clone()
                };
                SelectItem::new(name, &label)
            })
            .collect();
        select_submenu(items, theme.clone(), current, done)
    })
}

/// Submenu factory for the model row: embeds the `/model` picker
/// wholesale via [`ModelPickerSubmenu`].
fn model_submenu_factory(theme: SelectListTheme, catalog: Vec<ModelInfo>) -> SubmenuFactory {
    Box::new(move |current: &str, done: SubmenuDoneCallback| {
        let identity = current
            .split_once('/')
            .map(|(provider, id)| ModelIdentityRef { provider, id });
        // Coerce to the trait object via a typed binding; the
        // workspace lints deny `as` casts.
        let identity_ref: Option<&dyn ModelIdentity> = match identity.as_ref() {
            Some(r) => Some(r),
            None => None,
        };
        let inner = ModelSelectorComponent::new(theme.clone(), catalog.clone(), identity_ref, None);
        let outcome = inner.outcome_handle();
        Box::new(ModelPickerSubmenu {
            inner,
            outcome,
            done: Some(done),
        })
    })
}

/// Adapter that lets the existing model picker run as a submenu: it
/// forwards every call to the picker and translates the picker's
/// outcome slot into the submenu `done` callback after each input.
struct ModelPickerSubmenu {
    inner: ModelSelectorComponent,
    outcome: crate::modes::interactive::components::model_selector::OutcomeHandle,
    done: Option<SubmenuDoneCallback>,
}

impl Component for ModelPickerSubmenu {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<aj_tui::Line> {
        self.inner.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let handled = self.inner.handle_input(event);
        let outcome = self.outcome.take();
        if let Some(outcome) = outcome
            && let Some(done) = self.done.take()
        {
            match outcome {
                ModelSelectorOutcome::Confirmed(info) => {
                    done(Some(format!("{}/{}", info.provider, info.id)));
                }
                ModelSelectorOutcome::Cancelled => done(None),
            }
        }
        handled
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

/// Submenu factory for free-form string options (`model_url`): a
/// one-line editor pre-filled with the current value.
fn text_submenu_factory() -> SubmenuFactory {
    Box::new(move |current: &str, done: SubmenuDoneCallback| {
        let mut input = TextInput::new("> ");
        input.set_value(current);
        input.move_to_end();
        input.set_focused(true);
        Box::new(TextEditSubmenu {
            input,
            done: Some(done),
        })
    })
}

/// One-line text editor submenu: `Enter` commits the trimmed value
/// (empty meaning "unset"), `Esc` cancels.
struct TextEditSubmenu {
    input: TextInput,
    done: Option<SubmenuDoneCallback>,
}

impl Component for TextEditSubmenu {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<aj_tui::Line> {
        self.input.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();
        if kb.matches(event, "tui.select.cancel") {
            if let Some(done) = self.done.take() {
                done(None);
            }
            return true;
        }
        if kb.matches(event, "tui.input.submit") {
            if let Some(done) = self.done.take() {
                done(Some(self.input.value().trim().to_string()));
            }
            return true;
        }
        self.input.handle_input(event)
    }

    fn set_focused(&mut self, focused: bool) {
        self.input.set_focused(focused);
    }

    fn is_focused(&self) -> bool {
        self.input.is_focused()
    }
}

/// Submenu factory for the name-set toggle rows (`disabled_tools`,
/// `disabled_skills`): a nested [`SettingsList`] with one
/// enabled/disabled toggle per name. Closing the picker (`Esc`) commits
/// the aggregate when it changed — there is no cancel, since each
/// toggle is itself an edit.
///
/// Disabled names that aren't in `names` (e.g. stale entries from an
/// older binary, or a skill directory that was deleted) have no row, so
/// they can't be re-enabled here — but they are preserved verbatim in
/// the committed list rather than silently dropped.
fn name_toggle_submenu_factory(theme: SettingsListTheme, names: Vec<String>) -> SubmenuFactory {
    Box::new(move |current: &str, done: SubmenuDoneCallback| {
        let disabled: BTreeSet<String> = split_names(current);
        let initial = join_names(&disabled);
        let shared: Rc<RefCell<BTreeSet<String>>> = Rc::new(RefCell::new(disabled));

        let items: Vec<SettingItem> = names
            .iter()
            .map(|name| {
                let value = if shared.borrow().contains(name) {
                    "disabled"
                } else {
                    "enabled"
                };
                SettingItem::cycleable(
                    name.clone(),
                    name.clone(),
                    value,
                    vec!["enabled".to_string(), "disabled".to_string()],
                )
            })
            .collect();

        let done_slot: Rc<RefCell<Option<SubmenuDoneCallback>>> = Rc::new(RefCell::new(Some(done)));
        let shared_for_change = Rc::clone(&shared);
        let mut list = SettingsList::new(
            items,
            names.len().max(1),
            theme.clone(),
            move |id: &str, value: &str| {
                let mut set = shared_for_change.borrow_mut();
                if value == "disabled" {
                    set.insert(id.to_string());
                } else {
                    set.remove(id);
                }
            },
            move || {
                if let Some(done) = done_slot.borrow_mut().take() {
                    let joined = join_names(&shared.borrow());
                    // No-op close stays silent: committing an
                    // unchanged list would still fire the parent's
                    // on-change and produce a pointless notice.
                    done(if joined == initial {
                        None
                    } else {
                        Some(joined)
                    });
                }
            },
            SettingsListOptions::default(),
        );
        list.set_focused(true);
        Box::new(list)
    })
}

/// Parse a `", "`-joined name list back into a set. Inverse of
/// [`join_names`]; tolerant of stray whitespace and empty segments.
fn split_names(joined: &str) -> BTreeSet<String> {
    joined
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Canonical display/commit form of a disabled-names set: sorted,
/// `", "`-joined, empty string for "none disabled".
fn join_names(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_models::registry::ModelCost;
    use aj_tui::keys::{InputEvent, Key};

    use super::*;

    fn identity_settings_theme() -> SettingsListTheme {
        SettingsListTheme {
            label: Arc::new(|s, _| s.to_string()),
            value: Arc::new(|s, _| s.to_string()),
            description: Arc::new(|s| s.to_string()),
            hint: Arc::new(|s| s.to_string()),
            marker: Arc::new(|s| s.to_string()),
            cursor: "→ ".to_string(),
        }
    }

    fn identity_select_theme() -> SelectListTheme {
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

    fn current_values() -> SettingsCurrentValues {
        SettingsCurrentValues {
            model_key: ("anthropic".to_string(), "claude-sonnet-4".to_string()),
            model_url: None,
            thinking: "medium".to_string(),
            thinking_display: None,
            speed: "standard".to_string(),
            verbosity: None,
            theme: "dark".to_string(),
            disabled_tools: vec![],
            disabled_skills: vec![],
            hide_thinking_block: false,
            image_auto_resize: true,
            image_show_in_terminal: true,
            image_block: false,
            syntax_highlighting: false,
            auto_compact: true,
            compact_threshold: "0.85".to_string(),
            compact_keep_recent: "20000".to_string(),
        }
    }

    fn test_component() -> SettingsWindowComponent {
        SettingsWindowComponent::new(
            identity_settings_theme(),
            identity_select_theme(),
            vec![
                make_model("anthropic", "claude-sonnet-4", "Claude Sonnet 4"),
                make_model("openai", "gpt-5", "GPT-5"),
            ],
            vec!["dark".to_string(), "light".to_string()],
            vec!["bash".to_string(), "read_file".to_string()],
            vec!["tmux-subagents".to_string()],
            current_values(),
        )
    }

    /// Type a query into the search box to filter the list down to
    /// the wanted row. Assumes the query uniquely matches.
    fn search_for(component: &mut SettingsWindowComponent, query: &str) {
        for c in query.chars() {
            component.handle_input(&Key::char(c));
        }
    }

    fn enter() -> InputEvent {
        Key::enter()
    }
    fn escape() -> InputEvent {
        Key::escape()
    }
    fn down() -> InputEvent {
        Key::down()
    }

    /// Every schema entry must surface as a row (with the
    /// `model_api`/`model_name` pair folded into the model row), so
    /// a future config option can't silently miss the window. Bool
    /// options additionally must render as true/false toggles.
    #[test]
    fn every_config_option_has_a_row() {
        let component = test_component();
        for option in Config::OPTIONS {
            let id = match option.name {
                "model_api" | "model_name" => MODEL_SETTING_ID,
                other => other,
            };
            let value = component.inner.value_of(id);
            assert!(
                value.is_some(),
                "config option {} has no settings-window row",
                option.name
            );
            if matches!(option.kind, ValueKind::Bool) {
                let value = value.expect("checked above");
                assert!(
                    value == "true" || value == "false",
                    "bool option {} renders non-bool value {value:?}",
                    option.name
                );
            }
        }
    }

    #[test]
    fn cycling_a_bool_row_queues_a_change() {
        let mut component = test_component();
        let changes = component.changes_handle();
        search_for(&mut component, "hide");
        component.handle_input(&enter());
        let queued = std::mem::take(&mut *changes.lock().unwrap());
        assert_eq!(
            queued,
            vec![("hide_thinking_block".to_string(), "true".to_string())]
        );
    }

    #[test]
    fn thinking_display_cycle_starts_at_default() {
        let component = test_component();
        assert_eq!(
            component.inner.value_of("thinking_display"),
            Some("default")
        );
    }

    #[test]
    fn thinking_submenu_commits_selected_level() {
        let mut component = test_component();
        let changes = component.changes_handle();
        search_for(&mut component, "thinking");
        // Top match for "thinking" is the `thinking` row itself.
        component.handle_input(&enter());
        assert!(component.inner.has_active_submenu());
        // medium is pre-selected; one down lands on high.
        component.handle_input(&down());
        component.handle_input(&enter());
        assert!(!component.inner.has_active_submenu());
        let queued = std::mem::take(&mut *changes.lock().unwrap());
        assert_eq!(queued, vec![("thinking".to_string(), "high".to_string())]);
        assert_eq!(component.inner.value_of("thinking"), Some("high"));
    }

    #[test]
    fn model_submenu_commits_provider_id_pair() {
        let mut component = test_component();
        let changes = component.changes_handle();
        search_for(&mut component, "model");
        // Rows matching "model": model, model_url — the model row
        // ranks first (shorter exact match).
        component.handle_input(&enter());
        assert!(component.inner.has_active_submenu());
        // Current model (anthropic) is pre-selected; one down lands
        // on the gpt-5 row.
        component.handle_input(&down());
        component.handle_input(&enter());
        let queued = std::mem::take(&mut *changes.lock().unwrap());
        assert_eq!(
            queued,
            vec![(MODEL_SETTING_ID.to_string(), "openai/gpt-5".to_string())]
        );
        assert_eq!(
            component.inner.value_of(MODEL_SETTING_ID),
            Some("openai/gpt-5")
        );
    }

    #[test]
    fn model_submenu_escape_commits_nothing() {
        let mut component = test_component();
        let changes = component.changes_handle();
        search_for(&mut component, "model");
        component.handle_input(&enter());
        assert!(component.inner.has_active_submenu());
        component.handle_input(&escape());
        // The submenu result is consumed on the *next* input event's
        // delegation pass; rendering alone must already show the
        // parent unchanged after the list processes the close.
        component.handle_input(&down());
        assert!(!component.inner.has_active_submenu());
        assert!(changes.lock().unwrap().is_empty());
        assert_eq!(
            component.inner.value_of(MODEL_SETTING_ID),
            Some("anthropic/claude-sonnet-4")
        );
    }

    #[test]
    fn model_url_text_submenu_commits_typed_value() {
        let mut component = test_component();
        let changes = component.changes_handle();
        search_for(&mut component, "url");
        component.handle_input(&enter());
        assert!(component.inner.has_active_submenu());
        for c in "http://x".chars() {
            component.handle_input(&Key::char(c));
        }
        component.handle_input(&enter());
        let queued = std::mem::take(&mut *changes.lock().unwrap());
        assert_eq!(
            queued,
            vec![("model_url".to_string(), "http://x".to_string())]
        );
    }

    #[test]
    fn tools_submenu_commits_aggregate_on_close() {
        let mut component = test_component();
        let changes = component.changes_handle();
        search_for(&mut component, "disabled_to");
        component.handle_input(&enter());
        assert!(component.inner.has_active_submenu());
        // Toggle the first tool (bash) to disabled, then close.
        component.handle_input(&enter());
        component.handle_input(&escape());
        let queued = std::mem::take(&mut *changes.lock().unwrap());
        assert_eq!(
            queued,
            vec![("disabled_tools".to_string(), "bash".to_string())]
        );
    }

    #[test]
    fn skills_submenu_commits_aggregate_on_close() {
        let mut component = test_component();
        let changes = component.changes_handle();
        search_for(&mut component, "disabled_sk");
        component.handle_input(&enter());
        assert!(component.inner.has_active_submenu());
        // Toggle the only skill (tmux-subagents) to disabled, then close.
        component.handle_input(&enter());
        component.handle_input(&escape());
        let queued = std::mem::take(&mut *changes.lock().unwrap());
        assert_eq!(
            queued,
            vec![("disabled_skills".to_string(), "tmux-subagents".to_string())]
        );
    }

    #[test]
    fn tools_submenu_unchanged_close_commits_nothing() {
        let mut component = test_component();
        let changes = component.changes_handle();
        search_for(&mut component, "disabled_to");
        component.handle_input(&enter());
        // Toggle bash off and back on, then close: net no change.
        component.handle_input(&enter());
        component.handle_input(&enter());
        component.handle_input(&escape());
        assert!(changes.lock().unwrap().is_empty());
    }

    #[test]
    fn escape_on_main_list_closes_the_window() {
        let mut component = test_component();
        let outcome = component.outcome_handle();
        component.handle_input(&escape());
        assert_eq!(
            outcome.lock().unwrap().take(),
            Some(SettingsWindowOutcome::Closed)
        );
    }

    #[test]
    fn corrections_update_displayed_values_on_render() {
        let mut component = test_component();
        let corrections = component.corrections_handle();
        corrections
            .lock()
            .unwrap()
            .push(("speed".to_string(), "fast".to_string()));
        component.render(80);
        assert_eq!(component.inner.value_of("speed"), Some("fast"));
    }

    /// A project-mode window with the given keys marked as project-set.
    /// `current` shows the effective values; `inherited` carries the
    /// user values a clear reverts to (here, distinct from `current`
    /// for `theme` so the clear is observable).
    fn project_test_component(set_keys: &[&str]) -> SettingsWindowComponent {
        let mut current = current_values();
        current.theme = "dark".to_string();
        let mut inherited = current_values();
        inherited.theme = "light".to_string();
        let set: BTreeSet<String> = set_keys.iter().map(|s| s.to_string()).collect();
        SettingsWindowComponent::new_project(
            identity_settings_theme(),
            identity_select_theme(),
            vec![
                make_model("anthropic", "claude-sonnet-4", "Claude Sonnet 4"),
                make_model("openai", "gpt-5", "GPT-5"),
            ],
            vec!["dark".to_string(), "light".to_string()],
            vec!["bash".to_string(), "read_file".to_string()],
            vec!["tmux-subagents".to_string()],
            current,
            inherited,
            set,
        )
    }

    #[test]
    fn project_rows_are_inherited_unless_the_project_sets_them() {
        let component = project_test_component(&["theme"]);
        // The project sets `theme`, so its row is not inherited.
        assert!(!component.inner.is_inherited("theme"));
        // Everything else falls through to the user layer.
        assert!(component.inner.is_inherited("hide_thinking_block"));
        assert!(component.inner.is_inherited("auto_compact"));
    }

    #[test]
    fn project_model_row_set_when_either_model_key_is_set() {
        let component = project_test_component(&["model_name"]);
        assert!(!component.inner.is_inherited(MODEL_SETTING_ID));
    }

    #[test]
    fn model_url_renders_default_placeholder_when_empty() {
        let mut component = test_component();
        // `current_values()` leaves `model_url` unset, so the row is
        // empty and must show its "(default)" placeholder.
        search_for(&mut component, "model_url");
        let body = component
            .render(80)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("(default)"), "got: {body:?}");
    }

    #[test]
    fn clearing_a_project_row_queues_the_inherited_value_and_reverts_it() {
        // The clear chord is an `aj.*` action, so the aj-level
        // keybindings must be installed for `matches` to resolve it.
        crate::config::keybindings::install_global_manager_defaults();
        let mut component = project_test_component(&["theme"]);
        let clears = component.clears_handle();
        search_for(&mut component, "theme");
        component.handle_input(&Key::ctrl('x'));

        let queued = std::mem::take(&mut *clears.lock().unwrap());
        assert_eq!(
            queued,
            vec![("theme".to_string(), "light".to_string())],
            "clear should carry the inherited user value"
        );
        // The row optimistically reverts to the inherited value and is
        // no longer an override.
        assert_eq!(component.inner.value_of("theme"), Some("light"));
        assert!(component.inner.is_inherited("theme"));
    }

    #[test]
    fn clearing_an_inherited_row_is_a_noop() {
        crate::config::keybindings::install_global_manager_defaults();
        let mut component = project_test_component(&["theme"]);
        let clears = component.clears_handle();
        // `auto_compact` is inherited (the project doesn't set it).
        search_for(&mut component, "auto_compact");
        component.handle_input(&Key::ctrl('x'));
        assert!(clears.lock().unwrap().is_empty());
    }

    #[test]
    fn user_window_ignores_the_clear_chord() {
        crate::config::keybindings::install_global_manager_defaults();
        let mut component = test_component();
        let clears = component.clears_handle();
        search_for(&mut component, "theme");
        // Not project mode: the chord falls through to the list and
        // queues no clear.
        component.handle_input(&Key::ctrl('x'));
        assert!(clears.lock().unwrap().is_empty());
    }
}
