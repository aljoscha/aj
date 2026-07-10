//! The command palette overlay and its dispatch.
//!
//! A fuzzy-filtered list over the shared [`COMMANDS`] catalog. Confirming
//! a row applies the command's [`CommandAction`]. The palette runs the
//! dispatch inside its confirm callback, where the live [`EventContext`]
//! can open a child overlay or move focus:
//!
//! - Commands that open a read-only overlay (help, auth, session info)
//!   push the child on top of the palette (it stays underneath, so
//!   Esc returns to it) and, for the async ones, park a [`PendingFetch`]
//!   the host fills once its fetch lands.
//! - `Quit` sets [`EventContext::quit`].
//! - Everything else is parked in the command slot for the host loop
//!   (which owns the turn machinery and can fold notices). The palette
//!   stays on the stack underneath, and the drive loop decides its fate:
//!   an opener pushes its overlay on top (so cancel returns to the palette),
//!   a pure action or a declined opener pops the palette back to the editor.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use aj_app::commands::{COMMANDS, CommandAction};
use aj_app::keybindings::default_action_shortcut;
use vaxis::vxfw::{
    EventContext, FilterableSelect, ListView, OverlayWindow, SelectItem, SelectStyles, WidgetRef,
    to_widget_ref,
};

use crate::content_overlay::{help_rows, loading_rows, open_content_overlay};
use crate::overlay::{
    OpenOverlay, OverlayChrome, OverlayPlacement, OverlayStack, close_top, subtitle_confirm_close,
};

/// A read-only overlay whose content the host fetches asynchronously.
///
/// Usage is not here: it is an interactive overlay ([`crate::usage_overlay`])
/// the host opens directly, not a read-only content fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FetchKind {
    Auth,
    SessionInfo,
}

/// A request the palette parks for the host loop: fetch `kind` and fill
/// the just-opened overlay's `list` with the result.
pub(crate) struct PendingFetch {
    pub(crate) kind: FetchKind,
    pub(crate) list: Rc<RefCell<ListView>>,
}

/// Open the command palette on `stack` and move focus into its filter.
///
/// The confirm callback captures shared handles (never the widget that
/// opened the palette): it runs while an overlay widget is borrowed
/// during dispatch, so it must not re-enter it. `command_slot` and
/// `fetch_slot` are the host's pickup points, `chrome` is read live so a
/// runtime theme swap tints child overlays with the current palette.
pub(crate) fn open_palette(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &Rc<RefCell<OverlayChrome>>,
    command_slot: &Rc<RefCell<Option<CommandAction>>>,
    fetch_slot: &Rc<RefCell<Option<PendingFetch>>>,
    ctx: &mut EventContext,
) {
    let select = Rc::new(RefCell::new(FilterableSelect::new(
        palette_items(),
        palette_select_styles(&chrome.borrow().select),
    )));
    let focus = select.borrow().focus_target();
    // Recover the action from the confirmed row via its filter key, which
    // is `"{category} {title}"` and unique per command.
    let actions: HashMap<String, CommandAction> = COMMANDS
        .iter()
        .map(|c| (format!("{} {}", c.category, c.title), c.action))
        .collect();
    {
        let mut select = select.borrow_mut();
        let stack_c = Rc::clone(stack);
        let editor_c = Rc::clone(editor);
        let chrome_c = Rc::clone(chrome);
        let command_slot_c = Rc::clone(command_slot);
        let fetch_slot_c = Rc::clone(fetch_slot);
        select.on_confirm = Some(Box::new(move |ctx, item| {
            if let Some(action) = actions.get(&item.filter_key).copied() {
                dispatch_from_palette(
                    action,
                    &stack_c,
                    &editor_c,
                    &chrome_c,
                    &command_slot_c,
                    &fetch_slot_c,
                    ctx,
                );
            }
        }));
        let stack_cancel = Rc::clone(stack);
        let editor_cancel = Rc::clone(editor);
        select.on_cancel = Some(Box::new(move |ctx| {
            close_top(&stack_cancel, ctx, &editor_cancel)
        }));
    }
    let mut window = OverlayWindow::new("Commands", to_widget_ref(select));
    // The confirm/close hint resolves through the shared keybinding data
    // (Spec F): Enter/Esc labels from `format_keybinding`, the close-all label
    // from the keymap action. The Enter/Esc *handling* stays a fixed
    // `FilterableSelect` convention (see the NOTE in `crate::overlay`).
    window.subtitle = subtitle_confirm_close();
    {
        let ch = chrome.borrow();
        window.border_style = ch.border;
        window.title_style = ch.title;
        window.subtitle_style = ch.subtitle;
    }
    stack.borrow_mut().push(OpenOverlay {
        widget: to_widget_ref(Rc::new(RefCell::new(window))),
        focus: Rc::clone(&focus),
        placement: OverlayPlacement::Small,
    });
    ctx.request_focus(focus);
    ctx.redraw = true;
}

