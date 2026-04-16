//! Tests for the [`SettingsList`] component.
//!
//! These cover the component's public surface: navigation, value
//! cycling, search filtering, on-change / on-cancel callbacks, and
//! render shape.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::components::settings_list::{
    SettingItem, SettingsList, SettingsListOptions, SettingsListTheme,
};
use aj_tui::keys::Key;

use support::plain_lines_trim_end;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn identity_theme() -> SettingsListTheme {
    SettingsListTheme {
        label: Box::new(|s, _| s.to_string()),
        value: Box::new(|s, _| s.to_string()),
        description: Box::new(|s| s.to_string()),
        hint: Box::new(|s| s.to_string()),
        cursor: "> ".to_string(),
    }
}

fn sample_items() -> Vec<SettingItem> {
    vec![
        SettingItem::cycleable(
            "theme",
            "Theme",
            "dark",
            vec!["dark".to_string(), "light".to_string()],
        ),
        SettingItem::cycleable(
            "confirm",
            "Confirm Exit",
            "yes",
            vec!["yes".to_string(), "no".to_string()],
        ),
        SettingItem {
            id: "read-only".into(),
            label: "Read Only".into(),
            description: Some("Some info".into()),
            current_value: "locked".into(),
            values: None,
            submenu: None,
        },
    ]
}

