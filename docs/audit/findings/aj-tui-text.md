# Audit findings — aj-tui-text

- **Step:** T2
- **Date:** 2026-06-02
- **Audited commit:** 9142a6c
- **Scope:** `src/aj-tui/src/ansi.rs`, `src/aj-tui/src/word_wrap.rs`,
  `src/aj-tui/src/word_boundary.rs`, `src/aj-tui/src/style.rs`,
  `src/aj-tui/src/fuzzy.rs` (incl. their in-module `#[cfg(test)]` suites).
  The dedicated integration suites under `tests/` (`word_wrap.rs`,
  `wrap_ansi.rs`, `fuzzy.rs`, `truncate_to_width.rs`,
  `regression_regional_indicator_width.rs`) were skimmed for boundary
  coverage; their full audit is T5.

## Summary

The text/layout layer of `aj-tui` is the strongest unit seen so far on the
two dimensions that matter most for it: Unicode correctness and ANSI handling.
Width math is uniformly grapheme-cluster-based and delegates to `unicode-width`
0.2 (so default-text vs default-emoji presentation, VS16 promotion, ZWJ
sequences, and combining marks are all handled correctly), with a single
well-documented and well-tested regional-indicator override guarding streaming
flag-glyph drift. The ANSI machinery never splits an escape sequence across a
wrap point — every scanning loop extracts whole codes via `extract_ansi_code`
and `AnsiStyleTracker` re-emits active SGR + OSC-8 state at each line start —
and the OSC-8 BEL-vs-ST terminator preservation is a genuinely subtle bit of
correctness that is both implemented and tested. The often-feared O(n²) hazard
(`extract_ansi_code` called per byte while scanning plain-text extents) is in
fact linear, because that function returns in O(1) on any non-ESC byte; the
hot-path allocation story is also careful (ASCII fast paths, a thread-local
width memo, in-place Thai/Lao normalization). `fuzzy.rs` is a clean,
self-contained standalone util.

The boundary holds and there are no correctness defects. Findings are localized:
a chunk of `style.rs`'s color palette is unused public surface (defensible for a
generic framework, but worth confirming), the word-wrap tokenizer splits on the
ASCII space only while classifying tokens with Unicode whitespace (a minor
tab-as-wrap-opportunity fidelity gap), and a handful of contract/comment nits.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 0 | 4 | 4 |

## Findings

### [Minor][Simplicity] Word-wrap tokenizer splits on ASCII space only, but classifies tokens with Unicode whitespace — tabs/NBSP are never wrap opportunities — `src/aj-tui/src/ansi.rs:1097,1257`

**What:** `split_into_tokens_with_ansi` decides token boundaries with
`let char_is_space = ch == ' ';` (`:1097`) — only U+0020. A tab (`\t`,
width 3 per `visible_width`), a non-breaking space (U+00A0), or any other
Unicode whitespace is folded into the adjacent non-whitespace token. Then
`wrap_single_line` reclassifies each token with `token.trim().is_empty()`
(`:1257`), which *is* Unicode-aware. The two disagree: a run like `"\t"` or
`"a\u{00a0}b"` is emitted as a single non-whitespace token, so the wrapper
never records a wrap opportunity at it and treats it as an unbreakable word
(falling back to mid-word `break_long_word` only if it overflows). By
contrast `word_wrap.rs`'s `is_segment_whitespace` and the `word_boundary.rs`
helpers all use `char::is_whitespace`, so the two wrap implementations and the
cursor-motion helpers classify whitespace differently.
**Why it matters:** Simplicity / correctness-of-layout (minor). For the
common case (text wrapped on spaces) it's invisible, but tab-indented or
NBSP-containing content wraps less gracefully than the Unicode-aware
classifier the same function then applies implies it should. The split rule
and the classify rule being two different notions of "whitespace" inside one
function is the kind of invariant-by-convention that drifts.
**Suggested action:** Make the tokenizer split on `ch.is_whitespace()`
(matching the `token.trim().is_empty()` classifier and the rest of the layer),
or document explicitly why space-only splitting is the intended wrap policy for
this output-oriented wrapper. Note that `\t` already gets width-3 handling in
`visible_width`/`break_long_word`, so only the *break-opportunity* decision is
affected.
**Effort:** S

