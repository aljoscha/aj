//! End-to-end replay-vs-live parity test for tool rendering.
//!
//! Per `docs/aj-next-progress.md` Step 5 of the "Resume fidelity
//! follow-up" plan: runs one tool-call turn live, simulates a
//! process kill between the tool-result persistence and the next
//! inference, resumes from the on-disk log, and asserts that the
//! chat scrollback rendered from the captured live events matches
//! the chat scrollback rendered from `replay(&log)` byte-for-byte.
//!
//! Three tool kinds — `bash`, `edit_file`, `todo_write` — exercise
//! the three structured-rendering paths (`ToolDetails::Bash`,
//! `ToolDetails::Diff`, `ToolDetails::Todos`). Without this guard a
//! future refactor of one of these renderers could drift between
//! the live and resumed paths without any unit test catching it,
//! re-introducing the "scrollback looks different on resume" class
//! of bug.
//!
//! ## Why a kill switch instead of letting the agent finish
//!
//! The scenario we want to reproduce is "process dies after tool
//! result lands on disk but before the model sees it". We simulate
//! that by registering three bus listeners in order — persistence,
//! event capture, kill switch — and having the kill switch return
//! `Err` on the `MessagePersisted::ToolResult` event. The earlier
//! listeners have already done their work for that event, so the
//! on-disk log holds the tool_result entry and the captured event
//! list holds every event the live renderer would have seen up
//! through the kill point. The agent then bails out of its turn
//! loop with `TurnError::Fatal`, exactly mirroring a real crash
//! between persistence and the next inference.
//!
//! ## Why we filter `TurnUsage`
//!
//! The agent emits per-turn token-usage snapshots as a bus event
//! but does not persist them to disk today, so `replay(&log)`
//! never re-emits a matching event. Driving the captured live
//! `TurnUsage` events into the live pump would therefore introduce
//! a `Token Usage - ...` row that the replay-driven pump can't
//! reproduce — a divergence unrelated to the three tool rendering
//! paths the test is here to guard. Filtering keeps the comparison
//! focused; restoring usage persistence (if/when it lands) will
//! make the filter a no-op without changing the rest of the test.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj::config::theme::{Theme, ThemeHandle, chat_theme};
use aj::modes::interactive::event_pump::EventPump;
use aj::modes::interactive::layout::{SlotIndex, build_layout};
use aj_agent::bus::{Listener, listener_from_sync};
use aj_agent::events::AgentEvent;
use aj_agent::tool::ErasedToolDefinition;
use aj_agent::{Agent, TurnError};
use aj_conf::AgentEnv;
use aj_models::provider::Provider;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::types::{AssistantContent, AssistantMessage, StopReason, StreamOptions, ToolCall};
use aj_session::{ConversationLog, ConversationPersistence, persistence_listener, replay};
use aj_tools::{BashTool, EditFileTool, TodoWriteTool};
use aj_tui::component::Component;
use aj_tui::container::Container;
use aj_tui::terminal::Terminal;
use aj_tui::tui::Tui;
use anyhow::anyhow;
use tempfile::TempDir;
use tokio::sync::Mutex as TokioMutex;

/// Fixed dimensions for the test rendering surface. The actual
/// values are irrelevant — both the live and replay pumps use the
/// same numbers, and we only compare the resulting `Vec<String>`s
/// against each other — but they have to be wide enough that tool
/// bodies don't wrap unpredictably and tall enough that the chat
/// container doesn't truncate. 100×24 is a comfortable default.
const SCREEN_WIDTH: u16 = 100;
const SCREEN_HEIGHT: u16 = 24;

/// Headless [`Terminal`] used for both Tuis: width/height come
/// back, every write is dropped on the floor. The chat container's
/// rendered lines are read directly via `Container::render`, not
/// from the terminal's write buffer, so a no-op write sink is
/// sufficient.
struct StubTerminal {
    columns: u16,
    rows: u16,
}

impl StubTerminal {
    fn new() -> Self {
        Self {
            columns: SCREEN_WIDTH,
            rows: SCREEN_HEIGHT,
        }
    }
}

impl Terminal for StubTerminal {
    fn write(&mut self, _: &str) {}
    fn columns(&self) -> u16 {
        self.columns
    }
    fn rows(&self) -> u16 {
        self.rows
    }
    fn move_by(&mut self, _: i32) {}
    fn hide_cursor(&mut self) {}
    fn show_cursor(&mut self) {}
    fn clear_line(&mut self) {}
    fn clear_from_cursor(&mut self) {}
    fn clear_screen(&mut self) {}
    fn set_title(&mut self, _: &str) {}
    fn flush(&mut self) {}
}

