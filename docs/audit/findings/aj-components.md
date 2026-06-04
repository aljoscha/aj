# Audit findings — aj-components

- **Step:** A4
- **Date:** 2026-06-02
- **Audited commit:** e0b380f
- **Scope:** `src/aj/src/modes/interactive/components.rs` and every file
  under `src/aj/src/modes/interactive/components/`:
  `assistant_message.rs`, `auth_picker.rs`, `auth_status.rs`,
  `bash_execution.rs`, `command_palette.rs`, `diff.rs`, `footer.rs`,
  `header.rs`, `help_overlay.rs`, `loader_status.rs`, `login_dialog.rs`,
  `model_selector.rs`, `prompt_history.rs`, `session_selector.rs`,
  `thinking_selector.rs`, `tool_execution.rs`, `user_message.rs` (incl.
  their in-module `#[cfg(test)]` suites).

## Summary

The app-component layer is in good health and uniformly builds on the
aj-tui `Component` trait and framework primitives: every widget starts
with `impl_component_any!()`, the message/tool renderers delegate all
width/wrap/truncation to the T2 `ansi` authority (no fifth width impl is
born here), and the selectors/overlays compose the real `SelectList` +
`TextInput` + `OverlayWindow` rather than reinventing scroll/selection.
The tool renderer is a model of the AG2 contract: `render_details_body`
switches on the `ToolDetails` *variant* (never re-parsing strings), runs
every externally-sourced field through `sanitize_terminal_output`, and
the bubble caches its wrapped output. Test coverage on the leaf
renderers and selectors is genuinely edge-case-first.

The dominant theme is the one the task predicted: the **filterable
overlay list** is hand-copied across four components (model / session /
command-palette / prompt-history), and the selectors have *drifted into
two incompatible wiring patterns* (manual key-interception vs. the
`SelectList` `on_select`/`on_cancel` callbacks) for the same job — the
binary-scope instance of T4's `SelectList`-vs-`SettingsList` finding.
The streaming background-scan machinery is duplicated between the two
streaming selectors. Secondary findings confirm running themes: the
fully-wired-but-dead `ToolExecutionUpdate`/`update_partial` path
(AG1/AG2/A3), chronology/`docs/aj-next-plan §N` comments throughout, and
small copy-pasted test helpers/constants. No critical issues; the layer
boundary holds (no persistence/wire types leak into rendering).

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 2 | 5 | 2 |

## Findings

### [Major][Simplicity] The "search input + SelectList + outcome slot" overlay is hand-copied across four selectors, and they have drifted into two incompatible wiring patterns — `src/aj/src/modes/interactive/components/model_selector.rs:290-369`, `command_palette.rs:142-196`, `session_selector.rs:435-513`, `prompt_history.rs:399-467`, `thinking_selector.rs:105-125`, `auth_picker.rs:82-100`
**What:** Four selectors (`model_selector`, `command_palette`,
`session_selector`, `prompt_history`) each carry a byte-for-byte
near-identical `Component` impl: a `render` that stacks
`search.render()` + a blank row + `list.render()`; a `handle_input` that
intercepts `tui.select.cancel` → commit-cancel, `tui.input.submit` →
commit-selection, the four `tui.select.{up,down,pageUp,pageDown}` keys →
`list.handle_input`, and everything else → `search.handle_input` + a
filter recompute on change; plus identical `set_focused`/`is_focused`
and the `Arc<Mutex<Option<Outcome>>>` slot + `outcome_handle()` plumbing.
Meanwhile `thinking_selector` and `auth_picker` solve the *same* problem
the *other* way — they wire `SelectList::on_select`/`on_cancel` closures
to the outcome slot and forward input straight to the inner list with no
manual key matching (`thinking_selector.rs:83-92`, `auth_picker.rs:63-71`).
So the layer has two competing "wrap a `SelectList` into an outcome-slot
overlay" patterns, and the four manual-routing copies have already
diverged in incidental ways (model rebuilds the list per keystroke;
palette/session/history use `set_filter`; only some drain a scan).
**Why it matters:** Simplicity / sibling-duplication — the binary-scope
instance of T4's `SelectList` vs `SettingsList` Major. A keybinding or
behavior fix (e.g. adding `Home`/`End`, fixing an Esc edge) must be
remembered in four places, and the two wiring patterns mean a reader
can't predict from one selector how another routes Enter. The callback
pattern (`thinking`/`auth`) is clearly the smaller, less error-prone
shape; the manual-routing copies exist mostly because they pair the list
with a `TextInput` filter the callback path doesn't model.
**Suggested action:** Workshop a single `FilterableSelect` (or
`SelectOverlay`) helper that owns the `search` + `list` + outcome slot,
the key routing, and the filter-on-change, leaving each selector to
supply only its items, its commit mapping, and any per-row formatting.
At minimum, converge the four manual-routing selectors onto one shared
`handle_input` and unify the outcome-slot type (see below).
**Effort:** L

