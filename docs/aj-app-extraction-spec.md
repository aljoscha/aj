# Spec D: `aj-app` extraction

## Status: proposal (not started)

Companion to `docs/aj-next-vaxis-plan.md`. This spec covers the first and
largest piece of that plan: extracting the frontend-agnostic parts of the `aj`
binary into a shared library crate, `aj-app`, that both `aj` (aj-tui) and the
future `aj-next` (vaxis) depend on.

This work does not add features and does not touch behavior. `aj` builds and
passes its existing tests at the end of every phase. It is a move plus a handful
of small, principled decouplings.

## The invariant

`aj-app` must not depend on `aj-tui` or `vaxis`. That single rule is what makes
it shareable, and it is the acceptance test for every decision below. We add a
cheap CI guard: assert `aj-tui` and `vaxis` do not appear under `aj-app` in
`cargo tree -p aj-app`.

## Dependency graph after the extraction

```
aj-models  <-  aj-agent  <-  aj-tools
                   ^             ^
                   +- aj-session +
                        ^
                   aj-conf
                        ^
                   aj-app                 (frontend-agnostic; NO tui dep)
                     ^        ^
        aj (aj-tui) +        + aj-next (vaxis)
```

`aj-app` depends on `aj-agent`, `aj-session`, `aj-models`, `aj-tools`,
`aj-conf`, and the leaf externals the moved code already uses: `anyhow`, `clap`,
`arboard`, `image`, `base64`, `flate2`, `chrono`, `iana-time-zone`, `rand`,
`notify`, `tokio`, `serde`, `serde_json`. It does not need `toml`/`toml_edit`
directly: settings writes go through `aj-conf`'s `Config`/`ConfigLayer` methods,
which own the `toml_edit` machinery. `similar` stays in `aj` (diff rendering is
per-backend).

`aj` keeps its dependency on `aj-tui` and drops the deps that follow the moved
code where it no longer uses them directly.

## Inventory

Four categories, in increasing order of effort.

### Category 1: move verbatim (already TUI-agnostic)

These have zero `aj-tui` references and are already consumed by the headless
print path. They move to `aj-app` unchanged except for import-path updates.

- `cli.rs`, `cli/args.rs`, `cli/file_args.rs` (the `Args`/`Command`/`PrintFormat`
  clap surface, `InitialInput`, `process_file_args`, `ResolvedFiles`).
- `model.rs` (the `ModelSelection` precedence merge, `ResolvedModel`, `resolve`,
  `from_model_info`, key-resolver install, `apply_thinking_display`,
  `apply_verbosity`, the default-provider constants).
- `scripted.rs`, `system_prompt.rs`, `session_setup.rs`, `compaction.rs`,
  `auth.rs`, `usage.rs`, `export.rs`, `clipboard.rs`.
- `config/commands.rs` (the `COMMANDS` catalog, `CommandAction`,
  `ThinkingLevel`, `load_model_catalog`).
- `SYSTEM_PROMPT` (the `include_str!("../PROMPT.md")` const currently in `lib.rs`).
  It moves next to `system_prompt.rs` and both `session_setup` and the prompt
  assembler reference it from there.
- `shutdown_background_tasks` + `TASK_SHUTDOWN_GRACE` (currently in `modes.rs`).
- The non-interactive subcommand handlers from `main.rs`: `handle_list_sessions`
  and `handle_update_models_command` are pure `aj-session`/`aj-models` wrappers
  and move to `aj-app`. `print::run` moves too (print mode is fully headless).

`RunConfigSnapshot`'s fields are `pub(crate)` today. They become `pub` (or gain
constructors and accessors) once they cross the crate boundary. Same for any
other `pub(crate)` type that both binaries construct.

What stays in each binary's `main.rs`: tracing/dotenv setup and the top-level
dispatch, because the interactive branch differs (`aj` builds the aj-tui
`InteractiveMode`, `aj-next` builds the vaxis one). `main` calls into `aj-app`
for `Args`, `list-sessions`, `update-models`, and `print`, and wires only its own
interactive mode.

