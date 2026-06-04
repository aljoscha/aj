# Audit findings — aj-tui-editor

- **Step:** T3
- **Date:** 2026-06-02
- **Audited commit:** c477c5a
- **Scope:** `src/aj-tui/src/editor_component.rs`,
  `src/aj-tui/src/components/editor.rs`,
  `src/aj-tui/src/components/text_input.rs`,
  `src/aj-tui/src/components/text_box.rs`, `src/aj-tui/src/kill_ring.rs`,
  `src/aj-tui/src/undo_stack.rs`, `src/aj-tui/src/keybindings.rs`,
  `src/aj-tui/src/keys.rs`, `src/aj-tui/src/autocomplete.rs` (incl. the
  in-module `#[cfg(test)]` suites). The dedicated integration suites
  under `tests/` (`editor_*.rs`, `autocomplete*.rs`, `kill_ring.rs`,
  `keybindings*.rs`, `text_input*.rs`, `text_box.rs`) were skimmed for
  boundary coverage; their full audit is T5.

## Summary

The text-editor subsystem is the largest unit seen so far and holds up
well on the dimension that matters most for it: Unicode/grapheme cursor
correctness. Every cursor mutation is byte-offset based but advances on
grapheme boundaries via `unicode-segmentation`, all byte-slices into the
buffer happen on offsets that came from `grapheme_indices`/`char_indices`
or are saturating-clamped + char-boundary-snapped (`TextInput::set_value`),
and no path slices a buffer at an unvalidated arbitrary index — I found no
multibyte-boundary panic. The supporting data structures (`KillRing`,
`UndoStack`) are small, clean, well-contracted, and the keybinding layer
(`keys.rs`/`keybindings.rs`) is a genuinely strong, exhaustively-tested
seam: a string-descriptor grammar with a fail-closed parser, a
register-resolve-conflict manager with documented merge rules, and a
process-wide `RwLock` singleton that user overrides (from aj-conf) plug
into via `set_user_bindings`. `autocomplete.rs` is a cohesive
trait-based seam (one-shot `get_suggestions` vs. streaming
`AutocompleteSession`) with the filesystem-walk cancellation contract
documented and honored.

