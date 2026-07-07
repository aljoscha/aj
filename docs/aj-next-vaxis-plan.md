# `aj-next`: a vaxis-based sibling binary + shared `aj-app`

## Status: proposal (not started)

## Goal

Stand up a new binary crate, `aj-next`, that behaves like `aj` but uses the
`vaxis` crate (our libvaxis port) and its `vxfw` widget framework as the TUI
backend instead of `aj-tui`. `aj-next` lives as a sibling to `aj` for now, so
both build and run during the transition.

Two things drive the work:

1. **Factor out the common, TUI-agnostic parts of `aj` into a shared library
   crate (`aj-app`)** so both `aj` and `aj-next` reuse them instead of
   duplicating. This is the bulk of the value and the first phase.
2. **Change the terminal model.** `aj-tui` renders into the terminal's normal
   buffer and relies on native scrollback. `aj-next` uses the alternate screen
   with app-managed scrolling, the way the amp CLI does. This is not incidental.
   It reshapes how the transcript is rendered and scrolled.

## Companion specs

This plan is the umbrella. The detailed design lives in companion specs, each
owning a slice of the work and carrying its own resolved decisions. Read this
plan for the shape and phasing, then a spec for the detail.

| Spec | Covers | Plan phases | Decisions |
|---|---|---|---|
| This plan | Crate layout, what moves, the phased timeline | 0-9 | D1-D6 |
| D. `aj-app` extraction (`aj-app-extraction-spec.md`) | The shared crate: module inventory, the theme / keybinding / footer / settings seams, `SessionCore`, the shared turn primitives | 0-3 | D-1..D-4 |
| A. `AsyncApp` in vaxis (`vaxis-async-app-spec.md`) | The async vxfw driver: the shared `AppCore`, async capability detection, live resize, teardown, the host `select!` loop | 5 | A-1..A-3 |
| B. vxfw editor (`vaxis-editor-spec.md`) | The multi-line `TextArea` widget: the shared word engine, kill-ring, undo, history, autocomplete | 5 | B-1..B-4 |
| C. Chat model + reducer (`aj-app-chat-model-spec.md`) | `ChatState` and the pure `AgentEvent` reducer that both live and replay feed | 6 | C-1..C-4 |
| E. Alt-screen UX (`vaxis-altscreen-ux-spec.md`) | Follow-tail scrolling, the scrollbar thumb, in-app selection/copy, in-transcript search, the overlay/modal stack, focus, exit behavior | 5-9 | E-1..E-8 |
| F. Input, keymap, leader sequences (`vaxis-input-keymap-spec.md`) | Capture/bubble dispatch, the `KeymapController`, the leader-sequence engine, the dispatch-debug aids | 5, 8 | F-1..F-4 |

All spec-level decisions (A-F) are resolved. The plan-level decisions D1-D6 are
resolved by these specs, mapped at the end of this document.

## What we can rely on: the existing clean seam

The `aj` binary already has the architectural seam that makes this feasible. The
agent runtime (`aj-agent`) emits a typed `AgentEvent` stream, and every frontend
is just a subscriber. Print mode (`src/aj/src/modes/print.rs`) proves it. It is
a fully headless `AgentEvent` consumer with zero `aj-tui` references, built from
the same setup primitives the interactive mode uses.

Concretely, the following are already TUI-agnostic and consumed by the headless
print path today:

- `session_setup.rs`. The mode-agnostic composition root:
  `build_initial_run_config`, `restore_session_settings`, `prepare_log`,
  `build_agent`, `freeze_and_seed`, plus `RunConfigSnapshot`, `RestoreContext`,
  `BuiltAgent`, `PreparedLog`, `SessionSource`.
- `turn.rs`. The turn driver `drive_turn` (turn plus automatic
  compaction/overflow continuations), with a `reconfigure` callback.
- `compaction.rs`. `run_compaction` and `CompactionOutcome`.
- `model.rs`, `scripted.rs`. Model-selection policy (CLI > env > config
  precedence, registry resolution, API-key resolver install).
- `cli.rs`, `cli/args.rs`, `cli/file_args.rs`. The clap surface and file-arg
  resolution.
