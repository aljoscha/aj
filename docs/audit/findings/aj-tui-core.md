# Audit findings — aj-tui-core

- **Step:** T1
- **Date:** 2026-06-02
- **Audited commit:** d1775db
- **Scope:** `src/aj-tui/src/lib.rs`, `src/aj-tui/src/component.rs`,
  `src/aj-tui/src/container.rs`, `src/aj-tui/src/tui.rs`,
  `src/aj-tui/src/terminal.rs`, `src/aj-tui/src/capabilities.rs`,
  `src/aj-tui/Cargo.toml` (incl. the in-module `#[cfg(test)]` suites in
  `container.rs` and `tui.rs`). Leaf components, editor, and text utilities
  (T2/T3/T4) and the `tests/` suite (T5) are out of scope.

## Summary

The core of `aj-tui` is a well-designed, genuinely standalone UI framework: the
boundary holds (no `aj_agent`/`aj_models`/`aj_session`/`aj_conf` imports anywhere
in the core, and `crossterm` types are re-exported so the binary doesn't depend
on it directly), the `Component` trait is a small coherent seam, and the
panic/Drop story for terminal restoration is exemplary — raw-mode/paste/keyboard
state lives in process-wide atomics restored from both a `Drop` impl and a
once-installed panic hook, and the strict-width validator even tears the terminal
down *before* panicking. The differential renderer is the crate's center of
gravity (a 3500-line `tui.rs`) and is dense but carefully reasoned: every
non-obvious escape-sequence choice (`\r\n`-for-down vs. CUD, the sync envelope,
the Kitty delete-by-id bookkeeping, the snapshot-once-per-render dimension
sampling) is documented at the point it matters.

The findings are mostly localized. The highest-signal item is that the central
`Terminal` trait carries six methods (`move_by`, `clear_line`,
`clear_from_cursor`, `clear_screen`, `set_title`, `set_progress`) that **no
production code path calls** — the render engine inlines escape bytes into its
frame buffer and never routes through them, so they're dead surface on the very
abstraction that defines the framework's portability seam, exercised only by the
test doubles. Secondary themes: `Tui` is a 59-public-method god-object that mixes
the diff engine, the overlay compositor, and focus management in one type; a
dead `should_render()` public method that also hardcodes the wrong interval
constant; chronology in a couple of doc comments; and the same workspace-vs-direct
dependency-version drift seen elsewhere. None are correctness or boundary
violations.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 1 | 5 | 3 |

## Findings

