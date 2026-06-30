//! Interactive OAuth login dialog and its [`OAuthCallbacks`] driver.
//!
//! Unlike the other overlays (which are synchronous "write an outcome
//! slot, host polls it" selectors), an OAuth login is **async and
//! long-running**: the flow binds a localhost callback server, opens
//! the browser, and waits for the redirect — or for the user to paste
//! the redirect URL back if their browser is on another machine.
//!
//! The split mirrors the headless-provider pattern the credential
//! engine is built around:
//!
//! - The login flow ([`aj_models::oauth::OAuthProvider`]) runs on a
//!   spawned task and *asks* the UI for things via
//!   [`OAuthCallbacks`].
//! - [`TuiOAuthCallbacks`] satisfies those asks by writing into a
//!   shared [`LoginDialogState`] and requesting a render. Fire-and-
//!   forget asks (`on_auth`, `on_progress`) just push display lines;
//!   input-gathering asks (`on_prompt`, `on_manual_code_input`)
//!   install a [`oneshot`] sender and await its receiver.
//! - [`LoginDialogComponent`] renders that shared state and, on
//!   `Enter`, delivers the text-input value to whichever callback is
//!   awaiting. `Esc` flips a shared cancel flag the host polls to
//!   tear the dialog down and abort the task.
//!
//! No lock is ever held across an `.await`, so the plain
//! [`std::sync::Mutex`] shared between the UI thread and the login
//! task is safe.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aj_models::oauth::{OAuthAuthInfo, OAuthCallbacks, OAuthError};
use aj_tui::component::Component;
use aj_tui::components::text_input::TextInput;
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;
use aj_tui::tui::RenderHandle;
use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::config::theme::{ThemeColor, ThemeHandle};

/// A single display line in the dialog, tagged so the component can
/// color it through the theme.
#[derive(Clone)]
pub enum LoginLine {
    /// Plain informational text.
    Info(String),
    /// The authorization URL (rendered in the accent color).
    Url(String),
    /// A progress/status update emitted by the flow.
    Progress(String),
}

/// Display state shared between [`TuiOAuthCallbacks`] (writer, on the
/// login task) and [`LoginDialogComponent`] (reader, on the UI
/// thread).
#[derive(Default)]
pub struct LoginDialogState {
    /// Status lines shown top-to-bottom.
    pub lines: Vec<LoginLine>,
    /// When `Some`, the text input is active with this prompt label
    /// and the next `Enter` delivers its value to the awaiting
    /// callback.
    pub input_prompt: Option<String>,
    /// The authorization URL, set by [`OAuthCallbacks::on_auth`]. Held
    /// separately from its `Url` display line so the dialog can copy
    /// it to the clipboard on demand (Ctrl+Y) and auto-copy it the
    /// first time it appears.
    pub url: Option<String>,
}

/// Slot holding the sender an input-awaiting callback is blocked on.
/// The dialog takes it on `Enter` and fulfils it with the typed text.
type PendingInput = Arc<Mutex<Option<oneshot::Sender<String>>>>;

/// Theme closures for the dialog's line kinds.
struct DialogStyle {
    url: Arc<dyn Fn(&str) -> String>,
    progress: Arc<dyn Fn(&str) -> String>,
    info: Arc<dyn Fn(&str) -> String>,
    hint: Arc<dyn Fn(&str) -> String>,
}

/// The login dialog overlay component.
pub struct LoginDialogComponent {
    state: Arc<Mutex<LoginDialogState>>,
    pending_input: PendingInput,
    cancel: Arc<AtomicBool>,
    input: TextInput,
    style: DialogStyle,
    focused: bool,
    /// Set once the authorization URL has been auto-copied so the
    /// copy-on-first-display in `render` is idempotent.
    auto_copied: bool,
    /// Ephemeral feedback line (e.g. "Copied …") rendered at the
    /// bottom of the dialog.
    notice: Option<String>,
}