### Category 2: split data from machinery

Each of these is one file that mixes shareable data with aj-tui-bound code. We
split the file, move the data half to `aj-app`, and leave the machinery half in
`aj`.

#### 2a. Keybindings (`config/keybindings.rs`)

Move to `aj-app` (pure data, no aj-tui types):

- The action-ID constants: `ACTION_THINKING_TOGGLE`, `ACTION_TOOLS_EXPAND`,
  `ACTION_CLIPBOARD_PASTE_IMAGE`, `ACTION_PALETTE_OPEN`,
  `ACTION_OVERLAY_CLOSE_ALL`, `ACTION_HISTORY_TOGGLE_SCOPE`,
  `ACTION_HISTORY_OPEN`, `ACTION_AGENT_PICKER`, `ACTION_AGENT_TOGGLE_SCOPE`,
  `ACTION_TASK_KILL`, `ACTION_SUBMIT_STEERING`, `ACTION_DEQUEUE`,
  `ACTION_SETTINGS_CLEAR`.
- The `fixed_keys` module (`CTRL_C`, `CTRL_Y`).
- A description table for the `aj.*` actions: today `aj_keybindings()` pairs each
  action ID with an `aj_tui::keybindings::KeybindingDefinition` (default chord +
  description). Split it so the default-chord-plus-description data is expressed
  as a plain `&[(action_id, default_chord, description)]` table in `aj-app`.
  The `config/commands.rs` catalog already references these action IDs, so it
  resolves against the shared copy once both live in `aj-app`.

Stays in `aj` (bound to `aj_tui::keybindings`): `all_keybindings()` (merges
`tui_keybindings()` with the `aj.*` table), `install_global_manager`,
`install_global_manager_defaults`, and the `KeybindingsManager`/`IntoKeyList`
plumbing. `aj` builds `KeybindingDefinition`s from the shared table. `aj-next`
builds its own key registry from the same table against vaxis key matching.

#### 2b. Theme (`config/theme.rs`, ~2000 lines)

This is the one split with a real design change. Today `Theme` stores colors as
**pre-baked ANSI SGR prefix strings** (`fg: HashMap<ThemeColor, String>`, filled
by `fg_ansi(resolved, mode)` at parse time), and `Theme::fg`/`bg`/`fg_closure`/
`bg_closure` wrap text in those prefixes plus resets. That is aj-tui-specific:
the output is an ANSI string that aj-tui components paste into a `Line`.

The split:

- **Move to `aj-app` (backend-neutral palette):** `ThemeColor`, `ThemeBg`,
  `ColorMode`, `ThemeError`, the JSON schema (`ThemeJson`), variable resolution
  (`resolve_var`), the bundled `dark`/`light` palettes, the loader
  (`from_json`/`from_json_with_mode`/`load`), and the file watcher
  (`watch_user_theme`, `ThemeWatcherGuard`). `Theme` changes to store a
  **structured color value** per token instead of an ANSI string. Concretely,
  `fg: HashMap<ThemeColor, ThemeRgb>` where `ThemeRgb` is the resolved color,
  with the `ColorMode` retained on the `Theme` for backends that downsample. The
  parse step stores the resolved color rather than calling `fg_ansi`.
- **`ThemeHandle` moves to `aj-app` as a generic holder:** `Arc<RwLock<Theme>>`
  with `read()`, `replace()`, `name()`, `color_mode()`. The hot-reload
  mechanism (RwLock + watcher-fed `replace`) is backend-neutral. What does *not*
  move is the ANSI-closure builders `fg_closure`/`bg_closure` returning
  `Arc<dyn Fn(&str) -> String>`, since the `String` output is aj-tui's contract.
