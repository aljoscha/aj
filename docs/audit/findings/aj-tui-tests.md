# Audit findings — aj-tui-tests

- **Step:** T5
- **Date:** 2026-06-02
- **Audited commit:** b76044a
- **Scope:** The `aj-tui` integration test suite under
  `src/aj-tui/tests/` (~70 files, ~1000 test fns). Read in full: the
  shared harness (`support.rs`, `support/{virtual_terminal,async_tui,
  logging_terminal,ansi,fixtures,themes,env}.rs`, `support/README.md`).
  Sampled across categories: `support_smoke.rs`, `terminal.rs`,
  `perf_render.rs`, `render_cost.rs`, `resize_mid_render.rs`,
  `validator_skip_unchanged.rs`, `overlay_short_content.rs`,
  `overlay_style_leak.rs`, `wrap_ansi.rs`,
  `regression_regional_indicator_width.rs`, `editor_grapheme_wrap.rs`,
  `select_list.rs`, `markdown.rs`, `loader.rs`, `autocomplete.rs`, with
  cross-checks against `settings_list.rs`, `tui_render.rs`,
  `word_wrap.rs`.

## Summary

This is the strongest-engineered test suite in the workspace so far. The
`VirtualTerminal` harness is a genuine boundary seam: it implements the
real `Terminal` trait, feeds every write to a real VT100 parser, and lets
tests assert on the *rendered* result — viewport text, per-cell style
(`CellInfo`), cursor position, scrollback — rather than on internal
component state. The "happy-path-only" theme is firmly **REFUTED** here:
the suite is dominated by regression/edge tests (mid-render resize,
width-shrink re-validation, style-leak-into-next-line, OSC-8
BEL-vs-ST-preservation across wraps, partial-flag-grapheme width drift,
wide-glyph wrap boundaries). The harness is well-documented (a 327-line
README that even enumerates its *intentional simplifications* and
non-goals), layered into a one-line sync tier and a paused-clock async
tier, and self-tested by three meta-test files. Determinism is mostly
disciplined (paused tokio clock, RAII env guards + `serial_test`, fresh
`TempDir`s).

The findings are localized and largely confirm gaps already flagged in
the production audits from the *test* side. The highest-signal items: the
`ProcessTerminal` progress tests in `terminal.rs` use real `thread::sleep`
keyed to a 1000ms keepalive interval and assert on emission counts — an
inherently flaky, ~5s-wall-clock pattern unlike the rest of the suite;
the three production gaps (markdown adversarial-depth, select/settings
scroll-window extremes, the "escape never split across a wrap" property)
are confirmed absent; the 6 dead `Terminal` trait methods (T1) are
exercised only by the `VirtualTerminal` double and never asserted from a
production path; and a `ScriptedLines` fixture is hand-copied across three
files when it duplicates `fixtures::MutableLines`.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 1 | 5 | 3 |

## Findings

### [Major][Testing] `terminal.rs` progress tests use real `thread::sleep` keyed to a 1000ms keepalive interval and assert on emission *counts* — inherently flaky and ~5s of wall-clock — `src/aj-tui/tests/terminal.rs:102,133,163,195`
**What:** Four `ProcessTerminal` progress tests sleep on the real clock
and assert on how many `\x1b]9;4;3\x07` sequences a background keepalive
thread wrote to a disk log:
- `set_progress_true_keeps_re_emitting...` sleeps 2300ms and asserts
  `emissions >= 3` (`:133,138`).
- `redundant_set_progress_true...` sleeps 1300ms and asserts
  `emissions <= 4` (`:195,203`), with an inline comment admitting "at
  most 2 keepalive ticks in 1.3s" — i.e. the bound is timing-derived.
- `set_progress_false...` and `drop_stops_the_keepalive_thread...` sleep
  1300ms and assert `count == 1` (`:102,163`).
