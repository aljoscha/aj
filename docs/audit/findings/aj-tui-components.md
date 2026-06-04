# Audit findings — aj-tui-components

- **Step:** T4
- **Date:** 2026-06-02
- **Audited commit:** 94e21ea
- **Scope:** `src/aj-tui/src/components.rs`,
  `src/aj-tui/src/components/markdown.rs`,
  `src/aj-tui/src/components/image.rs`,
  `src/aj-tui/src/image_protocol.rs`,
  `src/aj-tui/src/components/loader.rs`,
  `src/aj-tui/src/components/cancellable_loader.rs`,
  `src/aj-tui/src/components/select_list.rs`,
  `src/aj-tui/src/components/settings_list.rs`,
  `src/aj-tui/src/components/overlay_window.rs`,
  `src/aj-tui/src/components/spacer.rs`,
  `src/aj-tui/src/components/text.rs`,
  `src/aj-tui/src/components/truncated_text.rs` (incl. their in-module
  `#[cfg(test)]` suites). The editor/text-input (T3), core component
  model (T1), and the `tests/` suite (T5) are out of scope.

## Summary

The leaf-component layer is in good health and largely confirms the
positive picture from T1/T2: every widget implements the same minimal
`Component` trait uniformly, delegates all width/wrap/truncation to the
authoritative `ansi` utilities (no fourth truncation impl is born here),
and gates image output cleanly on the T1 capability cache with a sane
text fallback. The composition story is clean — `OverlayWindow`,
`Loader`/`CancellableLoader`, and `SettingsList`'s submenu all wrap and
delegate to a child rather than reinventing input/focus/invalidate
plumbing — and the `markdown` parser/renderer is careful about ANSI
state propagation (the "re-open the outer style after every non-text
inline" machinery is subtle and correct). No malformed image input
panics: every encoder/decoder path is total over arbitrary bytes.

The findings are localized. The highest-signal item is unbounded
parser recursion in `markdown.rs` on pathological input (deeply nested
emphasis or blockquotes can overflow the stack) — a Major because it's a
correctness/availability risk on untrusted (model-emitted) text. Beyond
that: `select_list` and `settings_list` are near-sibling
re-implementations of the same scroll/filter/render skeleton that have
drifted apart (different wrap semantics, different filter modes,
different APIs); `markdown.rs` carries a width-handling tab constant
duplicated from `text.rs`; the two loaders and the two text components
each share a thin-wrapper relationship that is mostly fine; and there
are a handful of doc/contract nits. The boundary holds (no domain crates
leak in).

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 2 | 5 | 4 |

## Findings

### [Major][Contracts] Markdown parser recurses unboundedly on nested emphasis / blockquotes — pathological model output can overflow the stack — `src/aj-tui/src/components/markdown.rs:782,339,1847`
**What:** Three parse/render paths recurse with a depth proportional to
input nesting and **no depth cap**:
- `parse_inline` (`:782`) recurses on every emphasis/strikethrough/link
  open: `**a**`, `*a*`, `~~a~~`, `[a](u)` each call `parse_inline` on
  their inner span (`:805`, `:831`, `:856`, `:895`). Input like
  `"*".repeat(2*N)` followed by `"*".repeat(2*N)` (or deeply nested
  `[[[[...]]]]`) nests N frames.
- `parse_markdown` (`:339`) recurses on each blockquote level (`:420`),
  so `"> ".repeat(N)` nests N frames.
- `render_inline_tokens` (`:1847`) and `render_list` (`:1693`,`:1745`)
  then recurse over the same AST depth at render time.
The component renders **model-emitted** markdown — i.e. untrusted text
whose nesting depth an adversarial or merely buggy model controls.
A sufficiently deep input overflows the thread stack and aborts the
process (Rust stack overflow is not catchable). T2 already documented a
deliberate single-grapheme base case in `word_wrap.rs` that fixed a real
stack overflow, so the layer is aware of the hazard class; the markdown
parser has no equivalent guard.
**Why it matters:** Contracts / availability. The rest of the crate is
careful about degenerate input (saturating subtractions, `.max(1)`
clamps, total encoders); an uncatchable abort on a long run of `*` or
`>` is the one place a single message can take the whole TUI down. It's
also untested — the in-module suite and the (skimmed) `tests/markdown.rs`
exercise correctness, not adversarial depth.
**Suggested action:** Cap nesting depth in the parser: thread a `depth`
counter through `parse_inline` / `parse_markdown` (and the matching
render recursions) and, past a generous limit (e.g. 32–64), stop
descending and treat the remainder as literal text. Add a regression
test that feeds `"*".repeat(100_000)` / `"> ".repeat(100_000)` and
asserts it returns rather than aborts. Workshop the exact limit with the
user.
**Effort:** M

### [Major][Simplicity] `SelectList` and `SettingsList` are sibling re-implementations of one scroll/filter/render widget that have drifted apart — `src/aj-tui/src/components/select_list.rs:228,673,720`, `src/aj-tui/src/components/settings_list.rs:158,400,532`
**What:** The two list components independently re-implement the same
core machinery with no shared abstraction:
- **Scroll window:** both compute `start = clamp(selected -
  max_visible/2, 0, len - max_visible)` and render `start..end`
  (`select_list.rs:458`/`:690` vs `settings_list.rs:447-450`), then both
  emit a `"  (N/TOTAL)"` scroll indicator truncated to `width-2`
  (`select_list.rs:711` vs `settings_list.rs:484`).
- **Navigation:** both have an up/down handler that wraps at the ends
  (`move_selection` vs `move_up`/`move_down`) bound to the same
  `tui.select.up`/`down`/`confirm`/`cancel` keys
  (`select_list.rs:720` vs `settings_list.rs:532`). `SelectList` also
  honors `tui.select.pageUp`/`pageDown`; `SettingsList` does not.
- **Filter:** both keep a `filtered: Vec<usize>` index projection and
  run `fuzzy::fuzzy_filter` over `(index, text)` pairs and reset
  `selected = 0` (`select_list.rs:297` vs `settings_list.rs:357`).
The two have *diverged*, which is the cost of the duplication:
`SelectList` offers a `SubstringAllTokens` mode, configurable
`wrap_selection`, prefix/shortcut/description columns, and
`select.pageUp/Down`; `SettingsList` hardcodes wrapping, has no
substring mode, no page keys, and folds search into an embedded
`TextInput`. A bugfix or keybinding to one will not reach the other.
**Why it matters:** Simplicity / duplication. This is the
sibling-duplication theme T1 flagged for the core (overlay-routing
duplicated 4x), here at the component layer. The shared skeleton
(index-projection + centered scroll window + wrap-navigation + scroll
indicator) is a real abstraction begging to be extracted; the drift
already means the two widgets feel inconsistent to a user (page keys
work in one list but not the other).
**Suggested action:** Workshop with the user whether to extract a
shared `ScrollableList` helper (owning `selected`, `filtered`, the
centered-window computation, the wrap-navigation, and the
scroll-indicator row) that both components compose, leaving each to own
only its per-row rendering and activation semantics. At minimum, unify
the navigation key set (give `SettingsList` page keys) so the two don't
diverge on input behavior. No behavior change is forced if the user
prefers to keep them separate — but the divergence should be a recorded
decision.
**Effort:** L

### [Minor][Simplicity] `TAB_AS_SPACES = "   "` is duplicated across `text.rs` and `markdown.rs` (and `ansi.rs` hardcodes tab=3) — `src/aj-tui/src/components/markdown.rs:32`, `src/aj-tui/src/components/text.rs:15`
**What:** Both `Text` (`text.rs:15`) and `Markdown` (`markdown.rs:32`)
define a private `const TAB_AS_SPACES: &str = "   "` and call
`text.replace('\t', TAB_AS_SPACES)` before wrapping. `markdown.rs`'s doc
even notes the choice "matches the `Text` component's `TAB_AS_SPACES`
constant" — an invariant enforced by a comment, not by a shared
definition. T2 separately flagged that `ansi.rs`'s `visible_width`
hardcodes tab = 3 columns in three string literals. So "a tab is three
columns" is now expressed in at least four places across the crate, none
of them authoritative.
**Why it matters:** Simplicity / invariant-by-convention. If anyone
changes the tab width in one place the others silently disagree, and the
width math (`ansi.rs`) and the pre-normalization (`text.rs`/`markdown.rs`)
would diverge — a tab would be normalized to N spaces but measured as 3.
The cross-component comment ("matches the Text component's constant") is
exactly the chronology/convention smell the guidance discourages.
**Suggested action:** Lift one `const TAB_WIDTH: usize = 3` (and/or
`TAB_AS_SPACES`) into `ansi` (where the width authority lives, per T2)
and have both components reference it. Ties into T2's suggestion to name
the tab-width constant.
**Effort:** S

### [Minor][Boundaries] `CancellableLoader::handle_input` ignores its own `focused` field; the "while focused" contract is enforced only by the Tui router — `src/aj-tui/src/components/cancellable_loader.rs:46,145`
**What:** `CancellableLoader` tracks a `focused: bool` (`:46`, set via
`set_focused`/read via `is_focused`), and the doc on `set_on_abort`
(`:93`) and the module doc (`:27`) both say the cancel key fires "while
the loader is focused". But `handle_input` (`:145`) matches
`tui.select.cancel` and cancels **without checking `self.focused`**. The
contract holds today only because `Tui::handle_input` routes input
exclusively to the focused component (verified in `tui.rs:2067`), so the
field is effectively dead for gating purposes — `set_focused` stores a
value that `handle_input` never consults.
**Why it matters:** Boundaries / leaky-abstraction. The "only-if-focused"
behavior is a property of the *router*, not the component, yet the
component carries focus state and documents the gate as if it enforced
it. A future caller that dispatches events to an unfocused
`CancellableLoader` (e.g. a broadcast, or a different container) would
cancel unexpectedly. The tracked-but-unused field invites the reader to
believe a guard exists.
**Suggested action:** Either gate the cancel on `self.focused` in
`handle_input` (making the component self-enforcing and the field
load-bearing), or drop the `focused` field and `set_focused`/`is_focused`
overrides entirely and document that focus routing is the caller/router's
responsibility. Confirm with the user which contract is intended.
**Effort:** S

### [Minor][Simplicity] `Text` and `Markdown` duplicate the same cache + row-emission + padding pipeline — `src/aj-tui/src/components/text.rs:86`, `src/aj-tui/src/components/markdown.rs:1939`
**What:** `Text::render` and `Markdown::render` carry byte-for-byte the
same scaffolding: the `(cached_text, cached_width, cached_lines)` triple
+ `invalidate_cache`, the `content_width = width.saturating_sub(2 *
padding_x).max(1)` clamp, the per-row
`left_margin + line + right_margin` → `apply_background_to_line`-or-pad
loop, the `padding_y` top/bottom blank-row emission, and the identical
"cache the pre-fallback result, then return `vec![""]` if empty"
tail-fallback comment block (`text.rs:157-177` vs
`markdown.rs:2096-2117`). The two diverge only in what produces the
inner `content_lines` (word-wrap vs. block render).
**Why it matters:** Simplicity / duplication. The shared row-emission +
caching + padding envelope is a real seam; keeping two hand-copied
copies means a fix to one (e.g. the unreachable `vec![""]` fallback, or
the bg-padding interaction) must be remembered in both, and the
near-identical 20-line comment blocks are the kind of copy-paste that
rots. Lower severity than the list duplication because the two are
genuinely small and the divergent core is most of the work.
**Suggested action:** Optionally factor the cache+padding+row-emission
envelope into a small helper (e.g. a `render_padded_block(lines, width,
padding_x, padding_y, bg_fn)` free function plus a tiny cache struct)
that both call with their respective inner-line producers. Confirm
whether the user considers the duplication acceptable given the small
size; if so, record it.
**Effort:** M

### [Minor][Contracts] Kitty JPEG falls back to text with an inline `TODO: transcode to PNG`; no sixel path — image support is two-protocol, document it as a contract — `src/aj-tui/src/components/image.rs:160`, `src/aj-tui/src/capabilities.rs:27`
**What:** When the protocol is Kitty but the mime is not `image/png`,
`render_uncached` returns the text fallback with the comment "Kitty
doesn't accept JPEG inline; fall back... TODO: transcode to PNG"
(`image.rs:160-164`). Separately, the `ImageProtocol` enum has only
`Kitty` and `ITerm2` (`capabilities.rs:27`) — there is no sixel encoder,
so sixel-only terminals always get the text fallback. Both are
reasonable current scopes, but the JPEG limitation is recorded as an
inline `TODO` (a contract-stability smell, the same shape T1 flagged for
`cell_pixel_size`) rather than as a stated contract, and the
"no-sixel" decision is implicit.
**Why it matters:** Contracts. A caller can't tell from the `Image` API
that a JPEG payload silently degrades to text under Kitty, or that sixel
terminals never render inline. These are honest limitations, but they're
the kind that should be a documented contract on `Image::new` (which
mimes render inline under which protocol) rather than discovered from a
`TODO`.
**Suggested action:** Promote the JPEG-transcode `TODO` to a tracked
task and reword the comment as a current contract ("Kitty accepts only
PNG inline; non-PNG payloads render as the text fallback"). Add a
sentence to `Image`'s type doc enumerating the (mime × protocol) matrix
and the sixel gap. No behavior change.
**Effort:** S

### [Minor][Testing] No test pins `select_list`/`settings_list` scroll-window math at the list extremes, nor `OverlayWindow` degenerate-width behavior — `src/aj-tui/src/components/select_list.rs:458`, `src/aj-tui/src/components/overlay_window.rs:229`
**What:** The in-module suites are strong on filtering, selection, and
column layout, but two boundary behaviors are unpinned:
- `visible_window_start` (`select_list.rs:458`) and the equivalent in
  `settings_list.rs:447` clamp the centered window at both ends; the
  tests assert navigation wrap/clamp on `selected` but never assert that
  with `len > max_visible` and `selected` at `0` / `len-1` the rendered
  window actually shows the top / bottom slice (the
  `selected.saturating_sub(half).min(upper_bound)` arithmetic). A regression
  in the clamp would not fail a test.
- `OverlayWindow::render` has degenerate-width branches (`width < 4`
  returns `vec![""]` at `:229`; `render_edge` has `width < 2` and
  `interior < 5` arms at `:162`/`:168`) that no test exercises — the
  suite renders only at widths 20/40.
**Why it matters:** Testing at the boundary. These are exactly the
edge/clamp paths the rubric asks for; both are pure functions that are
cheap to pin, and both are the sort of off-by-one-prone code that
benefits from a guard.
**Suggested action:** Add unit tests: a `select_list`/`settings_list`
case with `len=10, max_visible=3` asserting the visible slice at
`selected=0`, mid, and `len-1`; and an `OverlayWindow` case at
`width=3`/`width=6` asserting the degenerate output is well-formed.
**Effort:** S

### [Nit][Comments] Chronology / cross-component framing in `markdown.rs` and `settings_list.rs` comments — `src/aj-tui/src/components/markdown.rs:226`, `src/aj-tui/src/components/settings_list.rs:584`
**What:** A few comments narrate history or reference sibling code as if
the reader can see the change: `markdown.rs:226` "The trim ran on
`String::is_empty` *before* the row-emission rewrite that pads every
row..."; `settings_list.rs:584-594` "A `if handled` gate *would
therefore be* functionally correct today, but *would be* structurally
fragile: a future TextInput change that adds a return-false-but-mutate
path would silently break filter updates" — a multi-paragraph rationale
that audits a *different* component's (`TextInput`'s) current behavior
inline. Both stand in for steady-state contracts.
**Why it matters:** Comments/documentation — the chronology theme
T1/T2/AG1/TO2 keep hitting. The `markdown.rs` one references a prior
implementation; the `settings_list.rs` one ties this component's
correctness to an audit of `TextInput`'s internal branches that will
rot if `TextInput` changes.
**Suggested action:** Restate `markdown.rs:226` as "padded/`bg_fn`-tinted
rows of spaces still count as blank (ANSI-aware via `is_blank_row`)".
Shorten the `settings_list.rs` block to the steady-state rule: "filter
recompute is unconditional after forwarding to the search input — it
must not depend on whether `TextInput` reported the event handled" and
drop the speculative future-PR framing.
**Effort:** S

### [Nit][Boundaries] `Image` implements both `impl_component_any!()` and a redundant hand-written `AsRef<dyn Any>` — `src/aj-tui/src/components/image.rs:121,199`
**What:** `Image` invokes `crate::impl_component_any!()` inside its
`Component` impl (`:121`, which supplies `as_any`/`as_any_mut`) **and**
separately hand-writes `impl AsRef<dyn Any> for Image` (`:199-203`). No
other component in the scope writes the `AsRef<dyn Any>` impl, and a
workspace search shows no caller relies on `Image: AsRef<dyn Any>` (the
container downcast path uses `as_any`/`as_any_mut` from the macro).
**Why it matters:** Boundaries / dead surface — minor. It's an extra
public trait impl that duplicates what the macro already provides and
that nothing consumes, inconsistent with every sibling component.
**Suggested action:** Remove the `impl AsRef<dyn Any> for Image` block
unless a caller needs it (none found). Confirm with `cargo check`.
**Effort:** S

### [Nit][Simplicity] `split_hard_break` returns a `had_marker` flag every caller ignores — `src/aj-tui/src/components/markdown.rs:528`
**What:** `split_hard_break` returns `(&str, bool)`; the only caller,
`join_paragraph_lines` (`:511`), binds the bool as `_hard_break` and
discards it. The doc (`:524-527`) explains the flag is "preserved on the
return for callers who care about the distinction; current callers
ignore it" — i.e. it's speculative surface for a hypothetical future
caller.
**Why it matters:** Simplicity / dead parameter. A return value no caller
uses is the inverse of YAGNI; the function could return `&str`. Trivial.
**Suggested action:** Change the signature to return `&str` and drop the
flag (and the "callers who care" doc sentence), or note explicitly why
the flag is retained. No behavior change.
**Effort:** S

### [Nit][Contracts] `select_list`/`settings_list` default `empty_message`/hint text is product-specific ("No matching commands", "Esc to cancel") in a palette-agnostic crate — `src/aj-tui/src/components/select_list.rs:196`, `src/aj-tui/src/components/settings_list.rs:379`
**What:** `SelectListLayout::default` sets `empty_message: "No matching
commands"` (`:196`) and `SettingsList::hint_text` hardcodes
`"Type to search · Enter/Space to change · Esc to cancel"` (`:379-384`).
The crate's stated design (echoed throughout these files' theme docs)
is "the tui crate stays palette-agnostic; the agent layer supplies the
strings". A default referring to "commands" and baking in key-hint copy
is product/keybinding-specific content embedded in a generic framework
leaf.
**Why it matters:** Boundaries/contracts — nit. `empty_message` is at
least overridable; the `SettingsList` hint is not (no setter), so the
copy and even the "Esc"/"Space" key names are fixed in the framework.
If a host rebinds cancel away from Esc the hint lies.
**Suggested action:** Make the `SettingsList` hint a theme/options field
(like `SelectList`'s `empty_message`) so the host supplies copy
consistent with its keybindings, and consider a less product-specific
default for `empty_message` ("No matches"). Workshop with the user.
**Effort:** S

## What's good

- **Uniform `Component` implementation across every widget.** All eleven
  components implement the same minimal trait, each starting with
  `crate::impl_component_any!()`, overriding only the methods they need,
  and relying on the trait's sensible defaults (`handle_input → false`,
  `invalidate → {}`, `set_available_height → {}`). Nobody reinvents
  layout, event dispatch, or focus plumbing — `Spacer` is four lines,
  `TruncatedText` is a pure `render`, and the interactive widgets add
  exactly the handlers they need. This is the consistency the rubric
  asks for.
