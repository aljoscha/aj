//! Interactive key-code logger, for verifying how the terminal reports
//! modifiers, function keys, and Kitty-protocol sequences.
//!
//! Run with: `cargo run -p aj-tui --example key_tester`
//!
//! Press any key to see how the crossterm layer parsed it. Ctrl+C exits.
//! This is a manual testing harness, not an automated test.
//!
//! # A note on raw bytes
//!
//! The reported output shows the *parsed* `KeyEvent` fields (code,
//! modifiers, event kind, state), not the raw CSI/Kitty byte sequence
//! the terminal emitted. Byte-level parsing is owned by crossterm at
//! the [`ProcessTerminal`] boundary — the crate deliberately does not
//! ship a hand-rolled stdin state machine. See the "in-process only"
//! and "out-of-scope features" sections of `src/lib.rs` and
//! `tests/support/README.md` for the rationale.
//!
//! If a test key shows up here with a surprising code, the culprit is
//! before this example in the input pipeline: either your terminal
//! isn't reporting the sequence the way you expect, or the Kitty
//! enhancement flags weren't accepted by the terminal (see
//! [`Terminal::kitty_protocol_active`]).
//!
//! [`Terminal::kitty_protocol_active`]: aj_tui::terminal::Terminal::kitty_protocol_active

use aj_tui::component::Component;
use aj_tui::impl_component_any;
use aj_tui::keys::InputEvent;
use aj_tui::terminal::ProcessTerminal;
use aj_tui::tui::{Tui, TuiEvent};

use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};

const MAX_LINES: usize = 20;

struct KeyLogger {
    log: Vec<String>,
    kitty_protocol_active: bool,
}

impl KeyLogger {
    fn new(kitty_protocol_active: bool) -> Self {
        Self {
            log: Vec::new(),
            kitty_protocol_active,
        }
    }

    fn push(&mut self, entry: String) {
        self.log.push(entry);
        if self.log.len() > MAX_LINES {
            self.log.remove(0);
        }
    }
}

impl Component for KeyLogger {
    impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        let sep = "=".repeat(width);
        let mut out = Vec::new();

        out.push(sep.clone());
        out.push(pad(
            "Key Code Tester - Press keys to see their codes (Ctrl+C to exit)",
            width,
        ));
        out.push(pad(
            &format!(
                "Kitty keyboard protocol: {}",
                if self.kitty_protocol_active {
                    "active (press/release + disambiguated modifiers)"
                } else {
                    "inactive (legacy encoding)"
                }
            ),
            width,
        ));
        out.push(sep.clone());
        out.push(String::new());

        for entry in &self.log {
            out.push(pad(entry, width));
        }

        let reserved = 25;
        let used = out.len();
        for _ in used..reserved {
            out.push(" ".repeat(width));
        }

        out.push(sep.clone());
        out.push(pad("Test these:", width));
        for tip in [
            "  - Shift + Enter       (Kitty: Char('\\r') + SHIFT, kind=Press)",
            "  - Alt/Option + Enter  (Kitty: Char('\\r') + ALT)",
            "  - Option/Alt + Backspace",
            "  - Cmd/Ctrl + Backspace",
            "  - Regular Backspace",
            "  - Ctrl + letter, then release (Kitty reports both edges)",
        ] {
            out.push(pad(tip, width));
        }
        out.push(sep);

        out
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let entry = format_event(event);
        self.push(entry);
        true
    }
}

fn pad(s: &str, width: usize) -> String {
    if s.chars().count() >= width {
        s.chars().take(width).collect()
    } else {
        format!("{:<width$}", s, width = width)
    }
}

/// Render a single `InputEvent` into a left-to-right line of labelled
/// columns. Each column has a stable width so log entries align, which
/// makes scanning for regressions easier.
fn format_event(event: &InputEvent) -> String {
    match event {
        InputEvent::Key(k) => format!(
            "Key   | code: {:<24} | mods: {:<20} | kind: {:<8} | state: {}",
            format_key_code(&k.code),
            format_modifiers(k.modifiers),
            format_kind(k.kind),
            format_state(k.state),
        ),
        InputEvent::Paste(text) => {
            let first = text.chars().next();
            format!(
                "Paste | len:  {:<4} bytes: {:<8} | first: {}",
                text.len(),
                format!("{}", text.bytes().len()),
                first
                    .map(|c| format!("{:?} (U+{:04X})", c, u32::from(c)))
                    .unwrap_or_else(|| "(empty)".to_string()),
            )
        }
        InputEvent::Resize(cols, rows) => {
            format!("Resize| cols: {:<4} | rows: {}", cols, rows)
        }
    }
}