impl LoginDialogComponent {
    /// Build the dialog over shared handles also held by the login
    /// task's [`TuiOAuthCallbacks`] and by the host's login session.
    pub fn new(
        theme: &ThemeHandle,
        state: Arc<Mutex<LoginDialogState>>,
        pending_input: PendingInput,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        let mut input = TextInput::new("> ");
        input.set_focused(true);
        Self {
            state,
            pending_input,
            cancel,
            input,
            style: DialogStyle {
                url: theme.fg_closure(ThemeColor::Accent),
                progress: theme.fg_closure(ThemeColor::Muted),
                info: theme.fg_closure(ThemeColor::Text),
                hint: theme.fg_closure(ThemeColor::Dim),
            },
            focused: true,
            auto_copied: false,
            notice: None,
        }
    }
}

/// Split `s` into successive `width`-character chunks so a long URL
/// stays fully visible (and copyable) instead of being truncated to
/// the overlay width. Counts characters, not bytes.
fn wrap_chars(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return vec![s.to_string()];
    }
    chars
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Wrap `text` as an OSC 8 hyperlink pointing at `uri`.
///
/// Callers gate emission on detected hyperlink support
/// ([`aj_tui::capabilities::get_capabilities`]) — including tmux, which
/// only forwards OSC 8 when the client advertises the `hyperlinks`
/// feature — so this helper just builds the sequence. The escapes are
/// OSC sequences, which [`crate::ansi`]-style width measurement
/// (`visible_width`) treats as zero-width, so they don't disturb the
/// overlay's width-based padding.
///
/// A long URL is rendered as several wrapped segments, each its own
/// hyperlink. The shared `id=` parameter tells the terminal those
/// segments are one logical link (per the OSC 8 spec for soft-wrapped
/// hyperlinks), so hover-highlighting spans the whole URL and clicking
/// any segment opens the full `uri`.
fn hyperlink(uri: &str, text: &str) -> String {
    // Defensive: a stray ESC/BEL in the URI would break out of the
    // OSC 8 sequence. Our login URLs are provider-constructed, but
    // strip control bytes regardless.
    let safe: String = uri.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]8;id=aj-oauth;{safe}\x1b\\{text}\x1b]8;;\x1b\\")
}

impl Component for LoginDialogComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<aj_tui::Line> {
        let (lines, prompt, url) = {
            let st = self.state.lock().expect("login dialog state poisoned");
            (st.lines.clone(), st.input_prompt.clone(), st.url.clone())
        };

        // Copy the URL to the clipboard the first time it appears so
        // the user never has to select it out of the overlay. Guarded
        // by `auto_copied` to stay idempotent across renders; this runs
        // on the UI (render) thread, which is where the OSC 52 write in
        // `copy_to_clipboard` must happen.
        if let Some(url) = &url
            && !self.auto_copied
        {
            crate::auth::copy_to_clipboard(url);
            self.auto_copied = true;
            let copy = crate::config::keybindings::fixed_keys::CTRL_Y;
            self.notice = Some(format!(
                "Copied the authorization URL to your clipboard ({copy} to copy again)."
            ));
        }

        let mut out: Vec<aj_tui::Line> = Vec::new();
        for line in &lines {
            match line {
                LoginLine::Info(t) => out.push((self.style.info)(t).into()),
                LoginLine::Progress(t) => out.push((self.style.progress)(t).into()),
                LoginLine::Url(u) => {
                    // Each wrapped chunk is its own hyperlink to the
                    // full URL, so clicking any visible part opens it —
                    // but only when the terminal forwards OSC 8 (tmux
                    // without the `hyperlinks` feature strips it). When
                    // it doesn't, the chunk text is itself part of the
                    // URL, so the plain styled chunk still shows the
                    // full link.
                    let hyperlinks = aj_tui::capabilities::get_capabilities().hyperlinks;
                    for chunk in wrap_chars(u, width) {
                        let styled = (self.style.url)(&chunk);
                        out.push(
                            if hyperlinks {
                                hyperlink(u, &styled)
                            } else {
                                styled
                            }
                            .into(),
                        );
                    }
                }
            }
        }