### [Major][Simplicity] The streaming background-scan machinery (`spawn_scan` + `SessionLoad`/`drain_loads` + loading flag) is duplicated between the two streaming selectors — `src/aj/src/modes/interactive/components/session_selector.rs:70-75,205-222,519-547`, `prompt_history.rs:230-269,481-…`
**What:** `session_selector` and `prompt_history` each define their own
`spawn_scan` (try-current-runtime → `spawn_blocking` else run inline,
sending batches over an `UnboundedSender` and calling
`render_handle.request_render()`), their own `SessionLoad::{Batch,Done}`
load enum, their own `loading: bool` + empty-list "Loading…" indicator,
and their own `drain_loads` that coalesces `try_recv` batches into a
single `SelectList::extend_items`. The two implementations are
structurally identical (down to the inline-on-no-runtime fallback and
the newest-first batch ordering); they differ only in the payload type
(`SessionPreview` vs a history entry) and the current-row pre-selection.
**Why it matters:** Simplicity / duplication. This is a self-contained,
non-trivial concurrency primitive (channel + blocking task + render-wake
+ coalesced drain) copied wholesale; a fix to the wake/coalesce logic or
the no-runtime fallback has to land twice. It compounds the selector
duplication above.
**Suggested action:** Extract a generic `StreamingScan<T>` (or a
`spawn_scan<T>` + `Drainer<T>` pair) that both selectors compose,
parameterized on the item type and an `extend` callback. Fold into the
shared selector abstraction if that is pursued.
**Effort:** M

### [Minor][Simplicity] `ToolExecutionComponent::update_partial` is fully implemented for an event the runtime never emits, and `bash_execution`'s doc claims the 100ms-debounced streaming "is wired through" — `src/aj/src/modes/interactive/components/tool_execution.rs:327-337`, `bash_execution.rs:11-19`
**What:** `update_partial` (`tool_execution.rs:331`) is a complete
mutating path — re-render body, derive image payload, rebuild children —
called only from the `ToolExecutionUpdate` bus event, which AG1/AG2
confirmed the runtime never emits (`ToolContext::emit_update` is a
permanent no-op). `bash_execution`'s module doc asserts the opposite:
"Live streaming (the `ToolExecutionUpdate` path that drives a 100ms
debounced partial snapshot) is wired through the generic component too —
this helper is called for every snapshot" (`bash_execution.rs:11-16`).
So the layer documents a working tool-progress-streaming feature that
cannot fire, and carries the rendering code for it.
**Why it matters:** Contracts/dead-code — the consuming end of the
AG2 `emit_update`-no-op / AG1 dead-`ToolExecutionUpdate` / A3
dead-handler theme. A reader concludes bash output streams live; it
doesn't. The throttling work in `aj-tools::bash` and this render path
are both dead until the runtime wires the event.
**Suggested action:** Resolve jointly with the AG1/AG2 decision. If
streaming is roadmap, keep `update_partial` but reword the
`bash_execution` doc to state the event is not currently emitted; if
not, drop `update_partial` (and the pump arm) so it isn't mistaken for
live behavior. Don't assert "is wired through" while it's a no-op.
**Effort:** S