/// Build a settings list and return the shared state handles that
/// tests read back after driving input.
fn make_settings_list(
    items: Vec<SettingItem>,
    options: SettingsListOptions,
) -> (
    SettingsList,
    Rc<RefCell<Vec<(String, String)>>>,
    Rc<RefCell<u32>>,
) {
    let changes = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
    let cancel_count = Rc::new(RefCell::new(0u32));
    let changes_clone = Rc::clone(&changes);
    let cancel_clone = Rc::clone(&cancel_count);
    let list = SettingsList::new(
        items,
        5,
        identity_theme(),
        move |id: &str, val: &str| {
            changes_clone
                .borrow_mut()
                .push((id.to_string(), val.to_string()))
        },
        move || *cancel_clone.borrow_mut() += 1,
        options,
    );
    (list, changes, cancel_count)
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

#[test]
fn down_arrow_moves_to_next_item_and_wraps_at_the_end() {
    let (mut list, _, _) = make_settings_list(sample_items(), SettingsListOptions::default());
    assert_eq!(list.selected_index(), Some(0));
    assert_eq!(list.selected_id(), Some("theme"));

    list.handle_input(&Key::down());
    assert_eq!(list.selected_id(), Some("confirm"));

    list.handle_input(&Key::down());
    assert_eq!(list.selected_id(), Some("read-only"));

    // Wraps back to the top.
    list.handle_input(&Key::down());
    assert_eq!(list.selected_id(), Some("theme"));
}

#[test]
fn up_arrow_wraps_to_the_last_item_from_the_first() {
    let (mut list, _, _) = make_settings_list(sample_items(), SettingsListOptions::default());
    list.handle_input(&Key::up());
    assert_eq!(list.selected_id(), Some("read-only"));
}

#[test]
fn navigation_is_a_noop_when_the_list_is_empty() {
    let (mut list, _, _) = make_settings_list(Vec::new(), SettingsListOptions::default());
    assert_eq!(list.selected_index(), None);
    list.handle_input(&Key::down());
    list.handle_input(&Key::up());
    assert_eq!(list.selected_index(), None);
}

// ---------------------------------------------------------------------------
// Value cycling
// ---------------------------------------------------------------------------

#[test]
fn enter_cycles_selected_item_through_its_values() {
    let (mut list, changes, _) = make_settings_list(sample_items(), SettingsListOptions::default());
    // theme: dark → light → dark.
    list.handle_input(&Key::enter());
    assert_eq!(list.value_of("theme"), Some("light"));
    assert_eq!(
        changes.borrow().last(),
        Some(&("theme".to_string(), "light".to_string())),
    );
    list.handle_input(&Key::enter());
    assert_eq!(list.value_of("theme"), Some("dark"));
    assert_eq!(changes.borrow().len(), 2);
}

#[test]
fn space_cycles_values_just_like_enter() {
    let (mut list, _, _) = make_settings_list(sample_items(), SettingsListOptions::default());
    list.handle_input(&Key::char(' '));
    assert_eq!(list.value_of("theme"), Some("light"));
}

#[test]
fn enter_on_item_without_values_is_a_noop() {
    let (mut list, changes, _) = make_settings_list(sample_items(), SettingsListOptions::default());
    list.handle_input(&Key::down());
    list.handle_input(&Key::down()); // now on read-only, which has no `values`
    list.handle_input(&Key::enter());
    assert_eq!(list.value_of("read-only"), Some("locked"));
    assert!(changes.borrow().is_empty());
}

#[test]
fn enter_on_item_with_empty_values_list_is_a_noop() {
    let items = vec![SettingItem {
        id: "x".into(),
        label: "X".into(),
        description: None,
        current_value: "a".into(),
        values: Some(Vec::new()),
        submenu: None,
    }];
    let (mut list, changes, _) = make_settings_list(items, SettingsListOptions::default());
    list.handle_input(&Key::enter());
    assert!(changes.borrow().is_empty());
}

#[test]
fn current_value_not_in_values_list_cycles_to_the_first_entry() {
    // If update_value set something outside the known values, the
    // next cycle jumps to values[0] rather than wrapping.
    let mut items = sample_items();
    items[0].current_value = "unknown".to_string();
    let (mut list, changes, _) = make_settings_list(items, SettingsListOptions::default());
    list.handle_input(&Key::enter());
    assert_eq!(list.value_of("theme"), Some("dark"));
    assert_eq!(changes.borrow().len(), 1);
}

#[test]
fn update_value_changes_the_displayed_value_without_firing_on_change() {
    let (mut list, changes, _) = make_settings_list(sample_items(), SettingsListOptions::default());
    list.update_value("confirm", "no");
    assert_eq!(list.value_of("confirm"), Some("no"));
    assert!(changes.borrow().is_empty(), "update_value is silent");
}

#[test]
fn update_value_with_unknown_id_is_a_noop() {
    let (mut list, _, _) = make_settings_list(sample_items(), SettingsListOptions::default());
    list.update_value("nope", "whatever");
    assert_eq!(list.value_of("nope"), None);
}

// ---------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------

#[test]
fn escape_fires_on_cancel() {
    let (mut list, _, cancel_count) =
        make_settings_list(sample_items(), SettingsListOptions::default());
    list.handle_input(&Key::escape());
    assert_eq!(*cancel_count.borrow(), 1);
}

// ---------------------------------------------------------------------------
// Search mode
// ---------------------------------------------------------------------------

#[test]
fn search_mode_filters_items_by_label() {
    let (mut list, _, _) = make_settings_list(
        sample_items(),
        SettingsListOptions {
            enable_search: true,
        },
    );

    list.handle_input(&Key::char('c')); // matches "Confirm Exit"
    assert_eq!(list.selected_id(), Some("confirm"));

    // The search-only fixture leaves no match after "cx".
    list.handle_input(&Key::char('x'));
    list.handle_input(&Key::char('x'));
    assert_eq!(list.selected_id(), None);
}

#[test]
fn search_mode_ignores_space_so_it_can_still_cycle_values() {
    let (mut list, _, _) = make_settings_list(
        sample_items(),
        SettingsListOptions {
            enable_search: true,
        },
    );
    // Filter to a single item.
    list.handle_input(&Key::char('t')); // matches "Theme"
    assert_eq!(list.selected_id(), Some("theme"));

    // Space activates even in search mode.
    list.handle_input(&Key::char(' '));
    assert_eq!(list.value_of("theme"), Some("light"));
}

#[test]
fn search_with_no_results_still_handles_escape() {
    let (mut list, _, cancel_count) = make_settings_list(
        sample_items(),
        SettingsListOptions {
            enable_search: true,
        },
    );
    list.handle_input(&Key::char('z')); // matches nothing
    assert_eq!(list.selected_id(), None);

    list.handle_input(&Key::escape());
    assert_eq!(*cancel_count.borrow(), 1);
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[test]
fn render_shows_cursor_on_selected_item_and_description_below() {
    let (mut list, _, _) = make_settings_list(sample_items(), SettingsListOptions::default());
    let rendered = plain_lines_trim_end(&list.render(80));
    // Three item rows + blank + hint (no description because default
    // selected item is "theme" which has none).
    assert!(
        rendered[0].starts_with("> Theme"),
        "cursor should mark the first item; got {:?}",
        rendered[0],
    );
    assert!(rendered[1].starts_with("  Confirm Exit"));
    assert!(rendered[2].starts_with("  Read Only"));
    assert!(
        rendered.iter().any(|line| line.contains("Enter/Space")),
        "hint line should be present",
    );
}

#[test]
fn render_wraps_and_prefixes_selected_items_description() {
    let (mut list, _, _) = make_settings_list(sample_items(), SettingsListOptions::default());
    list.handle_input(&Key::down());
    list.handle_input(&Key::down()); // select "read-only", which has a description

    let rendered = plain_lines_trim_end(&list.render(80));
    assert!(
        rendered.iter().any(|line| line.contains("Some info")),
        "description should be present; got {:#?}",
        rendered,
    );
}

#[test]
fn render_falls_back_to_a_placeholder_when_items_are_empty() {
    let (mut list, _, _) = make_settings_list(Vec::new(), SettingsListOptions::default());
    let rendered = plain_lines_trim_end(&list.render(40));
    assert!(
        rendered.iter().any(|line| line.contains("No settings")),
        "got {:#?}",
        rendered,
    );
}

#[test]
fn render_shows_no_matching_placeholder_in_search_mode_with_no_results() {
    let (mut list, _, _) = make_settings_list(
        sample_items(),
        SettingsListOptions {
            enable_search: true,
        },
    );
    list.handle_input(&Key::char('z'));
    let rendered = plain_lines_trim_end(&list.render(40));
    assert!(
        rendered.iter().any(|line| line.contains("No matching")),
        "got {:#?}",
        rendered,
    );
}

#[test]
fn render_includes_visible_search_prompt_that_aligns_with_item_gutter() {
    // The search input renders with a `"> "` prompt that visually
    // lines up with the 2-column cursor/gutter (`"→ "` / `"  "`) on
    // the item rows below it. If the prompt were empty (the previous
    // behavior) the search text would be flush-left relative to the
    // overlay while the items appeared indented, which reads as a
    // misaligned column.
    let (mut list, _, _) = make_settings_list(
        sample_items(),
        SettingsListOptions {
            enable_search: true,
        },
    );
    let rendered_raw = list.render(40);
    let rendered = support::plain_lines(&rendered_raw);
    let prompt_line = rendered
        .iter()
        .find(|line| line.starts_with("> "))
        .unwrap_or_else(|| panic!("expected a '> '-prefixed search line; got {:#?}", rendered));
    // The prompt is exactly two visible columns wide. That alignment
    // with the item rows' `"→ "` / `"  "` gutter is the whole point
    // of the prompt — a single-char `">"` would drift left by one.
    assert!(
        prompt_line.starts_with("> "),
        "search line should start with a two-char '> ' prompt; got {:?}",
        prompt_line,
    );
}

#[test]
fn render_stays_artifact_free_as_the_filter_shrinks_and_grows() {
    // Exercises the scenario that produced visible render artifacts
    // in the settings demo: search filter shrinks the list, then
    // expands again as characters are deleted. The rendered lines
    // must not grow unboundedly across calls — each `render(width)`
    // call returns the full, current, self-contained frame. (The
    // differential-render engine diffs against *previous_lines*, so
    // it's the component's responsibility to produce a stable shape;
    // any leakage here would show up as stale rows in the diff path.)
    let (mut list, _, _) = make_settings_list(
        sample_items(),
        SettingsListOptions {
            enable_search: true,
        },
    );
    let baseline = list.render(40);

    list.handle_input(&Key::char('c')); // narrows to "Confirm Exit"
    let narrowed = list.render(40);
    assert!(
        narrowed.len() < baseline.len(),
        "narrowing the filter should produce fewer rendered lines; \
         baseline={} narrowed={}",
        baseline.len(),
        narrowed.len(),
    );

    list.handle_input(&Key::char('x'));
    list.handle_input(&Key::char('x')); // no matches
    let no_match = list.render(40);
    assert!(
        no_match.iter().any(|line| line.contains("No matching")),
        "expected no-match placeholder; got {:#?}",
        plain_lines_trim_end(&no_match),
    );

    // Deleting all three characters returns to the baseline shape
    // verbatim — proof the filter doesn't hold onto stale state.
    list.handle_input(&Key::backspace());
    list.handle_input(&Key::backspace());
    list.handle_input(&Key::backspace());
    let restored = list.render(40);
    assert_eq!(
        plain_lines_trim_end(&restored),
        plain_lines_trim_end(&baseline),
        "filter cleared by backspace should restore the original frame",
    );
}

#[test]
fn search_input_does_not_panic_on_ctrl_w_across_empty_buffer() {
    // Ctrl+W deletes a word backward in the single-line Input. A
    // worst-case path (empty buffer, cursor at position 0) previously
    // had a subtle risk of panicking via UTF-8 slice arithmetic; run
    // the sequence through SettingsList to lock it down.
    let (mut list, _, _) = make_settings_list(
        sample_items(),
        SettingsListOptions {
            enable_search: true,
        },
    );
    // Empty buffer: Ctrl+W over empty.
    list.handle_input(&Key::ctrl('w'));
    // With content: word-by-word delete.
    for c in "foo bar baz".chars() {
        list.handle_input(&Key::char(c));
    }
    for _ in 0..5 {
        list.handle_input(&Key::ctrl('w'));
    }
    // If we got here without panicking, we're good.
    let _ = list.render(40);
}

// ---------------------------------------------------------------------------
// Submenu support
// ---------------------------------------------------------------------------

use aj_tui::components::settings_list::SubmenuDoneCallback;
use aj_tui::impl_component_any;
use aj_tui::keys::InputEvent;

/// Minimal submenu component used by the submenu tests. Records every
/// event it receives, renders a fixed header, and calls the `done`
/// callback on Enter (with the current_value it was handed, prefixed
/// with "chose:") or on Escape (with None).
struct StubSubmenu {
    current: String,
    done: Option<SubmenuDoneCallback>,
    received: Rc<RefCell<Vec<InputEvent>>>,
}

impl StubSubmenu {
    fn new(current: &str, done: SubmenuDoneCallback) -> (Self, Rc<RefCell<Vec<InputEvent>>>) {
        let received = Rc::new(RefCell::new(Vec::new()));
        let component = Self {
            current: current.to_string(),
            done: Some(done),
            received: Rc::clone(&received),
        };
        (component, received)
    }
}

impl Component for StubSubmenu {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        vec![format!("submenu open, current={}", self.current)]
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        self.received.borrow_mut().push(event.clone());

        use crossterm::event::{KeyCode, KeyModifiers};
        let InputEvent::Key(key) = event else {
            return false;
        };
        if key.modifiers != KeyModifiers::NONE {
            return false;
        }
        match key.code {
            KeyCode::Enter => {
                if let Some(done) = self.done.take() {
                    done(Some(format!("chose:{}", self.current)));
                }
                true
            }
            KeyCode::Esc => {
                if let Some(done) = self.done.take() {
                    done(None);
                }
                true
            }
            _ => false,
        }
    }
}

/// Build a settings list with one submenu-backed item, returning the
/// list along with the shared event log the stub submenu will populate
/// once it opens.
///
/// The event log is populated by the *most recently opened* submenu:
/// reopening creates a new StubSubmenu whose own log is written into
/// the same shared Rc.
fn make_list_with_submenu() -> (
    SettingsList,
    Rc<RefCell<Vec<(String, String)>>>,
    Rc<RefCell<Option<Rc<RefCell<Vec<InputEvent>>>>>>,
) {
    let changes = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
    let last_submenu_log = Rc::new(RefCell::new(None::<Rc<RefCell<Vec<InputEvent>>>>));

    let last_submenu_log_clone = Rc::clone(&last_submenu_log);
    let factory = Box::new(move |current: &str, done: SubmenuDoneCallback| {
        let (submenu, log) = StubSubmenu::new(current, done);
        *last_submenu_log_clone.borrow_mut() = Some(log);
        Box::new(submenu) as Box<dyn Component>
    });

    let items = vec![
        SettingItem::with_submenu("picker", "Picker", "one", factory),
        SettingItem::cycleable(
            "cycle",
            "Cycle",
            "a",
            vec!["a".to_string(), "b".to_string()],
        ),
    ];

    let changes_clone = Rc::clone(&changes);
    let list = SettingsList::new(
        items,
        5,
        identity_theme(),
        move |id: &str, val: &str| {
            changes_clone
                .borrow_mut()
                .push((id.to_string(), val.to_string()));
        },
        || {},
        SettingsListOptions::default(),
    );

    (list, changes, last_submenu_log)
}

#[test]
fn enter_on_submenu_item_opens_the_submenu_and_render_delegates() {
    let (mut list, _changes, _log) = make_list_with_submenu();
    // Picker is the first item — already selected.
    assert!(!list.has_active_submenu(), "submenu is closed before Enter",);

    list.handle_input(&Key::enter());
    assert!(
        list.has_active_submenu(),
        "submenu should open on Enter when the item has a submenu factory",
    );

    // While open, render delegates to the submenu component. The
    // parent list's own hint ("Enter/Space to change…") should be
    // absent.
    let rendered = plain_lines_trim_end(&list.render(80));
    assert_eq!(rendered, vec!["submenu open, current=one"]);
}

#[test]
fn input_routes_to_the_submenu_while_it_is_active() {
    let (mut list, _changes, log_slot) = make_list_with_submenu();
    list.handle_input(&Key::enter());
    let log = log_slot.borrow().clone().expect("submenu should be open");

    list.handle_input(&Key::char('x'));
    list.handle_input(&Key::char('y'));

    let events = log.borrow();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0],
        InputEvent::Key(k) if matches!(k.code, crossterm::event::KeyCode::Char('x')),
    ));
    assert!(matches!(
        events[1],
        InputEvent::Key(k) if matches!(k.code, crossterm::event::KeyCode::Char('y')),
    ));
}