- `system_prompt.rs`, `auth.rs`, `usage.rs`, `export.rs`, `clipboard.rs`.
- `config/commands.rs`. The command catalog (`COMMANDS`, `CommandAction`,
  thinking-level data) driving the palette and help.

So the split is not a rewrite of the core. It is a move plus a handful of small
decouplings, and then a fresh TUI layer against vaxis.

## The two backends differ in their rendering model

This is the crux, and it colors every downstream decision.

`aj-tui` is **line-oriented and immediate-mode**. A frame is a `Vec<Line>` where
each `Line` is a styled string with inline ANSI. Components implement
`render(&mut self, width) -> Vec<Line>`. The engine re-renders the whole tree
each frame, diffs against the previous frame, and emits only changed rows. Old
content scrolls into the terminal's real scrollback via `\r\n`. There is no cell
grid and no alternate screen.

`vaxis`/`vxfw` is **cell-grid and retained-mode**. `Vaxis` owns a front and back
cell buffer and diff-renders to a writer. `vxfw` is a Flutter-style retained
widget tree: widgets implement `draw(&mut self, &DrawContext) -> Surface` under
constraint-based layout, events propagate capture/target/bubble through an
`EventContext` command bus, and the `App` runtime owns focus, mouse hit-testing,
and tick scheduling. `App::run` enters the alternate screen, turns on mouse and
bracketed paste, and runs a synchronous frame loop.

The amp CLI (inspected under `/tmp/amp-inspect`) is the reference for the target
UX. It runs on the alternate screen (`?1049h`) with synchronized output
(`?2026h`) and a Flutter-style build/layout/paint/render pipeline with
capture/target/bubble events, hit-testing regions, and a media-query for size
and capabilities. Our `vxfw` already provides the equivalent primitives
(`DrawContext` for constraints and capabilities, `Surface::hit_test`,
`EventContext` phases, mouse enter/leave). So the amp takeaway is not a new
framework. It is the alt-screen model itself: **the transcript is no longer in
the terminal's scrollback. It lives inside a scrollable widget the app owns and
scrolls.**

Implications for `aj-next`:

- The chat transcript becomes a `vxfw` `ListView` of message widgets, not a
  stream of lines pushed into scrollback. Scroll position, follow-tail behavior,
  and page-up/down are app state now (Spec E).
- Every `aj-tui` component (assistant message, tool execution, diff, bash,
  footer, header, loaders, sub-agent box, selectors) is re-authored as a `vxfw`
  widget that draws into a `Surface`, not one that emits `Vec<Line>`.
- Theme colors become structured `vaxis::cell::Style`/`Color` values instead of
  ANSI SGR strings.
- Overlays (selectors, help, settings, login) become z-indexed `SubSurface`s or
  a modal-stack widget, not the `aj-tui` `OverlayWindow` compositor.

## Target crate layout

```
aj-models  <-  aj-agent  <-  aj-tools
                   ^             ^
                   +- aj-session +
                        ^
                   aj-conf                 (config.toml loader, paths, skills, env)
                        ^
                   aj-app                 (frontend-agnostic app logic; NO tui dep)
                     ^        ^
        aj (aj-tui) +        + aj-next (vaxis)
```

New crate `aj-app` depends on `aj-agent`, `aj-session`, `aj-models`,
`aj-tools`, `aj-conf`, and the leaf externals the moved code needs (`anyhow`,
`clap`, `arboard`, `image`, `base64`, `flate2`, `chrono`, `iana-time-zone`,
`rand`, `notify`, `tokio`, `serde`, `serde_json`). It must not depend on
`aj-tui` or `vaxis`. That no-TUI-dep rule is the invariant that keeps it
shareable.

The crate is named `aj-app` (chosen over `aj-core`, which reads too close to
`aj-agent` being the runtime core).

## What moves into `aj-app`

### Move verbatim (already TUI-agnostic)

`cli/` (args, file_args, and the `Args`/`Command`/`InitialInput` surface),
`model.rs`, `scripted.rs`, `system_prompt.rs`, `session_setup.rs`, `turn.rs`,
`compaction.rs`, `auth.rs`, `usage.rs`, `export.rs`, `clipboard.rs`,
`config/commands.rs`, the task-shutdown helper from `modes.rs`, and the
usage-summary math from `modes/interactive/shutdown.rs`.