### [Minor][Boundaries] Much of `style.rs`'s color palette is unused public surface — `src/aj-tui/src/style.rs:31,46,51,56,61,88,98,36,41`

**What:** A workspace-wide search shows several `style.rs` helpers have **no**
caller anywhere (production, tests, or examples): `inverse` (`:31`, one stray
reference, effectively unused), `fg256` (`:46`), `bg256` (`:51`), `fg_rgb`
(`:56`), `bg_rgb` (`:61`), `magenta` (`:88`), `white` (`:98`), and the generic
`fg`/`bg` entry points (`:36`,`:41`) — every real caller goes through a named
convenience (`dim`, `cyan`, `bold`, `gray`, …) or through `aj`'s own
`theme.fg/bg`. The actually-used set is roughly `bold`/`dim`/`italic`/
`underline`/`strikethrough`/`red`/`green`/`yellow`/`blue`/`cyan`/`gray`.
**Why it matters:** Boundaries / minimal public surface. For a *generic* UI
framework a complete-looking palette (256-color, RGB, every base color) is a
defensible API-completeness choice — unlike the dead `Terminal` trait methods
flagged in T1, these are leaf helpers, not a portability seam, and the cost of
keeping them is near zero. But several (256-color/RGB constructors, the generic
`fg`/`bg` taking a raw SGR code) leak the SGR-code-as-`u8` abstraction to
callers and have never been exercised, so they're untested public surface.
**Suggested action:** Confirm with the user whether `style.rs` is intended as a
"complete palette" public API. If yes, leave it and consider a doc note that it
is a convenience surface (some entries currently unused internally); if no,
trim the unused RGB/256/`fg`/`bg`/`inverse`/`magenta`/`white` helpers. Either
way, the unused ones lack any test, so if kept they warrant at least a smoke
test of the emitted SGR bytes.
**Effort:** S

### [Minor][Contracts] `visible_width` documents tab = 3 columns but the width contract (and where it's authoritative) isn't stated on `grapheme_width` — `src/aj-tui/src/ansi.rs:498,682,725`

**What:** Two functions compute width with different tab semantics:
`grapheme_width` (`:515`) returns 0 for a control char (so `\t` → 0), while
`visible_width` (`:695`) expands `\t` to 3 columns (`:725`). `break_long_word`
deliberately routes single graphemes through `visible_width` rather than
`grapheme_width` precisely to pick up the tab-3 expansion (the inline comment at
`:1168-1175` explains this well). The "tab = 3" magic number is a real layout
contract shared with the renderer, but it lives only in the `visible_width` doc
list (`:686`) and is hardcoded in three string-literal sites (`"   "` /
`+= 3` / `kept_width + 3`); there's no named constant and `grapheme_width`'s doc
doesn't note that it intentionally does *not* expand tabs (so a caller measuring
a lone `\t` via `grapheme_width` silently gets 0).
**Why it matters:** Contracts. The two functions form the layer's width
vocabulary; the difference (`grapheme_width` is per-cluster and tab-naive,
`visible_width` is whole-string and tab-aware) is load-bearing for wrapping but
documented only obliquely. A future caller picking `grapheme_width` for a string
that contains a tab will under-count.
**Suggested action:** Add one sentence to `grapheme_width`'s doc — "control
chars (including `\t`) are width 0 here; use `visible_width` if tabs should
expand" — and consider a `const TAB_WIDTH: usize = 3;` so the three literals and
the doc share one source of truth.
**Effort:** S

### [Minor][Testing] No in-module / boundary test pins the "escape sequence is never split across a wrap point" invariant for `break_long_word` — `src/aj-tui/src/ansi.rs:1125`

