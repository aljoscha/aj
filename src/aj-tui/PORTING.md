# `aj-tui` roadmap

Open work on the `aj-tui` crate.

---

## Phase C: Integration into `aj`

The crate is stable enough to start consuming from the agent. Scope
is intentionally light — we'll figure out the shape as we get there.

- [ ] Wire the agent's chat surface through `aj-tui::Tui` (Editor for
      input, Markdown for model output).
- [ ] Replace the placeholder `aj-ui` with `aj-tui`.
- [ ] Keep `aj-tui` standalone — no agent dependencies leaking back.

## Tracked parity gaps (low priority)

Real divergences from pi-tui that we know about but haven't fixed
because no input has surfaced the difference. Documented here so
future readers don't re-investigate.

#### `widthCache` — defer until benchmark-driven

**Pi** (`utils.ts:37-38, 201-256`): a module-level
`Map<string, number>` keyed on the input string, capped at 512
entries with FIFO eviction. ASCII-printable strings hit a fast
path that returns `str.length` directly and never touch the cache.
Non-ASCII strings (anything outside `0x20..=0x7E`) check the cache
before stripping ANSI/tabs and summing grapheme widths.

**Aj** (`src/ansi.rs:656-692`): same ASCII fast path. Non-ASCII
path allocates a stripped `String` and walks graphemes with no
cache.

**Where it might matter.** `visible_width` has ~54 call sites in
`aj-tui`. Hot ones: `word_wrap.rs` (per line, per token, per
grapheme during break), `ansi.rs` internals (truncate, wrap,
tracker), markdown table natural-width sizing, editor cursor
positioning, and the differential renderer in `tui.rs`. For an
editor full of CJK or emoji, the non-ASCII path runs many times
per keystroke.

**Why we haven't ported it.** The cost equation flips between JS
and Rust. Pi's JS `Map` lookup is cheap because strings are
interned and hashing is pointer-ish. A Rust
`HashMap<String, usize>` lookup hashes the string contents on
every probe — for short non-ASCII strings (a 4-byte emoji) the
hash + compare can cost more than the recompute. The cache is a
clear win on long non-ASCII strings (CJK paragraphs, emoji-dense
rows) and potentially a regression on short ones.

**Resolution criterion.** Add when a profiled aj session under
real CJK / emoji content shows non-ASCII `visible_width` is a
measurable fraction of render time. Without that data, porting
the cache is premature optimization; without the cache, our
non-ASCII path is straightforward and avoids HashMap insertion +
eviction bookkeeping.

#### `pooledStyleTracker` — JS-only optimization, no Rust equivalent needed

**Pi** (`utils.ts:1061-1062`, used at `1087, 1093, 1123`): a
module-level `pooledStyleTracker = new AnsiCodeTracker()` instance
reused inside `extractSegments` to avoid allocating a fresh
tracker per call. Calls `pooledStyleTracker.clear()` at entry to
reset state.

**Aj** (`src/ansi.rs:1322`):
`let mut style_tracker = AnsiStyleTracker::new();` per call.

**Why pooling doesn't translate.** Pi's `new AnsiCodeTracker()`
allocates a JS object plus an internal `Map<string, string>` of
SGR state; pooling sidesteps that allocation. Rust's
`AnsiStyleTracker` is a stack-allocated struct of `Option<bool>` /
`Option<u8>` / `Option<ActiveHyperlink>` fields; default
construction is genuinely zero-cost. The only heap allocation
inside the tracker is the `String` fields of `ActiveHyperlink`,
and those only fire when an OSC 8 hyperlink is actually parsed.

**Pooling in Rust would be net-negative.** Sharing a long-lived
instance requires either
`thread_local! { static …: RefCell<_> }` (slow on first access
per thread, branchy on subsequent ones) or a `Mutex` (contended
under any future multi-threaded driver). Either costs more than
the per-call default-construct we already do.

**Conclusion.** This isn't really a parity gap — Rust gets the
equivalent of pi's pool for free, by virtue of the type's shape.
Documented here so a future reader doesn't notice the missing
pool and "fix" it.

#### `truncate_fragment_to_width` — pi's no-ANSI-no-tabs fast path

**Pi** (`utils.ts:60-74`): after the ASCII fast path, an
intermediate fast path for non-ASCII inputs that contain neither
ANSI escapes nor tabs — skips the per-byte
`extractAnsiCode` / `hasTab` checks and runs a tight grapheme
loop.