- **Stays in `aj`:** the `fg`/`bg` ANSI methods (the current `fg_ansi`/`bg_ansi`
  + `reassert_bg` logic, now living as an extension over the shared `Theme`),
  the `fg_closure`/`bg_closure` builders, and every theme-struct builder that
  returns an aj-tui type: `chat_theme`/`ChatTheme`, `editor_theme`,
  `markdown_theme`, `select_list_theme`, `settings_list_theme`,
  `overlay_window_theme`, `editor_border_color_*`.
- **`aj-next` writes its own** builders that read structured colors from the
  shared `Theme` and produce `vaxis::cell::Style`/`Color` values.

`ThemeRgb` is the resolved-color type, promoted from the loader's existing
internal `ResolvedColor` to a public three-variant enum: `Rgb(u8, u8, u8)`,
`Ansi256(u8)` (an explicit palette index, e.g. a JSON integer color value), and
`Default` (the terminal default, the empty JSON value used by most text tokens).
A bare RGB triple would be wrong: it would lose the palette-index and
terminal-default cases the JSON schema already supports. `ColorMode` stays on
`Theme`, and each backend downsamples (`aj` via the existing `fg_ansi` path,
`aj-next` by handing the variant to vaxis, which renders per terminal
capability). We do not pre-downsample in `aj-app`, so no color fidelity is lost
at the boundary.

A new `ThemeColor` token, `KeybindHint`, is added to the palette for the
keybinding-hint accent that `aj-next`'s command palette shortcut column and splash
`Ctrl+O` hint use (Spec E). It defaults to `#275DD0` (RGB 39, 93, 208) in both
bundled themes and lives in the shared palette like every other token, so it
resolves as a `ThemeRgb` and downsamples per backend. `aj` does not consume it
today, but keeping it in the shared palette avoids a literal in `aj-next` and lets
a user theme it.

#### 2c. Footer data (`modes/interactive/footer_data.rs`)

`AgentFooters`/`AgentFooter` are data-only (strings and scalars keyed by
`AgentId`) and move to `aj-app`. Their one snag is that `context_usage` returns
`components::footer::ContextUsage`. Move `ContextUsage` (a two-field
`{ tokens: Option<u64>, context_window: u64 }` view struct) into `aj-app`
alongside `AgentFooters`. `aj`'s footer component imports it from `aj-app`.

#### 2d. Shutdown usage-summary math (`modes/interactive/shutdown.rs`)

The formatters (`build_usage_summary`, `build_usage_summary_from_parts`,
`format_usage_summary`, `format_resume_hint`, `format_session_usage_header`) are
TUI-agnostic and move. The `print_*` helpers that dim text via
`aj_tui::style::dim` stay in `aj`, or emit plain text that `aj` dims.

#### 2e. Tmux notice (`tmux_notice.rs`)

`build_warning` and its `TmuxOptions`-shaped input move to `aj-app`. The live
`aj_tui::tmux::options()` probe stays with the backend that consumes it for
capability detection.

### Category 3: restructure to share

The one structural change. It pays off in both binaries and is isolated in its
own phase because it touches `aj`'s session lifetime and turn loop.

#### 3a. `SessionCore` (from `SessionWorld`)

`SessionWorld` (in `modes/interactive/session.rs`) is 12/13 fields agnostic; the
lone TUI field is `pump: EventPump`. Extract the agnostic state and the agnostic
half of `build()` into `aj-app::SessionCore`.

`SessionCore` owns:

- `agent: Arc<TokioMutex<Agent>>`, `env: AgentEnv`, `registry: SubAgentRegistry`,
  `task_registry: TaskRegistry`, `message_queues: MessageQueues`,
  `sub_overrides` (see 3c), `log: Arc<TokioMutex<ConversationLog>>`,
  `session_id: String`, `event_rx: UnboundedReceiver<AgentEvent>`, the two
  subscription handles, `restore_notices: Vec<String>`.
