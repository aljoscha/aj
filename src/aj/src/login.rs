//! The OAuth login flow: provider/account action pickers, the login dialog
//! overlay, and the [`OAuthCallbacks`] driver that streams updates into it.
//!
//! Unlike the other overlays (synchronous "confirm parks an outcome, the
//! host polls it" selectors), an OAuth login is async and long-running:
//! the flow binds a localhost callback server, opens the browser, and
//! waits for the redirect, or for the user to paste the redirect URL back
//! when their browser is on another machine.
//!
//! The split mirrors the credential engine's headless-provider pattern:
//!
//! - The login flow ([`aj_models::oauth::OAuthProvider`]) runs on a
//!   spawned tokio task and *asks* the UI for things via
//!   [`OAuthCallbacks`].
//! - [`DialogCallbacks`] satisfies those asks by writing into a shared
//!   [`LoginDialogState`] and pinging a redraw. Fire-and-forget asks
//!   (`on_auth`, `on_progress`) push display lines; input-gathering asks
//!   (`on_prompt`, `on_manual_code_input`) install a [`oneshot`] sender
//!   and await its receiver.
//! - [`LoginDialog`] renders that shared state and, on Enter (via its
//!   inner [`TextField`]'s submit), delivers the typed value to whichever
//!   callback is awaiting. Esc/Ctrl+C flip a shared cancel flag the drive
//!   loop polls to tear the dialog down and abort the task.
//!
//! The redraw wake crosses threads: the login task runs on tokio, off the
//! `!Send` drive-loop thread, so it can't call `AsyncApp::request_redraw`
//! directly. Instead the callbacks send `()` on an [`UnboundedSender`] the
//! drive loop selects on, turning each ping into a repaint. No lock is
//! ever held across an `.await`, so the plain [`std::sync::Mutex`] shared
//! between the UI thread and the login task is safe.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use aj_app::auth::{LoginLine, auth_lines, browser_available, copy_to_clipboard, open_browser};
use aj_app::keybindings::{fixed_keys, format_keybinding};
use aj_app::theme::{Theme, ThemeColor};
use aj_models::auth::{
    AccountLabelDisplayMode, display_account_label, validate_account_label,
    validate_account_label_edit,
};
use aj_models::oauth::{OAuthAuthInfo, OAuthCallbacks, OAuthError};
use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use vaxis::cell::{Hyperlink, Segment, Style};
use vaxis::key::{Key, Modifiers};
use vaxis::vxfw::{
    Builder, CursorState, DrawContext, Event, EventContext, FilterableSelect, ListView, MaxSize,
    RelativePoint, RichText, ScrollView, ScrollableView, SelectItem, Size, Source, SubSurface,
    Surface, Text, Widget, WidgetRef, WidthBasis, draw_widget, to_widget_ref,
};

use crate::overlay::{
    OverlayChrome, OverlayPlacement, OverlayStack, close_all, close_top, subtitle_confirm_close,
    subtitle_login,
};
use crate::settings_ui::push_window;
use crate::terminal::TerminalCaps;
use crate::transcript::vaxis_color;

/// PgUp/PgDn step, in rows. A fixed jump rather than a viewport-derived
/// one keeps the widget from needing to know its drawn height. Matches
/// the read-only content overlays.
const PAGE_STEP: usize = 10;

/// Slot holding the sender an input-awaiting callback is blocked on. The
/// dialog's inner field takes it on submit and fulfils it with the typed
/// text. Shared: the dialog (UI thread) holds a clone, the originals move
/// into the login task's [`DialogCallbacks`].
type PendingInput = Arc<StdMutex<Option<oneshot::Sender<String>>>>;

/// Which contract governs the login dialog's active field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoginInputKind {
    /// Provider-owned OAuth code or redirect input.
    OAuth,
    /// A prospective account label governed by the shared auth validator.
    AccountLabel,
}

/// Display state shared between [`DialogCallbacks`] (writer, on the login
/// task) and [`LoginDialog`] (reader, on the UI thread).
#[derive(Default)]
pub(crate) struct LoginDialogState {
    /// Status lines shown top-to-bottom.
    pub(crate) lines: Vec<LoginLine>,
    /// When `Some`, the manual-paste field is active with this prompt
    /// label and the next submit delivers its value to the awaiting
    /// callback.
    pub(crate) input_prompt: Option<String>,
    /// Validation mode paired with `input_prompt`.
    input_kind: Option<LoginInputKind>,
    /// Field-local refusal text. It never includes rejected input bytes.
    input_error: Option<String>,
    /// The authorization URL, set by [`OAuthCallbacks::on_auth`]. Held
    /// separately from its `Url` display line so Ctrl+Y can copy it.
    pub(crate) url: Option<String>,
}

/// Resolved line colors for the dialog's line kinds and its input/notice
/// rows. `Copy` (all fields are `Copy`) so the row [`Builder`]
/// and the widget can each hold one.
///
/// NOTE: `progress`, `prompt`, and `notice` all resolve to `Muted` today.
/// They stay separate fields so a future theme can tint them by role.
#[derive(Clone, Copy)]
struct LoginStyles {
    info: Style,
    progress: Style,
    url: Style,
    prompt: Style,
    notice: Style,
    /// Whether the authorize-URL segment carries an OSC 8 hyperlink, from the
    /// probed `TerminalCaps`. Threaded so login honors the same capability as
    /// the transcript's markdown links.
    hyperlinks: bool,
}

impl LoginStyles {
    fn from_theme(theme: &Theme, caps: TerminalCaps) -> LoginStyles {
        let mode = theme.color_mode();
        let fg = |token: ThemeColor| Style {
            fg: vaxis_color(theme.fg_color(token), mode),
            ..Style::default()
        };
        LoginStyles {
            info: fg(ThemeColor::Text),
            progress: fg(ThemeColor::Muted),
            url: fg(ThemeColor::Accent),
            prompt: fg(ThemeColor::Muted),
            notice: fg(ThemeColor::Muted),
            hyperlinks: caps.hyperlinks,
        }
    }
}

/// Builds one row widget per state line, styled by kind, for the dialog's
/// read-only [`ListView`]. Reads the shared state on each call so lines
/// pushed by the login task show up on the next draw.
struct LineBuilder {
    state: Arc<StdMutex<LoginDialogState>>,
    styles: LoginStyles,
}

impl Builder for LineBuilder {
    fn item_at_idx(&self, idx: usize, _cursor: usize) -> Option<WidgetRef> {
        let st = self.state.lock().expect("login dialog state poisoned");
        let widget = match st.lines.get(idx)? {
            // The authorize URL is a `RichText` so a single soft-wrapped
            // `Segment` can carry the OSC 8 link (`Text` has no link field).
            // The whole wrapped URL then reads as one clickable link and
            // stays fully visible and copyable.
            LoginLine::Url(url) => {
                let mut widget = RichText::new(vec![url_segment(
                    url,
                    self.styles.url,
                    self.styles.hyperlinks,
                )]);
                widget.softwrap = true;
                widget.width_basis = WidthBasis::Parent;
                to_widget_ref(Rc::new(RefCell::new(widget)))
            }
            // Info and the headless instructions stay plain `Text`, wrapping
            // to the overlay width instead of truncating.
            LoginLine::Info(t) => text_line(t.clone(), self.styles.info),
            LoginLine::Progress(t) => text_line(t.clone(), self.styles.progress),
        };
        Some(widget)
    }
}

/// A soft-wrapping, parent-width [`Text`] row for the dialog's non-URL lines.
fn text_line(text: String, style: Style) -> WidgetRef {
    let mut widget = Text::new(text);
    widget.style = style;
    widget.softwrap = true;
    widget.width_basis = WidthBasis::Parent;
    to_widget_ref(Rc::new(RefCell::new(widget)))
}

/// Build the styled [`Segment`] for a [`LoginLine::Url`] row.
///
/// When `hyperlinks` is set the segment carries an OSC 8 target with the
/// shared `id=aj-oauth` param. A single linked segment soft-wraps into a
/// multi-row URL that terminals treat as one logical link (the shared id),
/// so clicking any row opens the whole URL. With hyperlinks off the segment
/// is the plain styled URL text, still fully visible and copyable.
///
/// The URI has control bytes stripped so a stray byte can't break out of the
/// escape. The OAuth URL is well-formed, this is defensive.
fn url_segment(url: &str, style: Style, hyperlinks: bool) -> Segment {
    let link = if hyperlinks {
        Hyperlink {
            uri: url.chars().filter(|c| !c.is_control()).collect(),
            params: "id=aj-oauth".to_string(),
        }
    } else {
        Hyperlink::default()
    };
    Segment {
        text: url.to_string(),
        style,
        link,
    }
}

/// The login dialog overlay.
///
/// This widget is the overlay's focus target (see `focus` on the pushed
/// [`OpenOverlay`](crate::overlay::OpenOverlay)), so every input event reaches it
/// at-target. It handles the dialog chords itself (Esc cancel, Ctrl+Y
/// copy, arrows/page scroll) and forwards the rest to the inner
/// [`TextField`] while a prompt is active. The field is never focused, so
/// its cursor is lifted onto this widget's surface at draw time.
pub(crate) struct LoginDialog {
    state: Arc<StdMutex<LoginDialogState>>,
    cancel: Arc<AtomicBool>,
    /// Owned by value and drawn unstamped: the field is never a focus or
    /// mouse target, so its widget identity is unused.
    field: vaxis::vxfw::TextField,
    /// Exact field contents mirrored by `TextField::on_change`, used to check
    /// a prospective account-label insertion before forwarding the event.
    input_value: Rc<RefCell<String>>,
    list: Rc<RefCell<ListView>>,
    styles: LoginStyles,
    /// Ephemeral feedback (e.g. the Ctrl+Y "Copied…" line) shown at the
    /// bottom of the dialog.
    notice: Option<String>,
    /// Set once the authorization URL has been auto-copied, so the copy
    /// in `draw` stays idempotent across frames.
    auto_copied: bool,
}