/// Apply a confirmed command from within the palette's confirm callback.
#[allow(clippy::too_many_arguments)]
fn dispatch_from_palette(
    action: CommandAction,
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &Rc<RefCell<OverlayChrome>>,
    command_slot: &Rc<RefCell<Option<CommandAction>>>,
    fetch_slot: &Rc<RefCell<Option<PendingFetch>>>,
    ctx: &mut EventContext,
) {
    let open_fetch = |kind: FetchKind, title: &str, ctx: &mut EventContext| {
        let ch = chrome.borrow();
        let list = open_content_overlay(stack, editor, &ch, title, loading_rows(), ctx);
        *fetch_slot.borrow_mut() = Some(PendingFetch { kind, list });
    };
    match action {
        // Help is static, so it opens fully populated. The rest open a
        // "Loading…" overlay the host fills after its async fetch.
        CommandAction::Help => {
            let ch = chrome.borrow();
            open_content_overlay(stack, editor, &ch, "Help", help_rows(), ctx);
        }
        CommandAction::OpenAuthStatus => open_fetch(FetchKind::Auth, "Auth status", ctx),
        CommandAction::OpenSessionInfo => open_fetch(FetchKind::SessionInfo, "Session info", ctx),
        // The palette is already the top overlay, so re-opening is a
        // no-op that leaves it in place.
        CommandAction::OpenCommandPalette => {}
        CommandAction::Quit => ctx.quit = true,
        // Host-applied commands (compact, export, and the config-editing
        // selectors) run in the drive loop, which owns the turn machinery and
        // the session world. We only park the action and leave the palette on
        // the stack. The drive loop pushes any child overlay on top of the
        // palette (so cancel returns here) or pops the palette back to the
        // editor when the command opened nothing.
        other => {
            *command_slot.borrow_mut() = Some(other);
        }
    }
}

/// One palette row per command: the `title` as the label, the `category` as
/// the dim prefix column, and the bound shortcut (resolved from keybinding
/// data per Spec F) in the right slot when the command carries an action. The
/// filter key is `"{category} {title}"` so typing a category surfaces its
/// whole group. The widget lays out the columns, so the label carries only
/// the title.
/// The palette's row styles: the shared chrome styles with a bold label. The
/// bold label is a palette-only divergence, so we clone and bold here rather
/// than in `select_styles_from_theme`, leaving the other list overlays plain.
fn palette_select_styles(base: &SelectStyles) -> SelectStyles {
    let mut styles = base.clone();
    styles.label.bold = true;
    styles
}

fn palette_items() -> Vec<SelectItem> {
    COMMANDS
        .iter()
        .map(|cmd| {
            let mut item = SelectItem::new(cmd.title, format!("{} {}", cmd.category, cmd.title))
                .with_prefix(cmd.category);
            if let Some(short) = cmd.action_id.and_then(default_action_shortcut) {
                item = item.with_shortcut(short);
            }
            item
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_has_one_row_per_command_with_grouping_filter_keys() {
        let items = palette_items();
        assert_eq!(items.len(), COMMANDS.len());
        for (item, cmd) in items.iter().zip(COMMANDS) {
            assert_eq!(item.label, cmd.title, "{item:?}");
            assert_eq!(item.prefix.as_deref(), Some(cmd.category), "{item:?}");
            assert_eq!(item.filter_key, format!("{} {}", cmd.category, cmd.title));
        }
    }

    #[test]
    fn palette_rows_resolve_shortcuts_from_binding_data() {
        // The palette-open command carries a bound action, so its row's
        // shortcut column holds the data-derived chord rather than a literal.
        let items = palette_items();
        let resolved = default_action_shortcut(aj_app::keybindings::ACTION_PALETTE_OPEN)
            .expect("palette-open has a default chord");
        assert!(
            items
                .iter()
                .any(|i| i.shortcut.as_deref() == Some(resolved.as_str())),
            "expected a row with resolved shortcut {resolved:?}"
        );
    }

    #[test]
    fn palette_bolds_labels_while_shared_styles_stay_plain() {
        use aj_app::theme::{ColorMode, Theme};

        use crate::overlay::select_styles_from_theme;

        let shared = select_styles_from_theme(&Theme::bundled_dark_with_mode(ColorMode::Truecolor));
        assert!(
            !shared.label.bold,
            "shared list-row label must stay plain so other overlays are not bold"
        );
        let palette = palette_select_styles(&shared);
        assert!(palette.label.bold, "palette must bold its own row labels");
        // Only the label diverges; the palette leaves the other columns alone,
        // so the shortcut keeps the shared bold hint styling.
        assert_eq!(palette.shortcut, shared.shortcut);
        assert_eq!(palette.prefix, shared.prefix);
        assert_eq!(palette.secondary, shared.secondary);
    }
}