The keepalive interval is 1000ms (per the test comments). A CI host under
load can deliver 1, 2, or 3 ticks in a 1300ms window, and a starved
scheduler can miss a tick in 2300ms — so `>= 3` and `<= 4` are both at
the mercy of scheduler jitter. Combined cost is ~5s of pure sleeping,
serialized (`serial_test`), against a global `WRITE_LOG_ENV` + disk file.
`loader.rs` shares the same wall-clock pattern at smaller scale
(`:60,100,122,140`, 20–100ms sleeps comparing `current_frame()`).
**Why it matters:** Testing — hidden coupling to wall-clock time, exactly
the flakiness the rubric calls out. The rest of the suite goes to real
trouble to be deterministic (the async tier uses
`#[tokio::test(start_paused = true)]` + `tokio::time::advance`, and
`resize_mid_render.rs` simulates a SIGWINCH with a *counter* rather than a
sleep). These count-based timing assertions are the one place a green
suite can go red on an unchanged tree.
**Suggested action:** Make the keepalive interval injectable (a
constructor/setter on `ProcessTerminal`, test-only via `cfg(test)` or a
`pub(crate)` knob) so the tests can drive a 1ms interval and assert
"at least one re-emit" / "no re-emit after cancel" deterministically; or
move the keepalive onto a tokio interval and drive it with the paused
clock like the async tier. At minimum, relax the upper-bound assertions
(`<= 4`) to a lower-bound liveness check and document the residual
flakiness. Same treatment for `loader.rs`'s frame-advance sleeps (its
`current_frame()` is already `pub` "for tests", so a deterministic clock
hook is feasible).
**Effort:** M

### [Minor][Testing] Confirmed gap: no test pins markdown parser/renderer behavior on adversarially deep nesting (T4's Major) — `src/aj-tui/tests/markdown.rs:71`
**What:** T4 flagged unbounded recursion in `parse_inline` /
`parse_markdown` / `render_inline_tokens` on model-emitted markdown
(`"*".repeat(N)`, `"> ".repeat(N)`, `[[[[...]]]]`) as a Major
availability risk. From the test side this is confirmed unguarded: the
only nesting tests are `renders_a_deeply_nested_list` (four fixed levels,
`:71-80`) and `renders_an_ordered_nested_list` (`:82`) — both shallow,
well-formed fixtures. A search of the whole suite finds adversarial
`repeat(100_000)` inputs only in `truncate_to_width.rs:10` (the text
layer), never against the markdown parser. So the one uncatchable-abort
hazard in the component layer has no regression guard.
**Why it matters:** Testing at the boundary — the missing edge/error-path
coverage on untrusted input. A depth cap added per T4 would have no test
to lock it in, and the current behavior (process abort on deep input) is
unasserted.
**Suggested action:** Once T4's depth cap lands, add a regression test
that feeds `"*".repeat(100_000)` and `"> ".repeat(100_000)` to
`Markdown::render` and asserts it returns (treating the overflow as
literal text) rather than aborting. Mirror the existing
`truncate_to_width.rs` large-input pattern.
**Effort:** S

### [Minor][Testing] Confirmed gap: select/settings scroll-window slice at list extremes is unpinned (T4's Minor) — `src/aj-tui/tests/select_list.rs`, `src/aj-tui/tests/settings_list.rs`
**What:** T4 noted that `visible_window_start`'s centered-clamp
arithmetic (`selected.saturating_sub(half).min(upper_bound)`) is untested
at the ends of the list. From the test side: `select_list.rs` asserts
navigation wrap/clamp on `selected` (`:230,234`) and column alignment,
and `settings_list.rs` has *no* `max_visible`/scroll-window assertions at
all (a grep for `max_visible`/`scroll`/`window` in `settings_list.rs`
returns nothing). Neither file asserts that with `len > max_visible` and
`selected` at `0` / `len-1` the rendered slice actually shows the
top / bottom window. A regression in the clamp would not fail a test.
**Why it matters:** Testing at the boundary — the off-by-one-prone scroll
math is exactly the clamp/edge path the rubric asks to pin, and it is the
diverged duplication T4 flagged (page keys in one list, not the other),
so the two need independent extreme-coverage.
**Suggested action:** Add a `select_list` and a `settings_list` case with
`len=10, max_visible=3` asserting the visible slice (via the rendered
rows) at `selected=0`, mid, and `len-1`. Cheap, pure, and locks the
divergent widgets to a shared expectation.
**Effort:** S

