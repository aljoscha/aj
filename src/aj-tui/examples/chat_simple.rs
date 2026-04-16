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

use std::sync::Arc;
use std::time::{Duration, Instant};

use aj_tui::autocomplete::{CombinedAutocompleteProvider, SlashCommand};
use aj_tui::component::Component;
use aj_tui::components::editor::{Editor, EditorTheme};
use aj_tui::components::loader::Loader;
use aj_tui::components::markdown::{Markdown, MarkdownTheme};
use aj_tui::components::select_list::SelectListTheme;
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

// ---------------------------------------------------------------------------
// Theme construction
// ---------------------------------------------------------------------------
//
// The tui crate is palette-agnostic by design (see PORTING.md F33): no
// `Default` impl ships for any of the theme types, so this example
// builds its own theme inline. A real agent would assemble these from
// a central palette / config file (mirroring pi-tui's
// `coding-agent` package layout).

fn select_list_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: Arc::new(style::cyan),
        selected_text: Arc::new(|s| style::bold(&style::cyan(s))),
        description: Arc::new(style::dim),
        scroll_info: Arc::new(style::dim),
        no_match: Arc::new(style::dim),
    }
}

fn editor_theme() -> EditorTheme {
    EditorTheme {
        border_color: Arc::new(style::dim),
        select_list: select_list_theme(),
    }
}

fn markdown_theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: Arc::new(style::bold),
        bold: Arc::new(style::bold),
        italic: Arc::new(style::italic),
        strikethrough: Arc::new(style::strikethrough),
        code: Arc::new(style::yellow),
        code_block: Arc::new(|s| s.to_string()),
        code_block_border: Arc::new(style::dim),
        link: Arc::new(style::cyan),
        link_url: Arc::new(style::dim),
        list_bullet: Arc::new(style::cyan),
        quote_border: Arc::new(style::dim),
        quote: Arc::new(style::italic),
        hr: Arc::new(style::dim),
        underline: Arc::new(style::underline),
        highlight_code: None,
        code_block_indent: None,
    }
}

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
            // `u128 → u64` truncation is fine here — we only want a
            // PRNG seed, not a faithful timestamp.
            v = u64::try_from(Instant::now().elapsed().as_nanos()).unwrap_or(u64::MAX)
                ^ 0x9E37_79B9_7F4A_7C15;
            if v == 0 {
                v = 1;
            }
        }
        // xorshift64.
        v ^= v << 13;
        v ^= v >> 7;
        v ^= v << 17;
        s.set(v);
        // The intermediate `usize` cast is needed to mod by `modulus:
        // usize`. We mod immediately so the value fits.
        usize::try_from(v).unwrap_or(usize::MAX) % modulus.max(1)
    })
}

enum BotState {
    Idle,
    Thinking {
        /// Absolute deadline at which the bot finishes its (simulated) turn.
        deadline: tokio::time::Instant,
        /// Raw pointer to the heap-allocated loader Component currently
        /// living in `tui.root` one slot before the editor.
        ///
        /// Held as a `*const dyn Component` rather than an index because
        /// pi-tui's [`Container::remove_child_by_ref`] takes a `&dyn
        /// Component`, and Rust's borrow checker won't let us hold a
        /// `&dyn Component` across the `&mut Container` borrow needed
        /// for the removal call. The raw pointer survives the borrow
        /// because the underlying heap allocation moves only when the
        /// `Box` itself is dropped — Vec resizes copy box pointers, not
        /// the boxed contents.
        ///
        /// SAFETY contract: the loader is owned by `tui.root` for the
        /// entire `Thinking` lifetime (the editor's `disable_submit`
        /// flag prevents `/clear` and `/delete` from firing during a
        /// turn), so the dereference inside `remove_child_by_ref` lands
        /// on a still-live allocation.
        loader_ptr: *const dyn Component,
    },
}

/// Return the index of the trailing editor, assuming the layout
/// convention this example maintains: the editor is always the last
/// child of the root container. A helper keeps that invariant in one
/// place.
fn editor_index(tui: &Tui) -> usize {
    tui.last_index()
        .expect("layout invariant: tui always ends with the editor")
}

/// Mutate the trailing editor. Panics if the layout invariant is
/// violated (no editor at the end) — in this example that would mean
/// a programming error, not a runtime condition worth recovering from.
fn with_editor<R>(tui: &mut Tui, f: impl FnOnce(&mut Editor) -> R) -> R {
    let idx = editor_index(tui);
    let editor = tui
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
        1,
        1,
    );
    tui.add_child(Box::new(welcome));

    let mut editor = Editor::new(tui.handle(), editor_theme());
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

    tui.add_child(Box::new(editor));
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
                if let BotState::Thinking { loader_ptr, .. } = bot_state {
                    // SAFETY: the loader lives in the root container for
                    // the entire Thinking state; see the `loader_ptr`
                    // field's contract.
                    tui.remove_child_by_ref(unsafe { &*loader_ptr });

                    let response = RESPONSES[pseudo_random_index(RESPONSES.len())];
                    let msg = Markdown::new(response, 1, 0, markdown_theme(), None);
                    let pos = editor_index(&tui);
                    tui.insert_child(pos, Box::new(msg));

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
                // Keep [0]=welcome, [last]=editor; drop everything in
                // between. Walk indices in reverse so each removal
                // doesn't shift the still-pending positions, and
                // re-fetch the raw pointer for each iteration so the
                // borrow on `tui.get(i)` is dropped before the
                // `&mut tui` borrow needed for the by-ref call.
                let last_idx = tui.last_index().unwrap_or(0);
                for i in (1..last_idx).rev() {
                    let ptr: *const dyn Component = match tui.get(i) {
                        Some(c) => c,
                        None => continue,
                    };
                    // SAFETY: `ptr` was acquired in this iteration's
                    // immutable borrow of the root container; nothing
                    // has mutated it since, and the heap allocation is
                    // still owned by it.
                    tui.remove_child_by_ref(unsafe { &*ptr });
                }
            }
            "/delete" => {
                let idx = editor_index(tui);
                if idx > 1 {
                    let ptr: *const dyn Component = match tui.get(idx - 1) {
                        Some(c) => c,
                        None => return,
                    };
                    // SAFETY: see `/clear` above — same single-shot
                    // pattern, no mutation between acquisition and use.
                    tui.remove_child_by_ref(unsafe { &*ptr });
                }
            }
            _ if matches!(bot_state, BotState::Idle) => {
                // User message rendered as markdown verbatim.
                let msg = Markdown::new(&text, 1, 0, markdown_theme(), None);
                let pos = editor_index(tui);
                tui.insert_child(pos, Box::new(msg));

                let loader = Loader::new(
                    tui.handle(),
                    Box::new(style::cyan),
                    Box::new(style::dim),
                    "Thinking...",
                );
                // Box up first so we can stash a raw pointer into the
                // heap allocation before transferring ownership to the
                // root container. The pointer remains valid because the
                // box owns the allocation and only moves when dropped,
                // not when the surrounding Vec resizes.
                let loader_box: Box<dyn Component> = Box::new(loader);
                let loader_ptr: *const dyn Component = &*loader_box;
                let pos = editor_index(tui);
                tui.insert_child(pos, loader_box);

                with_editor(tui, |e| e.disable_submit = true);

                *bot_state = BotState::Thinking {
                    deadline: tokio::time::Instant::now() + Duration::from_millis(1000),
                    loader_ptr,
                };
            }
            _ => {}
        }

        // Always clear the editor after a submit.
        with_editor(tui, |e| e.set_text(""));
    }
    tui.set_focus(Some(editor_index(tui)));
}
