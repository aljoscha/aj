//! Minimal chat interface demo.
//!
//! Run with: `cargo run -p aj-tui --example chat_simple`.
//!
//! A welcome banner, an editor at the bottom with `/`-triggered
//! autocomplete for slash commands, and a simulated bot that echoes a
//! canned response after a one-second delay. The commands `/delete`
//! (remove the most recent message) and `/clear` (remove every
//! message) are wired through the autocomplete provider. Press Ctrl+C
//! to exit.
//!
//! # Async shape
//!
//! The main loop is a `tokio::select!` over [`Tui::next_event`] and a
//! single-shot `tokio::time::sleep` that models the bot's thinking
//! delay. The `Tui` owns the input stream (via the underlying
//! `ProcessTerminal`) and the render throttle; user input, render
//! requests from the editor's autocomplete worker, and the bot's
//! timer all feed the same `select`. This is the pattern the rest of
//! `aj` will adopt when it wires `aj-tui` into its own event loop.

use std::time::{Duration, Instant};

use aj_tui::autocomplete::{CombinedAutocompleteProvider, SlashCommand};
use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::components::loader::Loader;
use aj_tui::components::markdown::Markdown;
use aj_tui::components::text::Text;
use aj_tui::style;
use aj_tui::terminal::ProcessTerminal;
use aj_tui::tui::{Tui, TuiEvent};

const RESPONSES: &[&str] = &[
    "That's interesting! Tell me more.",
    "I see what you mean.",
    "Fascinating perspective!",
    "Could you elaborate on that?",
    "That makes sense to me.",
    "I hadn't thought of it that way.",
    "Great point!",
    "Thanks for sharing that.",
];

/// Tiny non-cryptographic PRNG so we can pick a response without pulling
/// in a dependency. Seeded from the monotonic clock on first call.
fn pseudo_random_index(modulus: usize) -> usize {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|s| {
        let mut v = s.get();
        if v == 0 {
            // Seed from the monotonic clock the first time through.
            v = Instant::now().elapsed().as_nanos() as u64 ^ 0x9E37_79B9_7F4A_7C15;
            if v == 0 {
                v = 1;
            }
        }
        // xorshift64.
        v ^= v << 13;
        v ^= v >> 7;
        v ^= v << 17;
        s.set(v);
        (v as usize) % modulus.max(1)
    })
}

enum BotState {
    Idle,
    Thinking {
        /// Absolute deadline at which the bot finishes its (simulated) turn.
        deadline: tokio::time::Instant,
        /// Index of the loader sitting one slot before the editor.
        loader_index: usize,
    },
}

/// Return the index of the trailing editor, assuming the layout
/// convention this example maintains: the editor is always the last
/// child in `tui.root`. A helper keeps that invariant in one place.
fn editor_index(tui: &Tui) -> usize {
    tui.root
        .last_index()
        .expect("layout invariant: tui.root always ends with the editor")
}

/// Mutate the trailing editor. Panics if the layout invariant is
/// violated (no editor at the end) — in this example that would mean
/// a programming error, not a runtime condition worth recovering from.
fn with_editor<R>(tui: &mut Tui, f: impl FnOnce(&mut Editor) -> R) -> R {
    let idx = editor_index(tui);
    let editor = tui
        .root
        .get_mut_as::<Editor>(idx)
        .expect("layout invariant: trailing child is an Editor");
    f(editor)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut tui = Tui::new(Box::new(ProcessTerminal::new()));
    if let Err(e) = tui.start() {
        eprintln!("Failed to start terminal: {}", e);
        return;
    }

    let welcome = Text::new(
        "Welcome to Simple Chat!\n\nType your messages below. Type '/' for commands. Press Ctrl+C to exit.",
    );
    tui.root.add_child(Box::new(welcome));

    let mut editor = Editor::new();
    editor.set_focused(true);

    // Wire up the slash-command autocomplete provider. Typing `/` opens
    // the suggestion popup; `/delete` removes the last message and
    // `/clear` removes every message.
    let provider = CombinedAutocompleteProvider::new(
        vec![
            SlashCommand::new("delete")
                .with_description("Delete the last message")
                .into(),
            SlashCommand::new("clear")
                .with_description("Clear all messages")
                .into(),
        ],
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    );
    editor.set_autocomplete_provider(std::sync::Arc::new(provider));

    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(editor_index(&tui)));

    let mut bot_state = BotState::Idle;

    loop {
        // Build the bot's wake-up future lazily: when idle it never
        // fires, so the `if let` guards keep the select! branch
        // inactive. `tokio::time::sleep_until` is cancellation-safe so
        // we can rebuild it each loop iteration without worrying about
        // missed wake-ups.
        let bot_ready = async {
            match bot_state {
                BotState::Thinking { deadline, .. } => {
                    tokio::time::sleep_until(deadline).await;
                }
                BotState::Idle => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            maybe_event = tui.next_event() => {
                match maybe_event {
                    Some(TuiEvent::Input(event)) => {
                        if event.is_ctrl('c') {
                            break;
                        }
                        tui.handle_input(&event);
                        handle_submitted_text(&mut tui, &mut bot_state);
                    }
                    Some(TuiEvent::Render) => tui.render(),
                    None => break,
                }
            }

            _ = bot_ready => {
                if let BotState::Thinking { loader_index, .. } = bot_state {
                    tui.root.remove_child(loader_index);

                    let response = RESPONSES[pseudo_random_index(RESPONSES.len())];
                    let msg = Markdown::new(response);
                    let pos = editor_index(&tui);
                    tui.root.insert_child(pos, Box::new(msg));

                    with_editor(&mut tui, |e| e.disable_submit = false);
                    tui.set_focus(Some(editor_index(&tui)));
                    bot_state = BotState::Idle;
                    tui.request_render();
                }
            }
        }
    }

    tui.stop();
}

/// Pull submitted text out of the trailing editor and apply whatever
/// action the user asked for. Kept separate so the main loop stays
/// focused on event routing.
fn handle_submitted_text(tui: &mut Tui, bot_state: &mut BotState) {
    let submitted = with_editor(tui, |e| e.take_submitted());
    let Some(text) = submitted else { return };
    let trimmed = text.trim().to_string();

    if !trimmed.is_empty() {
        match trimmed.as_str() {
            "/clear" => {
                // Keep [0]=welcome, [last]=editor.
                while tui.root.len() > 2 {
                    tui.root.remove_child(1);
                }
            }
            "/delete" => {
                let idx = editor_index(tui);
                if idx > 1 {
                    tui.root.remove_child(idx - 1);
                }
            }
            _ if matches!(bot_state, BotState::Idle) => {
                // User message rendered as markdown verbatim.
                let msg = Markdown::new(&text);
                let pos = editor_index(tui);
                tui.root.insert_child(pos, Box::new(msg));

                let mut loader = Loader::new("Thinking...");
                loader.set_spinner_style(Box::new(style::cyan));
                loader.set_message_style(Box::new(style::dim));
                let pos = editor_index(tui);
                tui.root.insert_child(pos, Box::new(loader));

                with_editor(tui, |e| e.disable_submit = true);

                *bot_state = BotState::Thinking {
                    deadline: tokio::time::Instant::now() + Duration::from_millis(1000),
                    // The loader sits one slot before the editor.
                    loader_index: editor_index(tui) - 1,
                };
            }
            _ => {}
        }

        // Always clear the editor after a submit.
        with_editor(tui, |e| e.set_text(""));
    }
    tui.set_focus(Some(editor_index(tui)));
}