The findings are about *size and accreted surface*, not correctness.
`components/editor.rs` is a 3868-line, ~40-field `Editor` that fuses six
concerns (document model, grapheme cursor math, visual-line/wrap layout,
the autocomplete state machine + async pipeline + streaming-session
driver, paste-marker bookkeeping, and prompt history) into one type;
unlike T1's `Tui` it keeps a modest 20-method public surface, but the
private complexity is concentrated and the autocomplete machinery alone
is a state machine that wants its own home. Secondary themes: four dead
`#[allow(dead_code)]` items in the editor (a "backward-compatibility
shim", a reserved field, two reserved helpers), a small but exact
duplication between a provider method and a free function in
autocomplete, a per-keystroke O(document) visual-line-map rebuild on
vertical movement, and a `super`→`Meta` display-label asymmetry between
`keys.rs` and `keybindings.rs`. None are correctness or boundary
violations.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 1 | 5 | 4 |

## Findings

### [Major][Simplicity] `Editor` is a 3868-line, ~40-field type fusing six concerns; the autocomplete state machine + async pipeline + streaming-session driver is a subsystem embedded in it — `src/aj-tui/src/components/editor.rs:406,459,2180,2321,2487`

**What:** `Editor` (struct at `:406`) carries ~40 fields spanning six
distinct responsibilities: (1) the document model (`lines`,
`cursor_line`, `cursor_col`); (2) grapheme cursor math and visual-line
layout (`layout_width`, `preferred_visual_col`, `snapped_from_cursor_col`,
`build_visual_line_map`, `compute_vertical_move_column`,
`move_to_visual_line`); (3) the **autocomplete state machine**
(`autocomplete_state`/`_list`/`_prefix`/`_max_visible`, the
`AutocompleteMode`/`EnterOutcome` enums, `update_autocomplete`,
`maybe_trigger_*`, `accept_autocomplete_on_enter`); (4) the
**autocomplete async pipeline** (`autocomplete_request_id`/`_cancel`/
`_task`/`_rx`/`_tx`/`_render_handle`, `dispatch_autocomplete_request`,
`drain_autocomplete_results`, `apply_autocomplete_delivery`, the
`AutocompleteDelivery`/`AutocompleteSnapshot` structs); (5) the
**streaming-session driver** (`autocomplete_session`,
`pump_autocomplete_session`, `make_autocomplete_notify`,
`wait_for_pending_autocomplete`); and (6) paste-marker bookkeeping
(`pastes`, `paste_counter`, `find_next_marker` and the four
`marker_*` helpers) plus prompt history (`history`, `history_index`).
Roughly 1700 of the 3868 lines (~`:2180`–`:2840`) are the autocomplete
subsystem alone. The comment at `:451-458` explicitly defends keeping
autocomplete inline ("splitting it out would push routing logic into
every caller") — a real argument for the *state machine* living near the
editor, but the *async/streaming plumbing* (request-id guards,
cancellation tokens, the delivery channel, the nucleo-session tick loop)
is mechanism that doesn't need the editor's fields to function.

**Why it matters:** Simplicity / single-responsibility. This is the
crate's largest type and the hardest to reason about as a whole. A reader
asking "how does cursor movement work" wades through the autocomplete
pipeline and vice-versa; the six concerns share only `lines`/`cursor_*`
and `fire_change()` but are interleaved across 3800 lines. The
autocomplete pipeline in particular has its own lifecycle (spawn →
debounce → deliver → staleness-guard → apply) that is independently
testable and would be a natural `AutocompleteController` owning
`request_id`/`cancel`/`task`/`rx`/`tx`/`session` and exposing
`update(lines, cursor)` / `drain() -> Option<PopupUpdate>` to the editor.
This mirrors T1's `Tui` god-object theme, though `Editor`'s *public*
surface (20 methods) is far more disciplined — the sprawl is internal.

**Suggested action:** Workshop an extraction with the user. The
highest-value split is an `AutocompleteController` (or `…Pipeline`) that
owns the async/streaming machinery (`dispatch_*`, `drain_*`, `pump_*`,
the channel, the session, the request-id/snapshot staleness guards) and
hands the editor a small "here is the current popup list / apply this
item" interface; the editor keeps the *routing* decisions (which
keystroke opens/narrows/accepts). A second candidate is folding the
paste-marker scanner (`find_next_marker`, `parse_marker_tail`,
`marker_*`) into a small `PasteMarkers` helper type. Both reduce the
field count and let each subsystem be unit-tested at its own boundary. No
behavior change intended.

**Effort:** L

### [Minor][Simplicity] Four dead `#[allow(dead_code)]` items in `Editor`: a back-compat shim, a reserved field, and two reserved helpers — `src/aj-tui/src/components/editor.rs:153,534,2167,2697`

**What:** Four items are silenced with `#[allow(dead_code)]` and have no
production caller (verified workspace-wide):
- `marker_containing` (`:153`) — "reserved for rendering / visual-line
  layout work"; nothing calls it (`move_left`/`move_right` use
  `marker_ending_at`/`marker_starting_at` instead).
- `paste_buffer: Option<String>` field (`:534`) — "reserved for future
  bracketed paste tracking"; written once to `None` in `new` (`:697`),
  never read.
- `adjust_scroll` (`:2167`) — a "rough approximation" scroll helper; the
  real scroll math lives inline in `render` (`:3014`). No caller.
- `maybe_update_autocomplete_after_edit` (`:2697`) — explicitly a
  "backward-compatibility shim in case a future caller needs the old …
  semantics"; it just forwards to
  `maybe_retrigger_autocomplete_after_delete`. The only references are
  two doc-comments (`:3234`, `:3278`) that *name* it but call the
  narrower helpers.