- The lifecycle-truth sets, moved out of `EventPump`: `running_agents:
  HashSet<AgentId>` and `compacting: HashSet<AgentId>`. The pump's own comment
  calls `running_agents` "the single source of truth for what is running," and
  it is orchestration state, not view state. Read by the turn primitives, the
  quit-arm logic, and the busy notice. `SessionCore` exposes `is_running(id)`,
  `running_agents()`, `mark_running(id)`, `mark_idle(id)`, and the `compacting`
  equivalents. Whoever processes `AgentStart`/`AgentEnd` (the pump in `aj`, the
  reducer in `aj-next`) updates these; the spinner and footer read them.

`SessionCore::build(...)` performs everything `SessionWorld::build` does up to
pump construction: `prepare_log`, `build_agent`, `freeze_and_seed`, wiring the
`SubAgentRegistry`/`TaskRegistry`/`MessageQueues` into the agent, subscribing the
event channel and the persistence listener, and gathering `restore_notices`. Its
signature drops the `theme`/`render_settings` parameters (those were only for the
pump).

Each binary wraps `SessionCore` in its own view type: `aj`'s `SessionWorld`
becomes `SessionCore` + `pump` + `install`/`reconcile_sub_agent_settings`;
`aj-next`'s equivalent holds `SessionCore` + its chat model.

#### 3b. Turn orchestration seam

The cleanest boundary here is *not* to move the whole `spawn_turn`. Spawning is
tied to the run loop (a `JoinSet` and a per-turn `CancellationToken` map), and
those loops differ between `aj` (aj-tui `select!`) and `aj-next` (vaxis
`AsyncApp`). So we share the pure decision logic and keep the spawn glue thin and
per-binary:

- **Move to `aj-app`:** `apply_turn_config` (stamps the staged
  `RunConfigSnapshot` + per-sub `SubAgentOverrides` onto an agent before an
  inference, pure), `turn_policy` (builds a `TurnPolicy` from `Config`, pure),
  and `resolve_agent` (an `AgentId` -> shared-agent lookup, becomes a
  `SessionCore` method). `drive_turn` already lands in `aj-app` via `turn.rs`.
  The `SessionExit`/`SessionRequest` orchestration enums move too.
- **Stays per-binary (thin):** the `spawn_turn`/`spawn_wake_turn`/
  `spawn_prompt_turn` glue that owns the `JoinSet`, mints the cancel token, and
  calls the shared pieces. In `aj` it stays where it is; in `aj-next` it is a
  small function in the run loop. Its former dependency on
  `world.pump.is_running` becomes `core.is_running`.

This is a more honest seam than "move `spawn_turn`" from the earlier plan draft:
the config/agent-resolution logic is shared, the loop mechanics are not.

#### 3c. Settings-mutation core

The logic inside the `confirm_*_for_main`/`confirm_*_for_sub` handlers and
`persist_user`/`persist_project`/`persist_setting` that mutates
`RunConfigSnapshot`/`Config`/config layers and writes settings is TUI-agnostic.
Move those mutation-and-persist functions to `aj-app`, along with the
`SubAgentOverrides` data type (currently `pub(crate)` in `interactive.rs`). The
overlay *construction* and the selector-outcome routing stay per-binary and call
the shared setters. This keeps the "what a settings change does to config and
disk" rules in one place for both frontends.

These functions are already thin. `persist_user` mutates the user
`ConfigLayer`, refreshes the effective `Config`, and calls
`Config::persist_changed`. `persist_project` sets or clears project-layer keys
and calls `ConfigLayer::persist(&baseline, &path)`. All the robust write
mechanics already live in `aj-conf`: format-preserving `toml_edit` read-modify-
write, comment and key-order preservation, refusal to clobber invalid TOML, and
a cross-process `ConfigLock`. So there is no persistence redesign here and
nothing to defer. The `SettingsManager` consolidation sketched in the old
`aj-next-plan.md` is stale: that machinery already shipped in `aj-conf`, and both
binaries get it for free by calling these functions.

### Category 4: test support

`turn.rs`'s tests import scripted-provider and run-config builders from
`modes/interactive/test_support.rs`. Move the TUI-agnostic builders
(`scripted_run_config` and friends) into an `aj-app` test-support module so the
moved `turn.rs`/`session_setup.rs` tests compile there. The `StubTerminal` (an
`aj_tui::terminal::Terminal` impl) and `build_test_world` stay in `aj`.