`RunConfigSnapshot` and `SubAgentOverrides` have `pub(crate)` fields today. They
become `pub` (or gain constructors and accessors) once they cross the crate
boundary.

### Move with a small decoupling

These are the coupling points that make the split non-trivial. Each has a
targeted fix. They are worth doing carefully because getting the boundary right
is the whole point.

- **Theme palette vs theme builders** (`config/theme.rs`, ~2000 lines). Split
  it. The palette core (`Theme`, `ThemeColor`, `ThemeBg`, `ColorMode`,
  `ThemeError`, JSON loading, `ThemeHandle`, `watch_user_theme`,
  `ThemeWatcherGuard`) is config-driven data and moves to `aj-app`, but it must
  stop emitting ANSI SGR strings through `aj_tui::style`. It should expose
  colors as structured values (RGB and palette index). The backend-specific
  builder functions (`chat_theme`/`ChatTheme`, `editor_theme`, `markdown_theme`,
  `select_list_theme`, `settings_list_theme`, `overlay_window_theme`,
  `editor_border_color_*`) return `aj-tui` structs and stay in `aj`. `aj-next`
  writes its own builders that convert palette colors into `vaxis` styles. See
  Spec D (decision D-2).

- **Keybinding data vs machinery** (`config/keybindings.rs`). The action-ID
  string constants (`ACTION_*`, `fixed_keys::{CTRL_C, CTRL_Y}`) and the default
  binding descriptors (`aj_keybindings()`) are shared data and move to
  `aj-app`. The manager machinery (`KeybindingsManager`, `tui_keybindings`,
  `IntoKeyList`, the global registry) is bound to `aj-tui`'s key types and stays
  per-binary. `config/commands.rs` references the action-ID consts, so it
  resolves against the shared copy once both live in `aj-app`.

- **`footer_data.rs`**. `AgentFooters`/`AgentFooter` are a data-only per-agent
  store and move to `aj-app`, except they import `components::footer::ContextUsage`.
  Move `ContextUsage` into `aj-app` (either into the footer-data module or a
  shared small types module) so the store no longer reaches into a component.

- **`shutdown.rs`**. The usage-summary formatters are TUI-agnostic and move. The
  `print_*` helpers that dim text via `aj_tui::style::dim` stay per-binary, or
  emit plain text that each binary styles.

- **`tmux_notice.rs`**. `build_warning` and its input struct move to `aj-app`.
  The `aj_tui::tmux::options()` probe stays with whichever backend consumes it
  for capability detection.

- **Test support**. The scripted-provider and run-config test builders currently
  in `modes/interactive/test_support.rs` are imported by `turn.rs` tests. Move
  the TUI-agnostic builders into an `aj-app` test-support module. The
  `StubTerminal` and `build_test_world` helpers stay per-binary.

### Restructure to share: `SessionCore` and the turn primitives

This is the one real structural change, and it pays off in both binaries.

- **`SessionWorld` -> `SessionCore` + a per-binary view.** Today
  `SessionWorld` (in `modes/interactive/session.rs`) mixes TUI-agnostic session
  state (`agent`, `env`, `registry`, `task_registry`, `message_queues`,
  `sub_overrides`, `log`, `session_id`, `event_rx`, the subscription handles,
  `restore_notices`) with the `EventPump`. Extract the agnostic fields plus the
  agnostic half of `build()` (everything up to pump construction: `prepare_log`,
  `build_agent`, `freeze_and_seed`, registry/queue wiring, bus and persistence
  subscription) into an `aj-app::SessionCore`. The `pump`, `install`, and
  `reconcile_sub_agent_settings` stay in each binary because they touch the view.

- **Turn orchestration primitives.** `spawn_turn`, `spawn_wake_turn`,
  `apply_turn_config`, `turn_policy`, `resolve_agent`, and the
  `SessionExit`/`SessionRequest` types are pure orchestration except for one
  dependency: they read `EventPump::is_running` / `running_agents`. The pump
  treats that set as "literal truth" it does not own the display for, so move the
  `running_agents` and `compacting` lifecycle sets into `SessionCore`. Then the
  turn primitives move to `aj-app` and both binaries share them. The pump reads
  the lifecycle truth from `SessionCore` for its spinner and footer.