**What:** The headline ANSI-wrap invariant — a multi-byte CSI/OSC code is never
cut in half when a long word is force-broken — is the most important correctness
property of this layer. `break_long_word` enforces it by segmenting into
`Ansi`/`Grapheme` units first, but the in-module suite tests `visible_width`,
`truncate`, slicing, and the tracker; the only force-break-with-ANSI coverage is
in `tests/wrap_ansi.rs` (skimmed; it covers underline/hyperlink preservation and
an embedded-tab break, but I did not see an assertion that re-parses every
output line and checks no `extract_ansi_code` straddles a chunk boundary). The
property is currently load-bearing-by-construction, not pinned by an assertion.
**Why it matters:** Testing at the boundary. A refactor of the segment loop (or
a switch to byte-windowed scanning) could reintroduce a mid-escape split and no
test would fail. This is exactly the class of regression the layer most needs a
guard for.
**Suggested action:** Add a property-style test (in `tests/wrap_ansi.rs`, T5
scope): for a styled string force-broken at several narrow widths, assert that
feeding each output line back through `extract_ansi_code` reproduces only whole
codes and that the concatenated visible content round-trips. Confirm during T5
whether an equivalent already exists.
**Effort:** S

### [Nit][Comments] `grapheme_width` doc pins the behavior to "unicode-width 0.2" — version chronology in a contract — `src/aj-tui/src/ansi.rs:500,1856`