- **All width/wrap/truncation delegates to the T2 authority.** Every
  component routes through `ansi::{wrap_text_with_ansi,
  truncate_to_width, visible_width, apply_background_to_line}`.
  `truncated_text.rs` is **not** a fourth truncation impl — it's a thin
  single-line wrapper over `truncate_to_width` (the M2/TO1/T2-authoritative
  display-width truncation). The "multiple truncation impls" theme is
  REFUTED for this layer.
- **Image protocol code is total over arbitrary bytes — no panic on
  malformed input.** `kitty_sequence`/`iterm2_sequence` treat the
  payload as opaque bytes; `extract_kitty_image_ids` and
  `parse_kitty_image_id` walk bytes with bounds-checked indices and
  `from_utf8(...).ok()?`/`parse().ok()?` (never `unwrap`);
  `image_cell_footprint` guards every zero divisor and clamps to `(1,1)`.
  The explicitly-Major-by-rubric "panic on malformed image data" case is
  handled correctly. The cache key includes the protocol so a
  test-driven `set_capabilities` flip can't return a stale escape.
- **Capability gating is clean and ties to T1.** `Image::render` reads
  `get_capabilities().images` at render time and falls back to a muted
  `[image: …]` text row when `None`; OSC-8 hyperlink emission in
  `markdown.rs:1805` reads `get_capabilities().hyperlinks` inline. No
  per-component capability flags; the gate lives in the central cache, as
  T1's capabilities design intends.