**Why it matters:** Simplicity / dead code. Each is a small individual
cost, but together they're vestigial surface carrying speculative
"reserved for future" / "backward-compat" rationale — exactly the
chronology-and-speculation smell the guidance discourages. The
`maybe_update_autocomplete_after_edit` shim is the worst: two live
comments point at a function that does nothing the live helpers don't,
which actively misleads a reader tracing the autocomplete-after-edit
path.

**Suggested action:** Remove all four (and fix the two doc-comments at
`:3234`/`:3278` to name the helper that actually runs,
`maybe_retrigger_autocomplete_after_delete` /
`maybe_trigger_autocomplete_on_insert`). If `marker_containing` is
genuinely wanted for upcoming visual-layout work, track it as a task
rather than carrying it commented-as-reserved. No behavior change.

**Effort:** S

### [Minor][Simplicity] `extract_at_prefix` (method) and `extract_at_prefix_from_text` (free fn) are byte-for-byte duplicate prefix extractors — `src/aj-tui/src/autocomplete.rs:363,1481`

**What:** `CombinedAutocompleteProvider::extract_at_prefix` (`:363`) and
the free function `extract_at_prefix_from_text` (`:1481`) have identical
bodies: check `extract_quoted_prefix(...).starts_with("@\"")`, else find
the last delimiter, take the token after it, return `Some(token)` iff it
starts with `@`. The free function exists because
`FuzzyFileSession::update` (`:1419`) needs the same tokenization but
isn't a method on the provider; its doc even says "Shared between the
session constructor and `FuzzyFileSession::update` so both use the same
tokenization rules" — but it is *not* shared with the provider's own
`extract_at_prefix`, which is the third caller of the same logic.