## What stays in `aj` (and gets reimplemented in `aj-next`)

For clarity, the parts that are irreducibly aj-tui and are *not* touched by this
extraction: `event_pump.rs`, `layout.rs`, all of `components/*`, the theme-struct
builders, the keybindings manager plumbing, and the `run_session` input/overlay
loop in `interactive.rs`. `aj` keeps them. `aj-next` writes its own against
vaxis. The `AgentEvent` vocabulary and the `EventPump::handle` match remain the
behavioral spec the `aj-next` reducer (Spec C) follows.

## Phasing

Every phase ends green: `cargo fmt`, `cargo check`, `cargo clippy --workspace
--all-targets`, `cargo test`. `aj` works unchanged throughout.

### Phase 0: scaffold

Create the empty `aj-app` crate, add it to the workspace `members`, wire its
dependencies, add the `cargo tree` no-TUI-dep CI guard. Gate: `cargo check` on
the empty crate.

### Phase 1: move Category 1 (verbatim)

Move the verbatim modules, `SYSTEM_PROMPT`, the task-shutdown helper, the
non-interactive subcommand handlers, and print mode. Re-export from `aj` where
call sites still say `crate::...`, or update the paths. Flip `RunConfigSnapshot`
visibility. Move the Category 4 test builders. Gate: `aj` builds, all tests pass,
`aj print` and `aj list-sessions`/`update-models` behave identically.

### Phase 2: split Category 2 (data vs machinery)

Split keybindings (2a), theme (2b, the structured-color change), footer data
(2c), shutdown math (2d), tmux notice (2e). `aj`'s theme builders now read
structured colors from the shared `Theme` and produce aj-tui structs. Gate: `aj`
builds and passes tests, including the theme and keybinding tests, now against
the shared palette and action table.

### Phase 3: restructure Category 3

Move the lifecycle sets into `SessionCore`, extract `SessionCore` from
`SessionWorld`, carve the turn seam (3b), lift the settings-mutation core (3c).
This is the riskiest change to `aj`, isolated here so `aj`'s existing session and
event-pump tests catch regressions. Gate: `aj` builds and passes tests, session
switching/new/resume and turn cancellation behave identically.

## Decisions

- **D-1. Crate name. Resolved: `aj-app`.**
- **D-2. Theme storage. Resolved: change it.** `aj-app`'s `Theme` stores a
  structured color per token (`ThemeRgb`, a three-variant enum of
  `Rgb`/`Ansi256`/`Default`, with `ColorMode` retained on `Theme`); each backend
  downsamples. No fidelity loss at the boundary. This changes `aj`'s theme
  storage from pre-baked ANSI strings to structured colors, which is well-tested
  and low risk.
- **D-3. Turn seam. Resolved: split, do not move `spawn_turn`.** `spawn_turn`
  today mixes pure decision logic (which agent, what config to apply, what turn
  policy) with run-loop mechanics (a `JoinSet` of in-flight turns and a per-turn
  `CancellationToken` map, observed via the loop's `select!`). Those loop
  mechanics differ between `aj` (aj-tui `select!`) and `aj-next` (vaxis
  `AsyncApp`). So `aj-app` gets the pure pieces (`apply_turn_config`,
  `turn_policy`, `resolve_agent`), and each binary keeps its own thin
  spawn/cancel glue. Moving `spawn_turn` wholesale would force a shared loop
  shape onto both frontends, which is the wrong seam.
- **D-4. Settings persistence. Resolved: no deferral, nothing to redesign.**
  The robust write machinery (format-preserving `toml_edit` RMW, comment
  preservation, cross-process `ConfigLock`) already lives in `aj-conf`. The
  `persist_*` functions are already thin, TUI-agnostic wrappers over it and move
  to `aj-app` as-is. The old `aj-next-plan.md` `SettingsManager` proposal is
  stale.