        if let Some(prompt) = prompt {
            out.push(aj_tui::Line::default());
            out.push((self.style.hint)(&prompt).into());
            out.extend(self.input.render(width));
        }

        if let Some(notice) = &self.notice {
            out.push(aj_tui::Line::default());
            out.push((self.style.hint)(notice).into());
        }
        out
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        // Copy the authorization URL to the clipboard. Hard-coded
        // (like the global Ctrl+C handling) rather than a
        // rebindable action; checked before routing to the text input
        // so it works even while the manual-paste field is focused.
        if event.is_ctrl('y') {
            let url = self
                .state
                .lock()
                .expect("login dialog state poisoned")
                .url
                .clone();
            self.notice = Some(match url {
                Some(url) => {
                    crate::auth::copy_to_clipboard(&url);
                    "Copied the authorization URL to your clipboard.".to_string()
                }
                None => "No authorization URL to copy yet.".to_string(),
            });
            return true;
        }

        let kb = keybindings::get();

        if kb.matches(event, "tui.select.cancel") {
            // Flip the shared flag; the host polls it and tears the
            // dialog down + aborts the login task.
            self.cancel.store(true, Ordering::Relaxed);
            return true;
        }

        if kb.matches(event, "tui.input.submit") {
            let value = self.input.value().trim().to_string();
            // Only deliver when a callback is actually awaiting input
            // and the field is non-empty; otherwise swallow the stray
            // Enter while the flow is still working.
            let pending = self
                .pending_input
                .lock()
                .expect("pending input poisoned")
                .take();
            match pending {
                Some(tx) if !value.is_empty() => {
                    let _ = tx.send(value);
                    self.input.clear();
                    self.state
                        .lock()
                        .expect("login dialog state poisoned")
                        .input_prompt = None;
                }
                // Put the sender back if we didn't use it.
                other => {
                    *self.pending_input.lock().expect("pending input poisoned") = other;
                }
            }
            return true;
        }

        self.input.handle_input(event)
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

/// [`OAuthCallbacks`] implementation that drives a
/// [`LoginDialogComponent`] from the spawned login task.
pub struct TuiOAuthCallbacks {
    state: Arc<Mutex<LoginDialogState>>,
    pending_input: PendingInput,
    render: RenderHandle,
}

impl TuiOAuthCallbacks {
    /// Build the callbacks over the same shared handles the dialog
    /// component holds. `render` requests a repaint after each state
    /// mutation so the async updates show up promptly.
    pub fn new(
        state: Arc<Mutex<LoginDialogState>>,
        pending_input: PendingInput,
        render: RenderHandle,
    ) -> Self {
        Self {
            state,
            pending_input,
            render,
        }
    }

    fn push_line(&self, line: LoginLine) {
        self.state
            .lock()
            .expect("login dialog state poisoned")
            .lines
            .push(line);
        self.render.request_render();
    }

    /// Reveal the text input with `prompt`, park a sender in the
    /// shared slot, and await the value the dialog delivers. A dropped
    /// receiver (overlay torn down, or the flow cancelled this branch
    /// of a race) resolves to [`OAuthError::Cancelled`].
    async fn await_input(&self, prompt: &str) -> Result<String, OAuthError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.state.lock().expect("login dialog state poisoned");
            st.input_prompt = Some(prompt.to_string());
            *self.pending_input.lock().expect("pending input poisoned") = Some(tx);
        }
        self.render.request_render();
        rx.await.map_err(|_| OAuthError::Cancelled)
    }
}