/// Build an [`AgentEnv`] that doesn't read from the host:
/// deterministic working directory, no git root probe, no
/// `AGENTS.md` / `CLAUDE.md` ingestion. Matches the helper in
/// `src/aj-agent/src/lib.rs` event_protocol_tests so test runs in
/// CI and on a developer laptop see the same agent surface.
fn empty_env(working_directory: PathBuf) -> AgentEnv {
    AgentEnv {
        working_directory,
        git_root_directory: None,
        operating_system: "test".to_string(),
        today_date: "2024-01-01".to_string(),
        context_files: Vec::new(),
    }
}

/// Identity stamped on every scripted [`AssistantMessage`] in this
/// test module. Matches what [`ScriptedProvider`] stamps on every
/// emitted partial, so the agent's TUI / persistence listeners see a
/// coherent provider identity even in tests.
const SCRIPT_API: &str = "scripted";
const SCRIPT_PROVIDER: &str = "scripted";
const SCRIPT_MODEL: &str = "scripted";

/// Build a [`ModelInfo`] mirroring what [`ScriptedProvider`] stamps
/// onto every emitted [`AssistantMessage`] partial. The agent reads
/// identity off this struct for the TUI footer and the `/model`
/// selector; values are only checked for "matches what the provider
/// claims", so any consistent triple works.
fn scripted_model_info() -> ModelInfo {
    ModelInfo {
        id: SCRIPT_MODEL.to_string(),
        name: SCRIPT_MODEL.to_string(),
        api: SCRIPT_API.to_string(),
        provider: SCRIPT_PROVIDER.to_string(),
        base_url: "scripted://internal".to_string(),
        reasoning: false,
        supports_xhigh: false,
        supports_adaptive_thinking: false,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
    }
}

/// Build a finalized [`AssistantMessage`] with a single `tool_call`
/// block and `stop_reason: ToolUse`. The agent runs the tool,
/// persists the result, and would continue to the next inference —
/// except the kill switch fails the turn after the persistence event
/// has fired. The provider is configured with
/// [`ExhaustedBehavior::Panic`] in [`drive_live_turn`] so a
/// misconfigured kill switch that lets the agent reach a second
/// inference makes the failure immediate and loud.
fn one_tool_use_message(
    tool_use_id: &str,
    tool_name: &str,
    tool_input: serde_json::Value,
) -> AssistantMessage {
    AssistantMessage {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: tool_use_id.to_string(),
            name: tool_name.to_string(),
            arguments: tool_input,
        })],
        api: SCRIPT_API.to_string(),
        provider: SCRIPT_PROVIDER.to_string(),
        model: SCRIPT_MODEL.to_string(),
        response_id: Some("test-msg-1".to_string()),
        usage: Default::default(),
        stop_reason: StopReason::ToolUse,
        error: None,
        timestamp: 0,
    }
}

/// Stand up a fresh `Tui` with the production interactive-mode
/// layout and an event pump bound to a bundled theme. Both the
/// live and replay passes use this so layout, theme, and pump
/// configuration are identical across the two scrollbacks; the
/// only difference between them is the event sequence each
/// receives.
fn build_tui_and_pump() -> (Tui, EventPump) {
    let mut tui = Tui::new(Box::new(StubTerminal::new()));
    let theme = ThemeHandle::new(Theme::bundled_dark());
    build_layout(&mut tui, &theme);
    let pump = EventPump::new(chat_theme(&theme), false);
    (tui, pump)
}

/// Render and return the chat container's lines.
///
/// The chat container is the rendering target this test compares;
/// other slots (header, status, editor, footer) contain
/// session-metadata text that would diverge for reasons unrelated
/// to tool rendering (thread id timestamps, the loader's
/// running-vs-idle state at the moment of capture, etc.).
fn render_chat(tui: &mut Tui) -> Vec<String> {
    let chat = tui
        .get_mut_as::<Container>(SlotIndex::Chat.idx())
        .expect("chat slot present");
    chat.render(usize::from(SCREEN_WIDTH))
}