### [Minor][Testing] Confirmed gap: no property test asserts an ANSI escape is never split across a wrap point (T2's Minor) — `src/aj-tui/tests/wrap_ansi.rs`
**What:** T2 flagged that the headline invariant of the text layer — a
multi-byte CSI/OSC code is never cut in half by `break_long_word` /
`wrap_text_with_ansi` — is enforced by construction but never asserted.
`wrap_ansi.rs` is thorough on *style preservation* (underline closed with
`\x1b[24m` not full reset, bg-color carried across lines, OSC-8 reopened
with the same BEL/ST terminator) and includes an embedded-tab break
(`:177`), but no test re-parses each output line through
`extract_ansi_code` and asserts only whole codes survive a force-break at
several narrow widths. The closest is structural string-contains checks,
which would not catch a half-emitted `\x1b[3` chunk that happens to still
contain the substring.
**Why it matters:** Testing at the boundary — the layer's most important
correctness property has no guard, so a refactor of the segment loop to
byte-windowed scanning could reintroduce a mid-escape split silently.
**Suggested action:** Add a property-style test in `wrap_ansi.rs`: for a
styled string force-broken at widths 1..=10, feed each output line back
through `aj_tui::ansi::extract_ansi_code` and assert every escape is a
whole code, and that concatenated visible content round-trips.
**Effort:** S

### [Minor][Simplicity] `ScriptedLines` is hand-copied across three files and duplicates `fixtures::MutableLines` — `src/aj-tui/tests/render_cost.rs:33`, `src/aj-tui/tests/validator_skip_unchanged.rs:40`, `src/aj-tui/tests/perf_render.rs:32`
**What:** Three files define a private `ScriptedLines` component:
`Rc<RefCell<Vec<String>>>` with a `render` that clones the buffer and a
mutator (`set`/`replace`). This is functionally identical to
`fixtures::MutableLines` (`support/fixtures.rs:70`), which already offers
`set`/`append`/`push`/`clear` behind the same `Rc<RefCell<_>>` shape and
the same clone-on-render. The README's own "extract into `fixtures.rs`
once a component has earned a copy in three or more test files"
(`support/README.md:316-324`) rule is met — there are three copies, and a
shared fixture already exists that they reinvent.
**Why it matters:** Simplicity / duplication — the per-test boilerplate
the harness is supposed to absorb. A change to the fixture contract (e.g.
adding width recording) must be remembered in four places. The
`replace(idx, value)` variant in `render_cost.rs` is the only behavior
`MutableLines` lacks.
**Suggested action:** Replace the three `ScriptedLines` copies with
`fixtures::MutableLines`; add a `MutableLines::replace(idx, value)` (or
have the perf test `set` a freshly-cloned vec) to cover the spinner-row
swap. Confirms the README's own extraction rule.
**Effort:** S

