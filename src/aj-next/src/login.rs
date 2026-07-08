//! The OAuth login flow: the provider pickers, the login dialog overlay,
//! and the [`OAuthCallbacks`] driver that streams updates into it.
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
use aj_models::oauth::{OAuthAuthInfo, OAuthCallbacks, OAuthError};
use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use vaxis::cell::{Hyperlink, Segment, Style};
use vaxis::key::{Key, Modifiers};
use vaxis::vxfw::{
    Builder, CursorState, DrawContext, Event, EventContext, FilterableSelect, ListView, MaxSize,
    RelativePoint, RichText, SelectItem, Size, Source, SubSurface, Surface, Text, Widget,
    WidgetRef, WidthBasis, draw_widget, to_widget_ref,
};

use crate::overlay::{
    OverlayChrome, OverlayPlacement, OverlayStack, close_top, subtitle_confirm_close,
    subtitle_login,
};
use crate::settings_ui::push_window;
use crate::terminal::TERMINAL_HYPERLINKS;
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
    /// The authorization URL, set by [`OAuthCallbacks::on_auth`]. Held
    /// separately from its `Url` display line so Ctrl+Y can copy it.
    pub(crate) url: Option<String>,
}

/// Resolved line colors for the dialog's line kinds and its input/notice
/// rows. `Copy` (all fields are `vaxis` [`Style`]s) so the row [`Builder`]
/// and the widget can each hold one.
#[derive(Clone, Copy)]
struct LoginStyles {
    info: Style,
    progress: Style,
    url: Style,
    prompt: Style,
    notice: Style,
}