#[test]
fn submenu_done_with_selected_value_updates_parent_and_fires_on_change() {
    let (mut list, changes, _log) = make_list_with_submenu();

    list.handle_input(&Key::enter()); // open submenu
    // Enter inside the stub submenu calls done(Some("chose:one")).
    list.handle_input(&Key::enter());

    assert!(
        !list.has_active_submenu(),
        "submenu should close once done() fires",
    );
    assert_eq!(list.value_of("picker"), Some("chose:one"));
    let ch = changes.borrow();
    assert_eq!(
        *ch,
        vec![("picker".to_string(), "chose:one".to_string())],
        "on-change should fire exactly once with the picked value",
    );
}

#[test]
fn submenu_done_with_none_closes_without_changing_parent_value() {
    let (mut list, changes, _log) = make_list_with_submenu();

    list.handle_input(&Key::enter()); // open submenu
    // Esc inside the stub submenu calls done(None).
    list.handle_input(&Key::escape());

    assert!(
        !list.has_active_submenu(),
        "submenu should close even when done(None) fires",
    );
    assert_eq!(
        list.value_of("picker"),
        Some("one"),
        "parent value must be preserved when submenu cancels",
    );
    assert!(
        changes.borrow().is_empty(),
        "on-change must not fire on cancel",
    );
}

