//! The current branch's environment, edited through the settings list.
//!
//! Each confirmation changes one key at the host. The host records the complete
//! resulting map, so editing a stale row cannot overwrite unrelated variables.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::rc::{Rc, Weak};

use aj_app::keybindings::{ACTION_SETTINGS_CLEAR, action_shortcut};
use aj_session::validate_session_env;
use vaxis::vxfw::{EventContext, to_widget_ref};

use crate::interactive::OverlayHandles;
use crate::overlay::{OverlayPlacement, close_top, subtitle_edit_close};
use crate::settings_ui::{
    RowKind, SelectorActivity, SettingList, SettingRow, TextEditOverlay, push_window,
};
use crate::toasts::show_toast;

// NUL cannot occur in an environment key.
const ADD_ROW: &str = "\0add";
const FIELD_HINT: &str =
    r#"Enter to set  •  Esc to cancel  •  escapes: \n \t \\ \" (no outer quotes)"#;

#[derive(Clone)]
struct EnvUi {
    handles: OverlayHandles,
    session: String,
    values: Rc<RefCell<BTreeMap<String, String>>>,
    list: Weak<RefCell<SettingList>>,
}

/// A confirmed field edit, addressed to the session the window opened on.
pub(crate) struct EnvEdit {
    pub(crate) session: String,
    pub(crate) key: String,
    pub(crate) value: Option<String>,
    ui: EnvUi,
}

impl EnvEdit {
    /// Reconcile the open list only after the host has accepted the edit.
    pub(crate) fn applied(self) {
        let mut values = self.ui.values.borrow_mut();
        match self.value {
            Some(value) => {
                values.insert(self.key, value);
            }
            None => {
                values.remove(&self.key);
            }
        }
        if let Some(list) = self.ui.list.upgrade() {
            list.borrow().set_rows(rows(&values));
        }
    }
}

/// Open a settings-style window over the map read from the selected host.
pub(crate) fn open_session_env(
    handles: OverlayHandles,
    session: String,
    values: BTreeMap<String, String>,
) {
    let list = Rc::new(RefCell::new(SettingList::new(
        rows(&values),
        handles.chrome.select.clone(),
        false,
    )));
    let ui = EnvUi {
        handles,
        session,
        values: Rc::new(RefCell::new(values)),
        list: Rc::downgrade(&list),
    };
    let focus = list.borrow().focus_target();
    {
        let mut widget = list.borrow_mut();
        let open = ui.clone();
        widget.on_open = Some(Box::new(move |ctx, key, _| {
            if key == ADD_ROW {
                open.open_key(ctx);
            } else {
                open.open_value(ctx, key.to_string());
            }
        }));
        let clear = ui.clone();
        widget.on_clear = Some(Box::new(move |_ctx, key, _| {
            if key != ADD_ROW {
                clear.submit(key.to_string(), None);
            }
        }));
        let close = ui.clone();
        widget.on_close = Some(Box::new(move |ctx| {
            close_top(&close.handles.stack, ctx, &close.handles.editor);
        }));
    }
    let mut subtitle = subtitle_edit_close("edit");
    if let Some(clear) = action_shortcut(ACTION_SETTINGS_CLEAR) {
        subtitle.push_str(&format!("  •  {clear} to remove"));
    }
    push_window(
        &ui.handles.stack,
        &ui.handles.chrome,
        "Session environment",
        subtitle,
        to_widget_ref(list),
        focus,
        OverlayPlacement::Large,
    );
}

impl EnvUi {
    fn submit(&self, key: String, value: Option<String>) {
        self.handles
            .activity
            .borrow_mut()
            .push(SelectorActivity::EnvironmentEdit(EnvEdit {
                session: self.session.clone(),
                key,
                value,
                ui: self.clone(),
            }));
    }

    fn open_key(&self, ctx: &mut EventContext) {
        let ui = self.clone();
        self.open_field(
            ctx,
            "Add variable: name",
            "",
            Box::new(move |ctx, key| {
                if ui.values.borrow().contains_key(&key) {
                    ui.error(ctx, "That variable already exists. Edit its row instead.");
                    return;
                }
                if let Err(err) =
                    validate_session_env(&BTreeMap::from([(key.clone(), String::new())]))
                {
                    ui.error(ctx, &err.to_string());
                    return;
                }
                close_top(&ui.handles.stack, ctx, &ui.handles.editor);
                ui.open_value(ctx, key);
            }),
        );
    }

