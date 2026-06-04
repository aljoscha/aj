# Sub-agent observability — spec

Make sub-agents easy to observe and navigate in the interactive TUI:

1. Render each sub-agent's activity inside a styled, full-width **box**
   embedded in the main thread at the point the `agent` tool was
   called. The box has a tool-call-like gray background, a fixed
   height, and its content scrolls **within** the box as the sub-agent
   streams. Tool calls inside the box render **header-only**.
2. Show a **footer** indicator when sub-agents are running, e.g.
   `1 agent (alt+a)` / `2 agents (alt+a)`.
3. Add an **agent picker** overlay (`alt+a`) to switch the main chat
   view to a sub-agent's transcript and back to main. While observing a
   sub-agent the editor shows a marker in its top border. In the
   switched-to ("full") view the sub-agent's tool calls render with
   their full results, like the main thread.

This document is the implementation contract. It is split into stages
at the end for orchestration.

---

## 1. Background: what happens today

### 1.1 Sub-agent lifecycle

The `agent` builtin (`src/aj-tools/src/tools/agent.rs`) calls
`ToolContext::spawn_agent(task)`. In `aj-agent`
(`SessionContextWrapper::spawn_agent`, `src/aj-agent/src/lib.rs`) that:

- allocates an `AgentId::Sub(n)` (monotonic per session),
- emits `SubAgentStart { parent, child, task }` on the **parent's**
  bus *before* the child runs,
- builds a fresh `Agent` that **shares the parent's bus**
  (`set_bus(parent_bus.clone())`) and a child cancellation token,
- runs it to completion via `run_single_turn`,
- records the child's usage into the parent,
- emits `SubAgentEnd { parent, child, report }` (always, even on
  failure — the report carries the error string on failure).

Because the bus is shared, **every** child event (`AgentStart/End`,
`TurnStart`, `MessageStart/Update/End`, `ToolExecution*`, `TurnUsage`)
reaches the parent's listeners tagged with `AgentId::Sub(n)`. The
parent *model* only ever sees the final text report (the tool result);
the intermediate events are for the UI and persistence.

`SubAgentStart` is the first sub-agent event on the bus and carries the
`task`; `SubAgentEnd` is the last and carries the `report`. The
sub-agent's own first user message (inside `run_single_turn`) is the
task text, persisted on the sub-agent thread.

Sub-agents are exactly one level deep: the child's toolset has the
`agent` tool filtered out, so a sub-agent cannot spawn its own
sub-agent. `AgentId` is `Main | Sub(usize)`
(`src/aj-agent/src/events.rs`).

### 1.2 How sub-agent output is printed today

`src/aj/src/modes/interactive/event_pump.rs`:

- `MessageStart/Update/End` and `ToolExecution*` are dispatched
  **without inspecting `agent_id`**, so sub-agent messages and tool
  calls are appended into the **same** chat `Container` as the main
  agent, interleaved with no grouping.
- `SubAgentStart` / `SubAgentEnd` are **no-op placeholders**.
- `TurnUsage { Sub(n) }` appends a dim `(sub agent N) Token Usage…` row
  to the shared scrollback and does **not** move the footer.
- `AgentStart/End` drive only the loader spinner via a
  `running_agents: HashSet<AgentId>` refcount. Main's `AgentEnd` is the
  authoritative "all stopped" signal that drains the set.

Net: sub-agent output is visible but bleeds into the main transcript.

### 1.3 Layout & rendering primitives

- `SlotIndex` (`layout.rs`): `Header(0) Chat(1) Status(2) Editor(3)
  Footer(4)`. Chat is a plain `aj_tui::container::Container` that
  renders all children in append order; the chat *is* the terminal
  scrollback (no internal viewport — the terminal scrolls).
- `Container` (`src/aj-tui/src/container.rs`) renders **all** children
  sequentially; `get_mut_as::<T>(idx)`, `add_child`, `insert_child`,
  `len`, `remove_child_by_ref`.