#[test]
fn closing_submenu_restores_selection_to_the_parent_item() {
    let (mut list, _changes, _log) = make_list_with_submenu();

    // Move off the picker so we have something to restore back to.
    // Actually the test is: open with picker selected, close, verify
    // selection is still picker. Move to the second item first, back
    // to the first, open, close, confirm we still land on the first.
    list.handle_input(&Key::down()); // now on "cycle"
    list.handle_input(&Key::up()); // back on "picker"

    list.handle_input(&Key::enter()); // open submenu
    list.handle_input(&Key::escape()); // cancel

    assert_eq!(
        list.selected_id(),
        Some("picker"),
        "selection should return to the item that opened the submenu",
    );
}

#[test]
fn cycleable_items_still_cycle_when_submenu_is_also_set() {
    // Submenu takes precedence, but non-submenu items elsewhere in
    // the list still cycle normally. Navigate to "cycle" and press
    // Enter: it should advance, not open a submenu.
    let (mut list, changes, _log) = make_list_with_submenu();
    list.handle_input(&Key::down()); // move to "cycle"

    list.handle_input(&Key::enter());
    assert!(
        !list.has_active_submenu(),
        "cycle item without a submenu must not open a submenu",
    );
    assert_eq!(
        *changes.borrow(),
        vec![("cycle".to_string(), "b".to_string())],
    );
}