**Aj** (`src/ansi.rs:720-770`): goes straight to the general loop
that calls `extract_ansi_code` on every byte position.
`extract_ansi_code` early-returns `None` cheaply when the byte is
not `\x1b`, so the per-byte cost is one byte load + one compare.

**Cost analysis.** For a 100-byte non-ASCII non-ANSI string, pi
skips 100 ANSI checks; we do 100 fast-path-rejected ones. Compute
is dominated by grapheme segmentation, which is orders of
magnitude more expensive per grapheme than the per-byte scan. Net
cost: ~100 byte comparisons per truncation, called at most a few
dozen times per render. Sub-microsecond per call.

**Why we haven't ported it.** The fast path is trivial to add
(~8 lines of Rust) but doubles the code paths in
`truncate_fragment_to_width` for negligible perf benefit, and the
extra path is a maintenance hazard if the general loop ever
gains new behavior the fast path forgets to mirror. Revisit if a
future code-shape audit flags the divergence as worth closing for
literal parity rather than perf.

## Watch items (blocked on upstream)

### `key_id_matches` Kitty base-layout key

Non-Latin keyboard layouts (e.g. Cyrillic Ctrl+С) should match
`ctrl+c` via the Kitty CSI-u `base:shifted:base-layout` three-field
form. Crossterm 0.28/0.29 parse the form but discard the base-layout
field. Revisit if/when crossterm exposes it on `KeyEvent` or we
graduate to a CSI-u fork.

### Byte-form key sequences crossterm doesn't parse

Pi-tui's `matchesKey` operates on raw byte input and recognises a
wider set of legacy / xterm sequences than crossterm does. We delegate
byte parsing to crossterm at the `ProcessTerminal` boundary, so the
sequences below either never become an `Event::Key` (parser returns
`Err`, `Parser::advance` clears the buffer) or arrive in a divergent
shape. None have a known consumer in aj; revisit per-item when a
specific keystroke fails to dispatch on a target terminal.

- **xterm modifyOtherKeys** — `\x1b[27;<mod>;<code>~`. Pi parses
  these as (codepoint, modifier) and matches `ctrl+c` etc.
  Crossterm's `parse_csi_special_key_code` only recognises a small
  set of leading numbers (1, 2, 3, 4, 5, 6, 7, 8, 11..=15, 17..=21,
  23..=26, 28..=29, 31..=34) for `~`-terminated sequences and
  rejects 27, so the bytes are dropped. Modern terminals largely
  emit Kitty CSI-u (`\x1b[<code>;<mod>u`) instead, which crossterm
  does parse. xterm with `XTerm.vt100.modifyOtherKeys: 2` and some
  tmux setups are the remaining emitters in the wild.
- **rxvt modifier sequences** — `\x1b[2$`, `\x1b[2^`, `\x1bOa`,
  `\x1bOb`, etc. Crossterm has no entries for these; the `$` final
  byte is outside the 64..=126 range it considers complete, so the
  bytes accumulate and are eventually dropped. Affects urxvt only.
- **Ctrl-symbol legacy mapping** — raw `\x1c..\x1f` bytes. Pi
  recognises these as `ctrl+\`, `ctrl+]`, `ctrl+~`, `ctrl+_`
  (or `ctrl+-`) per the legacy ASCII mapping. Crossterm normalises
  them as `Char('4')..'7'` + Control (digit characters). In Kitty
  mode crossterm parses `\x1b[92;5u` etc. correctly, so this only
  affects raw-byte input from older terminals.
- **Raw 0x08 backspace ambiguity** — pi disambiguates `\x08` as
  `backspace` vs `ctrl+backspace` via the `WT_SESSION` env. Crossterm
  always normalises `\x08` to `Ctrl+H` (`Char('h')` + Control),
  losing the distinction. Affects local Windows Terminal users only.
- **Numpad Enter / Clear key** — `\x1bOM`, `\x1b[E`, `\x1bOE`. None
  are parsed by crossterm. The Kitty CSI-u keypad encodings *are*
  parsed (`\x1b[57414u` -> `KeyCode::Enter`), so modern terminals
  that opt in are covered.

The other side-effect of this division: anything pi exposes as a
public byte-level helper (`decodeKittyPrintable`, `decodePrintableKey`,
`isKeyRelease(data)`, `isKeyRepeat(data)`) has no aj counterpart — by
the time bytes reach our `InputEvent` layer, crossterm has already
turned CSI-u into `KeyCode::Char(c)` (use `as_char()`) and tagged
release/repeat events via `KeyEventKind` (use `is_key_release()` /
`is_key_repeat()`).