- **`OverlayWindow` is a clean compositor citizen.** It adds no input
  handling of its own and forwards `handle_input` / `wants_key_release` /
  `invalidate` / `set_focused` / `is_focused` straight to the child;
  pushes the resolved `inner_rows` via `set_available_height` *before*
  rendering; and stabilizes child output to a fixed height so the overlay
  doesn't jitter. It does NOT duplicate the core's overlay-routing/focus
  state machine (the thing T1 flagged as 4x-duplicated) — that stays in
  `Tui`; this is purely the visual frame. Its `InnerHeight::Dynamic`
  policy injection keeps height rules out of the framework.
- **Markdown inline-style propagation is subtle and correct.** The
  `InlineStyleContext` "re-open the outer style's opens after every
  non-text inline" mechanism (via the `get_style_prefix` NUL-sentinel
  trick) correctly prevents an inline code's `\x1b[39m` from stripping a
  heading's color from trailing text, and `apply_quote_style` re-opens
  quote styling after each `\x1b[0m` so syntect-highlighted code inside a
  blockquote keeps its border styling. These are real terminal-rendering
  hazards handled deliberately and explained at the point they matter.
- **Loader animation lifecycle is disciplined.** The animation pump is a
  cancellation-token-gated tokio task spawned only when the indicator
  actually animates (`frames.len() > 1` and active), gracefully no-ops
  when there's no runtime (standalone/tests), and is cancelled from both
  `stop()` and `Drop`. `current_frame` does its modulo before the
  `usize` conversion so the index can't overflow. `CancellableLoader`
  correctly forwards `invalidate` so the embedded `Text` cache clears on
  resize.