- Tool calls render through `ToolExecutionComponent`
  (`components/tool_execution.rs`): a gray full-width "bubble" built on
  `aj_tui::components::text_box::TextBox` (padding `1,1`, a status-tinted
  `bg_fn`). `TextBox::apply_bg_row` pads every row to full width and
  paints it via `aj_tui::ansi::apply_background_to_line`. The bundled
  themes point `tool_pending_bg` / `tool_success_bg` / `tool_error_bg`
  at one `toolBg` var — status is shown by the header glyph
  (`…` / `✓` / `✗`), not the tint.
- Overlays: a component with a cheap-to-clone `…OutcomeHandle`
  (`Arc<Mutex<Option<Outcome>>>`), wrapped in
  `aj_tui::components::overlay_window::OverlayWindow`, mounted with
  `tui.show_overlay(window, palette_overlay_options())`, tracked by an
  `OpenSelector` variant, polled each input tick by
  `handle_selector_outcome`. `palette_overlay_options()` is the compact
  preset (`PALETTE_OVERLAY_INNER_ROWS = 17`, `+4` chrome = 21 rows).
- Keybindings: app actions in `src/aj/src/config/keybindings.rs`
  (`ACTION_*` consts + defaults), intercepted in the main loop's
  `TuiEvent::Input` arm *before* `tui.handle_input`.
- Slash commands: `BUILTIN_COMMANDS` + `SlashAction` + `dispatch` in
  `src/aj/src/config/slash_commands.rs`; dispatched by
  `handle_slash_command` in `interactive.rs`.
- Replay (`src/aj-session/src/replay.rs`) projects the persisted log to
  the same `AgentEvent` stream a live run produces. It already tags
  sub-agent entries with `AgentId::Sub(n)` (via `agent_id_for`) but
  does **not** emit `SubAgentStart` / `SubAgentEnd`.

---

## 2. Goals / non-goals

**Goals**: the three numbered behaviors above; resume fidelity (boxes
reconstruct on `/resume`); no regression to the main-thread rendering,
the loader refcount, or persistence.

**Non-goals**: nested sub-agents (still one level); changing what the
parent model sees; changing the wire/persistence format; steering a
sub-agent from the picker (observe-only — superseded by
`docs/subagent-steering-spec.md`).

---

## 3. Locked decisions

1. **Tools in the box** render header-only; **tools in the switched-to
   full view** render with full results (like the main thread). Driven
   by the box's render mode.
2. **Picker default scope** lists `Main` + currently-**running**
   sub-agents. A scope toggle (`ctrl+t`, contextual to the picker, like
   prompt-history's scope toggle) switches to **all** sub-agents in the
   session, each labelled with a tasteful status. `Main` is always
   present in both scopes so you can return home.
3. **Footer** shows a **count only** plus the keybind:
   `N agent (alt+a)` / `N agents (alt+a)`, counting **running**
   sub-agents. Hidden when zero.
4. **Compact box height**: a fixed window a bit taller than the
   command-palette overlay. Single tunable constant
   `SUBAGENT_BOX_COMPACT_ROWS` (inner transcript rows); see §5.2.
5. **On spawn, do not auto-switch.** Stay on main; the user opts in via
   `alt+a`.

---

## 4. Architecture

Introduce a thin app-side `ChatView` component that **replaces the raw
`Container`** in `SlotIndex::Chat`. It owns the main transcript and is
the single owner of "which agent's transcript is active". Sub-agent
transcripts live inside `SubAgentBox` components that are children of
the main container, inserted at the spawn point. The generic
`aj_tui::Container` stays generic and unchanged.

```
SlotIndex::Chat
  └── ChatView
        ├── main: Container            // main-agent components, in order
        │     ├── UserMessageComponent
        │     ├── AssistantMessageComponent
        │     ├── ToolExecutionComponent
        │     ├── SubAgentBox(1)        // owns inner Container of Sub(1)'s components
        │     ├── ...
        │     └── SubAgentBox(2)
        ├── active: AgentId             // Main => render main; Sub(n) => render only box n, full
        └── sub_boxes: HashMap<usize, usize>   // sub id -> child index in `main`
```

- **active == Main**: `ChatView` renders `main` normally; every
  `SubAgentBox` draws **compact** (fixed-height window).
- **active == Sub(n)**: `ChatView` renders **only** `SubAgentBox(n)` in
  **full** mode (whole inner transcript, full results), hiding all
  other children. The chat area is the terminal scrollback, so "full"
  is just more lines and the terminal scrolls.

Event routing is **independent of the active view**: events always
update the owning agent's components, whether or not they're currently
visible, so switching shows current state immediately.

The event pump becomes per-agent: `current_assistant` and `tool_index`
are keyed by `AgentId`; each event is routed to the owning agent's
`Container` (the main container for `Main`, or `SubAgentBox(n)`'s inner
container for `Sub(n)`) obtained through `ChatView`.

---

## 5. Component specs

### 5.1 `aj-tui` — `Editor` top-bar label

File: `src/aj-tui/src/components/editor.rs`.

Add an optional label inlaid into the editor's **top border** (the same
rule that already shows `─── ↑ N more `).

