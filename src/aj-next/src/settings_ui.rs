//! Config-editing overlays: the thinking and model selectors, the settings
//! and project-settings windows, and the skills window.
//!
//! # Two shapes
//!
//! The thinking and model selectors are confirm-and-close pick lists built on
//! [`FilterableSelect`]. Confirming parks a [`SelectorActivity`] the host loop
//! applies through the shared settings core, then the overlay closes.
//!
//! The settings, project-settings, and skills windows stay open across edits.
//! They are built on [`SettingList`], a navigable list of `label  value` rows
//! with a filter box. Cycle rows (bools, enums) cycle in place; submenu rows
//! push a child overlay (a picker, a one-line editor, or a nested toggle
//! list). Every edit parks a [`SelectorActivity`] the host drains and applies.
//! The windows never close themselves on an edit; only Esc closes them.
//!
//! # Focus and the host boundary
//!
//! The top-level overlays are opened from the drive loop, which owns the
//! session world but has no [`EventContext`] to move focus. So the open
//! functions here only push onto the stack; the host posts a refocus app event
//! and the shell moves focus onto the top overlay. Submenus, by contrast, are
//! opened from inside a widget's capture handler (in dispatch), so they move
//! focus directly.
//!
//! NOTE(aljoscha): one consequence of opening from the host is that a
//! palette-launched selector does not keep the palette as its parent. The
//! palette confirm parks a command and closes back to the editor before the
//! host opens the selector, so its Esc returns to the editor, not the palette.
//! The read-only overlays (help, auth, ...) open from within the palette
//! dispatch and do keep it underneath. This is a known chaining gap versus
//! `aj`, which keeps the palette as the parent for palette-opened selectors.
//! Closing it means either reaching the session world from dispatch or teaching
//! the shared open/close paths to distinguish a confirm (tear down to the
//! editor) from a cancel (step back to the palette), neither of which is small.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use aj_agent::events::AgentId;
use aj_app::commands::{THINKING_LEVELS, thinking_level_name};
use aj_app::footer::format_tokens;
use aj_app::keybindings::{ACTION_SETTINGS_CLEAR, default_action_shortcut};
use aj_app::settings::ConfigTarget;
use aj_conf::{Config, ValueKind};
use aj_models::ThinkingConfig;
use aj_models::registry::ModelInfo;
use vaxis::cell::{Cell, Character, Style};
use vaxis::key::{Key, Modifiers};
use vaxis::vxfw::{
    Builder, DrawContext, Event, EventContext, FilterableSelect, ListView, MaxSize, OverlayWindow,
    RelativePoint, RichText, ScrollBars, SelectItem, SelectStyles, Size, Source, SubSurface,
    Surface, TextField, TextSpan, Widget, WidgetRef, WidthBasis, draw_widget, to_widget_ref,
};

use crate::keymap::action_matches;
use crate::overlay::{
    OpenOverlay, OverlayChrome, OverlayPlacement, OverlayStack, close_all, close_top,
    subtitle_confirm_close, subtitle_edit_close,
};

/// The synthetic settings row folding `model_api` + `model_name` into one
/// picker-backed entry. Its change value is a `provider/id` string.
pub(crate) const MODEL_SETTING_ID: &str = "model";

/// The "leave unset" sentinel for options whose absence has its own meaning
/// (`thinking_display`, `verbosity`). The host maps it back to key removal.
pub(crate) const UNSET_VALUE: &str = "default";

/// A confirmed edit parked by an overlay for the drive loop to apply through
/// the shared settings core. The overlays cannot reach the async cores or the
/// `SessionCore`, so they only record intent; the host reconciles.
pub(crate) enum SelectorActivity {
    /// A thinking level was confirmed for `target` (session-scoped).
    ThinkingConfirmed {
        target: AgentId,
        level: Option<ThinkingConfig>,
    },
    /// A model was confirmed for `target` (session-scoped).
    ModelConfirmed {
        target: AgentId,
        info: Box<ModelInfo>,
    },
    /// A settings-window value change to persist to `target`'s layer.
    SettingChange {
        target: ConfigTarget,
        id: String,
        value: String,
    },
    /// A project override cleared; `inherited` is the value the live effect
    /// reverts to.
    SettingClear { id: String, inherited: String },
    /// A skills-window toggle: enable or disable `name` for new sessions.
    SkillToggle { name: String, disable: bool },
}

/// Handles the drive loop keeps for a live settings window: the row list
/// (so an apply failure can revert a row and a theme swap can re-tint it) and
/// the window chrome (so a theme swap re-tints its border). Cleared when the
/// window closes.
pub(crate) struct SettingsUi {
    pub(crate) list: Rc<RefCell<SettingList>>,
    pub(crate) window: Rc<RefCell<OverlayWindow>>,
}

impl SettingsUi {
    /// Re-tint the open window after a runtime theme swap.
    pub(crate) fn restyle(&self, chrome: &OverlayChrome) {
        self.list.borrow().set_styles(chrome.select.clone());
        let mut window = self.window.borrow_mut();
        window.border_style = chrome.border;
        window.title_style = chrome.title;
        window.subtitle_style = chrome.subtitle;
    }
}

// ============================================================================
// Shared helpers
// ============================================================================

/// Push a titled overlay window wrapping `child` onto the stack, styled from
/// `chrome`. Returns the window handle so the caller can keep it for a later
/// re-tint. Does not move focus: the caller (host or dispatch) owns that.
pub(crate) fn push_window(
    stack: &Rc<RefCell<OverlayStack>>,
    chrome: &OverlayChrome,
    title: &str,
    subtitle: String,
    child: WidgetRef,
    focus: WidgetRef,
    placement: OverlayPlacement,
) -> Rc<RefCell<OverlayWindow>> {
    let mut window = OverlayWindow::new(title, child);
    window.subtitle = subtitle;
    window.border_style = chrome.border;
    window.title_style = chrome.title;
    window.subtitle_style = chrome.subtitle;
    let window = Rc::new(RefCell::new(window));
    stack.borrow_mut().push(OpenOverlay {
        widget: to_widget_ref(Rc::clone(&window)),
        focus,
        placement,
    });
    window
}

// ============================================================================
// Thinking selector
// ============================================================================

/// Pick-list rows for the thinking levels, current one tagged `(current)`.
fn thinking_items(current_name: &str) -> Vec<SelectItem> {
    THINKING_LEVELS
        .iter()
        .map(|level| {
            let label = if level.name == current_name {
                format!("{} (current)", level.name)
            } else {
                level.name.to_string()
            };
            SelectItem::new(label, level.name).with_description(level.description)
        })
        .collect()
}