- **Settings-mutation core.** The logic inside the `confirm_*_for_main/_for_sub`
  handlers and `persist_user`/`persist_project`/`persist_setting` that mutates
  `RunConfigSnapshot`/`Config` and writes TOML is TUI-agnostic. Lift those into
  `aj-app` functions the overlay handlers in each binary call. The overlay
  construction stays per-binary.

### Stays in `aj` (and gets reimplemented in `aj-next`)

The event pump (`event_pump.rs`), the layout (`layout.rs`), all of
`components/*`, the theme builder functions, and the `run_session` input/overlay
loop are `aj-tui`-specific. `aj` keeps them as-is. `aj-next` writes its own
against vaxis. The `AgentEvent` vocabulary and the pump's `handle()` match are
the behavioral spec the `aj-next` reimplementation follows.

## `aj-next` architecture

### Terminal driver: do not call `vxfw::App::run`

`vxfw::App::run` is a synchronous, thread-based frame loop. `aj` is async and
`select!`s terminal input against the `AgentEvent` channel, background-task
wakes, and a git/footer tick. So `aj-next` drives the vxfw widget tree from its
own async loop, using `Vaxis` (render) plus the async input front-end
(`vaxis::event_loop::async_input`) directly. This is exactly what the completed
libvaxis port anticipated (see `docs/vaxis-port-plan.md`, "Async integration").

The async driver replicates what `App::frame_loop` does, but on tokio:

1. Set up the terminal once: `enter_alt_screen`, `query_terminal` for
   capabilities, `set_bracketed_paste`, `set_mouse_mode`, color-scheme updates.
2. `select!` over: the async input receiver (vaxis `Event`s), the `AgentEvent`
   channel from `agent.subscribe_channel()`, background-task wake signals, the
   footer/git tick, and a render throttle.
3. On input events, run the vxfw dispatch (layout the last frame for
   hit-testing, mouse enter/leave diff, capture/target/bubble to the focus path
   or hit target), then drain the resulting `Command`s.
4. On any redraw request, do a layout pass (`draw_widget(root, ctx)`), update
   mouse/focus against the new surface, and `vx.render(writer)`.

Two known gaps from the port to handle in the driver:

- **Live resize.** `App::run` wires a fixed winsize snapshot for the SIGWINCH
  path. The async driver must wire a live ioctl-backed `WinsizeSource` (or handle
  SIGWINCH via `signal-hook` and re-query `TIOCGWINSZ`) so out-of-band resizes
  report the new size. In-band resize (DEC 2048) already works.
- **Pushing app events into the loop.** We do not need `vxfw::Event::App`.
  `AgentEvent`s arrive on our own channel and we translate them to model
  mutations directly, not through vxfw's event system.

The async driver lives in the `vaxis` crate as a sibling to `App`, sharing an
extracted `AppCore` (mouse, focus, tick, render) so `aj-next` and future async
hosts do not reimplement the frame loop. Spec A has the full design (decision D1,
resolved).

### Interactive shell: model + view, with an AgentEvent reducer

In `aj-tui` the event pump imperatively mutates component instances by index. In
a retained cell framework the natural and cleaner shape is a data model that the
widget tree renders from.

Recommended structure for `aj-next`'s interactive mode:

- A **chat model** owns the transcript as data: an ordered list of entries
  (user message, assistant message with streaming text/thinking snapshots, tool
  execution with its `ToolDetails`, compaction summary, sub-agent group), plus
  the per-agent streaming bookkeeping, the background-task table, and the footer
  data. Much of the current pump's state (`AgentRender`, `TaskInfo`,
  `AgentFooters`, `running_agents`, `compacting`, `message_queues`, `catalog`)
  is already data, not widgets.
- An **AgentEvent reducer** is the chat-domain state update function. It applies
  each event to the model and flags a redraw. This is the `aj-next` analogue of
  `EventPump::handle`, but it mutates data, not widgets. It is not a vaxis
  widget or component, and it is unit-testable without a terminal.
- The **chat view** is a `ListView` whose source builds a `vxfw` widget per
  model entry on demand. Streaming updates mutate the model entry and request a
  redraw. Follow-tail and scroll position are view state.