### [Minor][Comments] Chronology and `docs/aj-next-plan §N` / "legacy binary" / "future commits" framing pervade the module docs — `src/aj/src/modes/interactive/components.rs:12-13`, `assistant_message.rs:22-24`, `header.rs:9-14`, `footer.rs:14`, `loader_status.rs:11-31`, `diff.rs:13-15`, `bash_execution.rs:18-19`, `model_selector.rs:17`, `thinking_selector.rs:10`, `session_selector.rs:25`
**What:** The same chronology theme AG1/AG2/A3/T4 keep hitting.
`components.rs:12` "Filled in by the … step in Phase 1."; `header.rs:9`
"The full header … lands alongside the 'Selectors and theming' Phase 1
step; this scaffolding gives that work a place to plug into";
`loader_status.rs:25-31` "kept in sync with the legacy CLI's
`display_loader` so users don't see a different status word … between
the two binaries during the Phase 0 → Phase 2 window" and "future
commits can plug richer status"; numerous `docs/aj-next-plan.md §1.x/§4`
citations. These narrate roadmap and a sibling binary rather than
standing on their own.
**Why it matters:** Comments must stand on their own from the current
code; coupling them to plan-doc section numbers and a "legacy binary"
that is no longer the source of truth rots. Same family flagged in every
prior `aj`/`aj-agent` step.
**Suggested action:** Restate as steady-state (e.g. loader: "Default
working message; `set_message` can carry richer status"). Drop the
`§N`, "Phase 1", "legacy binary", and "future commits" framing. Resolve
with the workspace-wide chronology pass.
**Effort:** S

### [Minor][Simplicity] The `strip_ansi` test helper and `BASH_COLLAPSED_LINES` are copy-pasted across modules; outcome-handle plumbing has three inconsistent shapes — `src/aj/src/modes/interactive/components/diff.rs:105`, `bash_execution.rs:33,142`, `tool_execution.rs:800`, `command_palette.rs:38-56`, `auth_status.rs:29-47`, `help_overlay.rs:35-53`, `model_selector.rs:43`
**What:** Three forms of small duplication: (1) an identical ~18-line
`strip_ansi` byte-walker is pasted verbatim in `diff.rs`,
`bash_execution.rs`, and `tool_execution.rs` test modules. (2)
`BASH_COLLAPSED_LINES` is defined in `bash_execution.rs:33` with a
comment noting it "Mirrors `tool_execution::BASH_COLLAPSED_LINES`" — an
invariant-by-comment (and `tool_execution` actually names its analogue
`TEXT_COLLAPSED_LINES`, so the "mirror" reference is already stale). (3)
the outcome-slot handle is expressed three different ways across the
overlays: a bare `pub type OutcomeHandle = Arc<Mutex<Option<_>>>`
(model/session/thinking/auth_picker), a newtype wrapping the same with a
`take()`/`set()` API (command_palette, auth_status, help_overlay), each
hand-rolled.
**Why it matters:** Simplicity. Test-helper drift means a fix to ANSI
stripping touches three copies; the constant "mirror" comment is exactly
the convention-not-types smell; three outcome-handle shapes for one
concept is friction a reader must re-learn per file.
**Suggested action:** Lift a shared `strip_ansi` test helper into a
small `#[cfg(test)]` support module; pick one outcome-handle
abstraction (the newtype-with-`take()` is the cleaner one) and use it
everywhere; if the collapsed-line caps must agree, share one constant.
**Effort:** S

### [Minor][Simplicity] `auth_status` and `help_overlay` are near-identical read-only `SelectList` overlays — `src/aj/src/modes/interactive/components/auth_status.rs:91-115`, `help_overlay.rs:111-139`
**What:** Both build a `SelectList` with `show_selection_indicator:
false`, render it verbatim, and implement the same `handle_input`:
`tui.select.cancel || tui.input.submit` → write `Closed` to the outcome
slot, every other key swallowed (`return true`). They differ only in the
row builder and the outcome enum name (both single-variant `Closed`).
**Why it matters:** Simplicity / duplication. A "read-only list overlay
that closes on Esc/Enter and swallows the rest" is one abstraction
implemented twice; the swallow-all contract and the close-keys live in
two places.
**Suggested action:** Extract a `ReadOnlyListOverlay` (rows + theme in,
`Closed` outcome out) both reuse, or fold into the shared selector
helper as a "non-interactive" mode.
**Effort:** M

### [Minor][Contracts] `session_selector::truncate_for_display` counts `char`s, not display columns — wide/zero-width glyphs in a prompt can overflow the primary column — `src/aj/src/modes/interactive/components/session_selector.rs:421-433`
**What:** `format_primary` truncates the first user message with a
private `truncate_for_display` that caps on `chars().count()` and slices
by char index, not on display width. The rest of the layer routes
visible-width truncation through `ansi::truncate_to_width` (header,
footer, `TruncatedText`). A preview containing CJK/wide glyphs or
combining marks would be measured wrong here, and the layout comment in
`primary_column_layout` assumes a column budget in cells.
**Why it matters:** Contracts/consistency — a localized re-implementation
of width truncation that disagrees with the T2 authority the rest of the
binary uses, on attacker/user-influenced text (a pasted prompt). Low
blast radius (it only sizes a preview label that `SelectList` re-clamps
by cell width afterward), but it's the one place width math forks.
**Suggested action:** Replace `truncate_for_display` with
`ansi::truncate_to_width(one_line, PREVIEW_MAX_CHARS, "…", false)` so the
preview obeys the same display-width contract as every other row, then
drop the bespoke helper.
**Effort:** S

### [Nit][Boundaries] Hand-written `AsRef<dyn Any>` impls sit alongside `impl_component_any!()` on several components — `src/aj/src/modes/interactive/components/user_message.rs:82-86`, `header.rs:110-114`, `footer.rs:130-134`, `loader_status.rs:127-131`, `assistant_message.rs:442-446`, `tool_execution.rs:529-533`
**What:** Six components invoke `aj_tui::impl_component_any!()` (which
supplies `as_any`/`as_any_mut`) *and* hand-write a redundant `impl
AsRef<dyn Any>`. T4 flagged the identical redundancy on `aj-tui`'s
`Image`; the binary repeats it. The selectors that don't write the
`AsRef` impl behave identically, so nothing consumes it.
**Why it matters:** Dead surface / inconsistency — an extra trait impl
duplicating what the macro provides, present on some components and not
others.
**Suggested action:** Remove the hand-written `AsRef<dyn Any>` blocks
unless a downcaller needs them (none found; the pump uses `as_any_mut`).
Confirm with `cargo check`.
**Effort:** S

### [Nit][Simplicity] `model_selector` rebuilds the entire `SelectList` on every keystroke while its sibling selectors use `set_filter` — `src/aj/src/modes/interactive/components/model_selector.rs:165-220,345-350`
**What:** `rebuild_list` reconstructs a fresh `SelectList` (new items,
layout, theme, focus, selection) on every search change, with a comment
"SelectList isn't mutator-friendly for items / layout — the documented
path is to rebuild on change." The other filtering selectors
(`command_palette`, `session_selector`, `prompt_history`) build the list
once and call `set_filter`, contradicting that comment. The rebuild is
*defensible here* (the model filter does custom per-field fuzzy scoring
that `SelectList`'s built-in `FilterMode` can't express), but the
comment overstates it as a `SelectList` limitation rather than a
deliberate scoring choice.
**Why it matters:** Minor inconsistency + a comment that misattributes
the reason to the framework. A maintainer reading it might "fix" the
siblings to also rebuild.
**Suggested action:** Reword the comment to state the real reason
(per-field fuzzy scoring that `SelectList::set_filter`'s modes don't
support), or, if a scoring `FilterMode` is added to `SelectList`,
converge on `set_filter`. Fold into the shared-selector decision.
**Effort:** S

## What's good

- **`ToolDetails`-driven rendering, exactly as the AG2 contract intends.**
  `render_details_body` (`tool_execution.rs:666`) matches on the
  structured `ToolDetails` variant — `Text` / `Diff` / `Bash` /
  `SubAgentReport` / `Todos` / `Json` / `Image` — and never re-parses a
  formatted string back into structure. It reuses the authoritative
  `aj-tools` formatters (`format_todo_list`, `bash::stream_marker`) so
  the on-screen view matches the wire content the model saw. This is the
  UI-neutral-contract → renderer story working as designed.
- **All external text is sanitized before styling.** Every
  non-crate-origin field (summaries, commands, sub-agent reports, diff
  payloads, mime types) passes through `sanitize_terminal_output` in
  `render_details_body`, with a documented rationale (strip ANSI / CR /
  control bytes that would otherwise break width math or clobber cells).
  The "ragged right edge" and "control bytes in header" regressions are
  pinned by tests.
- **Width handling delegates to the T2 authority — no fifth impl.**
  `header`/`footer` use `ansi::truncate_to_width`; `tool_execution` uses
  `wrap_text_with_ansi`; `TextBox`/`Text` caches do the wrapping. The
  only fork is `session_selector::truncate_for_display` (Minor above).
  Header and footer both have width-sweep regression tests asserting no
  row exceeds the terminal width at widths `[0,1,2,…]`.
- **Diff rendering is correct and reuses the shared style palette.**
  `render_unified_diff` builds on `similar::TextDiff`, emits a 3-line
  context window with `…` hunk separators, omits the `---`/`+++` side
  that's empty (fresh-file / delete), and colors via `aj_tui::style`. It
  doesn't re-implement diffing or width math.
- **Renderer caching keeps the hot path cheap.** `tool_execution`
  composes `TextBox` + `Text` children whose caches key on
  `(text, width)` / `(child-lines, width, bg-sample)`, and
  `repeated_renders_return_byte_identical_output` pins the byte-identical
  cache-hit contract the diff-aware terminal write depends on.
  `assistant_message` rebuilds markdown widgets lazily and drops them in
  collapsed mode.
- **Edge-case-first test suites.** The selectors test filtering,
  pre-selection, streaming batch arrival order, current-row chase vs.
  user-navigation, substring-vs-fuzzy semantics, the version-query
  field-scoring regression, and `set_available_height` growth.
  `footer::format_tokens` and `session_selector::format_age/created` pin
  every band/bucket boundary. `bash_execution` covers the legacy vs.
  per-stream truncation markers and the trailing-newline pop.
- **`login_dialog` is a principled async overlay with a documented
  no-lock-across-await contract** and a clean callback/oneshot split that
  keeps the UI thread and the spawned OAuth task decoupled.

## Boundary & architecture notes

Dependency direction is correct: the components consume the public
surfaces of `aj-agent` (`ToolDetails`, events shapes via the pump),
`aj-models` (`ModelInfo`, `UserContent`/`ImageContent`,
`ThinkingConfig`, `oauth`), `aj-session` (`SessionPreview`,
`ConversationEntry`), `aj-tui`, and `aj-conf`/local `config` — none
reach the other way, and persistence/wire types don't leak into
rendering. The `ToolDetails` payload is consumed read-only and rendered
by variant; the components carry no agent-runtime or persistence state.

Two deliberate decoupling seams are worth noting: `model_selector`
defines its own narrow `ModelIdentity` trait (+ `ModelIdentityRef`) so
the host can supply the current `(provider, id)` without the selector
depending on the agent's model handle — a single-impl trait, but it
buys a real boundary (kept narrow on purpose). `auth_picker`/`auth_status`
take pre-computed rows so the components stay free of `aj_models::auth`.
Both are justified.

For synthesis: the four filterable-overlay copies + two read-only-overlay
copies + duplicated streaming-scan are the binary-scope continuation of
T1's "overlay routing duplicated 4x" and T4's `SelectList`/`SettingsList`
sibling-duplication — a single `SelectOverlay` abstraction would subsume
all of it and the two divergent wiring patterns.

## Test assessment

In-module `#[cfg(test)]` per convention, and the boundary coverage is
strong on the leaf renderers and selectors (see "What's good"). Tests
exercise the public component surface (`handle_input` + `render`), not
internals, and use identity themes so assertions read structural text.
Gaps: (1) the dead `update_partial`/`ToolExecutionUpdate` path is
untested (consistent with it never firing); (2) `login_dialog`'s async
callback/oneshot flow has the least direct coverage in the scope; (3)
the duplicated `strip_ansi` test helpers are a small fixture-quality
smell. No flakiness risk beyond `session_selector`/`prompt_history`'s
`Utc::now()` snapshot, which the tests pin with fixed `now` arguments to
the pure formatters. Markdown-fed-from-model-text (assistant/user
messages) inherits T4's unbounded-recursion hazard — see cross-cutting.

## Cross-cutting themes to bubble up

- **Sibling-component duplication (CONFIRMED, Major, widest instance).**
  Four filterable-overlay selectors + two read-only overlays + a
  duplicated streaming-scan primitive, plus two divergent
  `SelectList`-wiring patterns (manual key-routing vs.
  `on_select`/`on_cancel`). The binary-scope continuation of T1's
  overlay-routing-4x and T4's `SelectList`/`SettingsList`. One
  `SelectOverlay` abstraction subsumes it.
- **Half-wired feature across three layers (CONFIRMED).**
  `ToolContext::emit_update` no-op (AG2) → never-emitted
  `ToolExecutionUpdate` variant (AG1) → fully-implemented but dead
  `update_partial` + a `bash_execution` doc that claims streaming "is
  wired through" (A4). The frontend end of the dead-progress-streaming
  feature.
- **Chronology / `docs/aj-next-plan §N` / "legacy binary" comments
  (CONFIRMED, recurs).** Pervasive across the module docs, same family
  as AG1/AG2/A3/T4.
- **Width/truncation impls (REFUTED for this layer, one exception).**
  Everything routes through `ansi::*` except
  `session_selector::truncate_for_display`, which counts chars not
  display columns (Minor). No fifth wrapping algorithm.
- **Untrusted model text into the markdown component (NOTE, ties to
  T4 Major).** `assistant_message` and `user_message` feed raw
  model/user text into `aj-tui`'s `Markdown`, whose parser/renderer T4
  flagged as recursing unboundedly on pathological nesting. The fix
  belongs in `aj-tui` (depth cap), but these are the call sites that make
  it reachable from model output.
- **Redundant `AsRef<dyn Any>` alongside `impl_component_any!()`
  (CONFIRMED, mild).** Same dead-impl T4 flagged on `Image`, repeated on
  six binary components.
- **ToolDetails-driven, sanitized, cached rendering (worth replicating).**
  The tool renderer is the positive model: variant-switched (not
  string-reparsed), every external field sanitized, output cached for a
  byte-identical hot path.