**Why it matters:** Simplicity / duplication. Two copies of a
tokenization rule that the codebase explicitly wants kept in sync (the
whole `@`-prefix contract depends on the provider, the
`try_start_session` path, and the session's `update` agreeing). A change
to delimiter handling in one is a silent divergence.

**Suggested action:** Make `extract_at_prefix` delegate to the free
`extract_at_prefix_from_text` (the method adds no provider state), so all
three call sites route through one definition. One-line change.

**Effort:** S

### [Minor][Boundaries] `super` descriptor round-trips through `keys.rs` but `keybindings::format_key_segment` displays it as `Meta`, and accepts `meta`/`cmd` the parser rejects — `src/aj-tui/src/keybindings.rs:287`, `src/aj-tui/src/keys.rs:436,493`

**What:** Two display/parse layers disagree on the modifier vocabulary.
`keys.rs` is the canonical matcher: `parse_key_id` accepts only `ctrl`,
`alt`, `shift`, `super` (`:493`, rejecting `meta`/`cmd`), and
`format_key_descriptor` emits `super` (`:436`). But
`keybindings::format_key_segment` (`:287`) maps `"meta" | "cmd" |
"super" => "Meta"` for the *display* string — so a canonical binding
`super+k` displays as `Meta+K`, and `format_keybinding` will happily
title-case a `meta+k` or `cmd+k` descriptor into `Meta+K` even though
`key_id_matches` would reject those descriptors as unknown modifiers
(returning `false`, i.e. the binding silently never fires).

**Why it matters:** Boundaries / leaky-abstraction. The two functions
form the keybinding vocabulary's two halves (match vs. display) and use
different modifier alphabets. The display side advertises `meta`/`cmd`
as valid input, but a config author who writes `cmd+k` gets a binding
that *formats* correctly in help text yet never matches a keystroke —
the fail-closed strictness of `parse_key_id` (a genuinely good design,
documented at `:455-467`) is undercut by a display helper that pretends
the rejected spellings are fine.

**Suggested action:** Align the vocabularies: have `format_key_segment`
map only `"super" => "Super"` (or `"Meta"`, but pick one and match
`format_key_descriptor`'s `super` emission), and drop the
`"meta" | "cmd"` arm — or, if `cmd`/`meta` are meant to be accepted
aliases, add them to `parse_key_id` so the match side agrees. Either way
the display and match layers should recognize the same modifier set.

**Effort:** S

### [Minor][Simplicity] Vertical cursor movement rebuilds the whole-document visual-line map on every keystroke — O(document) per Up/Down/PageUp/PageDown — `src/aj-tui/src/components/editor.rs:1060,1076,1098,3399,3419`

**What:** `move_up`, `move_down`, `page_scroll`, and the Up/Down
history-aware branches in `handle_input` each call
`build_visual_line_map(width)` (`:1060`, `:1076`, `:1098`, `:3399`,
`:3419`), which iterates **every** logical line and word-wraps each one
wider than `width` via `segment_line` + `word_wrap_line_with_segments`
(`:1193-1226`). `render` builds an equivalent map again (`:2953-3006`).
So a single Up keystroke on a large document re-segments and re-wraps the
entire buffer, and a held arrow key does it per repeat. `segment_line`
itself does a `line.contains(PASTE_MARKER_PREFIX)` scan plus a
grapheme-indices pass per line.

**Why it matters:** Perf hot-path (the editing path, not the per-frame
diff path T1/T2 cleared). For the expected interactive prompt sizes
(a handful of lines) this is negligible and the code is correct. But the
editor accepts arbitrarily large pastes (inline below the 1000-char /
10-line marker threshold) and `set_text` of large content, so a
multi-thousand-line buffer makes each vertical move O(n) in document
size, and the same map is rebuilt twice per frame+move cycle (navigation
+ render). This is the one place in the subsystem where editing cost
scales with total buffer size rather than the current line.

**Suggested action:** Confirm the expected buffer-size envelope with the
user. If large buffers are in scope, consider caching the visual-line map
keyed on `(layout_width, document-revision)` and invalidating it on edit
(the editor already centralizes mutation through `fire_change` /
`reset_sticky_state`, so a revision counter is cheap), so navigation and
render share one build per frame. If prompts are always small, document
the O(document) cost as an accepted bound on the map builder rather than
leaving it implicit.

**Effort:** M

### [Minor][Contracts] `TextInput::set_on_change` silently drops its callback; the trait advertises a capability the single-line input doesn't have — `src/aj-tui/src/components/text_input.rs:346`

**What:** `EditorComponent::set_on_change` is a *required* trait method
(`editor_component.rs:57`), so `TextInput` must implement it — but its
body accepts and discards the callback (`:346-351`), documented as
"`TextInput` doesn't track an `on_change` callback; only the multi-line
`Editor` fires change events." A host that installs an on-change listener
through `Box<dyn EditorComponent>` and happens to be backed by a
`TextInput` gets silent no-ops with no signal.

**Why it matters:** Contracts. This is a capability the type structurally
lacks being exposed as a required (not defaulted) method, enforced by a
silent drop rather than the type system. Contrast the *optional*
`border_color`/`set_border_color` pair, which models "this editor may not
have a border" cleanly via `Option` + defaulted no-op so a host can
*detect* the absence (`border_color()` returns `None`). `on_change` has
no such detection seam: the host can't tell a real listener from a
swallowed one.

**Suggested action:** Either (a) make `set_on_change` an *optional*
defaulted trait method (matching `add_to_history`/`insert_text_at_cursor`)
so `TextInput` opts out by inheriting the no-op and the asymmetry is
visible in the trait definition, or (b) actually wire `on_change` into
`TextInput`'s mutation paths if single-line change events are wanted. The
required-but-silently-dropped middle ground is the one to avoid.

**Effort:** S

### [Nit][Comments] Chronology / speculation in editor comments: "Replaces the old inline synchronous call", "backward-compatibility shim", "reserved for future" — `src/aj-tui/src/components/editor.rs:534,2213,2699`

**What:** Several comments narrate change history or speculative futures
rather than the steady state: `:2213` "Replaces the old inline
synchronous call: this bumps the request id…" (the "old" call is gone and
unseeable); `:2699` "Kept as a backward-compatibility shim in case a
future caller needs the old 'just refresh if open' semantics"; `:532`
"Paste buffering (reserved for future bracketed paste tracking)" and
`:153` "reserved for rendering / visual-line layout work". Same family as
the T1/T2/AG1/TO2 chronology theme.

**Why it matters:** Comments/documentation. "Replaces the old X" forces a
reader to imagine a prior implementation; "reserved for future" /
"backward-compat shim" justify code that has no live use (see the dead-
code finding). Comments should describe what the code *does* now.

**Suggested action:** Restate `dispatch_autocomplete_request`'s doc as
the steady-state contract ("bumps the request id, cancels any prior
request, snapshots the buffer, and spawns a worker that calls the
provider; results flow back through `autocomplete_tx`") and drop the
"Replaces the old…" framing. The "reserved"/"shim" comments should
disappear with their dead items.

**Effort:** S

### [Nit][Simplicity] `EnterOutcome` is a single-variant enum threaded through one call site — `src/aj-tui/src/components/editor.rs:642,2810,3273`

**What:** `EnterOutcome` (`:642`) has exactly one variant, `Consumed`.
`accept_autocomplete_on_enter` returns it (`:2810`) and the sole caller
destructures it with `let EnterOutcome::Consumed = …` (`:3273`) and then
unconditionally `return true`. The doc honestly explains why it's a
single variant ("neither completion path continues on to message
submit"), but a one-variant enum with one producer and one consumer adds
a type for a value that carries no information.

**Why it matters:** Simplicity — minor over-engineering / premature
generality. The enum anticipates a future `Continue`-style variant that
doesn't exist, costing a type definition, an import, and an irrefutable-
pattern destructure for no present signal.

**Suggested action:** Make `accept_autocomplete_on_enter` return `()` (or
nothing) and have the caller `return true` directly; reintroduce an enum
if and when a second outcome is genuinely needed. Optional cleanup.

**Effort:** S

### [Nit][Simplicity] `safe_slice` does an O(n) char-index scan per call and is invoked several times per autocomplete request — `src/aj-tui/src/autocomplete.rs:867`

**What:** `safe_slice(s, start, end)` (`:867`) walks `s.char_indices()`
to translate char offsets to byte offsets. `get_suggestions`,
`try_start_session`, `apply_completion`, and `FuzzyFileSession::update`
each call it once or twice per request on the current line. It's correct
(handles the `start == 0` and `end <= start` edge cases explicitly) and
off the per-frame render path, but it re-scans the line from the start
every call rather than computing both offsets in one pass when `start`
and `end` are both needed.

**Why it matters:** Simplicity / minor allocation-free inefficiency. Not
a hot-path defect (autocomplete runs on edit, not per frame, and lines
are short), but the helper re-walks from byte 0 each call and the two
common callers want both `before` and `after` slices of the same line,
which is two scans where one would do.

**Suggested action:** Optional. If touched, compute the byte offsets for
`start` and `end` in a single `char_indices` pass, or document that the
per-call O(line) cost is accepted given autocomplete frequency and line
length. No behavior change.

**Effort:** S

### [Nit][Contracts] `jump_to_char` documents "needle is a single ASCII codepoint" as an assumption the type doesn't enforce — `src/aj-tui/src/components/editor.rs:1585`

**What:** `jump_to_char(needle: char, …)` (`:1585`) is correct for any
`char` (it encodes the needle to UTF-8 and uses `str::find`/`rfind`, so
multibyte needles work and never split a boundary), but its doc says
"tests assume the needle is a single ASCII codepoint, which is the
intended use of this surface." The character-jump entry in `handle_input`
(`:3217`) accepts any printable `KeyCode::Char(c)`, so a user *can* drive
it with a non-ASCII jump target (e.g. jump to `é`), which works but is
outside the documented "intended use."

**Why it matters:** Contracts — minor. The implementation is more general
than the stated contract, which is the safe direction (no panic), but the
doc undersells what the code guarantees and pins behavior to test
assumptions rather than the real input domain.

**Suggested action:** Reword the doc to state the actual contract: the
needle may be any `char`; matching is case-sensitive and byte-exact, and
the scan never splits a UTF-8 boundary. Drop the "tests assume ASCII"
framing. No code change.

**Effort:** S

## What's good

- **Grapheme-correct cursor math with no multibyte panic.** Every cursor
  move in both `Editor` and `TextInput` is a byte offset that advances on
  grapheme boundaries (`grapheme_boundaries`, `grapheme_indices`), every
  buffer byte-slice uses an offset that came from grapheme/char iteration
  or a saturating clamp, and the one place a clamp could land mid-codepoint
  (`TextInput::set_value`, `text_input.rs:86-92`) explicitly snaps forward
  to the next `char_boundary` with a documented rationale. The
  task's "Major if it can panic on a multibyte boundary" hazard does not
  materialize anywhere in scope.
- **`KillRing` and `UndoStack` are clean, minimal, well-contracted data
  structures.** `KillRing` (71 lines) documents its accumulate/prepend
  rotation semantics at the module head and ignores empty pushes so
  callers don't guard the "delete zero" boundary; `UndoStack` (117 lines)
  is a thin clone-on-push LIFO that is generic over the snapshot shape
  (`(String, usize)` for the input, a struct for the editor) with the
  clone-vs-no-reclone contract spelled out. Both have boundary-focused
  in-module tests.
- **The keybinding layer is an exemplary, exhaustively-tested seam.**
  `keys.rs`'s `key_id_matches`/`format_key_descriptor` handle the three
  terminal encodings of `Shift+letter` (legacy uppercase-with-SHIFT,
  Kitty disambiguate, Kitty alt-keys-clears-SHIFT), the `BackTab`↔
  `shift+tab` normalization, the literal-`+` key, and a strict round-trip
  contract — all pinned by ~30 tests including the explicit
  `format → match` round-trip sweep. `parse_key_id` *fails closed* on
  unknown modifiers (documented bug-factory rationale at `:455-467`).
  `keybindings.rs`'s manager has clearly-documented merge rules
  (override-replaces, empty-list-unbinds, conflicts-only-between-user-
  bindings) and the aj-conf override path plugs in cleanly via the
  process-wide `set_user_bindings` singleton.
- **`is_newline_event` is a precisely-scoped, well-reasoned recognizer.**
  The exact-modifier-match contract (only `Char('\n')` no-mods,
  `Char('j')+CTRL`, `Enter+ALT`) is documented with *why* each shape
  exists and why plain/Shift/Ctrl Enter are deliberately excluded (so
  user rebindings aren't shadowed), and the negative cases are tested.
- **`autocomplete.rs` is a cohesive trait seam.** The one-shot
  (`get_suggestions`) vs. streaming (`AutocompleteSession`) split is
  documented at the trait head with the editor's "try session first,
  fall back to one-shot" contract; the cancellation story (token honored
  in every walker thread, `Drop` fires the cancel, `build_parallel`
  quits promptly) is documented and implemented; the parallel walker's
  "score-inline-don't-cap-the-walk" reasoning (`:1059-1094`) correctly
  avoids the blind-cap-discards-late-matches bug. The `.git` exclusion is
  defense-in-depth (`filter_entry` + `path_has_git_component`).
- **`EditorComponent` is a small, well-modeled pluggable seam.** Four
  required methods, the rest defaulted no-ops or obvious fallbacks; the
  `border_color`/`set_border_color` `Option`-gated pair is the right way
  to model an optional capability (the host can *detect* absence), and the
  object-safety + dyn-dispatch round-trip is tested for both built-in
  editors.
- **Paste-marker atomicity is consistent across navigation, deletion,
  wrapping, and submit.** A large paste collapses to a `[paste #N …]`
  marker; cursor move/delete/word-motion treat the marker as one unit
  (`marker_starting_at`/`marker_ending_at`), `segment_line` keeps it
  un-split at wrap boundaries, and `get_expanded_text` restores the
  literal content for submission — one coherent contract threaded through
  every path.
- **`TextBox`'s render cache is correct without function-pointer
  comparison.** It samples `bg_fn("test")` to detect a changed background
  closure (since `dyn Fn` can't be compared), keys the cache on
  `(width, child_lines, bg_sample)`, and only invalidates on real
  structural change — the same careful caching discipline T1 praised in
  the core.

## Boundary & architecture notes

Dependency direction is correct and matches `CLAUDE.md`'s standalone-
framework intent: the editor subsystem pulls in only UI/util crates
(`crossterm`, `unicode-segmentation`, `nucleo`, `ignore`, `tokio`,
`async-trait`, `tokio-util`) and in-crate leaves (`ansi`, `word_wrap`,
`word_boundary`, `keys`, `keybindings`, `kill_ring`, `undo_stack`,
`select_list`). No `aj_agent`/`aj_models`/`aj_session`/`aj_conf` import
appears anywhere. Internal layering is one-directional: `editor.rs` and
`text_input.rs` depend on the leaves; `editor_component.rs` is the
pluggable trait both editors implement; `autocomplete.rs` is a standalone
provider/session seam the editor consumes.

Public-surface notes for synthesis: (1) `Editor::wait_for_pending_autocomplete`
(`:2436`) is a `pub async fn` whose doc says it's a test-driving helper
("for tests that drive the editor synchronously") — test-only surface
shipped in prod, the same theme T1 flagged for `Tui`'s observability
getters; here it's genuinely needed by the cross-crate integration tests
that can't reach a private method, so it can't be `pub(crate)` without a
test-only gate. (2) `keybindings::reset` (`:554`) is similarly a "primarily
a test helper" `pub fn`. (3) `Editor`'s 20-method public surface is
disciplined (constructor, text get/set, history, padding, autocomplete
config, submit polling) — *not* a god-object on the public axis, unlike
`Tui`; the sprawl is the ~40 private fields and the embedded autocomplete
subsystem (Major finding). (4) `keys::Key` is a large bag of convenience
constructors (`char`/`ctrl`/`alt`/`enter`/…) used by tests and the
keybindings layer — reasonable for a framework but worth confirming none
are dead.

## Test assessment

The boundary is well-covered. In-module suites are boundary-focused and
not implementation-ossifying: `undo_stack.rs` pins LIFO order, clear, and
tuple-shape round-trips; `keys.rs`'s ~30 tests pin the full
match/format/round-trip contract including the three Shift-letter
encodings and the newline-fallback recognizer; `editor_component.rs`
round-trips both built-in editors through `Box<dyn EditorComponent>`;
`editor.rs`'s in-module tests cover the identifier-alphabet and
symbol-context predicates (with non-ASCII negatives) plus the palette-
trigger paths. The large dedicated integration suite (skimmed for T5:
`editor_unicode.rs`, `editor_grapheme_wrap.rs`, `editor_sticky_column.rs`,
`editor_undo.rs`, `editor_kill_ring.rs`, `editor_paste_marker.rs`,
`editor_kitty_csiu.rs`, `editor_backslash_enter.rs`,
`editor_character_jump.rs`, `editor_page_scroll.rs`,
`autocomplete_session.rs`, `keybindings_user_overrides.rs`,
`kill_ring.rs`, `text_input*.rs`, etc.) appears to exercise exactly the
edge cases the in-scope code reasons about — grapheme wrapping, sticky-
column navigation across rewraps, paste-marker atomic motion, the CSI-u
paste decode, the backslash-Enter workarounds in both directions, and
user keybinding overrides. The boundary looks genuinely well-covered, not
happy-path-only; confirm depth in T5.

Gaps to note for T5: (1) The visual-line-map / sticky-column decision
table (`compute_vertical_move_column`, `:1276-1324`) is intricate (7
documented cases P/S/T/U) — confirm `editor_sticky_column.rs` exercises
all seven rows and the marker-continuation skip-forward branch
(`:1412-1427`). (2) The autocomplete async pipeline's staleness guards
(request-id + snapshot double-check in `apply_autocomplete_delivery`,
`:2355-2368`) are the kind of concurrency contract that wants a test for
the *stale*-delivery path (a delivery arriving after the buffer changed
must be dropped) — confirm coverage. (3) `wait_for_pending_autocomplete`
spins up to 500 iterations with a 2ms sleep waiting for nucleo
quiescence (`:2449-2461`); this is a test helper coupled to wall-clock
sleeps, a mild flakiness vector under a loaded CI — worth a T5 look at
whether the streaming-session tests are deterministic.

No flakiness risk in the in-scope in-module tests (pure functions / 
synchronous editor mutations; the keybindings tests that mutate the global
singleton use `reset` + `serial_test` per the manager's own doc).

## Cross-cutting themes to bubble up

- **God-object / single-responsibility (CONFIRMED, internal variant).**
  `Editor` fuses six concerns across 3868 lines — the same theme as T1's
  59-method `Tui`, but inverted: `Editor`'s *public* surface is
  disciplined (20 methods) while the *private* complexity (~40 fields, an
  embedded autocomplete subsystem) is the sprawl. Synthesis should note
  that the two largest types in the crate (`Tui`, `Editor`) both want
  decomposition, for different reasons.
- **Dead / speculative surface (CONFIRMED, mild).** Four
  `#[allow(dead_code)]` items in the editor carrying "reserved for
  future" / "backward-compat shim" rationale — folds into the TO2
  over-broad-pub-surface and the chronology themes.
- **Chronology in comments (CONFIRMED).** "Replaces the old inline
  synchronous call", "backward-compatibility shim", "reserved for future"
  — the same comment-hygiene theme as T1/T2/AG1/TO2.
- **Test-only public surface shipped in prod (CONFIRMED, mild).**
  `Editor::wait_for_pending_autocomplete` and `keybindings::reset` are
  `pub` for the integration tests; like T1's case they're *used* across
  module boundaries so can't trivially be `pub(crate)`. Fold into the
  workspace decision on test-only visibility (feature gate vs. `pub`).
- **Duplication between siblings (CONFIRMED, small).** The two
  `extract_at_prefix` copies in `autocomplete.rs`, and the near-identical
  Emacs-keybinding `handle_input` ladders + `insert_char` undo-coalescing
  logic shared (by copy) between `Editor` and `TextInput` — the single-
  line vs. multi-line split justifies *most* of the divergence, but the
  prefix-extraction and the undo-coalescing rule are genuinely identical
  and drift-prone.
- **Perf hot-path (PARTIALLY CONFIRMED — editing path, not render).**
  Unlike T1/T2 (which REFUTED the perf theme for render and text), the
  editor rebuilds the whole-document visual-line map per vertical
  keystroke — O(document), not O(line). Negligible for prompt-sized
  buffers but the one place editing cost scales with total buffer size.
  Synthesis should weigh against the expected buffer-size envelope.
- **Unicode/grapheme correctness (CONFIRMED CLEAN — replicate).** The
  editor matches the text layer (T2): grapheme-based cursor math
  everywhere, no multibyte-boundary panic, defensive char-boundary
  snapping on the one clamp that could land mid-codepoint. This is the
  positive example for the Unicode theme on the editing side.
- **anyhow in lib crates (NOT APPLICABLE).** No `anyhow`; the subsystem
  surfaces fallibility through `Option` (suggestions, sessions) and
  `Result<(), SessionInvalid>`, and the keybindings lock-poison cases use
  documented `expect`. No `thiserror` error type is needed at this layer.