### [Major][Boundaries] Six `Terminal` trait methods have no production caller — the render engine never routes through the abstraction it defines — `src/aj-tui/src/terminal.rs:147,164,167,170,173,191`
**What:** The `Terminal` trait declares `move_by` (`:147`), `clear_line`
(`:164`), `clear_from_cursor` (`:167`), `clear_screen` (`:170`), `set_title`
(`:173`), and `set_progress` (`:191`). A workspace-wide search shows **none** of
these are called from `tui.rs`'s render loop or from the `aj` binary — only from
the headless test doubles under `tests/` (`validator_skip_unchanged.rs`,
`resize_mid_render.rs`, `virtual_terminal_helpers.rs`). The renderer assembles its
own escape sequences (`\x1b[2K`, `\x1b[2J\x1b[H\x1b[3J`, CUU/CUD, `\r\n`) straight
into the `frame: String` buffer and commits with a single `terminal.write` +
`flush`. So the only production output verbs the engine actually uses are
`write`, `flush`, `start`, `stop`, `hide_cursor`/`show_cursor`,
`columns`/`rows`, `take_input_stream`, `cell_pixel_size`, and
`kitty_protocol_active`. The line-oriented mutators are vestigial. (`clear_line`'s
doc even spells out a contract — "must emit CSI 2K to match the inlined `\x1b[2K`
in tui.rs" — for an interchangeability that nothing exercises.)
**Why it matters:** Boundaries/abstractions dimension. `Terminal` is *the*
portability seam of the framework: it's what a non-stdout backend would implement.
Half of it is a contract nobody depends on, which (a) misleads an implementor of a
new backend into writing and testing `clear_screen`/`move_by` logic that will
never be called, (b) lets those methods silently diverge from the inlined escapes
the engine actually emits (the `clear_line`-must-match-`\x1b[2K` contract is
enforced by prose, not by use), and (c) inflates the trait's apparent
responsibility. A trait whose methods exist only to be implemented by test
doubles is a god-trait by accretion.
**Suggested action:** Decide with the user whether these are a deliberate
"complete terminal" API kept for future non-`Tui` consumers or accreted dead
surface. If the latter, drop `move_by`/`clear_line`/`clear_from_cursor`/
`clear_screen`/`set_title` from the trait (and the doubles), keeping the output
verbs the engine genuinely uses. `set_progress` is a separate case — it's a real
feature with a keepalive thread but currently has no caller either; confirm
whether the binary is meant to drive it.
**Effort:** M

### [Minor][Simplicity] `Tui` is a 59-public-method type that fuses the diff engine, the overlay compositor, and focus management — `src/aj-tui/src/tui.rs:928,1334,2356`
**What:** `Tui` (the struct at `:928`) carries 59 `pub fn`s and ~25 fields across
three loosely-related responsibilities: (1) the differential render engine
(`render`, `full_render`, `differential_render`, `compute_diff_range`,
`validate_line_widths`, the viewport/cursor tracking fields); (2) the overlay
compositor and its lifecycle (`show_overlay`, `hide_overlay`,
`set_overlay_hidden`, `composite_overlays`, `resolve_overlay_layout`, the
`overlays`/`next_overlay_id`/`next_focus_order` fields); and (3) focus and input
routing (`focus_overlay`, `unfocus_overlay`, `set_focus`,
`promote_focus_after_unfocus`, `handle_input_after_listeners`, the
`focused_*`/`saved_focus` fields). The overlay-focus state machine alone
(`focus_overlay` / `unfocus_overlay` / `promote_focus_after_unfocus` / the focus-
heal block in `handle_input_after_listeners`) is intricate enough to warrant its
own home, and its rules ("only `routing_active` capturing overlays are promotion
candidates", "non-capturing overlays never auto-steal") are encoded as filters
duplicated across four call sites.
**Why it matters:** Simplicity / single-responsibility. The three concerns share
the `request_render()` nudge and the line buffer but otherwise touch disjoint
field sets; bundling them makes the type hard to reason about as a whole and means
every reader of "how does rendering work" wades through overlay-focus code and
vice-versa. The duplicated `routing_active && !hidden && !non_capturing`
visibility/eligibility filters are the kind of invariant-by-convention that drifts.
**Suggested action:** Consider extracting an `OverlayStack` (owning
`overlays`, the id/focus-order counters, `show`/`hide`/`set_hidden`/composite, and
the focus-promotion rules) and possibly a small `FocusState`, leaving `Tui` as the
render engine that delegates to them. At minimum, centralize the
"is this overlay an eligible routing target" predicate (currently inlined at
`:1433`, `:2214`, `:2283`, `:2354`) into one helper alongside `overlay_is_visible`.
**Effort:** L

### [Minor][Simplicity] `should_render()` is a public method with no callers that also hardcodes `MIN_RENDER_INTERVAL` instead of the configured `render_interval` — `src/aj-tui/src/tui.rs:1868`
**What:** `pub fn should_render(&self)` compares `last_render_time.elapsed()`
against the module constant `MIN_RENDER_INTERVAL` (`:1871`). It has no caller
anywhere in the workspace (the async loop coalesces via the `throttle: Interval`
keyed on `self.render_interval`, not via `should_render`). Beyond being dead, it's
*inconsistent*: a caller who has set a custom interval via `set_render_interval`
(`:1191`) would get throttle decisions from `should_render` keyed on the wrong
(hardcoded) value. `last_render_time` is written on every render (`:1827`) but read
only here.
**Why it matters:** Simplicity / dead code, plus a latent correctness trap if the
method ever gets wired up. It advertises a throttle check that disagrees with the
engine's real throttle source of truth.
**Suggested action:** Remove `should_render` (and `last_render_time` if nothing
else consumes it), or, if it's meant to stay as a public convenience, key it off
`self.render_interval` and document that it's independent of the async throttle.
**Effort:** S

### [Minor][Contracts] `Container::remove_child_by_ref` is the only detach API and its `addr_eq`-on-fat-pointer identity contract is subtle — `src/aj-tui/src/container.rs:59`
**What:** The container's sole removal-by-target API takes `&dyn Component` and
matches on `std::ptr::addr_eq` of the underlying data pointer. There is no
index-based `remove(i)`. The contract (documented well at `:43-58`) requires the
caller to hold a reference to the *same heap instance* that was added; a distinct
instance with identical fields won't match. The in-module tests have to reach for
`unsafe { &*raw_ptr }` gymnastics (`:243-251`, `:273-274`) to exercise it because
a held `&dyn Component` can't coexist with the `&mut self` call — which is itself a
signal that the ergonomics push callers toward raw pointers. `addr_eq` on a fat
pointer compares only the data pointer (not the vtable), which is correct here but
relies on the reader knowing that.
**Why it matters:** Contracts / leaky-abstraction. Identity-by-address removal with
no index alternative is an unusual API shape; the borrow-checker friction it
creates (tests resort to `unsafe`) suggests real callers may too. The semantics are
documented but enforced by convention, and the "no index-based detach" gap is
noted only in a test comment (`:241-242`), not on the type.
**Suggested action:** Confirm whether `Tui`'s forwarders need anything beyond
by-ref removal; if an index-based `remove(index) -> Option<Box<dyn Component>>`
would serve real callers, add it (it sidesteps the borrow friction entirely). Keep
the by-ref variant for the "I hold the child, detach it wherever it is" case. No
behavior change required if by-ref is genuinely the only need — just record the
"no index detach" decision on the type doc rather than in a test.
**Effort:** S

### [Minor][Comments] Chronology in doc comments: "used to land", "tests had to rely on", "no longer spawns/injects" — `src/aj-tui/src/tui.rs:1748,1929,2455(approx),2545,terminal.rs:455`
**What:** Several docs narrate change history rather than the steady state:
`tui.rs:1748` "eliminating the brief cursor flicker that *used to land*"; `:1929`
"Without the counter, tests *had to rely* on throttle-timing asserts that were
prone to flaking"; the `show_overlay` doc (`:1345-1350`) "The Tui *no longer
injects* one on show"; `terminal.rs:455` "`start` *no longer spawns* a blocking
reader thread". These describe a prior implementation a reader can't see.
**Why it matters:** Comments-and-docs dimension; the same chronology theme TO2/AG1
flagged. "No longer X" comments are noise once the prior state is ancient and
force the reader to imagine a codebase that doesn't exist.
**Suggested action:** Restate as steady-state contracts: "the cursor move + show
commit atomically inside the sync envelope (no flicker)"; "components that need a
`RenderHandle` take one at construction; `show_overlay` does not inject one";
"input is surfaced asynchronously via `take_input_stream`". Keep `total_renders`'s
*rationale* (deterministic coalescing assertions) but drop the "tests had to rely
on" framing.
**Effort:** S

### [Minor][Contracts] `cell_pixel_size` carries a `TODO` for mid-session font/resize re-probe, an unhandled-edge contract — `src/aj-tui/src/terminal.rs:221-223`
**What:** The trait doc states pixel size is "Read once at component construction;
mid-session font-size changes are not handled today. TODO: re-probe on resize…".
This is a known, documented gap in the image-sizing contract: an image laid out at
construction keeps its cell-grid footprint even if the user changes font size or
the terminal's cell pixel ratio shifts on resize.
**Why it matters:** Contracts dimension. The limitation is honestly documented
(good), but a `TODO` on a trait method is a contract-stability smell — consumers
(the `Image` component) bake in the construction-time value with no invalidation
path. Worth tracking as a real follow-up rather than an inline `TODO`.
**Suggested action:** Either promote the `TODO` to a tracked task and reword the
doc to state the limitation as a deliberate current contract ("pixel size is
sampled at construction and not refreshed; resize does not re-probe"), or, if
cheap, hook a re-probe into the resize path so the value tracks SIGWINCH.
**Effort:** M

### [Nit][Dependencies] aj-tui pins direct dependency versions rather than sourcing from `[workspace.dependencies]` — `src/aj-tui/Cargo.toml:9-30`
**What:** `bitflags = "2"`, `crossterm = "0.28"`, `memchr = "2"`, `nucleo = "0.5"`,
`nucleo-matcher = "0.3"`, `syntect = "5"`, `tokio-stream = "0.1"`,
`unicode-segmentation`, `unicode-width`, plus dev-deps `pretty_assertions`,
`serial_test`, `vt100-ctt` are declared with inline versions. The workspace
manifest has a `[workspace.dependencies]` table that other crates draw from. These
particular deps are currently aj-tui-only (verified: no other crate's Cargo.toml
references them), so the drift risk is contained — but the convention in the
workspace (and the dependency-hygiene rubric) is to centralize.
**Why it matters:** Dependency-hygiene dimension; the manifest-drift theme. Not a
correctness issue while these stay single-consumer, but the moment another crate
needs `crossterm` (e.g. the binary, to construct events in a test) the version can
silently diverge. `crossterm` is also flagged in PORTING.md as straddling
0.28/0.29 for keyboard parsing, so it's a likely future cross-crate dep.
**Suggested action:** Move the shared-ish ones (at least `crossterm`,
`unicode-width`, `unicode-segmentation`) into `[workspace.dependencies]` and
reference them as `{ workspace = true }`. Genuinely aj-tui-only leaves
(`syntect`, `nucleo`, `vt100-ctt`) can stay local; the call is the user's.
**Effort:** S

### [Nit][Simplicity] `impl_component_any!` macro + the two `as_any`/`as_any_mut` trait methods are boilerplate every component must repeat — `src/aj-tui/src/component.rs:80-97`
**What:** `Component` requires `as_any`/`as_any_mut` (plus the `Any` supertrait) so
`Container::get_as`/`get_mut_as` can downcast. Every implementor must invoke
`impl_component_any!()` (the macro exists precisely because the two methods are
identical everywhere). This is the standard Rust downcast dance, but it's pure
ceremony imposed on every component author, and the downcast surface
(`get_as`/`get_mut_as`) is used by application code that "owns the layout and
knows the index" — a pattern that itself trades type safety for `Vec<Box<dyn>>`
ergonomics.
**Why it matters:** Simplicity dimension — minor. The macro keeps the boilerplate
to one line, so the cost is small; flagging it as a seam worth questioning rather
than a defect. If concrete-typed child access were rare it might not need to be in
the core trait at all.
**Suggested action:** No change required. Optionally note in the `Component` doc
*why* `Any` is a supertrait (to support `Container::get_as` downcasting) so a
reader understands the otherwise-unmotivated requirement. Revisit only if a
type-indexed child store would serve callers better than `downcast`.
**Effort:** S

### [Nit][Comments] `write_crash_log`/debug-log helpers `format!` into a `String` per line on a diagnostic path — `src/aj-tui/src/tui.rs:431-433,498-538`
**What:** `write_crash_log` and `RenderDebugRecord::append_to` build their bodies
with per-line `push_str(&format!(...))`. These run only on the crash path (about to
panic) or when `AJ_TUI_DEBUG_LOG` is set (deliberately opt-in, per the doc at
`:457-458`), so the allocation cost is irrelevant — this is *not* a hot-path
finding. Calling it out only because the audit tracks render hot-path allocations
and a reader scanning for them might pause here.
**Why it matters:** None in practice; included so the synthesis step can confirm
the perf-allocation theme does *not* apply to these paths.
**Suggested action:** None. The hot path (`render` → `differential_render`) is
allocation-conscious: it appends into a single reused `frame: String`, skips
byte-identical rows, and the width validator was deliberately restricted to changed
rows (`:2811-2814`) precisely to avoid per-frame full scans.
**Effort:** S

## What's good

- **The crate is genuinely standalone.** No `aj_agent` / `aj_models` /
  `aj_session` / `aj_conf` / `aj_tools` import appears anywhere in the core. The
  crate-level doc states the "in-process only" principle and the code honors it
  (filesystem via `ignore`, input via `crossterm`, no shell-outs). `crossterm`
  types are re-exported through the `InputEvent`/`Terminal` surface, so the `aj`
  binary depends on neither `crossterm` directly — a clean inward boundary.
- **Terminal restoration on panic is exemplary** (the explicitly-Major-by-rubric
  case, handled *correctly*). Raw-mode / bracketed-paste / keyboard-enhancement /
  cursor-hidden state lives in process-wide atomics; `restore_terminal_state`
  undoes exactly what's marked active and is idempotent; it's invoked from a
  once-installed panic hook (`install_panic_hook`, `terminal.rs:111`) *and* from
  `ProcessTerminal::stop`/`Drop` *and* from `Tui::stop`/`Drop`. The strict-width
  validator even calls `self.terminal.stop()` before `panic!` so cleanup is
  deterministic rather than relying solely on the hook (`tui.rs:2842`). Every
  swallowed error on the restore path is justified (the process is exiting).
- **The `Component` trait is a clean, minimal seam.** Eight methods, all but
  `render`/`as_any`/`as_any_mut` defaulted, each with a documented contract
  (`&mut self` in `render` justified, `wants_key_release` opt-in explained,
  `set_available_height` / `set_focused` lifecycle clear). Composition via
  `Container` is a thin vertical stack with no hidden coupling, and the
  "components that need a `RenderHandle` take one at construction" rule keeps the
  container free of a forwarding pass.
- **The render loop is hot-path-disciplined.** One dimension snapshot per render
  pass threaded through every consumer (the SIGWINCH-mid-render hazard is
  documented at `:1495-1509`); a single reused `frame` buffer wrapped in the DEC
  2026 sync envelope and committed with one `write`+`flush`; byte-identical rows
  skipped in both the diff loop and the width validator; `\r\n`-for-down vs. CUD
  chosen deliberately to avoid scroll-clobbering with the reasoning recorded inline.
- **Capability detection is well-contained.** `capabilities.rs` is one ordered
  match (tmux/screen/cmux short-circuit → Kitty family → iTerm2 → VSCode →
  Alacritty → conservative fallback), cached behind a `OnceLock<Mutex<Option<…>>>`,
  with conservative defaults documented. No scattered per-component feature flags;
  the doc states the "add the flag here, not a per-component option" rule.
- **`RenderHandle` is a tidy async seam.** Cloneable, coalescing, carries the
  shared terminal dimensions via atomics so components size themselves without
  threading dimensions through every `render`, and `detached()` gives tests/
  standalone components a no-op handle with documented semantics.

## Boundary & architecture notes

Dependency direction is correct and clean: `aj-tui` sits at the leaf of the UI
side of the graph and pulls in only UI/terminal crates (`crossterm`, `unicode-*`,
`syntect`, `nucleo`, `ignore`, `tokio`/`futures`). It depends on nothing from the
agent/models/session/conf side, matching `CLAUDE.md`'s "keep `aj-tui` standalone —
no agent dependencies leaking back" intent (echoed in PORTING.md). The crate
re-exports `crossterm`-derived types (`InputEvent`, the `Terminal` stream) so the
binary consumes a UI API rather than crossterm directly.

Public surface observations for the synthesis step: (1) `set_capabilities` /
`reset_capabilities_cache` are `pub` and exist purely for tests, but they're used
by the crate's *integration* tests across module boundaries (and component
`#[cfg(test)]` modules), so they can't be `pub(crate)` without a test-only feature
gate — the "test harness shipped in prod surface" theme, mild here. (2)
`ProcessTerminal::write_log_path`, `Tui::max_lines_rendered`, `Tui::full_redraws`,
`Tui::total_renders`, `Tui::hardware_cursor_currently_shown`,
`Tui::set_hardware_cursor_currently_shown` are all `pub` observability/escape
hatches whose docs say "exposed primarily for regression tests" — worth a
synthesis pass on whether they justify the public surface. (3) The six unused
`Terminal` trait methods (Major finding) are the standout boundary smell.

## Test assessment

In-module tests for the core are thin by design and correctly layered: `tui.rs`
has only two `#[cfg(test)]` tests, both on the pure `extract_cursor_position`
helper (including a good "two-markers-strip-only-the-first" regression that pins
the diagnostic-honesty contract). The heavy lifting — render strategy selection,
overlay compositing, resize, focus routing, panic/stop behavior — is covered by
the substantial `tests/` integration suite (60+ files, T5 scope), which is the
right place for engine-level behavior given it needs the `VirtualTerminal` double.
`container.rs`'s in-module suite is solid: it exercises `get_as`/`get_mut_as`
type-match and type-mismatch, out-of-bounds, the by-ref-removal happy path,
not-present, and the identity-vs-equality contract (a distinct twin isn't removed)
— genuine boundary/edge coverage, not just the happy path. The one smell is that
those tests must use `unsafe { &*raw_ptr }` to satisfy the borrow checker (see the
Container contract finding), which couples the test to the unusual API shape.

Gaps to note for T5: `capabilities.rs` and `terminal.rs` have *no* in-module
tests (capability detection and dimension-resolution fallback are both covered in
`tests/capabilities.rs` / `tests/terminal.rs`, so this is placement, not absence).
No flakiness risk in the in-scope in-module tests (pure functions, no clock/
network/fs). The capability cache is process-global behind a `Mutex`, so the
integration tests that mutate it use `serial_test` — a coupling the synthesis step
should note as a deliberate (if global-state-driven) choice.

## Cross-cutting themes to bubble up

- **Dead surface on a central abstraction (NEW, Major).** Six `Terminal` trait
  methods with no production caller — distinct from the TO2 "over-broad pub
  surface" theme in that here the dead items are *trait methods* defining the
  framework's portability seam, not helpers. Synthesis should weigh whether this
  is intentional API completeness or accretion.
- **Test-only public surface shipped in prod (CONFIRMED, mild).**
  `set_capabilities`/`reset_capabilities_cache` and the `Tui`/`ProcessTerminal`
  observability getters are `pub` for tests; here they're at least *used* by
  cross-module integration tests, unlike a pure test harness. Fold into the
  workspace decision on test-only visibility (feature gate vs. `pub`).
- **Chronology in comments (CONFIRMED).** "used to land", "no longer
  spawns/injects", "tests had to rely on" recur — the same comment-hygiene theme
  as AG1/TO2.
- **Manifest / dependency-version drift (CONFIRMED).** aj-tui pins direct versions
  instead of `[workspace.dependencies]`; contained today (single-consumer deps)
  but `crossterm` is a likely future cross-crate dep.
- **Perf hot-path allocations (REFUTED for the core).** The render hot path reuses
  one `frame` buffer, skips unchanged rows in both the diff loop and the validator,
  and the only per-line `format!` allocations are on the opt-in crash/debug-log
  paths. This crate is the positive example for the perf theme.
- **anyhow in lib crates (NOT APPLICABLE).** The core surfaces fallibility through
  `std::io::Result` (`Tui::start`, `Terminal::start`) and infallible-by-design
  constructors; no `anyhow` in the scoped files.
- **What held up clean (worth replicating):** the standalone boundary (zero domain
  deps), the multi-layered panic/Drop terminal restoration, the minimal documented
  `Component` trait, the single-snapshot SIGWINCH-safe render pass, and the
  self-contained capability detector.