/// Build a kill-switch [`Listener`] that returns `Err` the first
/// time it sees a `ToolExecutionEnd` event. Earlier listeners in
/// the bus's registration order have already observed the
/// preceding `MessageEnd { ToolResult }` (so persistence wrote it
/// and the capture buffer saw it) before this one fires, mirroring
/// a crash between tool-result persistence and the next inference.
fn kill_switch_listener() -> Listener {
    Arc::new(|event: &AgentEvent| {
        if matches!(event, AgentEvent::ToolExecutionEnd { .. }) {
            Box::pin(async { Err(anyhow!("kill switch: stop before next inference")) })
        } else {
            Box::pin(async { Ok(()) })
        }
    })
}

/// Drive a one-tool-call turn live against `tool`. Returns the
/// log's thread id (for resuming) and the full event sequence the
/// live renderer would have seen up to the simulated kill point.
async fn drive_live_turn(
    threads_dir: &Path,
    working_dir: &Path,
    tool: ErasedToolDefinition,
    tool_use_id: &str,
    tool_name: &str,
    tool_input: serde_json::Value,
    prompt: &str,
) -> (String, Vec<AgentEvent>) {
    // Persistence first so the file holds every event the kill
    // switch is about to abort on.
    let persistence = ConversationPersistence::new(threads_dir.to_path_buf());
    let mut log = ConversationLog::create(&persistence).expect("create log");
    log.set_system_prompt("test prompt".to_string())
        .expect("frozen system prompt");
    let thread_id = log.thread_id().to_string();
    let log_handle = Arc::new(TokioMutex::new(log));

    let message = one_tool_use_message(tool_use_id, tool_name, tool_input);
    // `chunk_size = 0` emits the tool call as a single delta carrying
    // the full serialized arguments — matches the legacy script
    // shape (one terminal event per inference) so the locked
    // bus-event sequence is preserved across the migration.
    // `ExhaustedBehavior::Panic` makes a runaway extra inference
    // (kill switch failed to bite) panic immediately.
    let provider: Arc<dyn Provider> = Arc::new(
        ScriptedProvider::from_messages(vec![message], 0, Duration::ZERO)
            .on_exhausted(ExhaustedBehavior::Panic),
    );
    let model_info = Arc::new(scripted_model_info());

    let env = empty_env(working_dir.to_path_buf());
    let mut agent = Agent::with_provider(
        env,
        "system prompt",
        vec![tool],
        Vec::new(),
        provider,
        model_info,
        StreamOptions::default(),
        None,
    );
    agent.set_assembled_system_prompt("test prompt".to_string());

    // 1. Persistence — writes terminal-state events to disk.
    let _h_persist = agent.subscribe(persistence_listener(Arc::clone(&log_handle)));

    // 2. Capture — every event in registration order, so the live
    //    render gets the exact sequence the production pump would
    //    have seen.
    let captured: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let cap_clone = Arc::clone(&captured);
    let _h_capture = agent.subscribe(listener_from_sync(move |event| {
        cap_clone
            .lock()
            .expect("captured events mutex poisoned")
            .push(event.clone());
    }));

    // 3. Kill switch — last, so persistence and capture both ran
    //    on the trigger event before the error bubbles up.
    let _h_kill = agent.subscribe(kill_switch_listener());

    match agent.prompt(prompt.to_string()).await {
        Ok(()) => panic!("expected kill switch to abort the turn before agent end"),
        Err(TurnError::Fatal(_)) => {}
        Err(TurnError::Recoverable(e)) => panic!("unexpected recoverable error: {e:#}"),
    }

    let events = captured
        .lock()
        .expect("captured events mutex poisoned")
        .clone();
    (thread_id, events)
}

/// Drive the captured `events` through a fresh event pump and
/// return the chat container's rendered lines. `TurnUsage` is
/// filtered out for the reason documented at the top of this file.
fn render_live(events: &[AgentEvent]) -> Vec<String> {
    let (mut tui, mut pump) = build_tui_and_pump();
    for event in events {
        if matches!(event, AgentEvent::TurnUsage { .. }) {
            continue;
        }
        pump.handle(&mut tui, event);
    }
    render_chat(&mut tui)
}

/// Replay the on-disk log into a fresh event pump and return the
/// chat container's rendered lines.
fn render_replay(threads_dir: &Path, thread_id: &str) -> Vec<String> {
    let persistence = ConversationPersistence::new(threads_dir.to_path_buf());
    let log = ConversationLog::resume(&persistence, thread_id).expect("resume log");
    let (mut tui, mut pump) = build_tui_and_pump();
    for event in replay(&log) {
        pump.handle(&mut tui, &event);
    }
    render_chat(&mut tui)
}