This gives `aj-next` a testable core and keeps widget internals out of the event
path. It is a deliberate improvement over the imperative pump, appropriate
because we are writing this layer fresh. We do not refactor `aj`'s pump to match.
The model types and the reducer live in `aj-app` and are consumed only by
`aj-next`, so the reduction is pure and terminal-free to test. Spec C has the
full design (decision D3, resolved).

### Print mode

`aj-next`'s print mode is nearly free. It reuses `aj-app`'s `session_setup` +
`drive_turn` and subscribes a plain JSONL or final-text listener, just like
`src/aj/src/modes/print.rs` does today. Build it first as the smoke test that
`aj-app` is sufficient to build a working frontend.

## Phased plan

Each phase ends green: `cargo fmt`, `cargo check`, `cargo clippy --workspace
--all-targets`, and `cargo test` clean. `aj` keeps working throughout.

### Phase 0: scaffold `aj-app`

Create the empty `aj-app` crate, add it to the workspace, wire its
dependencies. No code moved yet. Gate: `cargo check` on the empty crate.

### Phase 1: move the verbatim-agnostic modules into `aj-app`

Move `cli/`, `model.rs`, `scripted.rs`, `system_prompt.rs`, `session_setup.rs`,
`turn.rs`, `compaction.rs`, `auth.rs`, `usage.rs`, `export.rs`, `clipboard.rs`,
`config/commands.rs`, the shutdown usage-summary math, and the task-shutdown
helper. Update `aj` to depend on and re-export from `aj-app`. Fix the
`pub(crate)` visibility on `RunConfigSnapshot`. Move the TUI-agnostic test
builders. Gate: `aj` builds and its tests pass unchanged.

### Phase 2: decouple the hybrid modules

Split the theme palette from the builders (palette to `aj-app`, drop the
`aj_tui::style` dependency, expose structured colors). Split the keybinding
data from the machinery. Move `AgentFooters` and `ContextUsage`. Move
`build_warning`. Gate: `aj` builds and passes tests, now consuming the shared
palette and keybinding data.

### Phase 3: extract `SessionCore` and the turn primitives

Move the lifecycle sets (`running_agents`, `compacting`) into `SessionCore`.
Extract `SessionCore` from `SessionWorld` and the turn primitives into
`aj-app`. `aj`'s `SessionWorld` becomes `SessionCore` + `pump` + install. Lift
the settings-mutation core. Gate: `aj` builds and passes tests. This is the
highest-risk refactor of `aj`, so it is isolated in its own phase.

### Phase 4: scaffold `aj-next` + print mode

Create `aj-next` depending on `aj-app` + `vaxis`. Wire a thin `main.rs`
(tracing, dotenv, dispatch) that reuses `aj-app`'s `Args`. Implement print mode
by reusing `aj-app`. Gate: `aj-next print` reaches parity with `aj` print mode
on a scripted model.

### Phase 5: the async vxfw driver + a hello-world alt-screen shell

Build (or add to `vaxis`) the async vxfw driver: alt-screen setup, async input,
the frame/dispatch loop, live resize. Stand up a minimal `aj-next` interactive
shell: a root widget with a header, an empty chat scroll area, an editor
(`vxfw::TextField` or a richer editor), and a footer. No agent wiring yet. Gate:
`aj-next` opens on the alt screen, accepts input, resizes, and quits cleanly.

### Phase 6: chat model + AgentEvent reducer + streaming transcript

Implement the chat model and the reducer over the full `AgentEvent` vocabulary
(the `EventPump::handle` match is the spec). Render user and assistant messages
(including streaming text and thinking) in the scroll view. Wire submit ->
`spawn_turn` (from `aj-app`) -> events -> reducer -> redraw. Gate: a real
prompt streams into the transcript and scrolls correctly.

### Phase 7: the rest of the components

Tool execution, diff, bash output, loaders/spinner, compaction summary,
sub-agent boxes and grouping, background-task cells, footer data, pending
message. Each as a `vxfw` widget rendering from the model. Gate: a session with
tool calls, sub-agents, and background tasks renders at parity with `aj`.

### Phase 8: overlays, selectors, theming, keybindings

The command palette, model/thinking/speed/verbosity selectors, settings window,
help overlay, session selector, login dialog, as z-indexed overlays. Convert the
shared palette into vaxis styles. Hook the shared keybinding data into vaxis key
matching. Wire the settings-mutation core. Gate: selectors and settings work and
persist.