/// Open the thinking selector for `target`, pre-selecting `current`.
pub(crate) fn open_thinking(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    activity: &Rc<RefCell<Vec<SelectorActivity>>>,
    target: AgentId,
    current: Option<ThinkingConfig>,
) {
    let current_name = thinking_level_name(&current).to_string();
    let select = Rc::new(RefCell::new(FilterableSelect::new(
        thinking_items(&current_name),
        chrome.select.clone(),
    )));
    select
        .borrow()
        .select_matching(|item| item.filter_key == current_name);
    let focus = select.borrow().focus_target();
    {
        let mut sel = select.borrow_mut();
        let activity = Rc::clone(activity);
        let stack_c = Rc::clone(stack);
        let editor_c = Rc::clone(editor);
        sel.on_confirm = Some(Box::new(move |ctx, item| {
            if let Some(level) = aj_app::commands::parse_thinking_level(&item.filter_key) {
                activity
                    .borrow_mut()
                    .push(SelectorActivity::ThinkingConfirmed { target, level });
            }
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
    push_window(
        stack,
        chrome,
        "Thinking effort",
        subtitle_confirm_close(),
        to_widget_ref(select),
        focus,
        OverlayPlacement::Small,
    );
}

// ============================================================================
// Model selector
// ============================================================================

/// The unique filter key for a catalog entry: `"{provider} {id} {name}"`. The
/// confirm path recovers the [`ModelInfo`] by matching this back against the
/// catalog (provider + id are unique), and the label's `(current)` tag never
/// enters it.
fn model_filter_key(info: &ModelInfo) -> String {
    format!("{} {} {}", info.provider, info.id, info.name)
}

/// Pick-list rows for the model catalog, current one tagged `(current)`. The
/// description column carries the wire identity and context window so
/// same-named models across providers are distinguishable.
fn model_items(catalog: &[ModelInfo], current: Option<&(String, String)>) -> Vec<SelectItem> {
    catalog
        .iter()
        .map(|info| {
            let is_current = current.is_some_and(|(p, id)| *p == info.provider && *id == info.id);
            let label = if is_current {
                format!("{} (current)", info.name)
            } else {
                info.name.clone()
            };
            let description = format!(
                "{} · {} · {}",
                info.provider,
                info.id,
                format_tokens(info.context_window)
            );
            SelectItem::new(label, model_filter_key(info)).with_description(description)
        })
        .collect()
}

/// Open the model selector for `target`, pre-selecting the current model.
pub(crate) fn open_model(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    activity: &Rc<RefCell<Vec<SelectorActivity>>>,
    catalog: Arc<Vec<ModelInfo>>,
    target: AgentId,
    current: Option<(String, String)>,
) {
    let select = Rc::new(RefCell::new(FilterableSelect::new(
        model_items(&catalog, current.as_ref()),
        chrome.select.clone(),
    )));
    if let Some(info) = current
        .as_ref()
        .and_then(|(p, id)| catalog.iter().find(|m| m.provider == *p && m.id == *id))
    {
        let key = model_filter_key(info);
        select
            .borrow()
            .select_matching(|item| item.filter_key == key);
    }
    let focus = select.borrow().focus_target();
    {
        let mut sel = select.borrow_mut();
        let activity = Rc::clone(activity);
        let stack_c = Rc::clone(stack);
        let editor_c = Rc::clone(editor);
        let catalog_c = Arc::clone(&catalog);
        sel.on_confirm = Some(Box::new(move |ctx, item| {
            if let Some(info) = catalog_c
                .iter()
                .find(|m| model_filter_key(m) == item.filter_key)
            {
                activity
                    .borrow_mut()
                    .push(SelectorActivity::ModelConfirmed {
                        target,
                        info: Box::new(info.clone()),
                    });
            }
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
    push_window(
        stack,
        chrome,
        "Select model",
        subtitle_confirm_close(),
        to_widget_ref(select),
        focus,
        OverlayPlacement::Small,
    );
}

// ============================================================================
// SettingList: the stay-open editable list
// ============================================================================

/// How Enter edits a settings row.
pub(crate) enum RowKind {
    /// Cycle through these values in place, firing `on_change` with the next
    /// one. Used for bools and small enums.
    Cycle(Vec<String>),
    /// Open a child submenu (a picker, a text editor, or a toggle list). The
    /// widget fires `on_open` with the row id and current value; the host
    /// builds the submenu.
    Submenu,
}

/// One editable settings row.
pub(crate) struct SettingRow {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) value: String,
    pub(crate) description: String,
    pub(crate) kind: RowKind,
    /// Project mode: the value is inherited from the user layer (rendered
    /// muted, and the clear chord is inert on it).
    pub(crate) inherited: bool,
    /// Project mode: the inherited value a clear reverts this row to. The
    /// widget applies it optimistically and hands it to the host so the live
    /// effect reverts to the same value. Unused in the user window.
    pub(crate) clear_to: String,
}

/// The model shared between the widget, the row builder, and the filter
/// callback.
struct SettingListState {
    rows: Vec<SettingRow>,
    /// Indices into `rows`, filtered best-first by the query.
    visible: Vec<usize>,
    query: String,
    matcher: vaxis::fuzzy::FuzzyMatcher,
    /// Column width the value is aligned against, from the widest label.
    label_width: usize,
    project_mode: bool,
}

/// Builds one banded `label  value` row per visible index.
struct SettingRowBuilder {
    state: Rc<RefCell<SettingListState>>,
    styles: Rc<RefCell<SelectStyles>>,
}

impl Builder for SettingRowBuilder {
    fn item_at_idx(&self, idx: usize, cursor: usize) -> Option<WidgetRef> {
        let state = self.state.borrow();
        let &row_idx = state.visible.get(idx)?;
        let styles = self.styles.borrow();
        Some(build_setting_row(
            &state.rows[row_idx],
            idx == cursor,
            state.label_width,
            state.project_mode,
            &styles,
        ))
    }
}

/// Cap on the reserved description-panel height, in wrapped lines. Long
/// descriptions truncate rather than shrink the list to nothing.
const MAX_DESC_PANEL_HEIGHT: u16 = 4;

/// Minimum list rows kept visible when a description panel is reserved. Below
/// this the panel is dropped so a cramped overlay still shows the list.
const MIN_LIST_ROWS: u16 = 2;

/// Wrapped height of `text` soft-wrapped to `width`, capped at `cap`. Used to
/// size the description panel. Style does not affect wrapping, so this measures
/// with the default style.
fn wrapped_height(ctx: &DrawContext, text: &str, width: u16, cap: u16) -> u16 {
    if text.is_empty() || width == 0 || cap == 0 {
        return 0;
    }
    let mut rich = RichText::new(vec![TextSpan {
        text: text.to_string(),
        ..TextSpan::default()
    }]);
    rich.softwrap = true;
    let measure_ctx = ctx.with_constraints(
        Size {
            width: 0,
            height: 0,
        },
        MaxSize {
            width: Some(width),
            height: Some(cap),
        },
    );
    rich.draw(&measure_ctx).size.height
}

/// Render one settings row: an override marker (project mode) and the aligned
/// `label  value` primary columns. On the cursored row every cell carries the
/// band background. The row's description is not shown inline. The widget
/// draws the cursored row's description in a panel below the list.
fn build_setting_row(
    row: &SettingRow,
    selected: bool,
    label_width: usize,
    project_mode: bool,
    styles: &SelectStyles,
) -> WidgetRef {
    let band = selected.then_some(styles.selected_bg);
    let tint = |mut style: Style| -> Style {
        if let Some(bg) = band {
            style.bg = bg;
        }
        style
    };
    let span = |text: String, style: Style| TextSpan {
        text,
        style,
        ..TextSpan::default()
    };
    // Inherited project rows read muted so the override rows stand out.
    let primary = if row.inherited {
        styles.secondary
    } else {
        styles.label
    };
    let mut spans = Vec::new();
    if project_mode {
        let marker = if row.inherited { "  " } else { "\u{25cf} " };
        spans.push(span(marker.to_string(), tint(styles.secondary)));
    }
    spans.push(span(
        format!("{:<label_width$}  ", row.label),
        tint(primary),
    ));
    spans.push(span(row.value.clone(), tint(primary)));
    let mut rich = RichText::new(spans);
    rich.softwrap = false;
    rich.width_basis = WidthBasis::Parent;
    if let Some(bg) = band {
        rich.base_style = Style {
            bg,
            ..Style::default()
        };
    }
    let widget: WidgetRef = Rc::new(RefCell::new(rich));
    widget
}

/// Recompute the filtered view and reset the cursor to the top.
fn apply_setting_filter(state: &mut SettingListState, list: &mut ListView) {
    let SettingListState {
        rows,
        visible,
        query,
        matcher,
        ..
    } = state;
    *visible = matcher
        .filter(rows.iter().enumerate(), query, |(_, row)| {
            row.label.as_str()
        })
        .into_iter()
        .map(|(i, _)| i)
        .collect();
    list.item_count = Some(u32::try_from(visible.len()).expect("row count fits u32"));
    list.jump_to_item(0);
}

/// Selected-row info cloned out so callbacks can run without holding a state
/// borrow.
struct SelectedRow {
    id: String,
    value: String,
    clear_to: String,
    cycle: Option<Vec<String>>,
}

/// A navigable, filterable list of editable settings rows. See the module
/// docs for the edit flow.
pub(crate) struct SettingList {
    filter: Rc<RefCell<TextField>>,
    /// The row list, shared with `bars` (which draws it) so event handling and
    /// the value accessors drive the same list the thumb reflects.
    list: Rc<RefCell<ListView>>,
    /// Scroll bars wrapping the list (sharing its `Rc` via `bars.view`) for the
    /// vertical thumb. The horizontal bar is off (a settings list has no
    /// horizontal axis), and `ScrollBars` draws the thumb only while the list
    /// overflows its slot.
    bars: ScrollBars<ListView>,
    state: Rc<RefCell<SettingListState>>,
    styles: Rc<RefCell<SelectStyles>>,
    project_mode: bool,
    /// Fires on Enter over a [`RowKind::Cycle`] row with `(id, next_value)`.
    pub(crate) on_change: Option<Box<dyn FnMut(&mut EventContext, &str, &str)>>,
    /// Fires on Enter over a [`RowKind::Submenu`] row with `(id, value)`.
    pub(crate) on_open: Option<Box<dyn FnMut(&mut EventContext, &str, &str)>>,
    /// Fires on the clear chord over a project override with `(id,
    /// inherited_value)`. The widget has already reverted the row's display.
    pub(crate) on_clear: Option<Box<dyn FnMut(&mut EventContext, &str, &str)>>,
    /// Fires on Escape.
    pub(crate) on_close: Option<Box<dyn FnMut(&mut EventContext)>>,
}

impl SettingList {
    pub(crate) fn new(
        rows: Vec<SettingRow>,
        styles: SelectStyles,
        project_mode: bool,
    ) -> SettingList {
        let label_width = rows
            .iter()
            .map(|r| r.label.chars().count())
            .max()
            .unwrap_or(0);
        let state = Rc::new(RefCell::new(SettingListState {
            rows,
            visible: Vec::new(),
            query: String::new(),
            matcher: vaxis::fuzzy::FuzzyMatcher::new(),
            label_width,
            project_mode,
        }));
        let styles = Rc::new(RefCell::new(styles));
        let mut list_view = ListView::new(Source::Builder(Box::new(SettingRowBuilder {
            state: Rc::clone(&state),
            styles: Rc::clone(&styles),
        })));
        list_view.draw_cursor = false;
        // Wrap the list in scroll bars for the vertical thumb. The bars own the
        // list behind their shared `view` handle, which we keep a clone of for
        // the widget's own accessors and event handling.
        let mut bars = ScrollBars::new(list_view);
        bars.draw_horizontal_scrollbar = false;
        let list = Rc::clone(&bars.view);
        apply_setting_filter(&mut state.borrow_mut(), &mut list.borrow_mut());
        let filter = Rc::new(RefCell::new(TextField::new()));
        {
            let state = Rc::clone(&state);
            let list = Rc::clone(&list);
            filter.borrow_mut().on_change = Some(Box::new(move |ctx, text| {
                let mut state = state.borrow_mut();
                state.query = text.to_string();
                apply_setting_filter(&mut state, &mut list.borrow_mut());
                ctx.redraw = true;
            }));
        }
        SettingList {
            filter,
            list,
            bars,
            state,
            styles,
            project_mode,
            on_change: None,
            on_open: None,
            on_clear: None,
            on_close: None,
        }
    }

    pub(crate) fn focus_target(&self) -> WidgetRef {
        to_widget_ref(Rc::clone(&self.filter))
    }

    /// Replace the row styles (a runtime theme swap).
    pub(crate) fn set_styles(&self, styles: SelectStyles) {
        *self.styles.borrow_mut() = styles;
    }

    /// Set a row's displayed value (an optimistic edit, or a host correction
    /// after a failed apply). No-op for an unknown id.
    pub(crate) fn set_value(&self, id: &str, value: &str) {
        let mut state = self.state.borrow_mut();
        if let Some(row) = state.rows.iter_mut().find(|r| r.id == id) {
            row.value = value.to_string();
        }
    }

    /// Mark a row inherited (project mode), after a clear reverts it.
    pub(crate) fn set_inherited(&self, id: &str, inherited: bool) {
        let mut state = self.state.borrow_mut();
        if let Some(row) = state.rows.iter_mut().find(|r| r.id == id) {
            row.inherited = inherited;
        }
    }

    /// Replace the whole row set (an async fill once discovery lands),
    /// recompute the label column width, and re-apply the active filter.
    ///
    /// Safe to call while the window is open and focused: it keeps the filter
    /// field and list widgets, so focus survives, and resets the list cursor
    /// to the top exactly as construction does. The recompute of
    /// `label_width` and the filter re-derivation mirror [`SettingList::new`]
    /// and [`apply_setting_filter`].
    pub(crate) fn set_rows(&self, rows: Vec<SettingRow>) {
        let mut state = self.state.borrow_mut();
        state.label_width = rows
            .iter()
            .map(|r| r.label.chars().count())
            .max()
            .unwrap_or(0);
        state.rows = rows;
        apply_setting_filter(&mut state, &mut self.list.borrow_mut());
    }

    /// A row's current displayed value, for host reconciliation and tests.
    #[cfg(test)]
    pub(crate) fn value_of(&self, id: &str) -> Option<String> {
        self.state
            .borrow()
            .rows
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.value.clone())
    }

    /// The cursored row's id, value, and (for a cycle row) its value cycle.
    fn selected(&self) -> Option<SelectedRow> {
        let cursor = usize::try_from(self.list.borrow().cursor).expect("cursor fits usize");
        let state = self.state.borrow();
        let &row_idx = state.visible.get(cursor)?;
        let row = &state.rows[row_idx];
        Some(SelectedRow {
            id: row.id.clone(),
            value: row.value.clone(),
            clear_to: row.clear_to.clone(),
            cycle: match &row.kind {
                RowKind::Cycle(values) => Some(values.clone()),
                RowKind::Submenu => None,
            },
        })
    }

    /// The cursored row's description, for the below-list panel. Empty when
    /// the row carries no description (or there is no cursored row).
    fn selected_description(&self) -> String {
        let cursor = usize::try_from(self.list.borrow().cursor).expect("cursor fits usize");
        let state = self.state.borrow();
        state
            .visible
            .get(cursor)
            .map(|&i| state.rows[i].description.clone())
            .unwrap_or_default()
    }

    /// Whether the cursored row is a project override (clearable).
    fn selected_is_override(&self) -> bool {
        let cursor = usize::try_from(self.list.borrow().cursor).expect("cursor fits usize");
        let state = self.state.borrow();
        state
            .visible
            .get(cursor)
            .map(|&i| !state.rows[i].inherited)
            .unwrap_or(false)
    }

    /// The tallest wrapped description across the filtered rows, capped at
    /// [`MAX_DESC_PANEL_HEIGHT`]. Sizing the panel to the max (not just the
    /// cursored row) keeps the list from shifting as the cursor moves.
    fn description_panel_height(&self, ctx: &DrawContext, width: u16) -> u16 {
        if width == 0 {
            return 0;
        }
        let state = self.state.borrow();
        let mut max = 0;
        for &row_idx in &state.visible {
            let desc = &state.rows[row_idx].description;
            max = max.max(wrapped_height(ctx, desc, width, MAX_DESC_PANEL_HEIGHT));
            if max >= MAX_DESC_PANEL_HEIGHT {
                return MAX_DESC_PANEL_HEIGHT;
            }
        }
        max
    }
}

/// Tint the vertical scroll-bar thumb cells from `style`.
///
/// Applied on each draw so a runtime restyle (theme swap) is reflected without
/// rebuilding the bars. The hover and drag cells are set for completeness. The
/// list forwards no mouse events to the bars, so only the base thumb is drawn.
fn apply_thumb_style(bars: &mut ScrollBars<ListView>, style: Style) {
    let cell = |grapheme: &str| Cell {
        char: Character::new(grapheme, 1),
        style,
        ..Cell::default()
    };
    bars.vertical_scrollbar_thumb = cell("\u{2590}");
    bars.vertical_scrollbar_hover_thumb = cell("\u{2588}");
    bars.vertical_scrollbar_drag_thumb = cell("\u{2588}");
}

/// The value after `current` in `values`, wrapping around. Falls back to the
/// first value when `current` isn't in the set.
fn next_cycle_value(values: &[String], current: &str) -> String {
    match values.iter().position(|v| v == current) {
        Some(pos) => values[(pos + 1) % values.len()].clone(),
        None => values.first().cloned().unwrap_or_default(),
    }
}

impl Widget for SettingList {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let size = ctx.max.size();
        let mut surface = Surface::with_size(size);
        let filter_ctx = ctx.with_constraints(
            Size {
                width: 0,
                height: 0,
            },
            MaxSize {
                width: Some(size.width),
                height: Some(1),
            },
        );
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: draw_widget(&to_widget_ref(Rc::clone(&self.filter)), &filter_ctx),
            z_index: 0,
        });
        // Reserve a description panel below the list: a blank separator plus
        // the tallest wrapped description across the filtered rows. Wrapped to
        // `size.width - 4` and drawn at col 2 so each line is indented 2, the
        // block matches `aj`.
        let desc_width = size.width.saturating_sub(4);
        let panel_height = self.description_panel_height(ctx, desc_width);
        let reserved = if panel_height > 0 {
            1 + panel_height
        } else {
            0
        };
        // Keep at least a couple of list rows. On a cramped overlay we drop
        // the panel rather than starve the list.
        let (list_height, show_panel) = {
            let with_panel = size.height.saturating_sub(2 + reserved);
            if reserved > 0 && with_panel >= MIN_LIST_ROWS {
                (with_panel, true)
            } else {
                (size.height.saturating_sub(2), false)
            }
        };
        if list_height > 0 {
            let list_ctx = ctx.with_constraints(
                Size {
                    width: 0,
                    height: 0,
                },
                MaxSize {
                    width: Some(size.width),
                    height: Some(list_height),
                },
            );
            surface.children.push(SubSurface {
                origin: RelativePoint { row: 2, col: 0 },
                surface: {
                    // Tint the thumb from the live styles so a theme swap (via
                    // `set_styles`) is reflected without rebuilding the bars.
                    // `ScrollBars` draws the inner list (stamping its identity
                    // for wheel/key routing) and reserves the rightmost column
                    // for the thumb, which shows only while the list overflows.
                    apply_thumb_style(&mut self.bars, self.styles.borrow().scrollbar_thumb);
                    self.bars.draw(&list_ctx)
                },
                z_index: 0,
            });
        }
        if show_panel {
            let desc = self.selected_description();
            if !desc.is_empty() {
                let panel_ctx = ctx.with_constraints(
                    Size {
                        width: 0,
                        height: 0,
                    },
                    MaxSize {
                        width: Some(desc_width),
                        height: Some(panel_height),
                    },
                );
                let mut rich = RichText::new(vec![TextSpan {
                    text: desc,
                    style: self.styles.borrow().secondary,
                    ..TextSpan::default()
                }]);
                rich.softwrap = true;
                let widget: WidgetRef = Rc::new(RefCell::new(rich));
                surface.children.push(SubSurface {
                    origin: RelativePoint {
                        row: i32::from(2 + list_height + 1),
                        col: 2,
                    },
                    surface: draw_widget(&widget, &panel_ctx),
                    z_index: 0,
                });
            }
        }
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else {
            return;
        };
        if key.matches(Key::ESCAPE, Modifiers::empty()) {
            if let Some(cb) = self.on_close.as_mut() {
                cb(ctx);
            }
            ctx.consume_and_redraw();
            return;
        }
        // The clear chord is overlay-local (Spec F): matched here at-target
        // rather than by the global keymap, and only in project mode over an
        // actual override.
        if self.project_mode && action_matches(key, ACTION_SETTINGS_CLEAR) {
            if self.selected_is_override()
                && let Some(sel) = self.selected()
            {
                // Revert the row optimistically (value + muted marker) so the
                // window never shows an override the host is about to drop.
                self.set_value(&sel.id, &sel.clear_to);
                self.set_inherited(&sel.id, true);
                if let Some(cb) = self.on_clear.as_mut() {
                    cb(ctx, &sel.id, &sel.clear_to);
                }
            }
            ctx.consume_and_redraw();
            return;
        }
        if key.matches(Key::ENTER, Modifiers::empty())
            || key.matches(u32::from('j'), Modifiers::CTRL)
        {
            if let Some(sel) = self.selected() {
                match sel.cycle {
                    // A non-empty cycle set advances in place and fires the
                    // change. An empty set is an inert placeholder row (the
                    // skills window's loading and "no skills" states), so Enter
                    // neither changes a value nor fires a spurious `on_change`.
                    Some(values) if !values.is_empty() => {
                        let next = next_cycle_value(&values, &sel.value);
                        self.set_value(&sel.id, &next);
                        if let Some(cb) = self.on_change.as_mut() {
                            cb(ctx, &sel.id, &next);
                        }
                    }
                    Some(_) => {}
                    None => {
                        if let Some(cb) = self.on_open.as_mut() {
                            cb(ctx, &sel.id, &sel.value);
                        }
                    }
                }
            }
            ctx.consume_and_redraw();
            return;
        }
        if key.matches(Key::DOWN, Modifiers::empty())
            || key.matches(u32::from('n'), Modifiers::CTRL)
        {
            self.list.borrow_mut().next_item(ctx);
            return;
        }
        if key.matches(Key::UP, Modifiers::empty()) || key.matches(u32::from('p'), Modifiers::CTRL)
        {
            self.list.borrow_mut().prev_item(ctx);
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

// ============================================================================
// TextEditOverlay: a one-line editor submenu
// ============================================================================

/// A one-line text editor submenu (for `model_url` and the compaction
/// numbers). Enter submits the trimmed value, Esc cancels.
pub(crate) struct TextEditOverlay {
    field: Rc<RefCell<TextField>>,
    pub(crate) on_cancel: Option<Box<dyn FnMut(&mut EventContext)>>,
}

impl TextEditOverlay {
    pub(crate) fn new(current: &str) -> TextEditOverlay {
        let field = Rc::new(RefCell::new(TextField::new()));
        field.borrow_mut().insert_slice_at_cursor(current);
        TextEditOverlay {
            field,
            on_cancel: None,
        }
    }

    pub(crate) fn focus_target(&self) -> WidgetRef {
        to_widget_ref(Rc::clone(&self.field))
    }

    /// Install the submit handler. Fires on Enter with the trimmed value.
    pub(crate) fn set_on_submit(&self, on_submit: Box<dyn FnMut(&mut EventContext, &str)>) {
        // Wrap so the callback sees the trimmed value, matching the config
        // vocabulary the apply path parses.
        let mut on_submit = on_submit;
        self.field.borrow_mut().on_submit = Some(Box::new(move |ctx, text| {
            on_submit(ctx, text.trim());
        }));
    }
}

impl Widget for TextEditOverlay {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let size = ctx.max.size();
        let mut surface = Surface::with_size(size);
        let field_ctx = ctx.with_constraints(
            Size {
                width: 0,
                height: 0,
            },
            MaxSize {
                width: Some(size.width),
                height: Some(1),
            },
        );
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: draw_widget(&to_widget_ref(Rc::clone(&self.field)), &field_ctx),
            z_index: 0,
        });
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else {
            return;
        };
        if key.matches(Key::ESCAPE, Modifiers::empty()) {
            if let Some(cb) = self.on_cancel.as_mut() {
                cb(ctx);
            }
            ctx.consume_and_redraw();
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

// ============================================================================
// Settings window: schema-driven rows and the open path
// ============================================================================

/// The live values a settings window opens with. Strings use the same
/// canonical vocabulary the host's apply path parses.
pub(crate) struct SettingsValues {
    pub(crate) model_key: (String, String),
    pub(crate) model_url: Option<String>,
    pub(crate) thinking: String,
    pub(crate) thinking_display: Option<String>,
    pub(crate) speed: String,
    pub(crate) verbosity: Option<String>,
    pub(crate) theme: String,
    pub(crate) disabled_tools: Vec<String>,
    pub(crate) disabled_skills: Vec<String>,
    pub(crate) hide_thinking_block: bool,
    pub(crate) show_frame_stats: bool,
    pub(crate) image_auto_resize: bool,
    pub(crate) image_show_in_terminal: bool,
    pub(crate) image_block: bool,
    pub(crate) bash_rtk: bool,
    pub(crate) syntax_highlighting: bool,
    pub(crate) auto_compact: bool,
    pub(crate) compact_threshold: String,
    pub(crate) compact_keep_recent: String,
}

/// The configured theme name for display, defaulting to `light` (the
/// interactive default) when unset.
fn config_theme_name(config: &Config) -> String {
    config.theme.clone().unwrap_or_else(|| "light".to_string())
}

/// The `(provider, id)` a bare config layer resolves to, applying the default
/// provider and picking that provider's first catalog model when the id is
/// unset. Mirrors the run-config resolution so a project row shows what the
/// file pins.
fn config_model_key(config: &Config, catalog: &[ModelInfo]) -> (String, String) {
    let provider = config
        .model_api
        .clone()
        .unwrap_or_else(|| aj_app::model::DEFAULT_PROVIDER_ID.to_string());
    let id = config.model_name.clone().unwrap_or_else(|| {
        catalog
            .iter()
            .find(|m| m.provider == provider)
            .map(|m| m.id.clone())
            .unwrap_or_default()
    });
    (provider, id)
}

impl SettingsValues {
    /// The config-layer view of `config` (project-settings windows): every
    /// value read from the layer, so a project-set row shows exactly what the
    /// file pins and an unset row shows the inherited value.
    pub(crate) fn from_config(config: &Config, catalog: &[ModelInfo]) -> SettingsValues {
        SettingsValues {
            model_key: config_model_key(config, catalog),
            model_url: config.model_url.clone(),
            thinking: config
                .thinking
                .map(|l| l.to_string())
                .unwrap_or_else(|| "off".to_string()),
            thinking_display: config.thinking_display.map(|d| d.to_string()),
            speed: config
                .speed
                .map(|s| s.to_string())
                .unwrap_or_else(|| "standard".to_string()),
            verbosity: config.verbosity.map(|v| v.to_string()),
            theme: config_theme_name(config),
            disabled_tools: config.disabled_tools.clone(),
            disabled_skills: config.disabled_skills.clone(),
            hide_thinking_block: config.hide_thinking_block,
            show_frame_stats: config.show_frame_stats,
            image_auto_resize: config.image_auto_resize,
            image_show_in_terminal: config.image_show_in_terminal,
            image_block: config.image_block,
            bash_rtk: config.bash_rtk,
            syntax_highlighting: config.syntax_highlighting,
            auto_compact: config.auto_compact,
            compact_threshold: config.compact_threshold.to_string(),
            compact_keep_recent: config.compact_keep_recent.to_string(),
        }
    }
}

/// Variant list of a [`ValueKind::Enum`] option, in schema order.
fn enum_values(option: &aj_conf::ConfigOption) -> Vec<String> {
    match option.kind {
        ValueKind::Enum(variants) => variants.iter().map(|v| v.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Whether the project layer sets the option(s) a settings row stands for.
/// The model row folds `model_api` + `model_name`, so it is set when either
/// is.
fn row_is_project_set(row_id: &str, set_keys: &BTreeSet<String>) -> bool {
    if row_id == MODEL_SETTING_ID {
        set_keys.contains("model_api") || set_keys.contains("model_name")
    } else {
        set_keys.contains(row_id)
    }
}

/// Canonical `", "`-joined display form of a name set.
fn join_names(names: &[String]) -> String {
    let set: BTreeSet<String> = names.iter().cloned().collect();
    set.into_iter().collect::<Vec<_>>().join(", ")
}

/// Parse a `", "`-joined name list back into a set.
fn split_names(joined: &str) -> BTreeSet<String> {
    joined
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// The `(value, kind)` for one schema option, or `None` to skip it
/// (`model_name`, folded into the model row). `bool`/enum options cycle in
/// place; the rest open a submenu.
fn row_value_kind(
    name: &str,
    values: &SettingsValues,
    option: &aj_conf::ConfigOption,
) -> Option<(String, RowKind)> {
    let value_or_unset = |v: &Option<String>| v.clone().unwrap_or_else(|| UNSET_VALUE.to_string());
    Some(match name {
        "model_api" => (
            format!("{}/{}", values.model_key.0, values.model_key.1),
            RowKind::Submenu,
        ),
        "model_name" => return None,
        "model_url" => (
            values.model_url.clone().unwrap_or_default(),
            RowKind::Submenu,
        ),
        "thinking" => (values.thinking.clone(), RowKind::Submenu),
        "thinking_display" => {
            let mut vals = vec![UNSET_VALUE.to_string()];
            vals.extend(enum_values(option));
            (
                value_or_unset(&values.thinking_display),
                RowKind::Cycle(vals),
            )
        }
        "speed" => (values.speed.clone(), RowKind::Cycle(enum_values(option))),
        "verbosity" => {
            let mut vals = vec![UNSET_VALUE.to_string()];
            vals.extend(enum_values(option));
            (value_or_unset(&values.verbosity), RowKind::Cycle(vals))
        }
        "theme" => (values.theme.clone(), RowKind::Submenu),
        "disabled_tools" => (join_names(&values.disabled_tools), RowKind::Submenu),
        "disabled_skills" => (join_names(&values.disabled_skills), RowKind::Submenu),
        "hide_thinking_block" => (values.hide_thinking_block.to_string(), bool_cycle()),
        "show_frame_stats" => (values.show_frame_stats.to_string(), bool_cycle()),
        "image_auto_resize" => (values.image_auto_resize.to_string(), bool_cycle()),
        "image_show_in_terminal" => (values.image_show_in_terminal.to_string(), bool_cycle()),
        "image_block" => (values.image_block.to_string(), bool_cycle()),
        "bash_rtk" => (values.bash_rtk.to_string(), bool_cycle()),
        "syntax_highlighting" => (values.syntax_highlighting.to_string(), bool_cycle()),
        "auto_compact" => (values.auto_compact.to_string(), bool_cycle()),
        "compact_threshold" => (values.compact_threshold.clone(), RowKind::Submenu),
        "compact_keep_recent" => (values.compact_keep_recent.clone(), RowKind::Submenu),
        _ => return None,
    })
}

fn bool_cycle() -> RowKind {
    RowKind::Cycle(vec!["true".to_string(), "false".to_string()])
}

/// The row id a schema option maps to (the model pair folds to one row).
fn row_id_for(option_name: &str) -> &str {
    match option_name {
        "model_api" => MODEL_SETTING_ID,
        other => other,
    }
}

/// Build one row per [`Config::OPTIONS`] entry (folding the model pair). In
/// project mode `inherited` marks the rows the project does not set, and
/// `clear_to` carries the inherited value a clear reverts to.
fn build_setting_rows(
    values: &SettingsValues,
    inherited: &SettingsValues,
    project_mode: bool,
    set_keys: &BTreeSet<String>,
) -> Vec<SettingRow> {
    let mut rows = Vec::new();
    for option in Config::OPTIONS {
        let Some((value, kind)) = row_value_kind(option.name, values, option) else {
            // `model_name` folds into the model row; an unmapped option is a
            // schema/window drift we skip rather than crash on.
            if option.name != "model_name" {
                tracing::warn!(
                    option = option.name,
                    "config option has no settings-window row"
                );
            }
            continue;
        };
        let id = row_id_for(option.name).to_string();
        let clear_to = row_value_kind(option.name, inherited, option)
            .map(|(v, _)| v)
            .unwrap_or_default();
        rows.push(SettingRow {
            id: id.clone(),
            label: id,
            value,
            description: aj_app::settings::option_description(option),
            kind,
            inherited: project_mode && !row_is_project_set(row_id_for(option.name), set_keys),
            clear_to,
        });
    }
    rows
}

/// Pick-list rows for the theme catalog, current one tagged `(current)`.
fn theme_items(names: &[String], current: &str) -> Vec<SelectItem> {
    names
        .iter()
        .map(|name| {
            let label = if name == current {
                format!("{name} (current)")
            } else {
                name.clone()
            };
            SelectItem::new(label, name.clone())
        })
        .collect()
}

/// The catalog snapshot, theme names, and toggle-name sets a settings window's
/// submenus need. Held together so the on-open handler carries one bundle.
pub(crate) struct SettingsCatalogs {
    pub(crate) models: Arc<Vec<ModelInfo>>,
    pub(crate) themes: Vec<String>,
    pub(crate) tools: Vec<String>,
    pub(crate) skills: Vec<String>,
}

/// Open a settings window (user or project) targeting `target`. See the
/// module docs for the edit flow. `values` are the current displayed values,
/// `inherited` the user-layer values a project clear reverts to, and
/// `set_keys` the option names the project layer sets (empty for user).
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_settings(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    activity: &Rc<RefCell<Vec<SelectorActivity>>>,
    settings_ui: &Rc<RefCell<Option<SettingsUi>>>,
    target: ConfigTarget,
    values: SettingsValues,
    inherited: SettingsValues,
    set_keys: BTreeSet<String>,
    catalogs: SettingsCatalogs,
) {
    let project_mode = target == ConfigTarget::Project;
    let rows = build_setting_rows(&values, &inherited, project_mode, &set_keys);
    let list = Rc::new(RefCell::new(SettingList::new(
        rows,
        chrome.select.clone(),
        project_mode,
    )));
    let focus = list.borrow().focus_target();
    {
        let mut l = list.borrow_mut();
        // Cycle edits: stage the change for the host to persist.
        let activity_change = Rc::clone(activity);
        l.on_change = Some(Box::new(move |_ctx, id, value| {
            activity_change
                .borrow_mut()
                .push(SelectorActivity::SettingChange {
                    target,
                    id: id.to_string(),
                    value: value.to_string(),
                });
        }));
        // Submenu edits: build the matching child overlay.
        let stack_open = Rc::clone(stack);
        let editor_open = Rc::clone(editor);
        let chrome_open = chrome.clone();
        let activity_open = Rc::clone(activity);
        let list_open = Rc::clone(&list);
        l.on_open = Some(Box::new(move |ctx, id, value| {
            open_setting_submenu(
                ctx,
                &stack_open,
                &editor_open,
                &chrome_open,
                &activity_open,
                &list_open,
                target,
                &catalogs,
                id,
                value,
            );
        }));
        // Project clears: the widget already reverted the row.
        let activity_clear = Rc::clone(activity);
        l.on_clear = Some(Box::new(move |_ctx, id, inherited_value| {
            activity_clear
                .borrow_mut()
                .push(SelectorActivity::SettingClear {
                    id: id.to_string(),
                    inherited: inherited_value.to_string(),
                });
        }));
        // Esc closes and releases the host's live handles.
        let stack_close = Rc::clone(stack);
        let editor_close = Rc::clone(editor);
        let settings_ui_close = Rc::clone(settings_ui);
        l.on_close = Some(Box::new(move |ctx| {
            *settings_ui_close.borrow_mut() = None;
            close_top(&stack_close, ctx, &editor_close);
        }));
    }
    let title = if project_mode {
        "Project settings"
    } else {
        "Settings"
    };
    let window = push_window(
        stack,
        chrome,
        title,
        settings_subtitle(project_mode),
        to_widget_ref(Rc::clone(&list)),
        focus,
        OverlayPlacement::Large,
    );
    *settings_ui.borrow_mut() = Some(SettingsUi { list, window });
}

/// The settings window's key-hint subtitle. The project window advertises the
/// clear chord, all labels resolved from keybinding data (Spec F).
fn settings_subtitle(project_mode: bool) -> String {
    let mut hint = subtitle_edit_close("edit");
    if project_mode && let Some(clear) = default_action_shortcut(ACTION_SETTINGS_CLEAR) {
        hint.push_str(&format!("  \u{2022}  {clear} to clear"));
    }
    hint
}

/// Build and push the submenu for the activated settings row `id`, resolving
/// its kind from the id (the settings schema is fixed). The confirm path
/// updates the parent row, stages the change, and returns to the window.
#[allow(clippy::too_many_arguments)]
fn open_setting_submenu(
    ctx: &mut EventContext,
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    activity: &Rc<RefCell<Vec<SelectorActivity>>>,
    parent: &Rc<RefCell<SettingList>>,
    target: ConfigTarget,
    catalogs: &SettingsCatalogs,
    id: &str,
    value: &str,
) {
    match id {
        MODEL_SETTING_ID => {
            let current = value
                .split_once('/')
                .map(|(p, i)| (p.to_string(), i.to_string()));
            let items = model_items(&catalogs.models, current.as_ref());
            let current_key = current
                .as_ref()
                .and_then(|(p, i)| {
                    catalogs
                        .models
                        .iter()
                        .find(|m| m.provider == *p && m.id == *i)
                })
                .map(model_filter_key);
            let catalog = Arc::clone(&catalogs.models);
            open_picker_submenu(
                ctx,
                stack,
                editor,
                chrome,
                parent,
                activity,
                target,
                "Select model",
                id.to_string(),
                items,
                current_key,
                Box::new(move |item| {
                    catalog
                        .iter()
                        .find(|m| model_filter_key(m) == item.filter_key)
                        .map(|m| format!("{}/{}", m.provider, m.id))
                }),
            );
        }
        "thinking" => {
            let items = thinking_items(value);
            open_picker_submenu(
                ctx,
                stack,
                editor,
                chrome,
                parent,
                activity,
                target,
                "Thinking effort",
                id.to_string(),
                items,
                Some(value.to_string()),
                Box::new(|item| Some(item.filter_key.clone())),
            );
        }
        "theme" => {
            let items = theme_items(&catalogs.themes, value);
            open_picker_submenu(
                ctx,
                stack,
                editor,
                chrome,
                parent,
                activity,
                target,
                "Theme",
                id.to_string(),
                items,
                Some(value.to_string()),
                Box::new(|item| Some(item.filter_key.clone())),
            );
        }
        "disabled_tools" => open_toggles_submenu(
            ctx,
            stack,
            editor,
            chrome,
            parent,
            activity,
            target,
            "Disabled tools",
            id.to_string(),
            &catalogs.tools,
            value,
        ),
        "disabled_skills" => open_toggles_submenu(
            ctx,
            stack,
            editor,
            chrome,
            parent,
            activity,
            target,
            "Disabled skills",
            id.to_string(),
            &catalogs.skills,
            value,
        ),
        // Everything else edits as free-form text (model_url, the compaction
        // numbers).
        _ => open_text_submenu(
            ctx,
            stack,
            editor,
            chrome,
            parent,
            activity,
            target,
            id.to_string(),
            value,
        ),
    }
}

/// Push a pick-list submenu. `resolve` turns the confirmed item into the value
/// to commit (a model row resolves `provider/id`; the rest use the filter key).
#[allow(clippy::too_many_arguments)]
fn open_picker_submenu(
    ctx: &mut EventContext,
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    parent: &Rc<RefCell<SettingList>>,
    activity: &Rc<RefCell<Vec<SelectorActivity>>>,
    target: ConfigTarget,
    title: &str,
    id: String,
    items: Vec<SelectItem>,
    current_key: Option<String>,
    resolve: Box<dyn Fn(&SelectItem) -> Option<String>>,
) {
    let select = Rc::new(RefCell::new(FilterableSelect::new(
        items,
        chrome.select.clone(),
    )));
    if let Some(key) = current_key {
        select
            .borrow()
            .select_matching(|item| item.filter_key == key);
    }
    let focus = select.borrow().focus_target();
    {
        let mut sel = select.borrow_mut();
        let stack_c = Rc::clone(stack);
        let editor_c = Rc::clone(editor);
        let parent_c = Rc::clone(parent);
        let activity_c = Rc::clone(activity);
        sel.on_confirm = Some(Box::new(move |ctx, item| {
            if let Some(value) = resolve(item) {
                parent_c.borrow().set_value(&id, &value);
                activity_c
                    .borrow_mut()
                    .push(SelectorActivity::SettingChange {
                        target,
                        id: id.clone(),
                        value,
                    });
            }
            close_top(&stack_c, ctx, &editor_c);
        }));
        let stack_cancel = Rc::clone(stack);
        let editor_cancel = Rc::clone(editor);
        sel.on_cancel = Some(Box::new(move |ctx| {
            close_top(&stack_cancel, ctx, &editor_cancel)
        }));
    }
    push_submenu(ctx, stack, chrome, title, to_widget_ref(select), focus);
}

/// Push a one-line text-editor submenu committing the trimmed value on Enter.
#[allow(clippy::too_many_arguments)]
fn open_text_submenu(
    ctx: &mut EventContext,
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    parent: &Rc<RefCell<SettingList>>,
    activity: &Rc<RefCell<Vec<SelectorActivity>>>,
    target: ConfigTarget,
    id: String,
    value: &str,
) {
    let overlay = Rc::new(RefCell::new(TextEditOverlay::new(value)));
    let focus = overlay.borrow().focus_target();
    {
        let stack_c = Rc::clone(stack);
        let editor_c = Rc::clone(editor);
        let parent_c = Rc::clone(parent);
        let activity_c = Rc::clone(activity);
        let id_submit = id.clone();
        overlay.borrow().set_on_submit(Box::new(move |ctx, text| {
            parent_c.borrow().set_value(&id_submit, text);
            activity_c
                .borrow_mut()
                .push(SelectorActivity::SettingChange {
                    target,
                    id: id_submit.clone(),
                    value: text.to_string(),
                });
            close_top(&stack_c, ctx, &editor_c);
        }));
        let stack_cancel = Rc::clone(stack);
        let editor_cancel = Rc::clone(editor);
        overlay.borrow_mut().on_cancel = Some(Box::new(move |ctx| {
            close_top(&stack_cancel, ctx, &editor_cancel)
        }));
    }
    push_submenu(
        ctx,
        stack,
        chrome,
        "Edit value",
        to_widget_ref(overlay),
        focus,
    );
}

/// Push a nested enable/disable toggle-list submenu. Closing commits the
/// aggregated `", "`-joined disabled set when it changed, matching the
/// window's `disabled_*` list semantics.
#[allow(clippy::too_many_arguments)]
fn open_toggles_submenu(
    ctx: &mut EventContext,
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    parent: &Rc<RefCell<SettingList>>,
    activity: &Rc<RefCell<Vec<SelectorActivity>>>,
    target: ConfigTarget,
    title: &str,
    id: String,
    names: &[String],
    value: &str,
) {
    let disabled = split_names(value);
    let initial = names_join(&disabled);
    // Shared with the toggle rows' on_change so the close handler reads the
    // final set.
    let set = Rc::new(RefCell::new(disabled.clone()));
    let rows: Vec<SettingRow> = names
        .iter()
        .map(|name| SettingRow {
            id: name.clone(),
            label: name.clone(),
            value: if disabled.contains(name) {
                "disabled".to_string()
            } else {
                "enabled".to_string()
            },
            description: String::new(),
            kind: RowKind::Cycle(vec!["enabled".to_string(), "disabled".to_string()]),
            inherited: false,
            clear_to: String::new(),
        })
        .collect();
    let list = Rc::new(RefCell::new(SettingList::new(
        rows,
        chrome.select.clone(),
        false,
    )));
    let focus = list.borrow().focus_target();
    {
        let mut l = list.borrow_mut();
        let set_change = Rc::clone(&set);
        l.on_change = Some(Box::new(move |_ctx, name, value| {
            let mut set = set_change.borrow_mut();
            if value == "disabled" {
                set.insert(name.to_string());
            } else {
                set.remove(name);
            }
        }));
        let stack_close = Rc::clone(stack);
        let editor_close = Rc::clone(editor);
        let parent_close = Rc::clone(parent);
        let activity_close = Rc::clone(activity);
        let set_close = Rc::clone(&set);
        l.on_close = Some(Box::new(move |ctx| {
            let joined = names_join(&set_close.borrow());
            // A no-op close stays silent: committing an unchanged set would
            // fold a pointless notice.
            if joined != initial {
                parent_close.borrow().set_value(&id, &joined);
                activity_close
                    .borrow_mut()
                    .push(SelectorActivity::SettingChange {
                        target,
                        id: id.clone(),
                        value: joined,
                    });
            }
            close_top(&stack_close, ctx, &editor_close);
        }));
    }
    push_submenu(ctx, stack, chrome, title, to_widget_ref(list), focus);
}

/// `", "`-joined display form of a disabled set (sorted, empty for "none").
fn names_join(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// Push a submenu overlay and move focus onto it. Submenus open from a
/// widget's capture handler, so the [`EventContext`] moves focus directly.
fn push_submenu(
    ctx: &mut EventContext,
    stack: &Rc<RefCell<OverlayStack>>,
    chrome: &OverlayChrome,
    title: &str,
    child: WidgetRef,
    focus: WidgetRef,
) {
    push_window(
        stack,
        chrome,
        title,
        subtitle_confirm_close(),
        child,
        Rc::clone(&focus),
        OverlayPlacement::Small,
    );
    ctx.request_focus(focus);
    ctx.redraw = true;
}

// ============================================================================
// Skills window
// ============================================================================

/// One discovered skill, as the window needs it.
pub(crate) struct SkillRow {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) path: String,
    pub(crate) enabled: bool,
    pub(crate) disable_model_invocation: bool,
}

/// The skills window's fill handle: the drive loop replaces the loading
/// placeholder with the discovered rows through it once the off-loop
/// discovery walk lands. Parked on open so the fill targets this captured
/// list, never the stack's `top()`.
pub(crate) type SkillsFill = Rc<RefCell<SettingList>>;

/// Build one skills-window row per discovered skill. Each Enter toggles the
/// highlighted skill (see [`open_skills`]'s `on_change`), so every row is a
/// bool cycle. Shared by the drive loop's skills fill arm.
pub(crate) fn build_skill_rows(skills: Vec<SkillRow>) -> Vec<SettingRow> {
    skills
        .into_iter()
        .map(|skill| {
            let mut description = String::new();
            if skill.disable_model_invocation {
                description.push_str("[model-invocation disabled] ");
            }
            description.push_str(&skill.description);
            description.push_str(&format!(" ({})", skill.path));
            SettingRow {
                id: skill.name.clone(),
                label: skill.name,
                value: if skill.enabled {
                    "enabled".to_string()
                } else {
                    "disabled".to_string()
                },
                description,
                kind: RowKind::Cycle(vec!["enabled".to_string(), "disabled".to_string()]),
                inherited: false,
                clear_to: String::new(),
            }
        })
        .collect()
}

/// A non-interactive placeholder row for the skills window (the loading
/// state, and the empty "no skills" state). The empty cycle set makes Enter
/// inert (see the [`SettingList`] Enter handler), so the placeholder never
/// toggles or fires a change, and its blank id keeps any stray toggle from
/// naming a real skill.
pub(crate) fn skills_placeholder_row(label: &str) -> SettingRow {
    SettingRow {
        id: String::new(),
        label: label.to_string(),
        value: String::new(),
        description: String::new(),
        kind: RowKind::Cycle(Vec::new()),
        inherited: false,
        clear_to: String::new(),
    }
}

/// Open the skills window with a loading placeholder and park its fill handle
/// for the drive loop.
///
/// The window opens immediately (on top of whatever is on the stack, e.g. the
/// palette) so the user sees it right away. Discovery walks the skill tree off
/// the loop, and the drive loop replaces the placeholder with the discovered
/// rows through the parked fill handle once the walk lands. The `on_change`
/// (toggle) and `on_close` wiring operates by row id/name, so it works
/// unchanged once real rows replace the placeholder. Does not move focus: the
/// caller (host) posts the refocus event.
pub(crate) fn open_skills(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    activity: &Rc<RefCell<Vec<SelectorActivity>>>,
    fill_slot: &Rc<RefCell<Option<SkillsFill>>>,
) {
    let list = Rc::new(RefCell::new(SettingList::new(
        vec![skills_placeholder_row("Loading skills\u{2026}")],
        chrome.select.clone(),
        false,
    )));
    let focus = list.borrow().focus_target();
    {
        let mut l = list.borrow_mut();
        let activity_change = Rc::clone(activity);
        l.on_change = Some(Box::new(move |_ctx, name, value| {
            activity_change
                .borrow_mut()
                .push(SelectorActivity::SkillToggle {
                    name: name.to_string(),
                    disable: value == "disabled",
                });
        }));
        let stack_close = Rc::clone(stack);
        let editor_close = Rc::clone(editor);
        l.on_close = Some(Box::new(move |ctx| {
            close_top(&stack_close, ctx, &editor_close)
        }));
    }
    push_window(
        stack,
        chrome,
        "Skills",
        subtitle_edit_close("toggle"),
        to_widget_ref(Rc::clone(&list)),
        focus,
        OverlayPlacement::Large,
    );
    // Park the fill handle so the drive loop can replace the placeholder with
    // the discovered rows. The fill targets this captured list, never the
    // stack's `top()`, so a confirm of another opener from the still-open
    // palette can't misdirect it.
    *fill_slot.borrow_mut() = Some(list);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styles() -> SelectStyles {
        SelectStyles::default()
    }

    fn key(codepoint: u32, mods: Modifiers) -> Event {
        Event::KeyPress(Key {
            codepoint,
            mods,
            ..Key::default()
        })
    }

    fn enter() -> Event {
        key(Key::ENTER, Modifiers::empty())
    }

    fn send(list: &mut SettingList, event: &Event) {
        let mut ctx = EventContext::new();
        list.capture_event(&mut ctx, event);
    }

    fn cycle_row(id: &str, value: &str) -> SettingRow {
        SettingRow {
            id: id.to_string(),
            label: id.to_string(),
            value: value.to_string(),
            description: String::new(),
            kind: RowKind::Cycle(vec!["true".to_string(), "false".to_string()]),
            inherited: false,
            clear_to: String::new(),
        }
    }

    #[test]
    fn cycle_row_advances_value_and_fires_on_change() {
        let mut list = SettingList::new(vec![cycle_row("b", "true")], styles(), false);
        let sink: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
        let sink_c = Rc::clone(&sink);
        list.on_change = Some(Box::new(move |_ctx, id, value| {
            sink_c
                .borrow_mut()
                .push((id.to_string(), value.to_string()));
        }));

        send(&mut list, &enter());
        assert_eq!(
            sink.borrow().as_slice(),
            &[("b".to_string(), "false".to_string())]
        );
        // The row's displayed value advanced optimistically.
        assert_eq!(list.selected().map(|r| r.value), Some("false".to_string()));
    }

    #[test]
    fn submenu_row_fires_on_open_not_on_change() {
        let mut row = cycle_row("theme", "dark");
        row.kind = RowKind::Submenu;
        let mut list = SettingList::new(vec![row], styles(), false);
        let opened: Rc<RefCell<Option<(String, String)>>> = Rc::new(RefCell::new(None));
        let opened_c = Rc::clone(&opened);
        list.on_open = Some(Box::new(move |_ctx, id, value| {
            *opened_c.borrow_mut() = Some((id.to_string(), value.to_string()));
        }));
        list.on_change = Some(Box::new(|_ctx, _id, _v| {
            panic!("submenu row must not cycle")
        }));

        send(&mut list, &enter());
        assert_eq!(
            *opened.borrow(),
            Some(("theme".to_string(), "dark".to_string()))
        );
    }

    #[test]
    fn clear_chord_reverts_override_and_fires_on_clear() {
        let mut override_row = cycle_row("theme", "dark");
        override_row.inherited = false;
        override_row.clear_to = "light".to_string();
        let mut inherited_row = cycle_row("auto_compact", "true");
        inherited_row.inherited = true;
        let mut list = SettingList::new(vec![override_row, inherited_row], styles(), true);
        let sink: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
        let sink_c = Rc::clone(&sink);
        list.on_clear = Some(Box::new(move |_ctx, id, inherited| {
            sink_c
                .borrow_mut()
                .push((id.to_string(), inherited.to_string()));
        }));

        // Cursor on the override row: the clear reverts it to the inherited
        // value and hands the value to the host.
        send(&mut list, &key(u32::from('x'), Modifiers::CTRL));
        assert_eq!(
            sink.borrow().as_slice(),
            &[("theme".to_string(), "light".to_string())]
        );
        let selected = list.selected().expect("row");
        assert_eq!(selected.value, "light");
        // The row is now inherited, so a second clear is inert.
        send(&mut list, &key(u32::from('x'), Modifiers::CTRL));
        assert_eq!(
            sink.borrow().len(),
            1,
            "clearing an inherited row is a no-op"
        );
    }

    #[test]
    fn user_window_ignores_the_clear_chord() {
        let mut list = SettingList::new(vec![cycle_row("theme", "dark")], styles(), false);
        let fired = Rc::new(RefCell::new(false));
        let fired_c = Rc::clone(&fired);
        list.on_clear = Some(Box::new(move |_ctx, _id, _v| *fired_c.borrow_mut() = true));
        send(&mut list, &key(u32::from('x'), Modifiers::CTRL));
        assert!(
            !*fired.borrow(),
            "the clear chord is inert outside project mode"
        );
    }

    #[test]
    fn escape_fires_on_close() {
        let mut list = SettingList::new(vec![cycle_row("b", "true")], styles(), false);
        let closed = Rc::new(RefCell::new(false));
        let closed_c = Rc::clone(&closed);
        list.on_close = Some(Box::new(move |_ctx| *closed_c.borrow_mut() = true));
        send(&mut list, &key(Key::ESCAPE, Modifiers::empty()));
        assert!(*closed.borrow());
    }

    /// `set_rows` swaps the whole row set (the skills window's async fill) and
    /// re-applies the filter: the old rows are gone, the new rows show, the
    /// cursor resets to the first re-derived row, and the refreshed item count
    /// makes every new row reachable. Navigating to the second row is the part
    /// that fails on a stale filter: without the re-derivation the item count
    /// still reflects the single pre-swap row and clamps the cursor to the top.
    #[test]
    fn set_rows_replaces_rows_and_reapplies_the_filter() {
        let mut list = SettingList::new(vec![cycle_row("old", "true")], styles(), false);
        list.set_rows(vec![cycle_row("a", "true"), cycle_row("b", "false")]);
        assert_eq!(list.value_of("old"), None, "old row gone");
        assert_eq!(list.value_of("a").as_deref(), Some("true"));
        assert_eq!(list.value_of("b").as_deref(), Some("false"));
        assert_eq!(
            list.selected().map(|r| r.id),
            Some("a".to_string()),
            "cursor reset to the first re-derived row"
        );
        send(&mut list, &key(Key::DOWN, Modifiers::empty()));
        assert_eq!(
            list.selected().map(|r| r.id),
            Some("b".to_string()),
            "the second re-derived row is reachable via the refreshed item count"
        );
    }

    /// The skills placeholder row is inert on Enter: its empty cycle set fires
    /// no change and does not panic.
    #[test]
    fn skills_placeholder_row_is_inert_on_enter() {
        let mut list = SettingList::new(
            vec![skills_placeholder_row("Loading skills\u{2026}")],
            styles(),
            false,
        );
        list.on_change = Some(Box::new(|_ctx, _id, _v| {
            panic!("placeholder row must not fire a change")
        }));
        send(&mut list, &enter());
    }

    /// Every schema option surfaces as a row, with the `model_api` /
    /// `model_name` pair folded into the model row, mirroring `aj`'s drift
    /// guard.
    #[test]
    fn build_setting_rows_covers_every_schema_option() {
        let values = SettingsValues::from_config(&Config::default(), &[]);
        let inherited = SettingsValues::from_config(&Config::default(), &[]);
        let rows = build_setting_rows(&values, &inherited, false, &BTreeSet::new());
        for option in Config::OPTIONS {
            // `model_name` folds into the model row (`model_api`).
            if option.name == "model_name" {
                continue;
            }
            let id = row_id_for(option.name);
            assert!(
                rows.iter().any(|r| r.id == id),
                "config option {} has no settings-window row",
                option.name
            );
        }
        // The model pair folds to exactly one row.
        assert_eq!(rows.iter().filter(|r| r.id == MODEL_SETTING_ID).count(), 1);
        assert!(!rows.iter().any(|r| r.id == "model_name"));
    }

    /// `from_config` seeds `show_frame_stats` and it surfaces as a bool cycle
    /// row carrying the configured value.
    #[test]
    fn show_frame_stats_seeds_and_surfaces_as_a_bool_cycle_row() {
        let mut config = Config::default();
        config.show_frame_stats = true;
        let values = SettingsValues::from_config(&config, &[]);
        assert!(values.show_frame_stats, "from_config seeds the flag");

        let inherited = SettingsValues::from_config(&Config::default(), &[]);
        let rows = build_setting_rows(&values, &inherited, false, &BTreeSet::new());
        let row = rows
            .iter()
            .find(|r| r.id == "show_frame_stats")
            .expect("show_frame_stats row is present");
        assert_eq!(row.value, "true");
        match &row.kind {
            RowKind::Cycle(vals) => assert_eq!(vals, &["true".to_string(), "false".to_string()]),
            RowKind::Submenu => panic!("show_frame_stats must be a bool cycle row"),
        }
    }

    #[test]
    fn next_cycle_value_wraps_and_falls_back() {
        let values = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(next_cycle_value(&values, "a"), "b");
        assert_eq!(next_cycle_value(&values, "c"), "a");
        // An unknown current lands on the first value.
        assert_eq!(next_cycle_value(&values, "z"), "a");
    }

    #[test]
    fn thinking_items_tag_the_current_level() {
        let items = thinking_items("high");
        assert!(items.iter().any(|i| i.label == "high (current)"));
        assert!(items.iter().all(|i| i.filter_key != "high (current)"));
    }

    fn desc_row(id: &str, value: &str, description: &str) -> SettingRow {
        SettingRow {
            id: id.to_string(),
            label: id.to_string(),
            value: value.to_string(),
            description: description.to_string(),
            kind: RowKind::Cycle(vec!["true".to_string(), "false".to_string()]),
            inherited: false,
            clear_to: String::new(),
        }
    }

    /// The cursored row's description renders in a panel below the list,
    /// indented 2, and no description text appears inline on the `label  value`
    /// rows.
    #[test]
    fn description_renders_below_the_list_not_inline() {
        use crate::test_support::{draw_ctx, rows};

        let mut list = SettingList::new(
            vec![
                desc_row("alpha", "true", "First option description."),
                desc_row("bravo", "false", "Second option description."),
            ],
            styles(),
            false,
        );
        let ctx = draw_ctx(60, Some(20));
        let text = rows(&list.draw(&ctx));

        // The list rows carry only `label  value`: no inline description.
        assert!(
            text.iter().any(|l| l == "alpha  true"),
            "first row is label+value only: {text:?}"
        );
        assert!(
            text.iter().any(|l| l == "bravo  false"),
            "second row is label+value only: {text:?}"
        );
        // The cursored row's description sits below the list, indented 2. The
        // position check guards against drawing the panel above the list.
        let last_list_row = text
            .iter()
            .rposition(|l| l == "alpha  true" || l == "bravo  false")
            .expect("list rows present");
        let desc_row = text
            .iter()
            .position(|l| l == "  First option description.")
            .expect("cursored description rendered, indented 2");
        assert!(
            desc_row > last_list_row,
            "description panel sits below the list rows (desc {desc_row}, last row {last_list_row}): {text:?}"
        );
        // Only the cursored row's description shows.
        assert!(
            !text
                .iter()
                .any(|l| l.contains("Second option description.")),
            "the non-cursored row's description is not shown: {text:?}"
        );
    }

    /// Moving the cursor down swaps the panel to the newly cursored row's
    /// description.
    #[test]
    fn description_panel_tracks_the_cursor() {
        use crate::test_support::{draw_ctx, rows};

        let mut list = SettingList::new(
            vec![
                desc_row("alpha", "true", "First option description."),
                desc_row("bravo", "false", "Second option description."),
            ],
            styles(),
            false,
        );
        let ctx = draw_ctx(60, Some(20));

        send(&mut list, &key(Key::DOWN, Modifiers::empty()));
        let text = rows(&list.draw(&ctx));
        assert!(
            text.iter().any(|l| l == "  Second option description."),
            "the panel follows the cursor to the second row: {text:?}"
        );
        assert!(
            !text.iter().any(|l| l.contains("First option description.")),
            "the first row's description is no longer shown: {text:?}"
        );
    }

    /// Enough no-description rows to overflow a short list slot. Without a
    /// description the below-list panel is dropped, so the slot spans the whole
    /// overlay height minus the filter row and its separator.
    fn plain_rows(n: usize) -> Vec<SettingRow> {
        (0..n)
            .map(|i| cycle_row(&format!("row{i}"), "true"))
            .collect()
    }

    /// The composited rows carrying the vertical thumb glyph on the list slot's
    /// right edge.
    fn thumb_rows(surface: &Surface, width: u16) -> Vec<usize> {
        let last_col = usize::from(width - 1);
        crate::test_support::flatten(surface)
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row.get(last_col)
                    .is_some_and(|c| c.char.grapheme() == "\u{2590}")
            })
            .map(|(r, _)| r)
            .collect()
    }

    /// A list that overflows its slot draws the vertical thumb on the right
    /// edge, in the list slot below the filter row.
    #[test]
    fn scrollbar_thumb_appears_when_the_list_overflows() {
        use crate::test_support::draw_ctx;

        let mut list = SettingList::new(plain_rows(12), styles(), false);
        let ctx = draw_ctx(30, Some(6));
        let rows = thumb_rows(&list.draw(&ctx), 30);
        assert!(!rows.is_empty(), "a thumb is drawn for an overflowing list");
        assert!(
            rows.iter().all(|&r| r >= 2),
            "the thumb stays in the list slot below the filter row: {rows:?}"
        );
    }

    /// Scrolling the cursor to the bottom moves the thumb down: its top row
    /// sits lower than when the list is pinned at the top.
    #[test]
    fn scrollbar_thumb_tracks_scrolling() {
        use crate::test_support::draw_ctx;

        let mut list = SettingList::new(plain_rows(12), styles(), false);
        let ctx = draw_ctx(30, Some(6));
        let top = *thumb_rows(&list.draw(&ctx), 30)
            .first()
            .expect("thumb drawn at the top");
        // Drive the cursor to the bottom, redrawing so each move reconciles the
        // scroll position the thumb reads on the next draw.
        for _ in 0..12 {
            send(&mut list, &key(Key::DOWN, Modifiers::empty()));
            let _ = list.draw(&ctx);
        }
        let bottom = *thumb_rows(&list.draw(&ctx), 30)
            .first()
            .expect("thumb still drawn at the bottom");
        assert!(
            bottom > top,
            "the thumb moved down as the list scrolled (top {top}, bottom {bottom})"
        );
    }

    /// A list that fits its slot draws no thumb, matching the content overlays.
    #[test]
    fn scrollbar_thumb_absent_when_the_list_fits() {
        use crate::test_support::draw_ctx;

        let mut list = SettingList::new(plain_rows(3), styles(), false);
        let ctx = draw_ctx(30, Some(12));
        assert!(
            thumb_rows(&list.draw(&ctx), 30).is_empty(),
            "no thumb is drawn when the list fits the slot"
        );
    }
}