- Field: `top_bar_label: Option<String>` (default `None`).
- Method: `pub fn set_top_bar_label(&mut self, label: Option<String>)`
  — store and `self.invalidate()` on change.
- Render (top-border branch, around the existing `scroll_start > 0`
  logic): build the border from optional left segments, in order: the
  scroll indicator (when `scroll_start > 0`) then the label (when
  `Some`), each as `─── {text} `, followed by `─`-fill to `width`, all
  passed through `self.theme.border_color`. When neither is present,
  the plain `─`×width line as today. Truncate to `width`.

This is a generic capability; the app sets the agent-observation text.
No `Component`-trait change required (the host downcasts to `Editor`).

Tests: label appears in the rendered top row; absent when `None`;
top row never exceeds `width` for various widths; coexists with the
scroll indicator.

### 5.2 `aj` — `SubAgentBox` component (new)

File: `src/aj/src/modes/interactive/components/subagent_box.rs`.

Owns one sub-agent's transcript and renders it either compact (inline
box) or full (switched-to).

```rust
pub enum SubAgentStatus { Running, Done, Failed }

pub enum SubAgentBoxMode { Compact, Full }

pub struct SubAgentBox {
    agent_index: usize,        // the N in Sub(N)
    task: String,              // from SubAgentStart
    status: SubAgentStatus,
    inner: Container,          // this sub-agent's components, in order
    mode: SubAgentBoxMode,     // Compact by default
    compact_rows: usize,       // SUBAGENT_BOX_COMPACT_ROWS
    // gray bg closure (clone of ChatTheme.tool_pending_bg / shared toolBg)
    bg: Arc<dyn Fn(&str) -> String>,
    // accent/dim style closures from ChatTheme for the title/status
}
```

Constructor: `new(agent_index, task, theme: &ChatTheme,
compact_rows) -> Self` (status `Running`, mode `Compact`).

API for the pump / ChatView:
- `fn inner_mut(&mut self) -> &mut Container`
- `fn set_status(&mut self, SubAgentStatus)` (invalidate)
- `fn set_mode(&mut self, SubAgentBoxMode)` — when set, walk
  `inner`'s `ToolExecutionComponent` children and set their
  `header_only` to `matches!(mode, Compact)` (see §5.3); invalidate.
- `fn status(&self) -> SubAgentStatus`, `fn task(&self) -> &str`,
  `fn agent_index(&self) -> usize`

Rendering:

- **Compact** (`width >= MIN`):
  - Title row: `{glyph} agent {N} · {task-summary}` where glyph is
    `▸`/`✓`/`✗`-ish per status (reuse the tool glyph palette: dim
    spinner-ish for Running, green for Done, red for Failed), task
    summarized to one line. Painted full-width with the gray bg.
  - Body: render `inner` at `inner_width = width - 2` (1-col inset),
    take the **last `compact_rows` lines** (tail window). If more lines
    exist above the window, replace the first visible body row with a
    dim `… (N earlier lines)` hint (or prepend it within budget). Pad
    and bg-paint every row to full width via
    `aj_tui::ansi::apply_background_to_line` (mirror `TextBox::apply_bg_row`).
  - One bg-painted blank padding row top and bottom (match the tool
    bubble rhythm). Total height ≈ `compact_rows + 3..4`.
  - The auto-spacer inserted by the pump between chat children provides
    separation, as with tool bubbles.
