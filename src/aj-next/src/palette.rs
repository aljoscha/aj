//! The command palette overlay and its dispatch.
//!
//! A fuzzy-filtered list over the shared [`COMMANDS`] catalog. Confirming
//! a row applies the command's [`CommandAction`]. The palette runs the
//! dispatch inside its confirm callback, where the live [`EventContext`]
//! can open a child overlay or move focus:
//!
//! - Commands that open a read-only overlay (help, auth, session info,
//!   usage) push the child on top of the palette (it stays underneath, so
//!   Esc returns to it) and, for the async ones, park a [`PendingFetch`]
//!   the host fills once its fetch lands.
//! - `Quit` sets [`EventContext::quit`].
//! - Everything else is parked in the command slot for the host loop
//!   (which owns the turn machinery and can fold notices), and the palette
//!   closes back to the editor.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use aj_app::commands::{COMMANDS, CommandAction};
use aj_app::keybindings::default_action_shortcut;
use vaxis::vxfw::{
    EventContext, FilterableSelect, ListView, OverlayWindow, SelectItem, WidgetRef, to_widget_ref,
};

use crate::content_overlay::{help_rows, loading_rows, open_content_overlay};
use crate::overlay::{OpenOverlay, OverlayChrome, OverlayPlacement, OverlayStack, close_top};

/// A read-only overlay whose content the host fetches asynchronously.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FetchKind {
    Auth,
    SessionInfo,
    Usage,
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
    let select = Rc::new(RefCell::new(FilterableSelect::new(palette_items())));
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
    // TODO(aljoscha): resolve the confirm/cancel subtitle labels through
    // keybinding data (Spec F's hint-label rule). Enter/Esc are the
    // FilterableSelect's built-in keys, not rebindable actions in
    // `aj_app`'s vocabulary, so for now they keep the fixed convention.
    window.subtitle = "Enter to confirm  \u{2022}  Esc to close".to_string();
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
        CommandAction::OpenUsageStatus => open_fetch(FetchKind::Usage, "Usage", ctx),
        // The palette is already the top overlay, so re-opening is a
        // no-op that leaves it in place.
        CommandAction::OpenCommandPalette => {}
        CommandAction::Quit => ctx.quit = true,
        // Host-applied commands (compact, export, and the not-yet-wired
        // selectors) open no child, so the palette closes back to the
        // editor and the host applies the effect.
        other => {
            *command_slot.borrow_mut() = Some(other);
            close_top(stack, ctx, editor);
        }
    }
}

/// One palette row per command: a `category`-padded, `title`-padded
/// label with the bound shortcut appended (resolved from keybinding data
/// per Spec F), and a `"{category} {title}"` filter key so typing a
/// category surfaces its whole group.
fn palette_items() -> Vec<SelectItem> {
    let cat_w = COMMANDS
        .iter()
        .map(|c| c.category.chars().count())
        .max()
        .unwrap_or(0);
    let title_w = COMMANDS
        .iter()
        .map(|c| c.title.chars().count())
        .max()
        .unwrap_or(0);
    COMMANDS
        .iter()
        .map(|cmd| {
            let mut label = format!(
                "{cat:<cat_w$}  {title:<title_w$}",
                cat = cmd.category,
                title = cmd.title
            );
            if let Some(short) = cmd.action_id.and_then(default_action_shortcut) {
                label.push_str(&format!("  ({short})"));
            }
            SelectItem::new(label, format!("{} {}", cmd.category, cmd.title))
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
            assert!(item.label.contains(cmd.title), "{item:?}");
            assert!(item.label.contains(cmd.category), "{item:?}");
            assert_eq!(item.filter_key, format!("{} {}", cmd.category, cmd.title));
        }
    }

    #[test]
    fn palette_rows_resolve_shortcuts_from_binding_data() {
        // The palette-open command carries a bound action, so its row
        // shows the data-derived shortcut rather than a literal.
        let items = palette_items();
        let resolved = default_action_shortcut(aj_app::keybindings::ACTION_PALETTE_OPEN)
            .expect("palette-open has a default chord");
        assert!(
            items.iter().any(|i| i.label.contains(&resolved)),
            "expected a row with resolved shortcut {resolved:?}"
        );
    }
}