### Phase 9: parity pass and polish

Mouse interactions, image protocols (kitty/iterm via `vaxis::image`), OSC 52
clipboard, terminal title, tmux notice, shutdown banner, theme hot-reload.
Decide the endgame (keep both binaries, or cut over `aj-next` -> `aj` later).

### Phase 9 execution breakdown

Phases 0-8 are done. What the plan calls "Phase 9" spans two big deferred
components (the editor and the transcript UX, each owned by a companion spec)
plus the small chrome items. We execute it in ordered sub-chunks, each landing
green with the implementer/reviewer loop and its own commit:

- **9-Editor (Spec B).** The largest piece, so it is phased as Spec B lays out.
  - **B1a: shared editing primitives into `vaxis`.** Port `KillRing` and
    `UndoStack` verbatim, and build the pluggable word-motion engine (`CharClass`,
    `WordClassifier`, one engine, `ReadlineWords` + `EmacsWords` classifiers).
    Adopt the engine in `TextField` only if its ported word-motion tests stay
    green, else leave `TextField` faithful and use the engine in `TextArea` only.
  - **B1b: the `TextArea` widget.** Document model, cursor, vertical movement with
    the sticky column and atomic-segment snap, insert/delete/kill/yank/undo,
    history, width-aware wrapping, the bordered scroll window with the top-bar
    label and `up N more` indicator, submit-vs-newline, palette trigger. Swap the
    shell's `TextField` for it and wire history seed/record and hint labels.
  - **B2: paste markers and jump mode.** Large-paste markers with `expanded_text`,
    the `pastes` map, and char-jump mode.
  - **B3: autocomplete.** The provider traits, data types, and inline popup into
    `vxfw`; the concrete `@`/`/`/`#` providers into `aj-next`; async delivery
    through the host `select!`.
- **9-Markdown.** A markdown renderer for assistant, user, and compaction text
  (plain-wrapped today). The single biggest visual-parity gap.
- **9-Transcript (Spec E, remaining).** In-transcript search (not on Ctrl+F),
  free-form selection and copy over the transcript, and the transcript-focus
  keyboard mode.
- **9-Chrome.** Image protocols (kitty/iTerm2), remaining mouse interactions,
  terminal title, tmux notice, and the small wired-up stubs (launch-prompt input,
  clipboard image paste, palette subtitle labels, palette-to-selector Esc
  chaining, the E-8 grouped help screen).
- **9-Debug-overlay.** A small, opt-in frame-statistics box for diagnosing
  render-loop health. Off by default, toggled by a new `show_frame_stats` `bool`
  in `aj-conf`'s `Config` + `Config::OPTIONS` (so the settings window picks it up
  automatically as a cycle row, like `hide_thinking_block`, and the
  `test_options_table_matches_config_fields` drift test keeps the field and
  option paired). The flag is seeded onto the shell at build time like the other
  display toggles and live-applied through `apply_setting_change`, so flipping it
  in settings takes effect without a restart. The data is
  `AsyncApp::frame_stats` (Spec A): last/avg/max frame time, render rate (fps),
  changed-cell count, and screen size. The box is a non-interactive corner
  `SubSurface` appended in `Shell::draw` (top-right by default), sitting above
  the base content in the z-stack so it stays visible during interaction. It
  never takes focus or blocks mouse hit-testing outside its own cells. It shows
  the previous frame's numbers and freezes when the UI is idle, matching the
  honest redraw-rate reading (Spec A). Optionally a global debug chord (Spec F)
  can flip the in-memory flag without persisting, mirroring the thinking-block
  toggle.