- **Table column distribution is well-tested and bounded.**
  `distribute_column_widths` (a pure free function) has a thoughtful
  two-stage min/natural allocation with a `MAX_UNBROKEN_TOKEN_WIDTH` cap
  so one overlong token can't starve neighbours, and the in-module suite
  pins the fits / shrinks / collapses / capped-floor cases including the
  "never exceeds budget" invariant.

## Boundary & architecture notes

Dependency direction is correct and matches `CLAUDE.md`'s standalone-tui
intent: these components import only `crate::` siblings (`ansi`,
`component`, `capabilities`, `keybindings`, `keys`, `tui`, `style`,
`fuzzy`, `image_protocol`, `components::text`/`text_input`) plus leaf
external crates (`syntect`, `base64`, `tokio_util`, `unicode_width`,
`crossterm` types via re-export). No `aj_agent` / `aj_models` /
`aj_session` / `aj_conf` import appears anywhere. The themes are passed
in as `Arc<dyn Fn(&str)->String>` closures with **no `Default` impl** —
a deliberate, well-documented choice that keeps the crate
palette-agnostic and is consistent across `SelectListTheme`,
`SettingsListTheme`, `MarkdownTheme`, and `OverlayWindowTheme`.

Public-surface items for synthesis: (1) `next_kitty_image_id`,
`kitty_sequence`, `kitty_delete`, `kitty_delete_all`, `iterm2_sequence`,
`is_image_line`, `extract_kitty_image_ids`, `image_cell_footprint`,
`DEFAULT_CELL_PIXEL_SIZE` are all `pub` in `image_protocol` and are
genuinely consumed by `tui.rs`'s render engine (the diff engine needs
`extract_kitty_image_ids`/`kitty_delete*`), so the surface is justified —
unlike T2's unused `style.rs` palette. (2) `Loader::current_frame` and
`SettingsList::has_active_submenu` are `pub` "exposed for tests" escape
hatches; fold into the workspace test-only-visibility decision noted in
T1. (3) The product-specific default strings (last finding) are the one
place product content leaks into the generic crate.