impl LoginDialog {
    /// Build the dialog over shared handles also held by the login task's
    /// [`DialogCallbacks`] and by the host's [`LoginSession`].
    pub(crate) fn new(
        theme: &Theme,
        caps: TerminalCaps,
        state: Arc<StdMutex<LoginDialogState>>,
        pending_input: PendingInput,
        cancel: Arc<AtomicBool>,
    ) -> LoginDialog {
        let styles = LoginStyles::from_theme(theme, caps);
        let mut list = ListView::new(Source::Builder(Box::new(LineBuilder {
            state: Arc::clone(&state),
            styles,
        })));
        // Document scroll with no visible cursor: the arrow/page keys move
        // the hidden cursor to follow the viewport.
        list.draw_cursor = false;
        let list = Rc::new(RefCell::new(list));

        let mut field = vaxis::vxfw::TextField::new();
        let input_value = Rc::new(RefCell::new(String::new()));
        {
            let input_value = Rc::clone(&input_value);
            field.on_change = Some(Box::new(move |_ctx, value| {
                *input_value.borrow_mut() = value.to_string();
            }));
        }
        {
            // On submit the field hands us its (cleared) contents; deliver
            // them to the awaiting callback. Fires from inside
            // `LoginDialog::handle_event`, which must therefore not hold the
            // `state` or `pending` locks across the forward into the field,
            // since we re-take both here.
            let pending = Arc::clone(&pending_input);
            let st = Arc::clone(&state);
            let submitted_value = Rc::clone(&input_value);
            field.on_submit = Some(Box::new(move |_ctx, value| {
                let kind = st.lock().expect("login dialog state poisoned").input_kind;
                let value = if kind == Some(LoginInputKind::AccountLabel) {
                    value.to_string()
                } else {
                    value.trim().to_string()
                };
                let sender = pending.lock().expect("pending input poisoned").take();
                match sender {
                    // Only deliver a non-empty value; a stray empty submit
                    // leaves the prompt in place for the real paste.
                    Some(tx) if !value.is_empty() => {
                        *submitted_value.borrow_mut() = String::new();
                        let mut state = st.lock().expect("login dialog state poisoned");
                        state.input_prompt = None;
                        state.input_kind = None;
                        state.input_error = None;
                        drop(state);
                        let _ = tx.send(value);
                    }
                    // Put the sender back when we didn't use it.
                    other => *pending.lock().expect("pending input poisoned") = other,
                }
            }));
        }

        LoginDialog {
            state,
            cancel,
            field,
            input_value,
            list,
            styles,
            notice: None,
            auto_copied: false,
        }
    }

    fn input_kind(&self) -> Option<LoginInputKind> {
        self.state
            .lock()
            .expect("login dialog state poisoned")
            .input_kind
    }

    /// Check an insertion against the complete prospective raw buffer without
    /// mutating the field, cursor, viewport, or pending callback.
    fn account_insertion_is_safe(&self, inserted: &str) -> bool {
        let current = self.input_value.borrow();
        let cursor = self.field.byte_offset_to_cursor();
        let mut prospective = String::with_capacity(current.len() + inserted.len());
        prospective.push_str(&current[..cursor]);
        prospective.push_str(inserted);
        prospective.push_str(&current[cursor..]);
        validate_account_label_edit(&prospective).is_ok()
    }

    fn refuse_account_input(&self, message: &str) {
        self.state
            .lock()
            .expect("login dialog state poisoned")
            .input_error = Some(message.to_string());
    }

    fn clear_input_error(&self) {
        self.state
            .lock()
            .expect("login dialog state poisoned")
            .input_error = None;
    }

    /// Whether forwarding `key` to `TextField` reaches its final text-insert
    /// branch rather than one of its fixed editing commands.
    fn field_key_inserts_text(key: &Key) -> bool {
        key.text.is_some()
            && !key.matches(Key::BACKSPACE, Modifiers::empty())
            && !key.matches(Key::DELETE, Modifiers::empty())
            && !key.matches(u32::from('d'), Modifiers::CTRL)
            && !key.matches(Key::LEFT, Modifiers::empty())
            && !key.matches(u32::from('b'), Modifiers::CTRL)
            && !key.matches(Key::RIGHT, Modifiers::empty())
            && !key.matches(u32::from('f'), Modifiers::CTRL)
            && !key.matches(u32::from('a'), Modifiers::CTRL)
            && !key.matches(Key::HOME, Modifiers::empty())
            && !key.matches(u32::from('e'), Modifiers::CTRL)
            && !key.matches(Key::END, Modifiers::empty())
            && !key.matches(u32::from('k'), Modifiers::CTRL)
            && !key.matches(u32::from('u'), Modifiers::CTRL)
            && !key.matches(u32::from('b'), Modifiers::ALT)
            && !key.matches(Key::LEFT, Modifiers::ALT)
            && !key.matches(u32::from('f'), Modifiers::ALT)
            && !key.matches(Key::RIGHT, Modifiers::ALT)
            && !key.matches(Key::BACKSPACE, Modifiers::ALT)
            && !key.matches(u32::from('w'), Modifiers::CTRL)
            && !key.matches(u32::from('d'), Modifiers::ALT)
            && !key.matches(Key::ENTER, Modifiers::empty())
            && !key.matches(u32::from('j'), Modifiers::CTRL)
    }
}

impl Widget for LoginDialog {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let size = ctx.max.size();
        let mut surface = Surface::with_size(size);
        let zero = Size {
            width: 0,
            height: 0,
        };

        let (line_count, prompt, url, input_error) = {
            let st = self.state.lock().expect("login dialog state poisoned");
            (
                st.lines.len(),
                st.input_prompt.clone(),
                st.url.clone(),
                st.input_error.clone(),
            )
        };
        self.list.borrow_mut().item_count =
            Some(u32::try_from(line_count).expect("line count fits u32"));

        // Copy the URL to the clipboard the first time it appears, so the
        // user never has to select it out of the overlay. This runs in
        // draw, on the UI thread, which is where the OSC 52 write in
        // copy_to_clipboard must happen. The login task cannot do it,
        // its thread does not own the render stdout.
        if let Some(url) = &url {
            if !self.auto_copied {
                copy_to_clipboard(url);
                self.auto_copied = true;
                self.notice = Some(format!(
                    "Copied the authorization URL to your clipboard ({} to copy again).",
                    fixed_keys::CTRL_Y
                ));
            }
        }

        // Reserve the bottom rows: a prompt label + field pair while
        // prompting, and one row for the notice.
        let mut reserved: u16 = 0;
        if prompt.is_some() {
            reserved = reserved.saturating_add(2);
        }
        if self.notice.is_some() {
            reserved = reserved.saturating_add(1);
        }
        if input_error.is_some() {
            reserved = reserved.saturating_add(1);
        }
        let list_height = size.height.saturating_sub(reserved);

        if list_height > 0 {
            let list_ctx = ctx.with_constraints(
                zero,
                MaxSize {
                    width: Some(size.width),
                    height: Some(list_height),
                },
            );
            surface.children.push(SubSurface {
                origin: RelativePoint { row: 0, col: 0 },
                surface: draw_widget(&to_widget_ref(Rc::clone(&self.list)), &list_ctx),
                z_index: 0,
            });
        }

        let mut row = list_height;
        if let Some(prompt) = &prompt {
            let mut label = Text::new(prompt.clone());
            label.style = self.styles.prompt;
            label.width_basis = WidthBasis::Parent;
            let one_row = ctx.with_constraints(
                zero,
                MaxSize {
                    width: Some(size.width),
                    height: Some(1),
                },
            );
            // Drawn unstamped, like the field below: a per-frame widget's
            // identity is unused.
            surface.children.push(SubSurface {
                origin: RelativePoint {
                    row: i32::from(row),
                    col: 0,
                },
                surface: label.draw(&one_row),
                z_index: 0,
            });
            row = row.saturating_add(1);

            let field_surface = self.field.draw(&one_row);
            let field_cursor = field_surface.cursor;
            surface.children.push(SubSurface {
                origin: RelativePoint {
                    row: i32::from(row),
                    col: 0,
                },
                surface: field_surface,
                z_index: 0,
            });
            // The inner field is drawn but never focused (this dialog is),
            // so its own cursor never shows. Lift it onto this surface,
            // which carries the focused widget's identity, so the cursor
            // lands in the field while the dialog owns key routing.
            if let Some(cursor) = field_cursor {
                surface.cursor = Some(CursorState {
                    row: row.saturating_add(cursor.row),
                    col: cursor.col,
                    shape: cursor.shape,
                });
            }
            row = row.saturating_add(1);
        }

        if let Some(notice) = &self.notice {
            let mut text = Text::new(notice.clone());
            text.style = self.styles.notice;
            text.width_basis = WidthBasis::Parent;
            let one_row = ctx.with_constraints(
                zero,
                MaxSize {
                    width: Some(size.width),
                    height: Some(1),
                },
            );
            surface.children.push(SubSurface {
                origin: RelativePoint {
                    row: i32::from(row),
                    col: 0,
                },
                surface: text.draw(&one_row),
                z_index: 0,
            });
            row = row.saturating_add(1);
        }