/// Side-by-side dump used when the live and replay scrollbacks
/// diverge. Surfacing both transcripts in the test failure makes
/// regressions debuggable from the panic message alone rather
/// than requiring a re-run with extra println!s.
fn diff_lines(label: &str, live: &[String], replay_lines: &[String]) -> String {
    let pad = live.len().max(replay_lines.len());
    let mut diff = String::new();
    diff.push_str(&format!("live vs replay scrollback drift for {label}:\n"));
    for i in 0..pad {
        let l = live.get(i).map(String::as_str).unwrap_or("<missing>");
        let r = replay_lines
            .get(i)
            .map(String::as_str)
            .unwrap_or("<missing>");
        let marker = if l == r { "  " } else { "!!" };
        diff.push_str(&format!("{marker} live[{i:>3}]   = {l:?}\n"));
        diff.push_str(&format!("{marker} replay[{i:>3}] = {r:?}\n"));
    }
    diff
}

/// Glue: run the full live → kill → replay flow for one tool
/// fixture and assert the two chat scrollbacks match line for line.
async fn assert_live_matches_replay(
    fixture_name: &str,
    tool: ErasedToolDefinition,
    tool_use_id: &str,
    tool_name: &str,
    tool_input: serde_json::Value,
    prompt: &str,
) {
    let threads_dir = TempDir::new().expect("threads tempdir");
    let working_dir = TempDir::new().expect("working tempdir");

    let (thread_id, live_events) = drive_live_turn(
        threads_dir.path(),
        working_dir.path(),
        tool,
        tool_use_id,
        tool_name,
        tool_input,
        prompt,
    )
    .await;

    let live = render_live(&live_events);
    let replay_lines = render_replay(threads_dir.path(), &thread_id);

    assert!(
        live == replay_lines,
        "{}",
        diff_lines(fixture_name, &live, &replay_lines)
    );
}

// ---------------------------------------------------------------------------
// Per-tool fixtures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replay_renders_bash_tool_identically_to_live() {
    // `echo hello` is deterministic, completes in milliseconds, and
    // exercises the `ToolDetails::Bash { stdout, exit_code, .. }`
    // payload path the bash-execution component renders.
    let input = serde_json::json!({
        "command": "echo hello",
        "timeout": 5,
        "description": "Print a greeting",
    });
    assert_live_matches_replay(
        "bash",
        BashTool.into(),
        "tu-bash",
        "bash",
        input,
        "run echo",
    )
    .await;
}

#[tokio::test]
async fn replay_renders_edit_file_tool_identically_to_live() {
    // `edit_file` needs a real file with the `old_string` substring
    // in place, so we materialise one inside the working tempdir
    // and feed its absolute path to the model. The resulting
    // `ToolDetails::Diff { before, after, path }` payload exercises
    // the unified-diff rendering path.
    let threads_dir = TempDir::new().expect("threads tempdir");
    let working_dir = TempDir::new().expect("working tempdir");
    let sample_path = working_dir.path().join("sample.txt");
    std::fs::write(&sample_path, "hello world\n").expect("seed sample.txt");

    let input = serde_json::json!({
        "path": sample_path.to_string_lossy(),
        "old_string": "hello",
        "new_string": "goodbye",
    });

    let (thread_id, live_events) = drive_live_turn(
        threads_dir.path(),
        working_dir.path(),
        EditFileTool.into(),
        "tu-edit",
        "edit_file",
        input,
        "swap the greeting",
    )
    .await;

    let live = render_live(&live_events);
    let replay_lines = render_replay(threads_dir.path(), &thread_id);
    assert!(
        live == replay_lines,
        "{}",
        diff_lines("edit_file", &live, &replay_lines)
    );
}

#[tokio::test]
async fn replay_renders_todo_write_tool_identically_to_live() {
    // `todo_write` keeps the snapshot list inside the
    // `ToolDetails::Todos { items }` payload, which the
    // tool-execution component renders as a status-symbol-prefixed
    // bullet list (the same shape `todo_read` produces).
    let input = serde_json::json!({
        "todos": [
            {
                "id": "1",
                "content": "Wire up the parity test",
                "status": "in-progress",
                "priority": "high",
            },
            {
                "id": "2",
                "content": "Write the migration walker",
                "status": "todo",
                "priority": "medium",
            },
        ]
    });
    assert_live_matches_replay(
        "todo_write",
        TodoWriteTool.into(),
        "tu-todo",
        "todo_write",
        input,
        "plan the next steps",
    )
    .await;
}