## Test assessment

The in-module suites are boundary-focused and high quality, not
implementation-ossifying. `select_list.rs` covers both filter modes,
extend-while-filtered re-ranking with selection restore, prefix-column
stability across filtering, shortcut-over-description precedence,
selected-row-keeps-prefix-dim (a genuine ANSI-nesting regression),
wrap-vs-clamp navigation, and column-bounds reordering — real edge
coverage. `image.rs` is exemplary: it drives every (protocol × mime)
combination through `set_capabilities` (serialized with `serial_test`
because the cache is process-global, matching T1's note), asserts the
multi-row escape placement contract for both Kitty (escape on row 0) and
iTerm2 (escape on last row with the `\x1b[N-1A` cursor-up prefix), and
pins cache invalidation on width/protocol/filename change.
`image_protocol.rs` pins chunking, the cursor-up-only-when-rows>1 rule,
base64 name encoding, and footprint aspect-ratio/clamp math.
`overlay_window.rs` uses a `MockChild` to verify delegation of
input/focus/available-height and the stable-height padding/truncation.
`markdown.rs` pins the cache lifecycle and the table-distribution math.

Gaps (mostly for T5 confirmation, captured in the Testing finding):
adversarial markdown nesting depth (the Major), the scroll-window slice
at list extremes, and `OverlayWindow` degenerate widths. `loader.rs` and
`cancellable_loader.rs` have **no in-module tests** — the animation pump
and the cancel-key path are presumably covered in `tests/` (T5 scope);
worth confirming the cancel path is tested given the focus-gating finding.
No flakiness risk in the in-scope unit tests beyond the deliberate
`serial_test` serialization of the global capability cache.

## Cross-cutting themes to bubble up

- **Unbounded recursion on untrusted input (NEW, Major).** The markdown
  parser/renderer recurse with no depth cap on model-emitted text — the
  one uncatchable-abort hazard in the scope. Distinct from T2's
  *fixed* recursion guard in `word_wrap.rs`; synthesis should check
  whether other parsers in the workspace (e.g. `partial_json` in M2) cap
  depth.
- **Sibling-component duplication (CONFIRMED, Major).** `select_list`
  vs `settings_list` re-implement one scroll/filter/render widget and
  have drifted (page keys, filter modes, wrap config). This is the
  component-layer instance of T1's "overlay-routing duplicated 4x"
  theme. `text` vs `markdown` (render envelope) and `text` vs
  `truncated_text` (the latter is fine — a genuine thin wrapper) are
  milder instances.
- **Multiple width/truncation impls (REFUTED for this layer).** Every
  component delegates to `ansi::*`; `truncated_text.rs` is a wrapper, not
  a fourth impl. The "tab = 3 columns" *constant*, however, is duplicated
  four ways (Minor finding) — that's a shared-constant gap, not a
  separate algorithm.
- **Chronology / cross-component framing in comments (CONFIRMED).**
  `markdown.rs` "before the row-emission rewrite" and `settings_list.rs`'s
  inline audit of `TextInput`'s branches — same family as T1/T2/AG1/TO2.
- **Inline `TODO` as contract (CONFIRMED, mild).** `image.rs`'s "TODO:
  transcode JPEG to PNG" mirrors T1's `cell_pixel_size` TODO — a
  documented limitation that should be a stated contract + tracked task.
- **Test-only `pub` surface (CONFIRMED, mild).** `Loader::current_frame`,
  `SettingsList::has_active_submenu` "exposed for tests"; fold into the
  workspace decision. `image_protocol`'s broad `pub` surface is, by
  contrast, genuinely consumed by the render engine — justified.
- **Perf hot-path (REFUTED, consistent with T1/T2).** `Text`, `Markdown`,
  and `Image` all memoize their rendered output keyed on `(text, width
  [, protocol])`, so the per-frame cost on an unchanged component is a
  `Vec<String>` clone, not a re-parse/re-encode. This layer is a third
  positive example for the perf theme.
- **What held up clean (worth replicating):** the uniform minimal
  `Component` impls, total/panic-free image encoders, capability-gated
  image+hyperlink rendering tied to the central cache, `OverlayWindow`'s
  pure-visual delegation (not duplicating core overlay routing), and the
  no-`Default`-on-theme palette-agnostic discipline.