- **9-Perf (responsiveness).** A pass over the drive loop and the widgets to
  remove UI-thread stalls and per-frame rebuilds. Several land as small fixes,
  one is an involved cache.
  - **Drive-loop arm order.** The `biased;` `select!` polls arms top-to-bottom
    and takes the first ready. High-frequency arms that can flood (the
    autocomplete delivery channel, the agent-event bus) must sit BELOW the
    terminal-input arm, so a burst cannot starve typing. One-shot fill arms and
    the turn-join arm stay above input.
  - **Autocomplete tick budget + delivery coalescing.** The matcher tick runs on
    the UI thread, so its per-frame budget is a couple of milliseconds, and the
    host drains the whole delivery backlog per wake instead of once per notify.
  - **`TextArea` visual-line map cache.** The wrap map is memoized behind a width
    key and dropped on any edit, so streaming frames and repeated navigation
    reuse it instead of rebuilding an O(document) map every draw.
  - **Transcript entry render cache.** The involved one. During a streaming turn
    the transcript redraws every frame and the `ListView` `Builder` mints a fresh
    widget per visible entry (so `MarkdownView`'s width cache never survives a
    frame and tool diffs recompute), and the sub-agent box builds and draws its
    ENTIRE child transcript every frame before tail-windowing it. The fix is one
    cache of drawn surfaces keyed by entry identity, owned by the persistent
    `TranscriptView` and shared by `Rc<RefCell<..>>` into the `EntryBuilder`.
    - **Seam.** `item_at_idx` has the `ChatState` borrow but no width, and the
      cache lookup needs the width. So `item_at_idx` computes only the cheap
      per-entry fingerprint and returns a caching wrapper widget carrying
      `(AgentId, EntryId, fingerprint)` plus `Rc` handles. The wrapper's `draw`
      (which has the width) does the hit or miss: a hit clones the stored
      surface, a miss re-borrows `ChatState`, runs today's `build_entry_widget`
      plus a real draw, stores the surface, and returns it. Building the real
      widget therefore happens only on a miss, so a hit skips both build and
      draw. The `item_at_idx` borrow is dropped before the wrapper draws, so the
      miss re-borrow never overlaps.
    - **Key and slot.** One slot per `(AgentId active_view, EntryId)` holding
      `{ fingerprint, width, surface }`. A lookup hits only when the live
      fingerprint and width both match, else it rebuilds and replaces the slot,
      so stale `(fingerprint, width)` variants never accumulate.
    - **Fingerprint (the correctness lynchpin).** Computed layout-free from the
      cheap fields that change an entry's rendering. Assistant and reasoning: the
      content-block count, the summed text and thinking byte lengths, and
      `finalized`. Tool: the status, the `ToolDetails` presence and discriminant,
      a size proxy for streaming variants, `header_only`, and the entry's task
      status read from `chat.tasks()`. User: joined-text length and collapsible.
      Compaction: summary length and the two token counts. Notice and turn-usage
      are immutable after append. Sub-agent: its status plus the fold of every
      child entry's fingerprint, so a background-task update to a non-tail child
      cell still changes it. A missing field shows stale content, so the field
      set is the review focus and is pinned by a per-kind test that mutates the
      entry and asserts the render changed.
    - **Global clears.** Session-wide render inputs are handled by clearing the
      whole cache rather than threading them into every fingerprint. A theme swap
      already rebuilds the builder in `set_styles`. `TranscriptView::draw`
      compares the active view, `tools_expanded`, and `hide_thinking_block`
      against the last frame and clears on a change. A width change is a per-slot
      miss.
    - **Storing surfaces is safe today.** No transcript entry participates in
      event dispatch and the list draws no cursor, so replaying a stored surface
      (whose `widget` stamp points at the wrapper) does not break hit-testing.
      NOTE: the later transcript-focus mode would make entries interactive, at
      which point the wrapper must forward events to the real widget or the cache
      must store widgets for those kinds.
    - **Eviction.** The transcript is append-only and unbounded, so the map is
      bounded (the working set is the viewport). The policy is correctness-neutral
      since a stale-key miss just rebuilds.
  - **`ListView` scroll geometry (measured extents drive the thumb).** The chat
    scroll keeps its index-anchored position (top entry + line offset) and adds a
    measured-extent geometry that drives only the scrollbar thumb (Spec E section
    1). Each entry counts as an estimated extent (the running mean of the measured
    entries) until it is laid out at the current width, then its measured height
    replaces the estimate as it scrolls through the viewport. A prefix-sum
    (Fenwick) tree answers total-extent and viewport-top offset in `O(log n)`, so
    the thumb reflects real height and position without ever walking the whole
    transcript. We keep the index-anchored core rather than the reference's
    absolute-offset one: index anchoring gives viewport stability for free (an
    off-screen height change never moves the viewport), and the absolute model's
    extra benefits (offset-precise reveal, sub-entry animation) have no consumer
    with search dropped. The geometry only sizes the thumb, so a stale off-screen
    estimate is at worst a slightly-off thumb, never a scroll bug. Separately, the
    full-transcript copy/search layout is gone: with entry-relative selection
    (Spec E section 2) and search dropped (E-5), the `RenderedTextLayout` grid,
    `transcript_signature`, and the absolute `(row, col)` coordinate helpers in
    `transcript.rs` are removed. Selection extraction walks only the spanned
    entries through per-entry text providers laid out on demand from `ChatState`
    (which holds every entry independent of the view), so no `ListView` keep-alive
    is needed.
  - **Markdown parse / wrap split (width-independent cache).** `render_markdown`
    bundles parse (tabs → block AST → styled logical lines) with the width-only
    `wrap_spans`, so a width change re-parses every visible entry from scratch. The
    amp model separates the two: parse once into width-independent styled lines and
    cache them per entry, then run only the wrap in a width-keyed after pass (amp
    parses in `build`, which reruns only on a content change, and wraps in the text
    render object's layout, cached by width). This mainly sharpens resize and the
    focus-mode gutter shift. It does not help streaming, where the entry's text
    changes each frame and must re-parse regardless.
- **9-Endgame.** Decide whether to keep both binaries or cut `aj-next` over.

## Design decisions (resolved in companion specs)

These plan-level decisions are settled. Each is now owned by a companion spec.

- **D1. Where the async vxfw driver lives. Resolved (Spec A): in `vaxis`.** A
  sibling `AsyncApp` shares an extracted `AppCore` (mouse, focus, tick, render)
  with the threaded `App`, so `aj-next` and future async hosts do not
  reimplement the frame loop.
- **D2. Theme color representation. Resolved (Spec D, D-2): structured colors.**
  The shared palette stores resolved RGB plus a `ColorMode`, not pre-baked ANSI,
  and each backend downsamples.
- **D3. Model + reducer for `aj-next`; `aj`'s pump untouched. Resolved (Spec C).**
  The model types and the `AgentEvent` reducer live in `aj-app` and are consumed
  only by `aj-next`. `aj`'s imperative pump is left as-is.
- **D4. Alt-screen scroll and follow-tail UX. Resolved (Spec E).** Follow-tail
  with wheel + page-key scrolling and a transcript-focus keyboard mode, plus full
  in-app selection, copy, and in-transcript search.
- **D5. Crate name. Resolved: `aj-app`.**
- **D6. Editor widget. Resolved (Spec B): a new `vxfw::TextArea`.** A multi-line
  sibling of `TextField` that ports the `aj-tui` editor logic, with the
  word-motion engine shared between the two widgets.

Input dispatch was not an original plan-level decision. Spec F covers it:
capture/bubble via a root `KeymapController`, the leader-sequence engine, and the
debug aids. It resolves the seam question raised in Spec A (A-3).

## Risks and notes

- **The editor is the biggest single component.** The `aj-tui` editor is rich.
  Re-authoring it on vaxis (D6) is the largest widget-level task and a likely
  schedule risk. Scope it early. As of now `aj-next` still runs on the stopgap
  single-line `TextField` and the `TextArea` port (Spec B) has not started, so it
  is the main outstanding interactive-shell gap.

- **Phase 3 (`SessionCore` extraction) is the riskiest refactor of `aj`.** It
  moves the lifecycle-truth sets out of the pump. Keep it isolated and lean on
  `aj`'s existing tests to catch regressions.

- **Alt-screen means no free scrollback.** Users who rely on scrolling the
  terminal and searching their native scrollback lose that. The in-app scroll
  view must be good (search, copy, follow-tail). This is a UX shift to
  communicate, not just an implementation detail.

- **`aj-app` must never gain a TUI dependency.** That invariant is what keeps it
  shareable. A `cargo tree` check (no `aj-tui`, no `vaxis` under `aj-app`) is a
  cheap guard worth adding to CI.

- **`similar` (diff computation) placement.** Diff rendering is per-backend, but
  the before/after content already rides on `ToolDetails::Diff`. Keep diff
  rendering (and `similar`) per-binary unless we later want a shared unified-diff
  data model.