        if let Some(error) = input_error {
            let mut text = Text::new(error);
            text.style = self.styles.notice;
            text.width_basis = WidthBasis::Parent;
            let one_row = ctx.with_constraints(
                zero,
                MaxSize {
                    width: Some(size.width),
                    height: Some(1),
                },
            );
            surface.children.push(SubSurface {
                origin: RelativePoint {
                    row: i32::from(row),
                    col: 0,
                },
                surface: text.draw(&one_row),
                z_index: 0,
            });
        }

        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        if matches!(event, Event::Paste(_)) {
            let prompting = self
                .state
                .lock()
                .expect("login dialog state poisoned")
                .input_prompt
                .is_some();
            if prompting {
                let Event::Paste(text) = event else {
                    unreachable!("matched paste above")
                };
                if self.input_kind() == Some(LoginInputKind::AccountLabel)
                    && !self.account_insertion_is_safe(text)
                {
                    self.refuse_account_input(
                        "Account label paste rejected: use safe single-line Unicode within 256 bytes.",
                    );
                } else {
                    self.clear_input_error();
                    self.field.handle_event(ctx, event);
                }
            }
            ctx.consume_and_redraw();
            return;
        }

        let Event::KeyPress(key) = event else {
            return;
        };

        // Copy the authorization URL. Hard-coded (like the global Ctrl+C
        // handling) rather than a rebindable action, and checked before
        // routing to the field so it works while the paste field is active.
        if key.matches(u32::from('y'), Modifiers::CTRL) {
            let url = self
                .state
                .lock()
                .expect("login dialog state poisoned")
                .url
                .clone();
            self.notice = Some(match url {
                Some(url) => {
                    copy_to_clipboard(&url);
                    "Copied the authorization URL to your clipboard.".to_string()
                }
                None => "No authorization URL to copy yet.".to_string(),
            });
            ctx.consume_and_redraw();
            return;
        }

        // Esc cancels. Ctrl+C reaches us the same way: the keymap's
        // close-all chord is gated off while a login is active (see
        // `HostCtx::login_active`), so it falls through to here. We flip
        // the shared flag; the drive loop's cancel-poll tears the dialog
        // down and aborts the task.
        if key.matches(Key::ESCAPE, Modifiers::empty())
            || key.matches(u32::from('c'), Modifiers::CTRL)
        {
            self.cancel.store(true, Ordering::Relaxed);
            ctx.consume_and_redraw();
            return;
        }

        if key.matches(Key::DOWN, Modifiers::empty()) {
            self.list.borrow_mut().next_item(ctx);
            ctx.consume_and_redraw();
            return;
        }
        if key.matches(Key::UP, Modifiers::empty()) {
            self.list.borrow_mut().prev_item(ctx);
            ctx.consume_and_redraw();
            return;
        }
        if key.matches(Key::PAGE_DOWN, Modifiers::empty()) {
            for _ in 0..PAGE_STEP {
                self.list.borrow_mut().next_item(ctx);
            }
            ctx.consume_and_redraw();
            return;
        }
        if key.matches(Key::PAGE_UP, Modifiers::empty()) {
            for _ in 0..PAGE_STEP {
                self.list.borrow_mut().prev_item(ctx);
            }
            ctx.consume_and_redraw();
            return;
        }

        // Manual-paste editing: forward keyboard editing to the field only
        // while a prompt is active. Enter fires the field's on_submit, which
        // delivers the value to the awaiting callback.
        let prompting = self
            .state
            .lock()
            .expect("login dialog state poisoned")
            .input_prompt
            .is_some();
        if prompting {
            if self.input_kind() == Some(LoginInputKind::AccountLabel) {
                if key.matches(Key::ENTER, Modifiers::empty())
                    || key.matches(u32::from('j'), Modifiers::CTRL)
                {
                    let value = self.input_value.borrow();
                    if let Err(err) = validate_account_label(&value) {
                        self.refuse_account_input(&format!("Account label rejected: {err}"));
                        ctx.consume_and_redraw();
                        return;
                    }
                } else if Self::field_key_inserts_text(key)
                    && let Some(text) = &key.text
                    && !self.account_insertion_is_safe(text)
                {
                    self.refuse_account_input(
                        "Account label character rejected: use safe single-line Unicode within 256 bytes.",
                    );
                    ctx.consume_and_redraw();
                    return;
                }
            }
            self.clear_input_error();
            self.field.handle_event(ctx, event);
        }
        // Read-only otherwise: swallow every key so none leaks to the base
        // layout behind the modal.
        ctx.consume_and_redraw();
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// [`OAuthCallbacks`] driving a [`LoginDialog`] from the spawned login
/// task.
///
/// `Send + Sync` as the trait requires: every shared handle is an
/// `Arc<Mutex<_>>` or a channel sender. The redraw ping crosses threads
/// (the task runs on tokio, the dialog on the drive-loop thread), so the
/// callbacks never touch the widget directly, only the shared state and
/// the `redraw` sender.
pub(crate) struct DialogCallbacks {
    state: Arc<StdMutex<LoginDialogState>>,
    pending_input: PendingInput,
    redraw: UnboundedSender<()>,
}

impl DialogCallbacks {
    /// Build the callbacks over the same shared handles the dialog holds.
    /// `redraw` pings the drive loop to repaint after each state write.
    pub(crate) fn new(
        state: Arc<StdMutex<LoginDialogState>>,
        pending_input: PendingInput,
        redraw: UnboundedSender<()>,
    ) -> DialogCallbacks {
        DialogCallbacks {
            state,
            pending_input,
            redraw,
        }
    }

    /// Wake the drive loop so it repaints the dialog after a state write.
    /// A dropped receiver (the loop is gone) makes this a no-op.
    fn ping(&self) {
        let _ = self.redraw.send(());
    }

    fn push_line(&self, line: LoginLine) {
        self.state
            .lock()
            .expect("login dialog state poisoned")
            .lines
            .push(line);
        self.ping();
    }

    /// Reveal the paste field with `prompt`, park a sender in the shared
    /// slot, and await the value the dialog delivers. A dropped receiver
    /// (the dialog torn down, or this branch of a race cancelled) resolves
    /// to [`OAuthError::Cancelled`].
    async fn await_input(&self, prompt: &str, kind: LoginInputKind) -> Result<String, OAuthError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.state.lock().expect("login dialog state poisoned");
            st.input_prompt = Some(prompt.to_string());
            st.input_kind = Some(kind);
            st.input_error = None;
            *self.pending_input.lock().expect("pending input poisoned") = Some(tx);
        }
        self.ping();
        rx.await.map_err(|_| OAuthError::Cancelled)
    }

    /// Ask for a new account label before OAuth begins. Existing exact labels
    /// are represented through the shared grammar for collision guidance.
    pub(crate) async fn prompt_account_label(
        &self,
        existing: &[String],
    ) -> Result<String, OAuthError> {
        const REPRESENTATION_BUDGET: usize = 4_096;
        let mut labels = Vec::new();
        let mut represented_bytes = 0;
        let mut omitted = 0;
        for label in existing {
            let ordinary = display_account_label(label, AccountLabelDisplayMode::Ordinary);
            let represented = if ordinary.contains(' ') {
                display_account_label(label, AccountLabelDisplayMode::Ascii)
            } else {
                ordinary
            };
            let separator = usize::from(!labels.is_empty()) * 2;
            if represented_bytes + separator + represented.len() <= REPRESENTATION_BUDGET {
                represented_bytes += separator + represented.len();
                labels.push(represented);
            } else {
                omitted += 1;
            }
        }
        let labels = labels.join(", ");
        let prompt = match (labels.is_empty(), omitted) {
            (true, 0) => "Account name:".to_string(),
            (true, omitted) => {
                format!("Account name ({omitted} existing omitted; inspect auth status):")
            }
            (false, 0) => format!("Account name (existing: {labels}):"),
            (false, omitted) => format!(
                "Account name ({omitted} omitted; inspect auth status; existing: {labels}):"
            ),
        };
        self.await_input(&prompt, LoginInputKind::AccountLabel)
            .await
    }
}

#[async_trait]
impl OAuthCallbacks for DialogCallbacks {
    fn on_auth(&self, info: OAuthAuthInfo<'_>) {
        // When no browser is reachable here (SSH/headless), opening the
        // automatic URL is pointless: its redirect targets this machine's
        // loopback, which the remote browser can't reach. `auth_lines`
        // steers the user to the manual flow accordingly.
        let can_open = browser_available();
        let (lines, url) = auth_lines(can_open, &info, fixed_keys::CTRL_Y);
        {
            let mut st = self.state.lock().expect("login dialog state poisoned");
            st.lines.extend(lines);
            st.url = Some(url);
        }
        if can_open {
            open_browser(info.url);
        }
        self.ping();
    }

    async fn on_prompt(&self, message: &str) -> Result<String, OAuthError> {
        self.await_input(message, LoginInputKind::OAuth).await
    }

    fn on_progress(&self, message: &str) {
        self.push_line(LoginLine::Progress(message.to_string()));
    }

    async fn on_manual_code_input(&self) -> Result<String, OAuthError> {
        // Enter is the field's built-in submit key. Its handling is a fixed
        // convention, but the label resolves through `format_keybinding` so it
        // reads from one source (see the NOTE in `crate::overlay`).
        let submit = format_keybinding("enter");
        self.await_input(
            &format!(
                "On another machine? Paste the code shown after login (or the full redirect URL), \
                 then press {submit}:"
            ),
            LoginInputKind::OAuth,
        )
        .await
    }

    fn supports_manual_code_input(&self) -> bool {
        true
    }
}

/// Which explicit OAuth storage intent a login row names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoginTarget {
    /// Insert a new credential without replacing any exact key.
    NewAccount,
    /// Replace the selected existing bare credential or exact account key.
    ExistingAccount(Option<String>),
}

/// A sensitive action over one exact raw account identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AccountAction {
    ReplaceLogin {
        provider_id: String,
        provider_name: String,
        account_label: String,
    },
    Logout {
        provider_id: String,
        account_label: String,
    },
    SetDefault {
        provider_id: String,
        account_label: String,
    },
    LogoutWithNewDefault {
        provider_id: String,
        account_label: String,
        new_default: String,
    },
    LogoutAll {
        provider_id: String,
        expected_accounts: Vec<String>,
        inspect_index: usize,
    },
}