    fn open_value(&self, ctx: &mut EventContext, key: String) {
        let current = self.values.borrow().get(&key).cloned().unwrap_or_default();
        let ui = self.clone();
        self.open_field(
            ctx,
            &format!("Value: {}", field_text(&key)),
            &current,
            Box::new(move |ctx, value| {
                if let Err(err) =
                    validate_session_env(&BTreeMap::from([(key.clone(), value.clone())]))
                {
                    ui.error(ctx, &err.to_string());
                    return;
                }
                ui.submit(key.clone(), Some(value));
                close_top(&ui.handles.stack, ctx, &ui.handles.editor);
            }),
        );
    }

    fn open_field(
        &self,
        ctx: &mut EventContext,
        title: &str,
        current: &str,
        mut confirm: Box<dyn FnMut(&mut EventContext, String)>,
    ) {
        let overlay = Rc::new(RefCell::new(TextEditOverlay::new(&field_text(current))));
        let focus = overlay.borrow().focus_target();
        let ui = self.clone();
        overlay
            .borrow()
            .set_on_submit_raw(Box::new(move |ctx, text| match parse_field(text) {
                Ok(value) => confirm(ctx, value),
                Err(_) => ui.error(
                    ctx,
                    "Invalid escape. Use JSON string escapes without outer quotes.",
                ),
            }));
        let cancel = self.clone();
        overlay.borrow_mut().on_cancel = Some(Box::new(move |ctx| {
            close_top(&cancel.handles.stack, ctx, &cancel.handles.editor);
        }));
        push_window(
            &self.handles.stack,
            &self.handles.chrome,
            title,
            FIELD_HINT.to_string(),
            to_widget_ref(overlay),
            Rc::clone(&focus),
            OverlayPlacement::Small,
        );
        ctx.request_focus(focus);
        ctx.redraw = true;
    }

    fn error(&self, ctx: &mut EventContext, text: &str) {
        show_toast(&self.handles.toasts, text.to_string());
        ctx.redraw = true;
    }
}

fn rows(values: &BTreeMap<String, String>) -> Vec<SettingRow> {
    let mut rows = vec![SettingRow {
        id: ADD_ROW.to_string(),
        label: "Add variable".to_string(),
        value: String::new(),
        description: "Add an environment variable to this branch.".to_string(),
        kind: RowKind::Submenu,
        inherited: false,
        clear_to: String::new(),
    }];
    rows.extend(values.iter().map(|(key, value)| SettingRow {
        id: key.clone(), label: field_text(key), value: format!("\"{}\"", field_text(value)),
        description: "Enter edits this branch's value. An empty value is kept; remove exposes the host's inherited value.".to_string(),
        kind: RowKind::Submenu, inherited: false, clear_to: String::new(),
    }));
    rows
}

/// JSON string contents, without the enclosing quotes. Encoding non-ASCII as
/// UTF-16 escapes keeps arbitrary persisted text inert in the one-line widget.
/// Ordinary values remain plain text; whitespace is never trimmed.
fn field_text(value: &str) -> String {
    let quoted = serde_json::to_string(value).expect("a string serializes");
    let mut text = String::new();
    for ch in quoted[1..quoted.len() - 1].chars() {
        if (' '..='~').contains(&ch) {
            text.push(ch);
        } else {
            for unit in ch.encode_utf16(&mut [0; 2]) {
                write!(text, "\\u{unit:04x}").expect("write to string");
            }
        }
    }
    text
}

fn parse_field(text: &str) -> Result<String, serde_json::Error> {
    serde_json::from_str(&format!("\"{text}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_representation_preserves_arbitrary_text_and_keeps_controls_inert() {
        for text in [
            "",
            " ordinary ",
            "a\tb\r\nc",
            "quote\"slash\\",
            "界😀\u{202e}\u{1b}",
        ] {
            let encoded = field_text(text);
            assert!(encoded.bytes().all(|byte| (0x20..=0x7e).contains(&byte)));
            assert_eq!(parse_field(&encoded).unwrap(), text);
        }
        assert_eq!(parse_field(r"a\nb\tc\\d").unwrap(), "a\nb\tc\\d");
        assert!(parse_field(r"bad\q").is_err());
    }
}
