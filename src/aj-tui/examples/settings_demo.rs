//! Interactive demo of the `SettingsList` component.
//!
//! Run with: `cargo run -p aj-tui --example settings_demo`.
//!
//! A welcome banner, then a settings list surfaced inside an overlay.
//! Exercises every `SettingsList` shape:
//!
//! - Fuzzy-search filtering on label (type to narrow).
//! - Two-value cycling (`Theme`, `Confirm Exit`).
//! - Multi-value cycling (`Verbosity`).
//! - Descriptions on every item.
//! - A submenu-backed item (`Editor`) that opens a nested
//!   `SelectList` picker; `Enter` commits, `Escape` cancels, and the
//!   parent list's value updates.
//!
//! Press Escape to close the overlay (or the submenu, if one is open).
//! Press Ctrl+C to exit.

use std::cell::RefCell;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::components::select_list::{SelectItem, SelectList};
use aj_tui::components::settings_list::{
    SettingItem, SettingsList, SettingsListOptions, SettingsListTheme, SubmenuDoneCallback,
    SubmenuFactory,
};
use aj_tui::components::text::Text;
use aj_tui::style;
use aj_tui::terminal::ProcessTerminal;
use aj_tui::tui::{OverlayAnchor, OverlayOptions, SizeValue, Tui, TuiEvent};

/// Build the submenu factory for the `Editor` item. Capturing a pair
/// of closures keeps the `SelectList` callbacks self-contained: the
/// list's own Enter invokes `done(Some(chosen))`, and its Escape
/// invokes `done(None)`. The done callback is itself shared by both
/// paths via an `Rc<RefCell<Option<...>>>`.
fn editor_submenu_factory() -> SubmenuFactory {
    Box::new(move |current_value: &str, done: SubmenuDoneCallback| {
        // Store the done callback behind an Rc<RefCell<Option<_>>> so
        // both on_select and on_cancel can pull it (whichever fires
        // first wins; the slot is cleared after the first take).
        let slot: Rc<RefCell<Option<SubmenuDoneCallback>>> = Rc::new(RefCell::new(Some(done)));

        let items = vec![
            SelectItem::new("nano", "nano").with_description("Friendly, minimal editor"),
            SelectItem::new("vim", "vim")
                .with_description("Modal editor with a steep learning curve"),
            SelectItem::new("emacs", "emacs")
                .with_description("Extensible, self-documenting editor"),
            SelectItem::new("helix", "helix").with_description("Post-modern modal editor"),
            SelectItem::new("zed", "zed").with_description("Fast, collaborative editor"),
        ];

        let mut list = SelectList::new(items, 7);
        // Pre-select the current value so the highlight matches the
        // parent list's state.
        if let Some(pos) = list.items().iter().position(|i| i.value == current_value) {
            list.set_selected_index(pos);
        }

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

        Box::new(list) as Box<dyn Component>
    })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut tui = Tui::new(Box::new(ProcessTerminal::new()));
    if let Err(e) = tui.start() {
        eprintln!("Failed to start terminal: {}", e);
        return;
    }

    tui.root.add_child(Box::new(Text::new(
        "SettingsList demo.\n\nType to search · Up/Down to navigate · Enter/Space to activate · Esc closes · Ctrl+C exits.",
    )));

    // Theme: bold selected label, dim cursor/description/hint. Small
    // tweak from the default to make the demo easier to read on a
    // dark background.
    let theme = SettingsListTheme {
        label: Box::new(|s, selected| {
            if selected {
                style::bold(s)
            } else {
                s.to_string()
            }
        }),
        value: Box::new(|s, _| style::cyan(s)),
        description: Box::new(|s| style::dim(s)),
        hint: Box::new(|s| style::dim(s)),
        cursor: "→ ".to_string(),
    };

    let items = vec![
        SettingItem {
            id: "theme".into(),
            label: "Theme".into(),
            description: Some("Editor background color scheme.".into()),
            current_value: "dark".into(),
            values: Some(vec!["dark".into(), "light".into()]),
            submenu: None,
        },
        SettingItem {
            id: "verbosity".into(),
            label: "Verbosity".into(),
            description: Some(
                "How much detail to print while running. debug is the chattiest; off silences \
                 everything but errors."
                    .into(),
            ),
            current_value: "info".into(),
            values: Some(vec![
                "off".into(),
                "error".into(),
                "warn".into(),
                "info".into(),
                "debug".into(),
            ]),
            submenu: None,
        },
        SettingItem {
            id: "confirm".into(),
            label: "Confirm Exit".into(),
            description: Some("Prompt before quitting.".into()),
            current_value: "yes".into(),
            values: Some(vec!["yes".into(), "no".into()]),
            submenu: None,
        },
        SettingItem {
            id: "editor".into(),
            label: "Editor".into(),
            description: Some(
                "External editor to use when composing longer messages. Enter opens a picker."
                    .into(),
            ),
            current_value: "helix".into(),
            values: None,
            submenu: Some(editor_submenu_factory()),
        },
    ];

    // Record every change the user makes so the demo can display
    // recent choices alongside the settings list.
    let changes: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    let changes_for_cb = Rc::clone(&changes);

    // The on_cancel callback lets Esc dismiss the overlay. We close
    // the overlay by setting a shared "dismissed" flag that the main
    // loop polls between input events.
    let dismissed: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    let dismissed_for_cb = Rc::clone(&dismissed);

    let settings = SettingsList::new(
        items,
        8,
        theme,
        move |id: &str, value: &str| {
            changes_for_cb
                .borrow_mut()
                .push((id.to_string(), value.to_string()));
        },
        move || {
            *dismissed_for_cb.borrow_mut() = true;
        },
        SettingsListOptions {
            enable_search: true,
        },
    );

    let overlay_handle = tui.show_overlay(
        Box::new(settings),
        OverlayOptions {
            width: Some(SizeValue::Percent(80.0)),
            min_width: Some(40),
            max_height: Some(SizeValue::Absolute(20)),
            // Pin the vertical position rather than anchor Center: a
            // search-enabled list shrinks and grows as the user types
            // (zero matches → "No matching settings" is much shorter
            // than the full list). With Center, every height change
            // re-centers the overlay and the *top* row visibly jumps
            // up or down, which reads as bouncy. With an explicit
            // `row`, the top stays put and only the bottom pulls in
            // when the list shrinks. Matches the feel of host apps
            // that render the settings list inline at a fixed
            // position in the root container.
            anchor: OverlayAnchor::TopCenter,
            row: Some(SizeValue::Absolute(5)),
            ..Default::default()
        },
    );

    tui.render();

    'outer: loop {
        tokio::select! {
            maybe_event = tui.next_event() => match maybe_event {
                Some(TuiEvent::Input(event)) => {
                    if event.is_ctrl('c') {
                        break 'outer;
                    }
                    tui.handle_input(&event);
                }
                Some(TuiEvent::Render) => tui.render(),
                None => break 'outer,
            }
        }

        if *dismissed.borrow() {
            // User pressed Esc on the top-level settings list. Close
            // the overlay and exit.
            tui.hide_overlay(&overlay_handle);
            break 'outer;
        }
    }

    tui.stop();

    // Print a summary of what the user changed, so the demo leaves
    // something visible on the terminal after exit.
    let log = changes.borrow();
    if log.is_empty() {
        println!("No settings were changed.");
    } else {
        println!("Settings changes during this session:");
        for (id, value) in log.iter() {
            println!("  {id} -> {value}");
        }
    }
}