impl AccountAction {
    pub(crate) fn identity(&self) -> (&str, &str) {
        match self {
            Self::ReplaceLogin {
                provider_id,
                account_label,
                ..
            }
            | Self::Logout {
                provider_id,
                account_label,
            }
            | Self::SetDefault {
                provider_id,
                account_label,
            } => (provider_id, account_label),
            Self::LogoutWithNewDefault {
                provider_id,
                new_default,
                ..
            } => (provider_id, new_default),
            Self::LogoutAll {
                provider_id,
                expected_accounts,
                inspect_index,
            } => (
                provider_id,
                expected_accounts
                    .get(*inspect_index)
                    .expect("logout-all inspection index names an expected account"),
            ),
        }
    }
}

/// What confirming an auth picker row asks the host to do. Parked in the
/// shell's `auth_request` slot for the drive loop to drain after the confirming
/// keystroke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AuthPickerRequest {
    /// Start the provider's OAuth flow for an explicit creation or replacement.
    Login {
        provider_id: String,
        provider_name: String,
        target: LoginTarget,
    },
    /// Open the complete account-label inspection before a sensitive action.
    InspectAccount(AccountAction),
    /// Apply an action after its required inspection or acknowledgement.
    ApplyAccount(AccountAction),
    /// Remove a provider's bare credential.
    LogoutBare { provider_id: String },
}

/// One auth picker row with separate render, search, and action identity.
pub(crate) struct AuthRow {
    pub(crate) request: AuthPickerRequest,
    pub(crate) label: String,
    pub(crate) filter_key: String,
    pub(crate) summary: String,
}

/// Build one selectable row: the friendly label as the primary column, the
/// status summary as the muted description, and `"{id} {label}"` as the
/// filter key so typing either the id or the name finds it.
fn picker_items(rows: &[AuthRow]) -> Vec<SelectItem> {
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            SelectItem::new(row.label.clone(), row.filter_key.clone())
                .with_value(format!("auth-row-{index}"))
                .with_description(row.summary.clone())
        })
        .collect()
}

fn picker_requests(rows: Vec<AuthRow>) -> HashMap<String, AuthPickerRequest> {
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| (format!("auth-row-{index}"), row.request))
        .collect()
}