**What:** The `grapheme_width` doc says "Delegates to `UnicodeWidthStr::width`,
which (as of unicode-width 0.2) encodes the default emoji-presentation rule…"
and the regression test comment (`:1856`) similarly says "Delegating to
`unicode-width` 0.2 encodes that rule directly." Tying the documented contract to
a specific dependency minor version is a mild chronology smell: if the pin moves,
the prose silently goes stale, and the *contract* (text-presentation chars are
width 1, emoji-presentation width 2, VS16 promotes) is what matters, not which
version supplies it.
**Why it matters:** Comments/documentation — the chronology theme T1/AG1/TO2
flagged, in a softer form (it's a dependency version, not "used to"). The
substantive width contract is correct and well-explained; only the version
framing dates it.
**Suggested action:** State the contract as the steady-state behavior the layer
*relies on* ("we rely on `unicode-width`'s default-presentation rule: …") and
drop the "as of 0.2" qualifier, or move the version note to a single place
(e.g. a comment by the dependency in `Cargo.toml`).
**Effort:** S

### [Nit][Simplicity] `get_active_codes` allocates a `Vec<&str>` then a second `Vec<String>` of clones to build one SGR string — `src/aj-tui/src/ansi.rs:347`

**What:** `get_active_codes` first pushes attribute codes into
`Vec<&str>` (`:348`), then maps that into a fresh `Vec<String>` via
`.map(|s| s.to_string())` (`:374`) just so the owned `fg_color`/`bg_color`
clones can be appended before `join(";")`. Two vectors and N short-string
allocations per call. It's called once per wrapped continuation line
(`wrap_single_line` / `break_long_word`), so it is on the wrap path, though not
the per-frame diff path.
**Why it matters:** Simplicity / minor allocation. Not a hot-path defect (wrap
runs on content change, not every frame), but the double-vector dance is more
machinery than the task needs and reads awkwardly.
**Suggested action:** Build directly into a single `String` with
`write!`/`push_str` and a manual `;` separator, or push `&str` then the owned
colors into one `Vec<&str>` referencing borrowed slices. Optional cleanup; no
behavior change.
**Effort:** S

### [Nit][Contracts] `fuzzy` exact-match bonus is ASCII-case-insensitive only; non-ASCII exact matches don't get the boost — `src/aj-tui/src/fuzzy.rs:79`

**What:** `score` adds `EXACT_MATCH_BONUS` when
`query.eq_ignore_ascii_case(text)` (`:79`). For a non-ASCII exact match (e.g.
query `"café"` vs text `"Café"` differing only in a non-ASCII case fold, or even
identical non-ASCII strings that happen to compare equal — those still match,
fine) the *case-folded* boost silently degrades to ASCII rules. The doc does say
"case-insensitive (ASCII)", so it's documented; flagging because autocomplete/
selector content (model names, file paths) is mostly ASCII today but the rule is
a quiet limitation.
**Why it matters:** Contracts — the limitation is honestly documented, so this is
a nit. Worth noting that an exact non-ASCII match still scores (it just doesn't
get the tiebreak bonus over a longer partial match), so ranking degrades
gracefully rather than breaking.
**Suggested action:** None required. If non-ASCII exact-match ranking ever
matters, switch the comparison to a Unicode-aware case fold; otherwise keep the
documented ASCII rule.
**Effort:** S

### [Nit][Simplicity] `is_punctuation_grapheme` and the editor's three-class model only recognize ASCII punctuation — `src/aj-tui/src/ansi.rs:639`, `src/aj-tui/src/word_boundary.rs:1`

**What:** `is_punctuation_char` (`:646`) hardcodes an ASCII-only punctuation bag.
Unicode punctuation (curly quotes `“ ” ‘ ’`, em/en dashes `— –`, ellipsis `…`,
CJK punctuation) classifies as "word", so word-motion (`word_boundary.rs`) and
word-wrap break decisions treat `don’t`/`foo—bar` as single words. The
`word_boundary.rs` module doc (`:6-7`) explicitly states the three-class model is
ASCII-punctuation + whitespace + "everything else", so it's a documented design
choice.
**Why it matters:** Simplicity / minor behavioral limitation. Matches readline
defaults closely enough and is documented, so it's a nit, not a defect. Curly
quotes and em-dashes are common in pasted prose, so Alt-arrow word motion will
occasionally feel coarse.
**Suggested action:** None required given the documented scope. If desired,
broaden to `char::is_ascii_punctuation` plus a small Unicode-punctuation set, but
that risks surprising editor behavior — workshop with the user before changing.
**Effort:** S

## What's good

- **Unicode width is correct and grapheme-based throughout.** Every width
  computation flows through `grapheme_width` → `unicode-width` 0.2, so
  default-text-presentation chars (`☑ ❤ ✓`) are width 1, default-emoji chars
  (`✅ ⚡ 👍`) are width 2, VS16 promotes (`❤` 1 → `❤️` 2), and ZWJ/skin-tone
  sequences stay width 2. The in-module tests
  (`grapheme_width_text_presentation_chars_are_one_cell`,
  `…_emoji_default_…`, `…_vs16_promotes_…`) pin all three rules. The single
  override (regional indicators forced to 2, `:529-535`) is documented with its
  *why* (streaming flag-glyph drift) and guarded by a dedicated regression file
  that sweeps the whole `U+1F1E6..=U+1F1FF` block — exactly the bug the task
  asked about.
- **No escape sequence is ever split across a wrap/truncate/slice boundary.**
  Every scanning loop (`visible_width`, `truncate_to_width`,
  `truncate_fragment_to_width`, `slice_with_width`, `extract_segments`,
  `split_into_tokens_with_ansi`, `break_long_word`) extracts whole ANSI codes via
  `extract_ansi_code` and advances by `byte_len`, then walks plain text by
  grapheme. Styles are preserved across wraps by re-emitting
  `tracker.get_active_codes()` at each new line and closing bleed-prone
  attributes (underline, OSC-8) with `get_line_end_reset()`.
- **The feared O(n²) is actually linear.** The "call `extract_ansi_code` at every
  byte to find the plain-text extent" pattern is O(n) overall because
  `extract_ansi_code` returns in O(1) on any non-ESC byte (its first check is
  `bytes[pos] != b'\x1b'`). This matches T1's finding that the core render loop is
  allocation-careful — the text layer is too.
- **Hot-path allocation discipline is real.** `visible_width` fast-paths pure
  ASCII on `s.len()` (no cache pollution), memoizes the grapheme slow path in a
  thread-local FIFO cache (capacity 512, with a tested no-duplicate-on-race
  insert), and `normalize_terminal_output` scans bytes with `memchr` and returns
  *in place* without allocating on the overwhelmingly common (no Thai/Lao SARA AM)
  line — with a test asserting the buffer pointer is unchanged.
- **OSC-8 hyperlink handling is subtle and correct.** The tracker remembers the
  terminator (BEL vs ST) the opener used and re-emits the same form across wraps
  so clickability isn't lost on continuation rows; SGR reset (`\x1b[0m`)
  deliberately does *not* close an open hyperlink (`reset` vs `clear` split),
  matching terminal semantics. All of this is tested.
- **`extract_ansi_code`'s deliberately narrow CSI final-byte set is well-reasoned
  and well-tested.** Only `m G K H J` are recognized; cursor-motion/scroll
  commands are treated as literal so `visible_width` over-counts a stray escape
  rather than silently swallowing bytes it can't validate. Five tests pin the
  accept/reject/private-indicator/malformed-intermediate cases.
- **`fuzzy.rs` is a clean standalone util.** A thin wrapper over `nucleo_matcher`
  with a documented scoring convention (higher better, `None` = no match, empty
  query = 0), reusable-matcher + thread-local-convenience split, and a genuinely
  thoughtful `score_fields` that matches each token *within one field* to avoid a
  token being satisfied by characters straddling two fields — with a regression
  test (`gpt-5.5` must not match `gpt-5.1`+`GPT-5.1`). The exact-match bonus
  rationale (nucleo can't distinguish `cl`-vs-`cl` from `cl`-vs-`clone`) is
  documented.
- **`word_wrap.rs` content-preservation contract.** Unlike the output-oriented
  `wrap_text_with_ansi`, this wrapper carries exact byte offsets so editor cursor
  math survives a visual break; the single-grapheme base case (`:115`) is the
  fix for a real stack-overflow (recursive oversized-segment split on a width-2
  CJK char at width 1) and is pinned by a test with a comment recording *why*.

## Boundary & architecture notes

Dependency direction is correct: the text layer depends only on
`unicode-segmentation`, `unicode-width`, `memchr`, and `nucleo-matcher` — no
domain crates, matching `CLAUDE.md`'s standalone-framework intent. Internal
layering is clean and one-directional: `word_boundary.rs` and `word_wrap.rs`
depend on `ansi.rs` (`visible_width`, `is_whitespace_grapheme`,
`is_punctuation_grapheme`); `ansi.rs` depends on nothing in-crate; `style.rs` and
`fuzzy.rs` are independent leaves. All five modules are `pub mod` in `lib.rs`,
appropriate for a framework whose components and the `aj` binary consume them.

Two truncation/width impls theme (M2/TO1): this layer is the **third** place
width/truncation logic lives, but it is the *authoritative* one for terminal
rendering and is intentionally distinct from the `aj-tools`/`aj-models`
char/byte-count truncation (which truncates tool output, not terminal columns).
Worth confirming in synthesis that no caller uses the tools-layer truncation
where it should use `ansi::truncate_to_width` (display-width-correct). Within
this layer there are two *wrappers* by design — `ansi::wrap_text_with_ansi`
(output-oriented, trims whitespace, no byte offsets) and `word_wrap::word_wrap_line`
(content-preserving, byte offsets) — and the split is documented at both
module heads. The one wrinkle is that they classify whitespace differently (see
the ASCII-space finding); otherwise the duplication is justified by genuinely
different contracts.

Public-surface items for synthesis: the unused `style.rs` palette entries (above)
and the lower-level `word_boundary` skip helpers
(`skip_whitespace_forward`/`skip_word_class_forward`) which are `pub` to let the
editor splice paste-marker handling between the two steps — a legitimate seam,
documented at the module head.

## Test assessment

The in-module suites are strong and boundary-focused, not implementation-ossifying:
`ansi.rs` covers ASCII/CJK/tab/ANSI width, the three emoji-presentation width
rules, OSC-8 open/close/params/BEL-vs-ST/SGR-preserves-hyperlink, truncation
edge cases (zero width, empty, ellipsis-wider-than-width, pad), strict-vs-permissive
slice/segment wide-char boundary behavior, the Thai/Lao normalization byte-scan
edges (partial sequences at end-of-input, decoy lead bytes), and the width-cache
FIFO eviction + no-duplicate-on-race + ASCII-doesn't-pollute properties.
`word_wrap.rs` pins the content-preservation invariant and the single-grapheme
stack-overflow regression. `word_boundary.rs` covers the three-class model
including emoji-as-word and punctuation-run-as-word. `fuzzy.rs` pins the
cross-field token-borrowing regression and empty-query semantics. The integration
suites (`tests/`) add force-break-with-ANSI, very-large-Unicode-input truncation,
malformed-escape-without-hanging, and the regional-indicator sweep.

Gaps (mostly for T5 confirmation):
- **No explicit "escape never split across a wrap boundary" property test** (see
  the Testing finding) — the most important invariant of the layer is enforced by
  construction but not asserted.
- **No test for the tab/NBSP wrap-opportunity behavior** in `wrap_single_line`
  (the ASCII-space-split finding), so the discrepancy is uncovered.
- `width-1 column` wrapping of *styled* text and `strings wider than the entire
  viewport` are covered for plain text (`word_wrap` single-grapheme case,
  `truncate` large-input case) but I did not see a styled-text width-1 wrap case.

No flakiness risk: every function in scope is pure (the thread-local width cache
and fuzzy matcher are per-thread and behavior-transparent); no clock/network/fs
coupling.

## Cross-cutting themes to bubble up

- **Perf hot-path allocations (REFUTED for the text layer, matching T1's REFUTED
  for the core).** ASCII fast paths, a thread-local width memo, `memchr`-driven
  in-place normalization, linear (not O(n²)) plain-text scans. The lone
  double-vector in `get_active_codes` is off the per-frame path. This layer is a
  second positive example for the perf theme.
- **Chronology in comments (CONFIRMED, soft form).** `grapheme_width`'s "as of
  unicode-width 0.2" ties the contract to a dependency version; same family as the
  T1/AG1/TO2 "used to"/"no longer" smell, milder here.
- **Over-broad / dead public surface (CONFIRMED, mild).** `style.rs`'s
  256-color/RGB/`fg`/`bg`/`inverse`/`magenta`/`white` helpers have no caller —
  the TO2 "over-broad pub surface" theme, but defensible for a generic framework
  palette (unlike T1's dead *trait* methods on a portability seam). Fold into the
  workspace decision on intentional-completeness vs. trimming.
- **Multiple width/truncation implementations (CONFIRMED — this is the THIRD).**
  This layer is the display-width-authoritative truncation, distinct from the
  char/byte-count truncation in `aj-tools` (TO1) and `aj-models` (M2). Synthesis
  should verify callers pick the column-correct one for terminal output and the
  byte/char one for content limits — they solve different problems and the
  duplication is justified, but the *naming* (`truncate_to_width` vs
  `truncate.rs`) could mislead.
- **Inconsistent "whitespace" definitions across one layer (NEW, minor).** The
  output wrapper splits on ASCII space, the content-preserving wrapper and the
  word-boundary helpers use `char::is_whitespace`, and `is_whitespace_grapheme`
  uses an any-scalar rule. All documented individually, but a reader moving
  between them must re-learn the rule each time.
- **What held up clean (worth replicating):** grapheme-correct width everywhere,
  never-split-an-escape by construction, the documented `reset` vs `clear`
  SGR/hyperlink split, the OSC-8 terminator preservation, and `fuzzy`'s
  match-within-one-field design.