- **Full**:
  - A single header row: `agent {N} · {task-summary} — observing`
    (themed, not the heavy gray bg).
  - Then `inner.render(width)` verbatim (no window, full results,
    tools not header-only). The terminal scrolls.
- Degenerate width (`width < MIN_BUBBLE_WIDTH = 3`): plain fallback
  (header line + inner lines wrapped), as `ToolExecutionComponent` does,
  so the Tui's strict line-width check never trips.

`compact_rows` constant `SUBAGENT_BOX_COMPACT_ROWS`: define in this
module. Set it a bit taller than the palette overlay's inner rows
(reference: `PALETTE_OVERLAY_INNER_ROWS = 17`); start at **18** and
note it's the single knob to tune. Multiple parallel sub-agents stack;
that is acceptable and they scroll into history.

Width discipline: every returned line's `visible_width` must equal
`width` in the boxed paths (the Tui validates and panics otherwise).
Reuse the bg pipeline that already guarantees this.

Tests: compact render is fixed height and full-width rows; tail window
shows the most recent inner lines; `set_mode(Full)` flips tool children
to full and drops the window; status glyph reflects status; degenerate
width falls back without panicking.

### 5.3 `aj` — `ToolExecutionComponent` header-only mode

File: `src/aj/src/modes/interactive/components/tool_execution.rs`.

Add a `header_only: bool` (default `false`). When `true`,
`render(width)` returns just the wrapped header line (the existing
`header_line()` wrapped to `width` via `wrap_text_with_ansi`) — no
bubble, no bg, no body — so it composes cleanly inside the
`SubAgentBox`'s own gray background. Add:

- `pub fn set_header_only(&mut self, value: bool)` (invalidate/rebuild
  on change).
- Optionally a builder `with_header_only(self, bool)`.

`header_only` is orthogonal to `expanded`: a full-view sub-agent tool
has `header_only=false` and honors `expanded` for its body like a main
tool.

Tests: header-only render is a single (possibly wrapped) line carrying
the tool name and args, no bg padding rows; toggling back restores the
bubble.

### 5.4 `aj` — `ChatView` component (new)

File: `src/aj/src/modes/interactive/components/chat_view.rs`.

```rust
pub struct AgentEntry {
    pub id: AgentId,
    pub task: Option<String>,            // None for Main
    pub status: Option<SubAgentStatus>,  // None for Main
}

pub struct ChatView {
    main: Container,
    sub_boxes: HashMap<usize, usize>, // Sub(n) -> child index in main
    active: AgentId,
    theme: ChatTheme,
    compact_rows: usize,
}
```

API:
- `fn new(theme: ChatTheme) -> Self` (active = Main).
- `fn container_mut(&mut self) -> &mut Container` — the main container
  (main-agent routing + append).
- `fn ensure_sub_box(&mut self, n: usize, task: &str)` — if absent,
  append a `SubAgentBox::new(n, task, &theme, compact_rows)` to `main`
  and record its index in `sub_boxes`. Idempotent.
- `fn sub_box_mut(&mut self, n: usize) -> Option<&mut SubAgentBox>`.
- `fn agent_container_mut(&mut self, id: AgentId) -> Option<&mut Container>`
  — `Main` => `&mut main`; `Sub(n)` => `sub_box_mut(n)?.inner_mut()`.
- `fn set_active(&mut self, id: AgentId)` — store; update each box's
  mode: the active sub-agent's box => `Full`, all others => `Compact`;
  invalidate.
- `fn active(&self) -> AgentId`.
- `fn agents(&self) -> Vec<AgentEntry>` — `Main` first, then each known
  sub-agent (by ascending index) with task + status read from its box.
- `fn set_tools_expanded(&mut self, expanded: bool)` — walk `main`'s
  children: set `expanded` on each `ToolExecutionComponent`, and recurse
  into each `SubAgentBox`'s inner container doing the same. (Does not
  change `header_only`.)