/// Compose the dialog's authorization-step lines plus the URL we store
/// for clipboard copy.
///
/// Pure so the headless wording can be unit-tested without sniffing the
/// environment through [`crate::auth::browser_available`] or touching the
/// clipboard. `copy` is the rendered shortcut for the copy action.
///
/// The headless wording depends on whether the provider has a hosted
/// manual page ([`OAuthAuthInfo::manual_url`]). With one, the user pastes
/// a code that page shows. Without one (the redirect targets this
/// machine's loopback, which a remote browser can't reach), the user has
/// to copy the failed-redirect URL out of their browser's address bar, so
/// we say so explicitly.
fn auth_lines(can_open: bool, info: &OAuthAuthInfo<'_>, copy: &str) -> (Vec<LoginLine>, String) {
    let mut lines = Vec::new();

    if can_open {
        lines.push(LoginLine::Info(
            "Opening your browser to authorize…".to_string(),
        ));
        if let Some(instructions) = info.instructions {
            lines.push(LoginLine::Info(instructions.to_string()));
        }
        lines.push(LoginLine::Info(format!(
            "If it doesn't open, click or copy ({copy}) this URL:"
        )));
        lines.push(LoginLine::Url(info.url.to_string()));
        if let Some(manual) = info.manual_url {
            lines.push(LoginLine::Info(
                "On a different machine? Open this URL instead, then paste the code it shows:"
                    .to_string(),
            ));
            lines.push(LoginLine::Url(manual.to_string()));
        }
        return (lines, info.url.to_string());
    }

    lines.push(LoginLine::Info(
        "No browser detected on this machine (headless/SSH).".to_string(),
    ));
    match info.manual_url {
        Some(manual) => {
            lines.push(LoginLine::Info(format!(
                "Open this URL on another device ({copy} to copy), then paste the code it shows:"
            )));
            lines.push(LoginLine::Url(manual.to_string()));
            (lines, manual.to_string())
        }
        None => {
            lines.push(LoginLine::Info(format!(
                "Open this URL on another device and sign in ({copy} to copy):"
            )));
            lines.push(LoginLine::Url(info.url.to_string()));
            lines.push(LoginLine::Info(
                "Your browser will then try to reach this machine and show a connection error. \
                 That's expected: copy the full URL from its address bar and paste it here."
                    .to_string(),
            ));
            (lines, info.url.to_string())
        }
    }
}

#[async_trait]
impl OAuthCallbacks for TuiOAuthCallbacks {
    fn on_auth(&self, info: OAuthAuthInfo<'_>) {
        // When we can't reach a browser here (SSH/headless), opening the
        // automatic URL is pointless: its redirect targets this machine's
        // loopback, which the user's remote browser can't reach. The line
        // composition in `auth_lines` steers the user accordingly.
        let can_open = crate::auth::browser_available();
        let copy = crate::config::keybindings::fixed_keys::CTRL_Y;
        let (lines, url) = auth_lines(can_open, &info, copy);
        {
            let mut st = self.state.lock().expect("login dialog state poisoned");
            st.lines.extend(lines);
            st.url = Some(url);
        }
        if can_open {
            crate::auth::open_browser(info.url);
        }
        self.render.request_render();
    }

    async fn on_prompt(&self, message: &str) -> Result<String, OAuthError> {
        self.await_input(message).await
    }

    fn on_progress(&self, message: &str) {
        self.push_line(LoginLine::Progress(message.to_string()));
    }

    async fn on_manual_code_input(&self) -> Result<String, OAuthError> {
        // The dialog submits via `tui.input.submit`, so the prompt
        // resolves that action's key rather than hardcoding "Enter".
        let submit = keybindings::format_action_shortcut("tui.input.submit")
            .unwrap_or_else(|| "Enter".to_string());
        self.await_input(&format!(
            "On another machine? Paste the code shown after login (or the full redirect URL), then press {submit}:"
        ))
        .await
    }