/// Open an authentication picker and move focus into its filter (via the caller's
/// refocus event). Confirming a row parks the matching [`AuthPickerRequest`]
/// in `request_slot` and closes; Esc closes without a request.
///
/// Confirmation resolves the row's opaque `SelectItem::value`. Display and
/// filter text participate only in presentation and search, never identity.
fn open_auth_picker(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    request_slot: &Rc<RefCell<Option<AuthPickerRequest>>>,
    title: &str,
    rows: Vec<AuthRow>,
) {
    let select = Rc::new(RefCell::new(FilterableSelect::new(
        picker_items(&rows),
        chrome.select.clone(),
    )));
    let focus = select.borrow().focus_target();
    let map = picker_requests(rows);
    {
        let mut sel = select.borrow_mut();
        let request_c = Rc::clone(request_slot);
        let stack_c = Rc::clone(stack);
        let editor_c = Rc::clone(editor);
        sel.on_confirm = Some(Box::new(move |ctx, item| {
            if let Some(value) = item.value.as_deref()
                && let Some(request) = map.get(value)
            {
                *request_c.borrow_mut() = Some(request.clone());
            }
            // A confirmed pick is terminal: tear the whole stack down
            // (palette and picker) so the host can continue with a login
            // dialog, account inspection, or direct bare mutation. Cancel
            // below uses `close_top`, returning to the palette underneath.
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
        title,
        // The confirm/close hint resolves through the shared keybinding data
        // (Spec F). Enter/Esc *handling* stays a fixed `FilterableSelect`
        // convention (see the NOTE in `crate::overlay`).
        subtitle_confirm_close(),
        to_widget_ref(Rc::clone(&select)),
        focus,
        OverlayPlacement::Small,
    );
}

/// Open the `/login` picker over OAuth provider/account action rows.
pub(crate) fn open_login_picker(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    request_slot: &Rc<RefCell<Option<AuthPickerRequest>>>,
    rows: Vec<AuthRow>,
) {
    open_auth_picker(stack, editor, chrome, request_slot, "Log in", rows);
}

/// Open the `/logout` picker over stored provider/account rows. Each confirmed
/// row retains its exact raw removal or default-resolution action.
pub(crate) fn open_logout_picker(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    request_slot: &Rc<RefCell<Option<AuthPickerRequest>>>,
    rows: Vec<AuthRow>,
) {
    open_auth_picker(stack, editor, chrome, request_slot, "Log out", rows);
}

/// Open the store-default account picker.
pub(crate) fn open_default_account_picker(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    request_slot: &Rc<RefCell<Option<AuthPickerRequest>>>,
    rows: Vec<AuthRow>,
) {
    open_auth_picker(stack, editor, chrome, request_slot, "Default account", rows);
}

/// Open the explicit resolution picker for removing a default account that
/// still has siblings.
pub(crate) fn open_default_logout_picker(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    request_slot: &Rc<RefCell<Option<AuthPickerRequest>>>,
    rows: Vec<AuthRow>,
) {
    open_auth_picker(
        stack,
        editor,
        chrome,
        request_slot,
        "Choose a new default or remove all accounts",
        rows,
    );
}

const ACCOUNT_INSPECTION_CELL_LIMIT: usize = 65_535;
const OVER_LIMIT_PREFIX_CELLS: usize = 512;

/// Non-softwrapped account-label inspection in front of a sensitive action.
///
/// The complete representation rides in a real [`ScrollView`] through 65,535
/// logical cells. Longer legacy representations are classified in `usize`
/// first and only a bounded represented prefix enters vaxis. Their action is
/// armed by one explicit acknowledgement before a later Enter can confirm.
struct AccountConfirmation {
    /// The identity row is kept concrete so the non-softwrap contract is both
    /// inspectable and asserted at draw, rather than hidden behind WidgetRef.
    text: Rc<RefCell<Text>>,
    view: Rc<RefCell<ScrollView>>,
    represented_cells: usize,
    surface_representation_cells: usize,
    over_limit: bool,
    acknowledged: bool,
    last_width: u16,
    action: AccountAction,
    request_slot: Rc<RefCell<Option<AuthPickerRequest>>>,
    stack: Rc<RefCell<OverlayStack>>,
    editor: WidgetRef,
    warning_style: Style,
}

impl AccountConfirmation {
    fn new(
        action: AccountAction,
        request_slot: Rc<RefCell<Option<AuthPickerRequest>>>,
        stack: Rc<RefCell<OverlayStack>>,
        editor: WidgetRef,
        warning_style: Style,
    ) -> Self {
        let (_, raw_label) = action.identity();
        let creation_valid = validate_account_label(raw_label).is_ok();
        let represented = display_account_label(raw_label, AccountLabelDisplayMode::Ordinary);
        // A creation-valid ordinary label is bounded to 512 cells by contract.
        // Only the scalar-escaped legacy branch can approach the u16 limit, and
        // every one of its bytes is one terminal cell.
        let represented_cells = if creation_valid {
            usize::from(vaxis::gwidth::gwidth(
                &represented,
                vaxis::gwidth::Method::Unicode,
            ))
        } else {
            represented.len()
        };
        let over_limit = represented_cells > ACCOUNT_INSPECTION_CELL_LIMIT;
        let shown = if over_limit {
            represented
                .chars()
                .take(OVER_LIMIT_PREFIX_CELLS)
                .collect::<String>()
        } else {
            represented
        };
        let surface_representation_cells = shown.len();
        let mut text_widget = Text::new(shown);
        text_widget.softwrap = false;
        let text = Rc::new(RefCell::new(text_widget));
        let mut view = ScrollView::new(Source::Slice(vec![to_widget_ref(Rc::clone(&text))]));
        view.draw_cursor = false;
        Self {
            text,
            view: Rc::new(RefCell::new(view)),
            represented_cells,
            surface_representation_cells,
            over_limit,
            acknowledged: false,
            last_width: 1,
            action,
            request_slot,
            stack,
            editor,
            warning_style,
        }
    }
}

impl Widget for AccountConfirmation {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        debug_assert!(!self.text.borrow().softwrap);
        debug_assert!(self.surface_representation_cells <= ACCOUNT_INSPECTION_CELL_LIMIT);
        if !self.over_limit {
            let represented_cells = ctx.string_width(&self.text.borrow().text);
            self.represented_cells = represented_cells;
            self.surface_representation_cells = represented_cells;
        }
        let size = ctx.max.size();
        self.last_width = size.width.max(1);
        let warning_rows = if self.over_limit { 2 } else { 1 };
        let mut surface = Surface::with_size(size);
        if size.height > 0 {
            let view_ctx = ctx.with_constraints(
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
                surface: draw_widget(&to_widget_ref(Rc::clone(&self.view)), &view_ctx),
                z_index: 0,
            });
        }
        if size.height > 1 {
            let message = if self.over_limit && self.acknowledged {
                "Incomplete inspection acknowledged. Press Enter again to continue with the exact raw account."
                    .to_string()
            } else if self.over_limit {
                format!(
                    "Only a clipped prefix is shown. This legacy account exceeds the 65,535-cell \
                     terminal inspection limit ({} cells).",
                    self.represented_cells
                )
            } else {
                "Use Left/Right or Home/End to inspect the complete account label.".to_string()
            };
            let mut warning = Text::new(message);
            warning.style = self.warning_style;
            warning.softwrap = true;
            warning.width_basis = WidthBasis::Parent;
            let warning_ctx = ctx.with_constraints(
                Size {
                    width: 0,
                    height: 0,
                },
                MaxSize {
                    width: Some(size.width),
                    height: Some(size.height.saturating_sub(1).min(warning_rows)),
                },
            );
            surface.children.push(SubSurface {
                origin: RelativePoint { row: 1, col: 0 },
                surface: warning.draw(&warning_ctx),
                z_index: 0,
            });
        }
        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else {
            self.view.borrow_mut().handle_event(ctx, event);
            return;
        };
        if key.matches(Key::ESCAPE, Modifiers::empty()) {
            close_top(&self.stack, ctx, &self.editor);
        } else if key.matches(Key::HOME, Modifiers::empty()) {
            self.view.borrow_mut().set_scroll_left(0);
            ctx.consume_and_redraw();
        } else if key.matches(Key::END, Modifiers::empty()) {
            let max_left = self
                .surface_representation_cells
                .saturating_sub(usize::from(self.last_width));
            self.view
                .borrow_mut()
                .set_scroll_left(u32::try_from(max_left).expect("bounded surface offset fits u32"));
            ctx.consume_and_redraw();
        } else if key.matches(Key::ENTER, Modifiers::empty()) {
            if self.over_limit && !self.acknowledged {
                self.acknowledged = true;
                self.warning_style.bold = true;
                ctx.consume_and_redraw();
            } else {
                *self.request_slot.borrow_mut() =
                    Some(AuthPickerRequest::ApplyAccount(self.action.clone()));
                close_all(&self.stack, ctx, &self.editor);
            }
        } else {
            self.view.borrow_mut().handle_event(ctx, event);
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// Open account inspection for one sensitive raw-identity action.
pub(crate) fn open_account_confirmation(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    request_slot: &Rc<RefCell<Option<AuthPickerRequest>>>,
    theme: &Theme,
    action: AccountAction,
) {
    let provider_id = action.identity().0.to_string();
    let warning_style = LoginStyles::from_theme(theme, TerminalCaps::default()).notice;
    let confirm = Rc::new(RefCell::new(AccountConfirmation::new(
        action,
        Rc::clone(request_slot),
        Rc::clone(stack),
        Rc::clone(editor),
        warning_style,
    )));
    let focus = to_widget_ref(Rc::clone(&confirm));
    push_window(
        stack,
        chrome,
        &format!("Confirm account action — {provider_id}"),
        "Left/Right inspect · Enter acknowledge/confirm · Esc close".to_string(),
        to_widget_ref(confirm),
        focus,
        OverlayPlacement::Small,
    );
}

/// Build the login dialog over the shared handles and push it as the top
/// overlay. The caller spawns the login task over the same `state` /
/// `pending_input` and stores the `cancel` flag, then posts the refocus
/// event to move focus onto the dialog.
pub(crate) fn open_login_dialog(
    stack: &Rc<RefCell<OverlayStack>>,
    chrome: &OverlayChrome,
    theme: &Theme,
    caps: TerminalCaps,
    provider_name: &str,
    state: Arc<StdMutex<LoginDialogState>>,
    pending_input: PendingInput,
    cancel: Arc<AtomicBool>,
) {
    let dialog = Rc::new(RefCell::new(LoginDialog::new(
        theme,
        caps,
        state,
        pending_input,
        cancel,
    )));
    let focus = to_widget_ref(Rc::clone(&dialog));
    push_window(
        stack,
        chrome,
        &format!("Log in \u{2014} {provider_name}"),
        subtitle_login(),
        to_widget_ref(dialog),
        focus,
        OverlayPlacement::Small,
    );
}

#[cfg(test)]
mod tests {
    use aj_app::theme::ColorMode;

    use super::*;

    fn theme() -> Theme {
        Theme::bundled_dark_with_mode(ColorMode::Truecolor)
    }

    /// Build a dialog plus its shared handles, mirroring what `start_login`
    /// wires on the host side.
    fn make() -> (
        LoginDialog,
        Arc<StdMutex<LoginDialogState>>,
        PendingInput,
        Arc<AtomicBool>,
    ) {
        let state = Arc::new(StdMutex::new(LoginDialogState::default()));
        let pending: PendingInput = Arc::new(StdMutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let dialog = LoginDialog::new(
            &theme(),
            TerminalCaps::default(),
            Arc::clone(&state),
            Arc::clone(&pending),
            Arc::clone(&cancel),
        );
        (dialog, state, pending, cancel)
    }

    fn callbacks(
        state: &Arc<StdMutex<LoginDialogState>>,
        pending: &PendingInput,
    ) -> (DialogCallbacks, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            DialogCallbacks {
                state: Arc::clone(state),
                pending_input: Arc::clone(pending),
                redraw: tx,
            },
            rx,
        )
    }

    fn key_event(codepoint: u32, mods: Modifiers, text: Option<&str>) -> Event {
        Event::KeyPress(Key {
            codepoint,
            mods,
            text: text.map(Into::into),
            ..Key::default()
        })
    }

    /// on_progress and on_auth mutate the shared state (so the dialog
    /// renders the new lines) and each fires a redraw ping.
    #[test]
    fn callbacks_update_state_and_ping_redraw() {
        let (_dialog, state, pending) = {
            let (d, s, p, _c) = make();
            (d, s, p)
        };
        let (cb, mut rx) = callbacks(&state, &pending);

        cb.on_progress("Starting login\u{2026}");
        assert!(rx.try_recv().is_ok(), "on_progress pings a redraw");

        let info = OAuthAuthInfo {
            url: "https://auth.example.com/authorize?x=1",
            manual_url: None,
            instructions: Some("Complete login in your browser."),
        };
        let expected_url = info.url;
        cb.on_auth(info);
        assert!(rx.try_recv().is_ok(), "on_auth pings a redraw");

        let st = state.lock().unwrap();
        // The seeded progress line plus the auth_lines composition.
        assert!(matches!(st.lines.first(), Some(LoginLine::Progress(t)) if t.contains("Starting")));
        assert!(
            st.lines
                .iter()
                .any(|l| matches!(l, LoginLine::Url(u) if u == expected_url))
        );
        assert_eq!(st.url.as_deref(), Some(expected_url));
    }

    /// The dialog auto-copies the URL the first time it appears (on the
    /// UI thread in draw), sets the notice once, and stays idempotent on
    /// later frames.
    #[test]
    fn draw_auto_copies_the_url_once() {
        let (mut dialog, state, _pending, _cancel) = make();
        state.lock().unwrap().url = Some("https://auth.example.com/x".to_string());

        let ctx = crate::test_support::draw_ctx(60, Some(20));
        let _ = dialog.draw(&ctx);
        assert!(dialog.auto_copied);
        assert!(
            dialog
                .notice
                .as_deref()
                .is_some_and(|n| n.contains("Copied")),
            "notice: {:?}",
            dialog.notice
        );

        // A later frame does not re-copy or overwrite the notice.
        dialog.notice = Some("kept".to_string());
        let _ = dialog.draw(&ctx);
        assert_eq!(dialog.notice.as_deref(), Some("kept"));
    }

    /// on_prompt parks a oneshot and reveals the field; the dialog's typed
    /// text + Enter delivers the value to the awaiting callback.
    #[tokio::test]
    async fn enter_delivers_typed_text_to_awaiting_callback() {
        let (mut dialog, state, pending, _cancel) = make();
        let (cb, _rx) = callbacks(&state, &pending);

        let fut = tokio::spawn(async move { cb.on_prompt("paste:").await });
        // Let the spawned task install its sender + prompt.
        tokio::task::yield_now().await;
        assert!(state.lock().unwrap().input_prompt.is_some(), "prompt shown");

        let mut ctx = EventContext::new();
        for c in "code123".chars() {
            dialog.handle_event(
                &mut ctx,
                &key_event(u32::from(c), Modifiers::empty(), Some(&c.to_string())),
            );
        }
        dialog.handle_event(&mut ctx, &key_event(Key::ENTER, Modifiers::empty(), None));

        let got = fut.await.expect("join").expect("input");
        assert_eq!(got, "code123");
        // The prompt clears after delivery.
        assert!(state.lock().unwrap().input_prompt.is_none());
    }

    #[tokio::test]
    async fn enter_delivers_pasted_text_to_awaiting_callback() {
        let (mut dialog, state, pending, _cancel) = make();
        let (cb, _rx) = callbacks(&state, &pending);

        let fut = tokio::spawn(async move { cb.on_prompt("paste:").await });
        tokio::task::yield_now().await;
        assert!(state.lock().unwrap().input_prompt.is_some(), "prompt shown");

        let mut ctx = EventContext::new();
        dialog.handle_event(&mut ctx, &Event::Paste("code123".to_string()));
        dialog.handle_event(&mut ctx, &key_event(Key::ENTER, Modifiers::empty(), None));

        let got = fut.await.expect("join").expect("input");
        assert_eq!(got, "code123");
        assert!(state.lock().unwrap().input_prompt.is_none());
    }

    #[tokio::test]
    async fn account_label_rejects_unsafe_and_overlength_edits_atomically() {
        let (mut dialog, state, pending, _cancel) = make();
        let (cb, _rx) = callbacks(&state, &pending);
        let fut = tokio::spawn(async move { cb.prompt_account_label(&[]).await });
        tokio::task::yield_now().await;

        let accepted = "a".repeat(250);
        let mut ctx = EventContext::new();
        dialog.handle_event(&mut ctx, &Event::Paste(accepted.clone()));
        let _ = dialog.draw(&crate::test_support::draw_ctx(12, Some(8)));
        let snapshot = (
            dialog.input_value.borrow().clone(),
            dialog.field.byte_offset_to_cursor(),
            dialog.field.draw_offset,
            dialog.field.prev_cursor_col,
            dialog.field.prev_cursor_idx,
        );

        for rejected in [
            Event::Paste("\n".to_string()),
            Event::Paste("bbbbbbb".to_string()),
            key_event(0x202e, Modifiers::empty(), Some("\u{202e}")),
        ] {
            dialog.handle_event(&mut ctx, &rejected);
            assert_eq!(
                (
                    dialog.input_value.borrow().clone(),
                    dialog.field.byte_offset_to_cursor(),
                    dialog.field.draw_offset,
                    dialog.field.prev_cursor_col,
                    dialog.field.prev_cursor_idx,
                ),
                snapshot,
                "a rejected edit changed field or viewport state"
            );
            assert!(pending.lock().unwrap().is_some(), "sender stayed parked");
            assert!(!fut.is_finished(), "callback did not fire");
        }
        let error = state.lock().unwrap().input_error.clone().unwrap();
        assert!(
            !error.contains('\u{202e}'),
            "diagnostic echoed rejected text"
        );

        dialog.handle_event(&mut ctx, &Event::Paste("bbbbbb".to_string()));
        let accepted = format!("{accepted}bbbbbb");
        assert_eq!(accepted.len(), 256);
        let cursor = dialog.field.byte_offset_to_cursor();
        dialog.handle_event(
            &mut ctx,
            &key_event(u32::from('z'), Modifiers::empty(), Some("z")),
        );
        assert_eq!(dialog.input_value.borrow().as_str(), accepted);
        assert_eq!(
            dialog.field.byte_offset_to_cursor(),
            cursor,
            "typed insertion crossing the bound is atomic"
        );
        assert!(pending.lock().unwrap().is_some());
        assert!(!fut.is_finished());

        dialog.handle_event(&mut ctx, &key_event(Key::ENTER, Modifiers::empty(), None));
        assert_eq!(fut.await.unwrap().unwrap(), accepted);
    }

    #[tokio::test]
    async fn incomplete_account_label_stays_editable_after_rejected_submission() {
        let (mut dialog, state, pending, _cancel) = make();
        let (cb, _rx) = callbacks(&state, &pending);
        let fut = tokio::spawn(async move { cb.prompt_account_label(&["wo\nrk".into()]).await });
        tokio::task::yield_now().await;
        let prompt = state.lock().unwrap().input_prompt.clone().unwrap();
        assert!(
            prompt.contains("\\!\\u{77}"),
            "legacy label represented: {prompt}"
        );

        let mut ctx = EventContext::new();
        dialog.handle_event(&mut ctx, &Event::Paste("work ".to_string()));
        dialog.handle_event(&mut ctx, &key_event(Key::ENTER, Modifiers::empty(), None));
        assert_eq!(dialog.input_value.borrow().as_str(), "work ");
        assert_eq!(dialog.field.byte_offset_to_cursor(), 5);
        assert!(pending.lock().unwrap().is_some());
        assert!(!fut.is_finished());
        assert!(state.lock().unwrap().input_error.is_some());

        dialog.handle_event(
            &mut ctx,
            &key_event(Key::BACKSPACE, Modifiers::empty(), None),
        );
        dialog.handle_event(&mut ctx, &key_event(Key::ENTER, Modifiers::empty(), None));
        assert_eq!(fut.await.unwrap().unwrap(), "work");
    }

    #[tokio::test]
    async fn account_prompt_keeps_distinct_sub_limit_common_prefixes() {
        let (dialog, state, pending, _cancel) = make();
        let (cb, _rx) = callbacks(&state, &pending);
        let left = format!("{}x", "a".repeat(96));
        let right = format!("{}y", "a".repeat(96));
        let expected_left = left.clone();
        let expected_right = right.clone();
        let fut = tokio::spawn(async move {
            cb.prompt_account_label(&[left, right, "a b".into(), "a    b".into()])
                .await
        });
        tokio::task::yield_now().await;
        let prompt = state.lock().unwrap().input_prompt.clone().unwrap();
        assert!(
            prompt.contains(&expected_left),
            "left tail missing: {prompt}"
        );
        assert!(
            prompt.contains(&expected_right),
            "right tail missing: {prompt}"
        );
        assert!(
            !prompt.contains("[clipped"),
            "sub-limit labels were rewritten"
        );
        assert!(prompt.contains("\\!\\u{61}\\u{20}\\u{62}"), "{prompt}");
        assert!(prompt.contains("\\u{20}\\u{20}\\u{20}\\u{20}"), "{prompt}");
        assert!(!prompt.contains("a    b"));
        drop(dialog);
        drop(pending.lock().unwrap().take());
        assert!(matches!(fut.await.unwrap(), Err(OAuthError::Cancelled)));
    }

    #[tokio::test]
    async fn exact_limit_existing_label_uses_width_safe_prompt_guidance() {
        let (mut dialog, state, pending, _cancel) = make();
        let (cb, _rx) = callbacks(&state, &pending);
        let exact_limit = format!("{}\u{0100}", "a".repeat(10_921));
        assert_eq!(
            display_account_label(&exact_limit, AccountLabelDisplayMode::Ordinary).len(),
            65_535
        );
        let fut = tokio::spawn(async move { cb.prompt_account_label(&[exact_limit]).await });
        tokio::task::yield_now().await;

        let prompt = state.lock().unwrap().input_prompt.clone().unwrap();
        assert!(prompt.contains("1 existing omitted"), "{prompt}");
        assert!(
            prompt.len() < 4_500,
            "prompt exceeded its bounded representation budget"
        );
        let surface = dialog.draw(&crate::test_support::draw_ctx(40, Some(14)));
        let rows = crate::test_support::rows(&surface).join("\n");
        assert!(
            rows.contains("omitted"),
            "width-safe guidance is visible: {rows}"
        );

        drop(dialog);
        drop(pending.lock().unwrap().take());
        assert!(matches!(fut.await.unwrap(), Err(OAuthError::Cancelled)));
    }

    #[tokio::test]
    async fn submitting_account_label_cannot_erase_the_following_oauth_prompt() {
        let (mut dialog, state, pending, _cancel) = make();
        let (cb, _rx) = callbacks(&state, &pending);
        let fut = tokio::spawn(async move {
            let label = cb.prompt_account_label(&[]).await?;
            let code = cb.on_prompt("OAuth code:").await?;
            Ok::<_, OAuthError>((label, code))
        });
        tokio::task::yield_now().await;

        let mut ctx = EventContext::new();
        dialog.handle_event(&mut ctx, &Event::Paste("work".to_string()));
        dialog.handle_event(&mut ctx, &key_event(Key::ENTER, Modifiers::empty(), None));
        tokio::task::yield_now().await;
        assert_eq!(
            state.lock().unwrap().input_prompt.as_deref(),
            Some("OAuth code:")
        );
        assert!(pending.lock().unwrap().is_some());

        dialog.handle_event(&mut ctx, &Event::Paste("code".to_string()));
        dialog.handle_event(&mut ctx, &key_event(Key::ENTER, Modifiers::empty(), None));
        assert_eq!(fut.await.unwrap().unwrap(), ("work".into(), "code".into()));
    }

    /// While a prompt is active, the field's cursor is lifted onto the
    /// dialog's own surface: the field is drawn unstamped and never focused,
    /// so only the lift can show a cursor.
    #[test]
    fn prompt_cursor_is_lifted_onto_the_dialog_surface() {
        let (mut dialog, state, _pending, _cancel) = make();
        state.lock().unwrap().input_prompt = Some("paste:".to_string());
        let mut ctx = EventContext::new();
        for c in "abc".chars() {
            dialog.handle_event(
                &mut ctx,
                &key_event(u32::from(c), Modifiers::empty(), Some(&c.to_string())),
            );
        }
        let surface = dialog.draw(&crate::test_support::draw_ctx(40, Some(14)));
        let cursor = surface.cursor.expect("an active prompt shows a cursor");
        assert_eq!(cursor.col, 3, "the cursor sits after the typed text");
    }

    /// Ctrl+Y with a stored URL copies it and leaves a "copied" notice.
    #[test]
    fn ctrl_y_copies_url_and_notes_it() {
        let (mut dialog, state, _pending, _cancel) = make();
        state.lock().unwrap().url = Some("https://auth.example.com/x".to_string());
        let mut ctx = EventContext::new();
        dialog.handle_event(&mut ctx, &key_event(u32::from('y'), Modifiers::CTRL, None));
        assert_eq!(
            dialog.notice.as_deref(),
            Some("Copied the authorization URL to your clipboard.")
        );
    }

    /// Ctrl+Y with no URL yet notes that instead of touching the clipboard.
    #[test]
    fn ctrl_y_without_url_reports_nothing_to_copy() {
        let (mut dialog, _state, _pending, _cancel) = make();
        let mut ctx = EventContext::new();
        dialog.handle_event(&mut ctx, &key_event(u32::from('y'), Modifiers::CTRL, None));
        assert_eq!(
            dialog.notice.as_deref(),
            Some("No authorization URL to copy yet.")
        );
    }

    /// Esc and Ctrl+C both flip the shared cancel flag the drive loop polls.
    #[test]
    fn esc_and_ctrl_c_set_the_cancel_flag() {
        for event in [
            key_event(Key::ESCAPE, Modifiers::empty(), None),
            key_event(u32::from('c'), Modifiers::CTRL, None),
        ] {
            let (mut dialog, _state, _pending, cancel) = make();
            let mut ctx = EventContext::new();
            dialog.handle_event(&mut ctx, &event);
            assert!(cancel.load(Ordering::Relaxed));
        }
    }

    /// A dropped receiver (the dialog torn down mid-await) resolves the
    /// awaited input to `Cancelled` rather than hanging.
    #[tokio::test]
    async fn dropped_receiver_cancels_await() {
        let (dialog, state, pending, _cancel) = make();
        let (cb, _rx) = callbacks(&state, &pending);
        let fut = tokio::spawn(async move { cb.on_prompt("paste:").await });
        tokio::task::yield_now().await;
        // Drop the dialog (and thus never deliver); the parked sender is
        // dropped when the flow is cancelled by dropping the callbacks.
        drop(dialog);
        // Take and drop the parked sender to simulate teardown.
        let _ = pending.lock().unwrap().take();
        match fut.await.expect("join") {
            Err(OAuthError::Cancelled) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    /// The Url row's segment carries the OSC 8 link with the shared
    /// `id=aj-oauth` param when hyperlinks are enabled, so the soft-wrapped
    /// URL reads as one clickable link.
    #[test]
    fn url_segment_links_when_hyperlinks_on() {
        let url = "https://auth.example.com/authorize?client_id=abc&state=xyz";
        let seg = url_segment(url, Style::default(), true);
        assert_eq!(seg.text, url);
        assert_eq!(seg.link.uri, url);
        assert_eq!(seg.link.params, "id=aj-oauth");
    }

    /// With hyperlinks off the segment is the plain styled URL: no OSC 8
    /// target, but the full URL text still shows so it stays copyable.
    #[test]
    fn url_segment_plain_when_hyperlinks_off() {
        let url = "https://auth.example.com/authorize";
        let seg = url_segment(url, Style::default(), false);
        assert_eq!(seg.text, url);
        assert_eq!(seg.link, Hyperlink::default());
    }

    /// A stray control byte in the URL is stripped from the emitted OSC 8
    /// uri so it can't break out of the escape. The display text is left
    /// untouched (the OAuth URL is well-formed in practice).
    #[test]
    fn url_segment_strips_control_bytes_from_uri() {
        let seg = url_segment("https://x/\u{1b}evil", Style::default(), true);
        assert_eq!(seg.link.uri, "https://x/evil");
        assert!(!seg.link.uri.contains('\u{1b}'));
    }

    /// At a narrow width the long spaceless URL soft-wraps across rows
    /// rather than truncating, so the whole URL stays visible and copyable,
    /// and every drawn cell carries the shared-id link (that shared id is
    /// what keeps a wrapped run one logical link).
    #[test]
    fn url_row_wraps_full_url_and_carries_link() {
        let url =
            "https://auth.example.com/authorize?client_id=abcdef123456&scope=read+write&state=xyz";
        let state = Arc::new(StdMutex::new(LoginDialogState::default()));
        state
            .lock()
            .unwrap()
            .lines
            .push(LoginLine::Url(url.to_string()));
        let builder = LineBuilder {
            state: Arc::clone(&state),
            styles: LoginStyles::from_theme(&theme(), TerminalCaps::default()),
        };
        let widget = builder.item_at_idx(0, 0).expect("url row widget");

        // Narrow enough to force several wrapped rows, tall enough that all
        // of them are visible.
        let ctx = crate::test_support::draw_ctx(20, Some(20));
        let surface = draw_widget(&widget, &ctx);

        // The token is spaceless, so a correct soft-wrap loses no
        // characters: concatenating the trimmed rows reassembles the URL.
        let joined = crate::test_support::rows(&surface).concat();
        assert_eq!(joined, url, "full URL must survive the wrap");

        let linked: Vec<_> = crate::test_support::flatten(&surface)
            .into_iter()
            .flatten()
            .filter(|c| !c.char.grapheme().trim().is_empty())
            .collect();
        assert!(!linked.is_empty(), "URL cells present");
        assert!(
            linked
                .iter()
                .all(|c| c.link.uri == url && c.link.params == "id=aj-oauth"),
            "every URL cell carries the shared-id link"
        );
    }

    /// End-to-end: the real [`LoginDialog`], rendered through the real
    /// `vaxis` alt-screen diff, emits the URL's OSC 8 open with the shared
    /// `id=aj-oauth` param, and keeps emitting it after a redraw that adds a
    /// progress line above the URL (the login task streams these constantly).
    ///
    /// This drives the whole path the interactive login uses: the
    /// `LineBuilder` -> `ListView` -> `RichText` widget tree, surface
    /// compositing, and the renderer's cell-diff. It guards against the diff
    /// stripping the link or drowning the stream in redundant link-close
    /// escapes when the blank cells below the URL are skipped.
    #[test]
    fn login_url_emits_osc8_open_through_real_render_across_redraw() {
        use vaxis::Winsize;
        use vaxis::vaxis::{Options as VaxisOptions, Vaxis};

        let url =
            "https://auth.example.com/authorize?client_id=abcdef123456&scope=read+write&state=xyz";

        let state = Arc::new(StdMutex::new(LoginDialogState::default()));
        {
            let mut st = state.lock().unwrap();
            st.lines
                .push(LoginLine::Progress("Starting login\u{2026}".to_string()));
            st.lines.push(LoginLine::Url(url.to_string()));
        }
        let pending: PendingInput = Arc::new(StdMutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let dialog = Rc::new(RefCell::new(LoginDialog::new(
            &theme(),
            TerminalCaps::default(),
            Arc::clone(&state),
            pending,
            cancel,
        )));
        let dialog_ref = to_widget_ref(Rc::clone(&dialog));

        // A window taller than the drawn dialog so the diff has blank rows to
        // skip below the URL, which is where the close-escape spam surfaced.
        let winsize = Winsize {
            rows: 40,
            cols: 40,
            x_pixel: 0,
            y_pixel: 0,
        };
        let mut vx = Vaxis::new(VaxisOptions::default());
        let mut sink: Vec<u8> = Vec::new();
        vx.enter_alt_screen(&mut sink).expect("alt");
        vx.resize(&mut sink, winsize).expect("resize");

        let ctx = crate::test_support::draw_ctx(40, Some(14));

        let render_frame = |vx: &mut Vaxis| -> Vec<u8> {
            let surface = draw_widget(&dialog_ref, &ctx);
            surface.render(vx.window(), None);
            let mut out = Vec::new();
            vx.render(&mut out).expect("render");
            out
        };

        let open: Vec<u8> = {
            let mut o = b"\x1b]8;id=aj-oauth;".to_vec();
            o.extend_from_slice(url.as_bytes());
            o.extend_from_slice(b"\x1b\\");
            o
        };
        let count = |haystack: &[u8], needle: &[u8]| -> usize {
            if needle.is_empty() || haystack.len() < needle.len() {
                return 0;
            }
            let (mut n, mut i) = (0usize, 0usize);
            while i + needle.len() <= haystack.len() {
                if &haystack[i..i + needle.len()] == needle {
                    n += 1;
                    i += needle.len();
                } else {
                    i += 1;
                }
            }
            n
        };

        let first = render_frame(&mut vx);
        assert!(
            count(&first, &open) >= 1,
            "the first render emits the URL's OSC 8 open"
        );

        // A progress line accretes above the URL, pushing it down. Insert it
        // before the URL line so the URL genuinely shifts to new rows.
        state.lock().unwrap().lines.insert(
            1,
            LoginLine::Progress("Waiting for the browser\u{2026}".to_string()),
        );

        let second = render_frame(&mut vx);
        assert!(
            count(&second, &open) >= 1,
            "the URL's OSC 8 open survives the redraw that shifts it down"
        );
        // The link closes a bounded number of times, not once per blank cell
        // below the URL (that would be hundreds on this grid).
        let clears = count(&second, vaxis::ctlseqs::OSC8_CLEAR.as_bytes());
        assert!(
            clears <= 8,
            "expected a bounded number of link closes, got {clears}"
        );
    }

    /// The inverse of the OSC 8 render test: with the probed `hyperlinks`
    /// capability off, the same dialog renders the URL as plain text and emits
    /// no OSC 8 hyperlink. This pins the dialog's caps consumption. Were the
    /// dialog to ignore the caps and assume hyperlinks are always on, the
    /// `id=aj-oauth` open would appear and redden this test.
    #[test]
    fn login_url_omits_osc8_when_hyperlinks_capability_off() {
        use vaxis::Winsize;
        use vaxis::vaxis::{Options as VaxisOptions, Vaxis};

        let url =
            "https://auth.example.com/authorize?client_id=abcdef123456&scope=read+write&state=xyz";

        let state = Arc::new(StdMutex::new(LoginDialogState::default()));
        state
            .lock()
            .unwrap()
            .lines
            .push(LoginLine::Url(url.to_string()));
        let pending: PendingInput = Arc::new(StdMutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let dialog = Rc::new(RefCell::new(LoginDialog::new(
            &theme(),
            TerminalCaps {
                hyperlinks: false,
                ..TerminalCaps::default()
            },
            Arc::clone(&state),
            pending,
            cancel,
        )));
        let dialog_ref = to_widget_ref(Rc::clone(&dialog));

        let winsize = Winsize {
            rows: 40,
            cols: 40,
            x_pixel: 0,
            y_pixel: 0,
        };
        let mut vx = Vaxis::new(VaxisOptions::default());
        let mut sink: Vec<u8> = Vec::new();
        vx.enter_alt_screen(&mut sink).expect("alt");
        vx.resize(&mut sink, winsize).expect("resize");

        let ctx = crate::test_support::draw_ctx(40, Some(14));
        let surface = draw_widget(&dialog_ref, &ctx);
        surface.render(vx.window(), None);
        let mut out = Vec::new();
        vx.render(&mut out).expect("render");

        // `id=aj-oauth` appears only in the OSC 8 params, never as printed
        // text, so its absence proves no hyperlink was emitted.
        let marker = b"id=aj-oauth";
        assert!(
            !out.windows(marker.len()).any(|w| w == marker),
            "no OSC 8 hyperlink when the capability is off",
        );
        // The URL still renders, just as plain styled text.
        let rows = crate::test_support::rows(&surface);
        assert!(
            rows.iter().any(|r| r.contains("auth.example.com")),
            "the URL still renders as text: {rows:?}",
        );
    }

    fn confirmation_for(
        raw_label: String,
    ) -> (AccountConfirmation, Rc<RefCell<Option<AuthPickerRequest>>>) {
        let request = Rc::new(RefCell::new(None));
        let stack = Rc::new(RefCell::new(OverlayStack::default()));
        let editor: WidgetRef = Rc::new(RefCell::new(Text::new("")));
        let action = AccountAction::Logout {
            provider_id: "provider".to_string(),
            account_label: raw_label,
        };
        (
            AccountConfirmation::new(action, Rc::clone(&request), stack, editor, Style::default()),
            request,
        )
    }

    #[test]
    fn account_confirmation_preserves_internal_space_without_softwrap() {
        let (mut one, _) = confirmation_for("a b".to_string());
        let (mut many, _) = confirmation_for("a    b".to_string());
        let ctx = crate::test_support::draw_ctx(2, Some(4));
        assert!(!one.text.borrow().softwrap);
        assert!(!many.text.borrow().softwrap);
        let _ = one.draw(&ctx);
        let _ = many.draw(&ctx);
        assert!(one.view.borrow().has_more_right());
        assert!(many.view.borrow().has_more_right());

        let mut event_ctx = EventContext::new();
        let mut final_rows = String::new();
        for expected_left in 1..=4 {
            many.handle_event(
                &mut event_ctx,
                &key_event(Key::RIGHT, Modifiers::empty(), None),
            );
            final_rows = crate::test_support::rows(&many.draw(&ctx)).join("\n");
            assert_eq!(many.view.borrow().scroll_left(), expected_left);
        }
        assert!(
            final_rows.contains(" b"),
            "final cells remain in order: {final_rows:?}"
        );
        assert!(!many.view.borrow().has_more_right());
    }

    #[test]
    fn account_confirmation_keeps_rtl_graphemes_in_logical_cells_without_isolates() {
        let raw = "אב";
        let (mut confirmation, _) = confirmation_for(raw.to_string());
        let rows = crate::test_support::rows(
            &confirmation.draw(&crate::test_support::draw_ctx(20, Some(4))),
        )
        .join("\n");
        assert!(rows.contains(raw), "logical source order changed: {rows:?}");
        assert!(!rows.contains('\u{2068}'), "vaxis must not inject FSI");
        assert!(!rows.contains('\u{2069}'), "vaxis must not inject PDI");
    }

    #[test]
    fn account_confirmation_end_uses_the_active_width_method() {
        let raw = format!("{}個", "👋🏿".repeat(31));
        assert!(
            validate_account_label(&raw).is_ok(),
            "creation-valid fixture"
        );
        let (mut confirmation, _) = confirmation_for(raw);
        let mut ctx = crate::test_support::draw_ctx(10, Some(4));
        ctx.width_method = vaxis::gwidth::Method::Wcwidth;
        let _ = confirmation.draw(&ctx);
        assert_eq!(confirmation.represented_cells, 126);

        let mut event_ctx = EventContext::new();
        confirmation.handle_event(
            &mut event_ctx,
            &key_event(Key::END, Modifiers::empty(), None),
        );
        let tail = crate::test_support::rows(&confirmation.draw(&ctx)).join("\n");
        assert!(
            tail.contains('個'),
            "End reaches the final grapheme: {tail:?}"
        );
        assert!(!confirmation.view.borrow().has_more_right());
    }

    #[test]
    fn exact_limit_account_confirmation_scrolls_to_the_final_tail() {
        let raw = format!("{}\u{0100}", "a".repeat(10_921));
        let (mut confirmation, _) = confirmation_for(raw);
        assert_eq!(confirmation.represented_cells, 65_535);
        assert_eq!(confirmation.surface_representation_cells, 65_535);
        assert!(!confirmation.over_limit);

        let ctx = crate::test_support::draw_ctx(24, Some(4));
        let _ = confirmation.draw(&ctx);
        let mut event_ctx = EventContext::new();
        confirmation.handle_event(
            &mut event_ctx,
            &key_event(Key::END, Modifiers::empty(), None),
        );
        let tail = crate::test_support::rows(&confirmation.draw(&ctx)).join("\n");
        assert!(
            tail.contains("\\u{100}"),
            "final tail is reachable: {tail:?}"
        );
    }

    #[test]
    fn over_limit_account_requires_acknowledgement_and_keeps_raw_identity() {
        let raw = format!("{}\u{1000}", "a".repeat(10_921));
        let (mut confirmation, request) = confirmation_for(raw.clone());
        assert_eq!(confirmation.represented_cells, 65_536);
        assert_eq!(
            confirmation.surface_representation_cells, OVER_LIMIT_PREFIX_CELLS,
            "only a bounded represented prefix enters vaxis"
        );
        assert!(confirmation.over_limit);

        let mut ctx = EventContext::new();
        confirmation.handle_event(&mut ctx, &key_event(Key::ENTER, Modifiers::empty(), None));
        assert!(confirmation.acknowledged);
        assert!(
            request.borrow().is_none(),
            "acknowledgement is not the action"
        );
        let warning = crate::test_support::rows(
            &confirmation.draw(&crate::test_support::draw_ctx(80, Some(4))),
        )
        .join("\n");
        assert!(
            warning.contains("Incomplete inspection acknowledged"),
            "{warning}"
        );

        confirmation.handle_event(&mut ctx, &key_event(Key::ENTER, Modifiers::empty(), None));
        assert!(matches!(
            request.borrow().as_ref(),
            Some(AuthPickerRequest::ApplyAccount(AccountAction::Logout {
                provider_id,
                account_label,
            })) if provider_id == "provider" && account_label == &raw
        ));
    }

    #[test]
    fn over_limit_account_end_stays_within_the_bounded_surface_extent() {
        let raw = format!("{}\u{1000}", "a".repeat(10_921));
        let (mut confirmation, _) = confirmation_for(raw);
        let draw_ctx = crate::test_support::draw_ctx(24, Some(4));
        let _ = confirmation.draw(&draw_ctx);

        // The complete legacy extent can exceed every scroll offset type, but
        // only the bounded disclosed prefix belongs to the vaxis surface.
        confirmation.represented_cells = usize::try_from(u32::MAX).unwrap() + 25;
        let mut event_ctx = EventContext::new();
        confirmation.handle_event(
            &mut event_ctx,
            &key_event(Key::END, Modifiers::empty(), None),
        );
        assert_eq!(
            confirmation.view.borrow().scroll_left(),
            u32::try_from(OVER_LIMIT_PREFIX_CELLS - 24).unwrap()
        );
        let rows = crate::test_support::rows(&confirmation.draw(&draw_ctx));
        assert!(
            rows[0].contains("\\u{61}"),
            "bounded prefix disappeared: {rows:?}"
        );
        assert!(!confirmation.view.borrow().has_more_right());
    }

    fn sample_rows() -> Vec<AuthRow> {
        vec![
            AuthRow {
                request: AuthPickerRequest::Login {
                    provider_id: "anthropic".to_string(),
                    provider_name: "Anthropic (Claude Pro/Max)".to_string(),
                    target: LoginTarget::NewAccount,
                },
                label: "Anthropic (Claude Pro/Max)".to_string(),
                filter_key: "anthropic Anthropic (Claude Pro/Max)".to_string(),
                summary: "subscription".to_string(),
            },
            AuthRow {
                request: AuthPickerRequest::Login {
                    provider_id: "openai".to_string(),
                    provider_name: "OpenAI".to_string(),
                    target: LoginTarget::NewAccount,
                },
                label: "OpenAI".to_string(),
                filter_key: "openai OpenAI".to_string(),
                summary: "not configured".to_string(),
            },
        ]
    }

    /// Rows carry the friendly label, the status summary as description,
    /// and an id-plus-label filter key so typing either finds the row.
    #[test]
    fn picker_items_carry_label_summary_and_filter_key() {
        let items = picker_items(&sample_rows());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "Anthropic (Claude Pro/Max)");
        assert_eq!(items[0].description.as_deref(), Some("subscription"));
        assert!(items[0].filter_key.contains("anthropic"));
        assert!(items[0].filter_key.contains("Anthropic (Claude Pro/Max)"));
        assert_eq!(items[0].value.as_deref(), Some("auth-row-0"));
    }

    /// Confirm resolution uses only the opaque value. Search-key collisions do
    /// not collapse exact action identity.
    #[test]
    fn auth_request_resolves_by_opaque_value() {
        let items = picker_items(&sample_rows());
        let map = picker_requests(sample_rows());
        assert!(matches!(
            map.get(items[0].value.as_deref().expect("opaque value")),
            Some(AuthPickerRequest::Login { provider_id, provider_name, .. })
                if provider_id == "anthropic" && provider_name.contains("Anthropic")
        ));
        assert!(!map.contains_key("nope"));
    }

    /// Opening either picker pushes an overlay onto the stack.
    #[test]
    fn openers_push_an_overlay() {
        let editor: WidgetRef = Rc::new(RefCell::new(Text::new("")));
        let chrome = OverlayChrome::from_theme(&theme());

        let stack = Rc::new(RefCell::new(OverlayStack::default()));
        let request = Rc::new(RefCell::new(None));
        open_login_picker(&stack, &editor, &chrome, &request, sample_rows());
        assert!(stack.borrow().is_open(), "login picker pushed");

        let stack = Rc::new(RefCell::new(OverlayStack::default()));
        let request = Rc::new(RefCell::new(None));
        open_logout_picker(&stack, &editor, &chrome, &request, sample_rows());
        assert!(stack.borrow().is_open(), "logout picker pushed");
    }
}