impl LoginStyles {
    fn from_theme(theme: &Theme) -> LoginStyles {
        let mode = theme.color_mode();
        let fg = |token: ThemeColor| Style {
            fg: vaxis_color(theme.fg_color(token), mode),
            ..Style::default()
        };
        LoginStyles {
            info: fg(ThemeColor::Text),
            progress: fg(ThemeColor::Muted),
            url: fg(ThemeColor::Accent),
            prompt: fg(ThemeColor::Dim),
            notice: fg(ThemeColor::Dim),
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
                let mut widget =
                    RichText::new(vec![url_segment(url, self.styles.url, TERMINAL_HYPERLINKS)]);
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
/// [`OpenOverlay`](crate::overlay::OpenOverlay)), so every key reaches it
/// at-target. It handles the dialog chords itself (Esc cancel, Ctrl+Y
/// copy, arrows/page scroll) and forwards the rest to the inner
/// [`TextField`] while a prompt is active. The field is never focused, so
/// its cursor is lifted onto this widget's surface at draw time.
pub(crate) struct LoginDialog {
    state: Arc<StdMutex<LoginDialogState>>,
    cancel: Arc<AtomicBool>,
    field: Rc<RefCell<vaxis::vxfw::TextField>>,
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
        state: Arc<StdMutex<LoginDialogState>>,
        pending_input: PendingInput,
        cancel: Arc<AtomicBool>,
    ) -> LoginDialog {
        let styles = LoginStyles::from_theme(theme);
        let mut list = ListView::new(Source::Builder(Box::new(LineBuilder {
            state: Arc::clone(&state),
            styles,
        })));
        // Document scroll with no visible cursor: the arrow/page keys move
        // the hidden cursor to follow the viewport.
        list.draw_cursor = false;
        let list = Rc::new(RefCell::new(list));

        let field = Rc::new(RefCell::new(vaxis::vxfw::TextField::new()));
        {
            // On submit the field hands us its (cleared) contents; deliver
            // them to the awaiting callback. Fires from the field's own
            // handle_event, which we call from `LoginDialog::handle_event`,
            // so the borrows below can't collide with the widget's.
            let pending = Arc::clone(&pending_input);
            let st = Arc::clone(&state);
            field.borrow_mut().on_submit = Some(Box::new(move |_ctx, value| {
                let value = value.trim().to_string();
                let sender = pending.lock().expect("pending input poisoned").take();
                match sender {
                    // Only deliver a non-empty value; a stray empty submit
                    // leaves the prompt in place for the real paste.
                    Some(tx) if !value.is_empty() => {
                        let _ = tx.send(value);
                        st.lock().expect("login dialog state poisoned").input_prompt = None;
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
            list,
            styles,
            notice: None,
            auto_copied: false,
        }
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

        let (line_count, prompt, url) = {
            let st = self.state.lock().expect("login dialog state poisoned");
            (st.lines.len(), st.input_prompt.clone(), st.url.clone())
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
            let label_ref = to_widget_ref(Rc::new(RefCell::new(label)));
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
                surface: draw_widget(&label_ref, &one_row),
                z_index: 0,
            });
            row = row.saturating_add(1);

            let field_surface = draw_widget(&to_widget_ref(Rc::clone(&self.field)), &one_row);
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
            let notice_ref = to_widget_ref(Rc::new(RefCell::new(text)));
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
                surface: draw_widget(&notice_ref, &one_row),
                z_index: 0,
            });
        }

        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
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

        // Manual-paste editing: forward printables/backspace/submit to the
        // field only while a prompt is active. Enter fires the field's
        // on_submit, which delivers the value to the awaiting callback.
        let prompting = self
            .state
            .lock()
            .expect("login dialog state poisoned")
            .input_prompt
            .is_some();
        if prompting {
            self.field.borrow_mut().handle_event(ctx, event);
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
    async fn await_input(&self, prompt: &str) -> Result<String, OAuthError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.state.lock().expect("login dialog state poisoned");
            st.input_prompt = Some(prompt.to_string());
            *self.pending_input.lock().expect("pending input poisoned") = Some(tx);
        }
        self.ping();
        rx.await.map_err(|_| OAuthError::Cancelled)
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
        self.await_input(message).await
    }

    fn on_progress(&self, message: &str) {
        self.push_line(LoginLine::Progress(message.to_string()));
    }

    async fn on_manual_code_input(&self) -> Result<String, OAuthError> {
        // Enter is the field's built-in submit key. Its handling is a fixed
        // convention, but the label resolves through `format_keybinding` so it
        // reads from one source (see the NOTE in `crate::overlay`).
        let submit = format_keybinding("enter");
        self.await_input(&format!(
            "On another machine? Paste the code shown after login (or the full redirect URL), \
             then press {submit}:"
        ))
        .await
    }

    fn supports_manual_code_input(&self) -> bool {
        true
    }
}

/// What confirming a provider row in the login/logout picker asks the
/// host to do. Parked in the shell's `auth_request` slot for the drive
/// loop to drain after the confirming keystroke.
pub(crate) enum AuthPickerRequest {
    /// Start the provider's OAuth browser login flow.
    Login {
        provider_id: String,
        provider_name: String,
    },
    /// Remove the provider's stored `auth.json` credential.
    Logout { provider_id: String },
}

/// One provider row for a picker: the id returned to the host, the
/// friendly label shown as the primary column, and a status summary shown
/// as the dim description.
pub(crate) struct AuthRow {
    pub(crate) provider_id: String,
    pub(crate) label: String,
    pub(crate) summary: String,
}

/// Which action the picker's confirm parks.
#[derive(Clone, Copy)]
enum PickerMode {
    Login,
    Logout,
}

/// Build one selectable row: the friendly label as the primary column, the
/// status summary as the dim description, and `"{id} {label}"` as the
/// filter key so typing either the id or the name finds it.
fn picker_items(rows: &[AuthRow]) -> Vec<SelectItem> {
    rows.iter()
        .map(|row| {
            SelectItem::new(
                row.label.clone(),
                format!("{} {}", row.provider_id, row.label),
            )
            .with_description(row.summary.clone())
        })
        .collect()
}

/// Resolve a confirmed row's filter key back to the action the picker
/// should park. Returns `None` for a key absent from the map (a row not
/// backed by a provider, which shouldn't happen but is handled safely).
fn auth_request_for(
    map: &HashMap<String, (String, String)>,
    mode: PickerMode,
    filter_key: &str,
) -> Option<AuthPickerRequest> {
    let (provider_id, provider_name) = map.get(filter_key).cloned()?;
    Some(match mode {
        PickerMode::Login => AuthPickerRequest::Login {
            provider_id,
            provider_name,
        },
        PickerMode::Logout => AuthPickerRequest::Logout { provider_id },
    })
}

/// Open a provider picker and move focus into its filter (via the caller's
/// refocus event). Confirming a row parks the matching [`AuthPickerRequest`]
/// in `request_slot` and closes; Esc closes without a request.
///
/// The confirmed id is recovered through a filter-key -> (id, name) map,
/// the same indirection the command palette and session selector use,
/// since the widget hands the confirm callback only the row's filter key.
fn open_auth_picker(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    request_slot: &Rc<RefCell<Option<AuthPickerRequest>>>,
    title: &str,
    rows: Vec<AuthRow>,
    mode: PickerMode,
) {
    let select = Rc::new(RefCell::new(FilterableSelect::new(
        picker_items(&rows),
        chrome.select.clone(),
    )));
    let focus = select.borrow().focus_target();
    let map: HashMap<String, (String, String)> = rows
        .into_iter()
        .map(|row| {
            (
                format!("{} {}", row.provider_id, row.label),
                (row.provider_id, row.label),
            )
        })
        .collect();
    {
        let mut sel = select.borrow_mut();
        let request_c = Rc::clone(request_slot);
        let stack_c = Rc::clone(stack);
        let editor_c = Rc::clone(editor);
        sel.on_confirm = Some(Box::new(move |ctx, item| {
            if let Some(request) = auth_request_for(&map, mode, &item.filter_key) {
                *request_c.borrow_mut() = Some(request);
            }
            close_top(&stack_c, ctx, &editor_c);
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

/// Open the `/login` picker over the OAuth providers.
pub(crate) fn open_login_picker(
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
        "Log in",
        rows,
        PickerMode::Login,
    );
}

/// Open the `/logout` picker over the providers with a stored credential.
pub(crate) fn open_logout_picker(
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
        "Log out",
        rows,
        PickerMode::Logout,
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
    provider_name: &str,
    state: Arc<StdMutex<LoginDialogState>>,
    pending_input: PendingInput,
    cancel: Arc<AtomicBool>,
) {
    let dialog = Rc::new(RefCell::new(LoginDialog::new(
        theme,
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
            styles: LoginStyles::from_theme(&theme()),
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

    fn sample_rows() -> Vec<AuthRow> {
        vec![
            AuthRow {
                provider_id: "anthropic".to_string(),
                label: "Anthropic (Claude Pro/Max)".to_string(),
                summary: "subscription".to_string(),
            },
            AuthRow {
                provider_id: "openai".to_string(),
                label: "OpenAI".to_string(),
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
    }

    /// Confirm resolution maps a row's filter key back to the right action:
    /// Login carries the id and name, Logout carries the id, and an unknown
    /// key resolves to nothing.
    #[test]
    fn auth_request_resolves_by_mode() {
        let map: HashMap<String, (String, String)> = sample_rows()
            .into_iter()
            .map(|row| {
                (
                    format!("{} {}", row.provider_id, row.label),
                    (row.provider_id, row.label),
                )
            })
            .collect();
        let key = "anthropic Anthropic (Claude Pro/Max)";
        assert!(matches!(
            auth_request_for(&map, PickerMode::Login, key),
            Some(AuthPickerRequest::Login { provider_id, provider_name })
                if provider_id == "anthropic" && provider_name.contains("Anthropic")
        ));
        assert!(matches!(
            auth_request_for(&map, PickerMode::Logout, key),
            Some(AuthPickerRequest::Logout { provider_id }) if provider_id == "anthropic"
        ));
        assert!(auth_request_for(&map, PickerMode::Login, "nope").is_none());
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