### [Minor][Testing] The 6 dead `Terminal` trait methods (T1) are exercised only by the `VirtualTerminal` double, never asserted from a production path — coverage inflation — `src/aj-tui/tests/support/virtual_terminal.rs:376,394,398,402,406,412`
**What:** T1 found that `move_by`, `clear_line`, `clear_from_cursor`,
`clear_screen`, `set_progress`, and `set_title` are dead on the render
path — `Tui` writes a single composed frame buffer and never routes
through them. `VirtualTerminal` nonetheless implements all six
(`:376-419`), and the hand-rolled doubles `ShrinkingTerminal` /
`ShrinkOnSecondColumnsRead` delegate the ones they bother to implement to
the inner `VirtualTerminal`. Crucially, **no test asserts these methods
are called or observes their effect through a `Tui` render** — the writes
they would produce (`\x1b[2K`, CUD) are asserted in `tui_render.rs` only
as bytes the *frame-buffer diff path* emits inline, not via the trait
methods. So the methods exist, are "covered" by being implemented in a
double, and contribute to the illusion of a fully-tested `Terminal`
seam while being production-dead. (Conversely, the doubles *omit*
`set_progress`/`kitty_protocol_active`/`take_input_stream`/
`cell_pixel_size` and lean on the trait defaults — confirming those four
aren't on any tested path either.)
**Why it matters:** Testing — the "dead-surface tested only by doubles"
theme, confirmed. The harness faithfully models a trait whose six methods
the engine doesn't use; that's harmless but it means a reader can't tell
from the suite which `Terminal` methods are load-bearing. It also means
trimming the dead methods (if T1's suggestion is taken) touches only the
double, with zero production-test fallout — useful to record.
**Suggested action:** Tie to T1's decision on the dead methods. If they're
kept as an intentional "complete terminal" API, add a short doc note on
`VirtualTerminal` that these six are present for trait-completeness and
not on the `Tui` render path; if trimmed, the only test change is
deleting the matching `VirtualTerminal` impls. No new tests warranted —
the point is to stop the double implying coverage.
**Effort:** S

### [Nit][Simplicity] Two hand-rolled `Terminal` doubles re-implement ~14 forwarding methods each; a delegating wrapper in `support/` would absorb the boilerplate — `src/aj-tui/tests/resize_mid_render.rs:81`, `src/aj-tui/tests/validator_skip_unchanged.rs:124`
**What:** `ShrinkOnSecondColumnsRead` and `ShrinkingTerminal` each
hand-write a full `impl Terminal` that delegates every method to an inner
`VirtualTerminal` except the one (`columns()`) they override. That's ~14
trivial `fn x(&mut self) { self.inner.x() }` lines per double, repeated
across two files, purely to vary the reported column count.
**Why it matters:** Simplicity / duplication — the same delegation
skeleton copied twice. Both tests are about one thing: "the engine reads a
shrinking width." The boilerplate obscures that intent and rots if the
trait gains a method.
**Suggested action:** Add a `support` helper — either a
`VirtualTerminal::with_columns_override(Rc<Cell<u16>>)` knob or a small
`DelegatingTerminal { inner, columns_fn }` wrapper — so both tests express
only the column-override behavior. Optional; the current code is correct,
just verbose.
**Effort:** S

### [Nit][Comments] Chronology / "earlier shapes" framing in the support README and `wrap_text_with_ansi` regression comment — `src/aj-tui/tests/support/README.md:96,233`, `src/aj-tui/tests/wrap_ansi.rs:177`
**What:** The README has a "Deviations from a byte-stream testing model"
section (`:96`) framed around "Earlier shapes of this kind of test
harness drove components with raw byte strings... assertions ported across
from any byte-oriented source need to account for..." and a "Historical
note" (`:233`) narrating that "An older in-source `TestTerminal` used to
live in `src/terminal.rs`. It's been retired." Both narrate change history
rather than stating the steady-state contract. `wrap_ansi.rs:177`'s
comment ("`break_long_word` *previously* counted a tab grapheme as zero
columns...") is the same shape T2/T4 flagged in production comments.
**Why it matters:** Comments/documentation — the chronology theme that has
recurred in every prior TUI step (T1/T2/T4), here in test docs. The
"deviations from a byte-stream model" framing only makes sense to a reader
who knows the prior model; for everyone else it's noise around what are
actually just the harness's current contracts (typed input, CSI 2K, etc.).
**Suggested action:** Restate the "Deviations" section as the harness's
present contract ("Tests drive typed `InputEvent`s, not raw bytes;
`clear_line` emits CSI 2K") and drop the "earlier shapes / ported across"
framing. Delete or shrink the "Historical note" to a one-line statement of
why `VirtualTerminal` is the only terminal double. Reword the `wrap_ansi`
comment to the steady-state rule (the *why* — tab is width 3 in
`visible_width` — is worth keeping; the "previously counted zero" is not).
**Effort:** S

### [Nit][Testing] `tui_render.rs` (70KB) and `markdown.rs` (135KB / 126 tests) are very large single files; coherent but at the edge of navigable — `src/aj-tui/tests/markdown.rs`, `src/aj-tui/tests/tui_render.rs`
**What:** Two test files dominate the suite by size. `markdown.rs` is one
126-test file covering the entire `Markdown` surface; `tui_render.rs` is a
70KB engine-diff file. Both are *internally* well-organized (clear
`// ---` section banners: Nested lists, Tab normalization, Tables,
Combined features, Pre-styled text, etc.), so this is coherence-at-scale,
not sprawl — but a 135KB file is slow to open and the section banners are
doing the job a module split otherwise would.
**Why it matters:** Testing / organization — minor. The one-file-per-
public-surface taxonomy (documented in the README) is sound and the rest
of the suite follows it cleanly; these two are just outliers in size. No
correctness concern.
**Suggested action:** Optional: split `markdown.rs` along its existing
banner sections into a couple of files (e.g. `markdown_tables.rs`,
`markdown_spacing.rs`) if it keeps growing. Leave `tui_render.rs` as-is
unless it grows further — engine-diff tests benefit from being co-located.
No action required now; flagged so the taxonomy decision is explicit.
**Effort:** S

## What's good

- **The harness is a genuine boundary seam, not an internals reach-through.**
  `VirtualTerminal` implements the real `Terminal` trait and feeds every
  `write` to a real VT100 parser (`vt100-ctt`); tests assert on the
  *rendered* result — `viewport()`, `viewport_trimmed()`, per-cell
  `CellInfo` (fg/bg/bold/italic/underline/inverse/wide), `cursor()`,
  `scroll_buffer()`. `overlay_style_leak.rs` asserting `!cell.italic` on
  row 1 after the compositor ran, and `editor_grapheme_wrap.rs` asserting
  `visible_width(line) <= width` plus exact CJK split points, are textbook
  boundary tests. The shared `Rc<RefCell<State>>` handle (one clone to the
  `Tui`, one to the test) is the right ownership shape for a synchronous
  read-back.
- **Happy-path-only is firmly REFUTED.** The suite is *dominated* by
  regression and edge tests: mid-render SIGWINCH not tripping the
  validator (`resize_mid_render.rs`), width-shrink forcing re-validation
  of byte-identical rows (`validator_skip_unchanged.rs`, with
  `#[should_panic(expected = "exceeds terminal width")]` corners),
  partial-flag-grapheme width drift sweeping the whole
  `U+1F1E6..=U+1F1FF` block (`regression_regional_indicator_width.rs`),
  OSC-8 BEL-vs-ST terminator preserved across wraps
  (`wrap_ansi.rs:298`), emoji-on-the-wrap-boundary, and overlay-shorter-
  than-terminal. This is the strongest crate on the testing dimension, as
  T1/T2/T4 anticipated.
- **Determinism is taken seriously where it counts.** The async tier
  (`async_tui.rs`) is built around `#[tokio::test(start_paused = true)]` +
  `tokio::time::advance`, with a deliberate `render_now()` (no await) vs
  `wait_for_render().await` naming asymmetry that makes the engine choice
  obvious at the call site. `resize_mid_render.rs` simulates a race with a
  *counter*, not a sleep. Env mutation goes through an RAII `EnvGuard`
  paired with `#[serial_test::serial]` (the `unsafe` set_var blocks carry
  a safety comment justified by the serialization). Filesystem tests
  (`autocomplete.rs`) use fresh `TempDir`s and walk in-process
  (`ignore::WalkBuilder`) so nothing depends on host `/tmp` layout or
  external binaries.
- **The harness documents its own contract — including non-goals.** The
  327-line `support/README.md` has an API table, "Behavior notes"
  (cursor is `(row,col)`; `clear_line` is CSI 2K), an "Intentional
  simplifications" section, and an explicit "Out-of-scope features" list
  (no image protocols, no raw stdin byte parsing). This is the
  contract-documentation discipline the guidance asks for, applied to test
  infrastructure.
- **Framework self-tests guard the seam.** `support_smoke.rs` (plug-in +
  render round-trip), `virtual_terminal_helpers.rs` (clear/reset/resize/
  writes-log/CellInfo/scroll_buffer), and `support_framework.rs` (env
  guard, fixtures, theme factories, ansi helpers) fail *first* and with a
  focused message when the harness regresses, instead of producing a
  cascade of unrelated component-test failures. Excellent investment.
- **Escape-byte assertions are confined to where the bytes ARE the
  contract.** Only `tui_render.rs`, `overlay_options.rs`,
  `overlay_image_bypass.rs`, `tui_stop.rs`, and
  `virtual_terminal_helpers.rs` assert on raw writes — and there the diff
  protocol (CSI 2K, CUD vs `\r\n` to avoid scroll) *is* the boundary
  contract being tested. Everywhere else tests assert on rendered viewport
  state, so the suite does not broadly ossify implementation detail.
- **`identity_*` vs `default_*` theme factories.** A small but real
  ergonomic win: structural tests use `identity_select_list_theme()` and
  never have to strip ANSI; styling tests use the `default_*` flavor and
  assert on `CellInfo`. Keeps the two concerns from leaking into each
  other.

## Boundary & architecture notes

The `tests/support/` non-`mod.rs` layout (one `support.rs` with `#[path]`
attributes loading siblings) is the idiomatic way to share test code
across Cargo's per-file integration binaries while satisfying the
workspace `clippy::mod_module_files` lint, and it keeps the dev-only deps
(`vt100-ctt`, `serial_test`, `tempfile`) out of the crate's public
surface. `LoggingVirtualTerminal` being a zero-cost `type` alias over
`VirtualTerminal` (rather than a separate impl) is the right call — the
writes log lives on every terminal, and the alias only signals intent.

Placement matches the crate's documented deviation from the workspace
`AGENTS.md` default: `aj-tui` puts pure-function tests (e.g. `wrap_ansi`,
`word_wrap`, `fuzzy`, `truncate_to_width`) under `tests/` rather than
in-module, with a stated rationale ("every test file targets one public
surface; keep `src/` modules focused on implementation"). That is a
defensible, documented choice and is applied consistently; the in-module
`#[cfg(test)]` suites (T1/T2/T4 confirmed) are reserved for *private*
helpers with no public counterpart. The synthesis step should record this
as an intentional per-crate exception to the workspace default rather than
a drift.

One placement nit worth noting for synthesis: `terminal.rs` and
`capabilities.rs` tests live under `tests/` and drive `ProcessTerminal` /
the global capability cache through their public API — correct placement,
but they are also the two files carrying the suite's only real flakiness
(wall-clock sleeps; global-cache `serial_test`).

## Test assessment

The suite genuinely exercises the boundary/contract and is the workspace's
high-water mark for edge coverage. The confirmed gaps are all *known* from
the production audits and are narrow:

- **Markdown adversarial depth** (T4 Major) — unguarded; the only nesting
  tests are shallow well-formed fixtures.
- **Select/settings scroll-window at extremes** (T4 Minor) —
  `settings_list.rs` has no scroll-window assertions at all.
- **"Escape never split across a wrap" property** (T2 Minor) — style
  *preservation* is well-tested; the never-split *property* is not pinned.
- **Dead `Terminal` methods** (T1) — implemented in the double, asserted
  by no production path; coverage-by-double.

Fixture/helper quality is high (`MutableLines`, `StaticOverlay`,
`InputRecorder`, the ansi/theme helpers), with the one lapse being the
`ScriptedLines` triplicate that should be `MutableLines`. The flakiness
risk is concentrated in `terminal.rs` (Major) and `loader.rs` (Minor):
both assert on a real-clock background animation/keepalive, against the
grain of the otherwise-deterministic suite that has a paused-clock async
tier precisely for timing concerns. Organization is a coherent
one-file-per-surface taxonomy with clear section banners inside the large
files; `markdown.rs` (135KB) and `tui_render.rs` (70KB) are size outliers
but not sprawl.

## Cross-cutting themes to bubble up

- **Happy-path-only coverage (REFUTED — strongly).** The
  strongest-tested crate refutes the theme: the suite is built around
  regressions, race simulations, Unicode/grapheme edges, and
  `#[should_panic]` overflow corners. Carry this as the positive
  counter-example to the happy-path concern raised elsewhere.
- **Dead-surface-tested-only-by-doubles (CONFIRMED).** The 6 dead
  `Terminal` methods (T1) are implemented in `VirtualTerminal` and
  asserted from no production path; the trait double makes the seam look
  fully covered. Synthesis should pair this with T1's dead-method finding:
  trimming them is test-cheap (touches only the double).
- **Chronology in comments (CONFIRMED).** The support README's
  "Deviations from a byte-stream model" / "Historical note" sections and
  `wrap_ansi.rs`'s "previously counted a tab as zero" comment are the same
  "used to / earlier shape" smell flagged in T1/T2/T4 production code, now
  in test docs.
- **Duplication the harness should absorb (CONFIRMED, mild).**
  `ScriptedLines` hand-copied across 3 files duplicates
  `fixtures::MutableLines`; two hand-rolled delegating `Terminal` doubles
  repeat ~14 forwarding methods. The harness has the right extraction
  *rule* documented (3-file threshold) — these just haven't followed it.
- **Wall-clock coupling in timing tests (NEW, Major).** `terminal.rs`
  (and `loader.rs`) sleep on the real clock and assert emission/frame
  counts keyed to fixed intervals — inherently CI-flaky and slow, and
  inconsistent with the suite's own paused-clock async tier. Synthesis
  should check whether other crates' background-thread tests share this.
- **Per-crate testing-placement exception (CONFIRMED, intentional).**
  `aj-tui` deliberately puts pure-function tests under `tests/` against
  the workspace `AGENTS.md` default, with a documented rationale. Record
  as an intentional exception, not drift.
- **What held up clean (worth replicating):** the real-VT-parser boundary
  seam, the sync/async two-tier helper split with deliberate name
  asymmetry, the RAII env guard + `serial_test` discipline for global
  state, the framework self-tests that fail-first, and the
  `identity_*`/`default_*` theme split.