- `fn set_hide_thinking_block(&mut self, hide: bool)` — same walk for
  `AssistantMessageComponent` in `main` and inside each box.

Rendering (`Component::render`):
- `active == Main`: `self.main.render(width)`.
- `active == Sub(n)`: render only that box (it is in `Full` mode). If
  the box is missing (shouldn't happen), fall back to `main`.

`invalidate`: forward to `main` (which forwards to every child,
including boxes).

Downcast plumbing: `impl_component_any!` + `AsRef<dyn Any>` like the
other components.

Tests: routing returns the right container per `AgentId`;
`set_active(Sub(n))` renders only box n's content and hides main rows;
`set_active(Main)` restores; `agents()` lists Main + subs with status.

### 5.5 `aj` — event-pump per-agent refactor

File: `src/aj/src/modes/interactive/event_pump.rs`.

Replace the single `current_assistant: Option<usize>` and
`tool_index: HashMap<String, usize>` with per-agent state:

```rust
#[derive(Default)]
struct AgentRender {
    current_assistant: Option<usize>,
    tool_index: HashMap<String, usize>,
}
// in EventPump:
agents: HashMap<AgentId, AgentRender>,
running_sub_agents: usize,   // for the footer indicator
```

Routing: every helper that currently does
`tui.get_mut_as::<Container>(SlotIndex::Chat.idx())` now does
`tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx())` then
`.agent_container_mut(agent_id)`. Thread `agent_id` into
`handle_message_start/update/end`, `append_tool_execution`,
`update_tool_execution_partial/result`, `append_user_message`,
`append_notice`, `append_styled_notice`, `append_turn_usage`,
`ensure_assistant_message`, and `push_chat_child`. The index values are
container-local (per agent), so they don't collide.

Sub-agent tools: when `append_tool_execution` targets a `Sub(n)`
container, construct the `ToolExecutionComponent` with `header_only =
true` (compact box default). Switching the box to `Full` later flips
them via `SubAgentBox::set_mode`.

Event handling changes:
- `SubAgentStart { child: Sub(n), task, .. }`: `chat.ensure_sub_box(n,
  task)`; `running_sub_agents += 1`; refresh footer agent indicator.
- `SubAgentEnd { child: Sub(n), report, .. }`: set the box status to
  `Done` (or `Failed` — see below); `running_sub_agents` saturating
  `-= 1`; refresh footer. (The box already holds the streamed
  transcript; the `report` text is the sub-agent's final assistant
  message, already rendered inside the box, so we do not append it
  again. Optionally store it for the picker label.)
  - Status: a failed sub-agent reports its error as the `report`; we do
    not have a structured failure flag on `SubAgentEnd`. Mark `Done`
    normally; if the report begins with the agent's error synthesis
    (or, simpler and robust: leave `Done` unless we later add a flag).
    For this iteration: `Done`. Failure styling is a follow-up; note it.
- `ToolExecution*` (Start/Update/End) for `tool == "agent"` on the
  parent: **skip** — do not create or update a tool bubble. The
  `SubAgentBox` (created by the immediately-following `SubAgentStart`,
  finalized by `SubAgentEnd`) is the visual representation of that
  call; rendering the bubble too would duplicate the report. The
  parent's `agent` `ToolExecutionStart` fires just before
  `spawn_agent` emits `SubAgentStart`, so the box lands in the same
  spot the bubble would have.
- `AgentStart/End`: unchanged loader-refcount logic. Per-agent cleanup:
  on `AgentEnd { agent_id }` clear `agents.get_mut(agent_id)` bookkeeping
  for that agent only (so a sub-agent's end no longer needs the special
  "don't clear main" handling — each agent owns its own map). Keep
  Main's `AgentEnd` as the authoritative loader drain.
- `TurnStart { agent_id }`: reset that agent's `current_assistant`.
- `TurnUsage { Sub(n) }`: route the dim usage row into box n's inner
  container (not the shared scrollback). `Main` unchanged (drives
  footer).

Footer indicator: add `EventPump::sync_agent_indicator(tui)` that sets
`Footer::set_agent_activity(Some(AgentActivity { running, open_hint }))`
when `running_sub_agents > 0`, else `None`. `open_hint` is the resolved
`ACTION_AGENT_PICKER` key (e.g. `alt+a`) from the keybindings manager.

`set_tools_expanded` / `set_hide_thinking_block`: delegate to the new
`ChatView` methods, then `tui.invalidate()` + `request_render()`.

New pump accessors for the host:
- `fn agents(&self, tui: &mut Tui) -> Vec<AgentEntry>` — read from
  `ChatView::agents()`.
- `fn set_active_view(&mut self, tui: &mut Tui, id: AgentId)` — call
  `ChatView::set_active(id)`, then `tui.invalidate()` +
  `request_render()` (a view switch repaints the whole chat region).

Update the existing pump tests that reach
`get_mut_as::<Container>(SlotIndex::Chat.idx())` to go through
`ChatView::container_mut()` (main-agent assertions are unchanged in
spirit). Add tests: a `Sub(n)` message routes into box n's inner
container, not the main container; `SubAgentStart` creates exactly one
box; the footer indicator tracks running count; sub-agent tools are
header-only in the box.

### 5.6 `aj` — `Footer` agent-activity indicator

File: `src/aj/src/modes/interactive/components/footer.rs`.

```rust
pub struct AgentActivity {
    pub running: usize,
    pub open_hint: String, // resolved key label, e.g. "alt+a"
}
```

- Field `agent_activity: Option<AgentActivity>` + `set_agent_activity`.
- Render it as a part joined with the existing `  ·  ` separator,
  formatted `"{n} agent ({hint})"` for `n == 1` else
  `"{n} agents ({hint})"`. Keep the whole row dim and width-truncated
  as today. Place it after model/cwd/context-usage (left-to-right).

Tests: 1 vs N pluralization; hidden when `None`; row respects width.

### 5.7 `aj` — `AgentPicker` overlay (new)

File: `src/aj/src/modes/interactive/components/agent_picker.rs`.

Modeled on `thinking_selector.rs` (a `SelectList` + outcome handle)
with a prompt-history-style scope toggle.

```rust
pub enum AgentPickerOutcome { Confirmed(AgentId), Cancelled }
pub type AgentPickerOutcomeHandle = Arc<Mutex<Option<AgentPickerOutcome>>>;

enum Scope { Active, All }

pub struct AgentPickerComponent {
    inner: SelectList,
    outcome: AgentPickerOutcomeHandle,
    agents: Vec<AgentEntry>, // full snapshot (incl. Main)
    active: AgentId,
    scope: Scope,            // Active by default
    theme: SelectListTheme,
}
```

- `new(theme, agents: Vec<AgentEntry>, active: AgentId) -> Self`:
  build items for `Scope::Active` initially.
- Item construction: one per visible entry.
  - `value` encodes the id: `"main"` or `"sub:N"` (decoded on confirm).
  - `label`: `"main agent"` for Main; `"agent N · {task-summary}"` for
    a sub-agent. Mark the currently-active one, e.g. ` (current)`.
  - In `Scope::All`, surface status in the `shortcut`/right column,
    tastefully: `running` (accent), `done` (dim), `failed` (red). In
    `Scope::Active` omit the status column (all are running).
- Visible set:
  - `Active`: `Main` + entries with `status == Some(Running)`.
  - `All`: `Main` + all sub-agent entries.
  - `Main` is always present.
- `on_select` → `Confirmed(decode(value))`; `on_cancel` → `Cancelled`.
- `handle_input`: first check `ACTION_AGENT_TOGGLE_SCOPE` (`ctrl+t`);
  on match flip `scope`, rebuild items (preserving selection by value
  where possible), return `true`. Otherwise delegate to `inner`.
- `outcome_handle()`, `set_focused`, `is_focused`, `render` mirror the
  thinking selector.

Tests: identity-theme render lists Main + running subs in `Active`;
toggling to `All` reveals finished subs with status labels; confirm
emits the decoded `AgentId`; cancel emits `Cancelled`; Main always
present.

### 5.8 `aj` — keybindings, slash command, overlay wiring

**Keybindings** (`src/aj/src/config/keybindings.rs`):
- `pub const ACTION_AGENT_PICKER: &str = "aj.agent.open";` default
  `alt+a`.
- `pub const ACTION_AGENT_TOGGLE_SCOPE: &str = "aj.agent.toggle_scope";`
  default `ctrl+t` (contextual — only the agent picker reads it; shares
  the mnemonic with the history scope toggle, which is fine because the
  two overlays are never up at once). Add both to `aj_keybindings()`.
  Add default-binding tests mirroring the existing ones.

**Slash command** (`src/aj/src/config/slash_commands.rs`):
- Add a `BUILTIN_COMMANDS` entry: `name "agents"`, `title "switch"`,
  `category "agent"`, `action_id Some(ACTION_AGENT_PICKER)`.
- Add `SlashAction::OpenAgentPicker` and `"agents" =>
  SlashAction::OpenAgentPicker` in `dispatch`. Add a dispatch test.

**Overlay wiring** (`src/aj/src/modes/interactive.rs`):
- `OpenSelector::AgentPicker { handle, outcome, parent_palette }`.
- `handle_slash_command`: `SlashAction::OpenAgentPicker` arm — read the
  agent snapshot via `pump.agents(tui)` and the active id via the pump/
  ChatView; build `AgentPickerComponent`, wrap in `OverlayWindow`
  (`"Agents"` title, `palette_overlay_options()`, subtitle that
  includes the scope-toggle hint — mirror prompt-history's subtitle
  phrasing), `tui.show_overlay`, return the selector. Add `AgentPicker`
  to the set of selectors that may carry a `parent_palette` so the
  palette can chain into it.
- Main-loop chord: add an `agent_picker_open_request: Arc<AtomicBool>`,
  set by intercepting `ACTION_AGENT_PICKER` (gated like
  `ACTION_PALETTE_OPEN`: only when no selector/login is up); drain it
  after `tui.handle_input` and dispatch the synthetic `/agents` through
  `handle_slash_command` (mirror the `/palette` and `/history` blocks).
- `handle_selector_outcome`: `OpenSelector::AgentPicker` arm —
  - `None` (still open) => `StillOpen(...)`.
  - `Confirmed(id)` => `pump.set_active_view(tui, id)`; update the
    editor marker (see below); if a `parent_palette` is present, hide
    it; close the overlay; `Closed { notice: None, .. }`.
  - `Cancelled` => one-level pop: if `parent_palette`, restore it (like
    the other selectors); else close to editor.
- `close_all_overlays`: add the `AgentPicker` arm (hide handle + parent
  palette), folding it into the shared match.

**Editor marker** (host side, `interactive.rs`):
- A small helper `apply_editor_agent_marker(tui, id)`:
  `Editor::set_top_bar_label(Some(format!("observing agent {n}")))`
  for `Sub(n)`, `None` for `Main`. Call it from the `AgentPicker`
  confirm path. The marker text is app-owned; the editor only styles it.

### 5.9 `aj-session` — replay synthesizes `SubAgentStart` / `SubAgentEnd`

File: `src/aj-session/src/replay.rs`.

Sub-agent entries are contiguous in append order (a sub-agent runs
fully within one tool call; the listener anchors the first entry at the
parent head and chains the rest on the sub thread). Track the currently
open sub-agent run while walking entries:

- Maintain `open_sub: Option<usize>` and `open_sub_report: String`
  (last assistant text seen for the open sub).
- Before projecting each entry, compute its `agent_id`:
  - If it differs from the open sub (a transition away — to `Main`, to
    a different `Sub`, or about to open a new one): emit
    `SubAgentEnd { parent: Main, child: Sub(open), report:
    open_sub_report }` and clear `open_sub`.
  - If the entry is `Sub(n)` and no sub is open: emit
    `SubAgentStart { parent: Main, child: Sub(n), task }` where `task`
    is the entry's user-message text when this first entry is a user
    message (it is, in practice — the task), else `""`. Set
    `open_sub = Some(n)`.
- While `open_sub` is set, when projecting a `Sub` assistant message,
  record its last text block into `open_sub_report`.
- After the loop, if `open_sub` is still set, emit the closing
  `SubAgentEnd`.

`SubAgentStart` must be emitted **before** that sub-agent's first
message events, and `SubAgentEnd` **after** its last — matching the live
ordering so the pump builds/finalizes the box identically.

Tests (extend the existing sub-agent replay test): a seeded log with a
sub-agent thread yields `SubAgentStart` (with the task) before any
`Sub(1)` message event and `SubAgentEnd` (with the report) after the
last; `parent == Main`, `child == Sub(1)`; a log with two sub-agents
produces two correctly-bracketed pairs; a main turn after a sub run
closes the sub before the main events resume.

---

## 6. Edge cases

- **Parallel sub-agents**: multiple boxes stack in main view; routing is
  by `Sub(n)`. Footer counts all running. Each box is independent.
- **Finishing while observed**: `SubAgentEnd` updates the (currently
  full-view) box's status; the view stays until the user switches. The
  picker's `Active` scope no longer lists it, but `Main` is always there
  and `ctrl+t` reveals it under `All`.
- **Switch while a turn runs**: allowed; routing is independent of the
  active view. Switching only repaints. After `set_active`, the host
  must `tui.invalidate()` + `request_render()`.
- **Cancellation (Ctrl+C)**: child cancellation propagates; the parent
  still emits `SubAgentEnd` (report carries the error). Box becomes
  `Done` (failure styling is a noted follow-up).
- **Resume**: replay emits `SubAgentStart` → box created (status
  `Running` momentarily) → `Sub(n)` events fill it → `SubAgentEnd` →
  status `Done`. No `AgentStart/End` from replay, so the loader and the
  footer agent count stay at rest after resume.
- **Per-agent bookkeeping**: each `AgentId` owns its `current_assistant`
  / `tool_index`, so a sub-agent's `AgentEnd` clears only its own state;
  the main agent's pending `agent` tool call (whose body is the sub run)
  is untouched. This subsumes today's special-case comment.

---

## 7. Testing summary

Unit tests live beside each component (`#[cfg(test)]`). Cover:

- Editor top-bar label (render/width).
- `ToolExecutionComponent` header-only.
- `SubAgentBox` compact window + full mode + status glyph + width.
- `ChatView` routing + active-view rendering + `agents()`.
- Event pump: sub-agent routing into the box, box creation on
  `SubAgentStart`, footer running count, header-only sub tools, the
  updated main-agent tests.
- Footer pluralization + hide-when-zero.
- `AgentPicker` scopes + toggle + confirm/cancel decode.
- Keybinding defaults; `/agents` dispatch.
- Replay `SubAgentStart/End` synthesis.

Run `cargo fmt`, `cargo check`, and `cargo clippy --workspace
--all-targets`, plus `cargo test` for touched crates, before each
commit.

---

## 8. Implementation stages (orchestration)

Stage 1 (independent, parallelizable):
- 1a `aj-tui` Editor `set_top_bar_label` (§5.1).
- 1b `aj` `ToolExecutionComponent` header-only (§5.3).
- 1c `aj-session` replay `SubAgentStart/End` (§5.9).
- 1d `aj` Footer agent-activity indicator (§5.6).

Stage 2 (depends on 1b):
- 2a `aj` `SubAgentBox` (§5.2).
- 2b `aj` `ChatView` (§5.4) — depends on 2a.

Stage 3 (depends on 2):
- 3 `aj` event-pump per-agent refactor + layout swap to `ChatView` +
  footer wiring (§5.5). The big integration.

Stage 4 (depends on 3 + 1a):
- 4 `aj` `AgentPicker` (§5.7) + keybindings/slash/overlay wiring +
  editor marker (§5.8).

Each stage: implement, run fmt/check/clippy/tests for the touched
crates, review, commit with a `scope: summary` message.