    fn supports_manual_code_input(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_tui::capabilities::{TerminalCapabilities, reset_capabilities_cache, set_capabilities};
    use aj_tui::keys::Key;

    /// Pin the process-wide hyperlink capability for a test, restoring
    /// detection on drop. Only `hyperlinks` is meaningful here; the
    /// other fields are left at their conservative defaults so this
    /// doesn't perturb image/true-color-sensitive code elsewhere.
    /// Paired with `#[serial_test::serial]` because the cache is shared.
    struct CapsGuard;
    impl Drop for CapsGuard {
        fn drop(&mut self) {
            reset_capabilities_cache();
        }
    }
    fn with_hyperlinks(hyperlinks: bool) -> CapsGuard {
        set_capabilities(TerminalCapabilities {
            hyperlinks,
            true_color: false,
            images: None,
        });
        CapsGuard
    }

    fn theme() -> ThemeHandle {
        ThemeHandle::new(crate::config::theme::Theme::bundled_dark())
    }

    fn make() -> (
        LoginDialogComponent,
        Arc<Mutex<LoginDialogState>>,
        PendingInput,
        Arc<AtomicBool>,
    ) {
        let state = Arc::new(Mutex::new(LoginDialogState::default()));
        let pending: PendingInput = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let dialog = LoginDialogComponent::new(
            &theme(),
            Arc::clone(&state),
            Arc::clone(&pending),
            Arc::clone(&cancel),
        );
        (dialog, state, pending, cancel)
    }

    #[test]
    fn esc_sets_cancel_flag() {
        let (mut dialog, _state, _pending, cancel) = make();
        dialog.handle_input(&Key::escape());
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn ctrl_y_without_url_reports_nothing_to_copy() {
        // No URL set yet: Ctrl+Y must not touch the clipboard and
        // should leave an explanatory notice. (With a URL present the
        // copy path is exercised by hand — it writes to the real
        // clipboard / stdout, which a unit test shouldn't do.)
        let (mut dialog, _state, _pending, _cancel) = make();
        assert!(dialog.handle_input(&Key::ctrl('y')));
        let body = dialog
            .render(80)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("No authorization URL to copy yet"), "{body}");
    }

    #[tokio::test]
    async fn enter_delivers_input_to_awaiting_callback() {
        let (mut dialog, state, pending, _cancel) = make();
        let callbacks = TuiOAuthCallbacks::new(
            Arc::clone(&state),
            Arc::clone(&pending),
            RenderHandle::detached(),
        );

        // Park an input request, then type + submit on the dialog.
        let fut = tokio::spawn(async move { callbacks.on_prompt("paste:").await });
        // Give the spawned task a moment to install its sender.
        tokio::task::yield_now().await;
        for c in "code123".chars() {
            dialog.handle_input(&Key::char(c));
        }
        dialog.handle_input(&Key::enter());

        let got = fut.await.expect("join").expect("input");
        assert_eq!(got, "code123");
        // The input prompt is cleared after delivery.
        assert!(state.lock().unwrap().input_prompt.is_none());
    }

    #[test]
    fn url_lines_wrap_to_width() {
        let long = "https://example.com/".to_string() + &"a".repeat(100);
        let chunks = wrap_chars(&long, 40);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.chars().count() <= 40));
        assert_eq!(chunks.concat(), long);
    }

    #[test]
    fn hyperlink_wraps_text_in_osc8() {
        assert_eq!(
            hyperlink("https://x/y", "label"),
            "\x1b]8;id=aj-oauth;https://x/y\x1b\\label\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn hyperlink_strips_control_bytes_from_uri() {
        // A stray ESC in the URI must not survive into the sequence.
        let h = hyperlink("https://x/\x1bevil", "t");
        assert!(h.contains("https://x/evil"), "{h:?}");
        // The only ESC bytes are the OSC 8 framing, never the injected one.
        assert_eq!(h.matches('\x1b').count(), 4);
    }

    #[test]
    #[serial_test::serial]
    fn url_line_renders_as_hyperlink_when_supported() {
        let _caps = with_hyperlinks(true);
        let (mut dialog, state, _pending, _cancel) = make();
        // Skip the copy-on-first-render side effect (clipboard/stdout).
        dialog.auto_copied = true;
        let url = "https://example.com/oauth/authorize?x=1";
        state
            .lock()
            .unwrap()
            .lines
            .push(LoginLine::Url(url.to_string()));
        let body = dialog
            .render(80)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains(&format!("\x1b]8;id=aj-oauth;{url}\x1b\\")),
            "expected OSC 8 open for {url} in {body:?}"
        );
        assert!(body.contains(url), "{body:?}");
    }

    #[test]
    #[serial_test::serial]
    fn url_line_renders_plain_when_hyperlinks_unsupported() {
        let _caps = with_hyperlinks(false);
        let (mut dialog, state, _pending, _cancel) = make();
        dialog.auto_copied = true;
        let url = "https://example.com/oauth/authorize?x=1";
        state
            .lock()
            .unwrap()
            .lines
            .push(LoginLine::Url(url.to_string()));
        let body = dialog
            .render(80)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !body.contains("\x1b]8;"),
            "must not emit OSC 8 when hyperlinks are unsupported; got {body:?}"
        );
        // The URL text still shows so the user can copy it manually.
        assert!(body.contains(url), "{body:?}");
    }

    /// Collect the line bodies (ignoring the line kind) for assertions.
    fn joined(lines: &[LoginLine]) -> String {
        lines
            .iter()
            .map(|l| match l {
                LoginLine::Info(t) | LoginLine::Url(t) | LoginLine::Progress(t) => t.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn urls(lines: &[LoginLine]) -> Vec<String> {
        lines
            .iter()
            .filter_map(|l| match l {
                LoginLine::Url(u) => Some(u.clone()),
                _ => None,
            })
            .collect()
    }

    /// Headless with no hosted manual page (e.g. openai-codex): the user
    /// must copy the failed-redirect URL out of the address bar, so the
    /// wording has to call that out and we surface the authorize URL.
    #[test]
    fn auth_lines_headless_without_hosted_page_explains_address_bar_copy() {
        let info = OAuthAuthInfo {
            url: "https://auth.example.com/authorize?x=1",
            manual_url: None,
            instructions: Some("ignored when headless"),
        };
        let (lines, stored) = auth_lines(false, &info, "Ctrl+Y");
        let body = joined(&lines);
        assert!(body.contains("headless/SSH"), "{body}");
        assert!(body.contains("address bar"), "{body}");
        assert!(body.contains("connection error"), "{body}");
        assert_eq!(urls(&lines), vec![info.url.to_string()]);
        assert_eq!(stored, info.url);
    }

    /// Headless with a hosted manual page (e.g. anthropic): lead with the
    /// manual URL and tell the user to paste the code it shows.
    #[test]
    fn auth_lines_headless_with_hosted_page_says_paste_code() {
        let info = OAuthAuthInfo {
            url: "http://localhost:1455/auth/callback?x=1",
            manual_url: Some("https://hosted.example.com/code"),
            instructions: None,
        };
        let (lines, stored) = auth_lines(false, &info, "Ctrl+Y");
        let body = joined(&lines);
        assert!(body.contains("paste the code it shows"), "{body}");
        assert!(!body.contains("address bar"), "{body}");
        assert_eq!(urls(&lines), vec!["https://hosted.example.com/code"]);
        assert_eq!(stored, "https://hosted.example.com/code");
    }

    /// With a browser available we open the automatic URL, store it for
    /// copy, and list a hosted manual URL as a secondary fallback.
    #[test]
    fn auth_lines_with_browser_lists_both_urls() {
        let info = OAuthAuthInfo {
            url: "http://localhost:1455/auth/callback?x=1",
            manual_url: Some("https://hosted.example.com/code"),
            instructions: Some("Complete login in your browser."),
        };
        let (lines, stored) = auth_lines(true, &info, "Ctrl+Y");
        let body = joined(&lines);
        assert!(body.contains("Opening your browser"), "{body}");
        assert!(body.contains("Complete login in your browser."), "{body}");
        assert_eq!(
            urls(&lines),
            vec![
                info.url.to_string(),
                "https://hosted.example.com/code".to_string()
            ]
        );
        assert_eq!(stored, info.url);
    }
}