/// `KeyCode` variants that carry data (`Char`, `F`, `Media`, `Modifier`)
/// get their payload in parens. Anything else renders its variant name,
/// matching how we'd describe it in docs.
fn format_key_code(code: &KeyCode) -> String {
    match code {
        KeyCode::Char(c) => format!("Char({:?}) U+{:04X}", c, u32::from(*c)),
        KeyCode::F(n) => format!("F({})", n),
        KeyCode::Media(m) => format!("Media({:?})", m),
        KeyCode::Modifier(m) => format!("Modifier({:?})", m),
        other => format!("{:?}", other),
    }
}

/// Compact modifier list: `CTRL|SHIFT`, `-` if none.
fn format_modifiers(mods: KeyModifiers) -> String {
    if mods.is_empty() {
        return "-".to_string();
    }
    let mut parts = Vec::new();
    if mods.contains(KeyModifiers::CONTROL) {
        parts.push("CTRL");
    }
    if mods.contains(KeyModifiers::ALT) {
        parts.push("ALT");
    }
    if mods.contains(KeyModifiers::SHIFT) {
        parts.push("SHIFT");
    }
    if mods.contains(KeyModifiers::SUPER) {
        parts.push("SUPER");
    }
    if mods.contains(KeyModifiers::HYPER) {
        parts.push("HYPER");
    }
    if mods.contains(KeyModifiers::META) {
        parts.push("META");
    }
    parts.join("|")
}

fn format_kind(kind: KeyEventKind) -> String {
    match kind {
        KeyEventKind::Press => "Press".to_string(),
        KeyEventKind::Repeat => "Repeat".to_string(),
        KeyEventKind::Release => "Release".to_string(),
    }
}

fn format_state(state: KeyEventState) -> String {
    if state.is_empty() {
        return "-".to_string();
    }
    let mut parts = Vec::new();
    if state.contains(KeyEventState::KEYPAD) {
        parts.push("KEYPAD");
    }
    if state.contains(KeyEventState::CAPS_LOCK) {
        parts.push("CAPS_LOCK");
    }
    if state.contains(KeyEventState::NUM_LOCK) {
        parts.push("NUM_LOCK");
    }
    parts.join("|")
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let terminal = ProcessTerminal::new();
    let mut tui = Tui::new(Box::new(terminal));
    if let Err(e) = tui.start() {
        eprintln!("Failed to start terminal: {}", e);
        return;
    }
    let kitty = tui.terminal().kitty_protocol_active();
    tui.add_child(Box::new(KeyLogger::new(kitty)));
    tui.set_focus(Some(0));

    loop {
        match tui.next_event().await {
            Some(TuiEvent::Input(event)) => {
                if event.is_ctrl('c') {
                    break;
                }
                tui.handle_input(&event);
            }
            Some(TuiEvent::Render) => tui.render(),
            None => break,
        }
    }

    tui.stop();
}

// ---------------------------------------------------------------------------
// Unit tests for the formatters
// ---------------------------------------------------------------------------
//
// This example isn't an integration test, but the formatters are pure
// and easy to regression-guard. A syntax-only smoke test here will catch
// accidental column-width drift without needing a TTY.

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    #[test]
    fn format_key_code_reports_char_with_codepoint() {
        assert_eq!(format_key_code(&KeyCode::Char('a')), "Char('a') U+0061");
        assert_eq!(format_key_code(&KeyCode::Char('中')), "Char('中') U+4E2D");
    }

    #[test]
    fn format_key_code_reports_functional_keys() {
        assert_eq!(format_key_code(&KeyCode::F(12)), "F(12)");
        assert_eq!(format_key_code(&KeyCode::Enter), "Enter");
        assert_eq!(format_key_code(&KeyCode::BackTab), "BackTab");
    }

    #[test]
    fn format_modifiers_emits_pipe_separated_list() {
        assert_eq!(format_modifiers(KeyModifiers::empty()), "-");
        assert_eq!(
            format_modifiers(KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            "CTRL|SHIFT",
        );
        assert_eq!(format_modifiers(KeyModifiers::SUPER), "SUPER");
    }

    #[test]
    fn format_event_is_stable_across_common_shapes() {
        let press = InputEvent::Key(KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        });
        let line = format_event(&press);
        // Columns are separated by ` | ` so assertions can grep
        // individual fields.
        assert!(line.contains("code: Char('a') U+0061"));
        assert!(line.contains("mods: CTRL"));
        assert!(line.contains("kind: Press"));
        assert!(line.contains("state: -"));
    }

    #[test]
    fn format_event_renders_paste_and_resize_branches() {
        let paste = InputEvent::Paste("hello".to_string());
        assert!(format_event(&paste).starts_with("Paste"));

        let resize = InputEvent::Resize(80, 24);
        let line = format_event(&resize);
        assert!(line.starts_with("Resize"));
        assert!(line.contains("cols: 80"));
        assert!(line.contains("rows: 24"));
    }
}
