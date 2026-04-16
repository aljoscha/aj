# Example design notes

This file documents the rendering choices each example in this
directory makes, and the intentional behavior details a reader
might notice if they run a given example under different terminals
or compare its escape-byte output against alternative rendering
strategies.

## Intentional design choices

The rendering engine and examples make a handful of choices that a
byte-for-byte comparison against another rendering strategy would
flag. They're all deliberate; this section is the reference for why.

### CSI `2K` vs CSI `K` on line clears

Every full-line clear the render engine emits is `\x1b[2K` (erase
whole line), not `\x1b[K` (erase from cursor to EOL). Visually
identical on the viewport, byte-for-byte different in the writes log.
See `src/terminal.rs` for the rationale: cursor-relative erase can
leave stale bytes to the left of the cursor on a redraw that didn't
start at column 0.

### SGR sequence composition

`crossterm` and our `style` module compose SGR codes independently,
which can produce different byte-level orderings than other styling
libraries (for example `\x1b[1m\x1b[38;2;…m` vs
`\x1b[38;2;…m\x1b[1m`). The rendered cells end up identical; the
escape bytes may not.

### `set_progress` OSC 9;4 clear form

`ProcessTerminal::set_progress(false)` emits `\x1b]9;4;0;\x07` (with
the trailing semicolon), matching the ConEmu / Windows Terminal
progress protocol. Both this form and the shorter `\x1b]9;4;0\x07`
clear the indicator on terminals that honor the protocol, and both
are ignored elsewhere.

### Kitty keyboard protocol negotiation

`ProcessTerminal::start` unconditionally pushes
`DISAMBIGUATE_ESCAPE_CODES | REPORT_EVENT_TYPES` via crossterm's
`PushKeyboardEnhancementFlags`. Crossterm takes care of the
enhancement-flag round-trip internally, so we don't issue a separate
query first. On terminals that support the Kitty keyboard protocol
the flags take effect; elsewhere they're quietly ignored.

### OSC 8 hyperlinks gated on detected capabilities

`Markdown::render_link` reads
`aj_tui::capabilities::get_capabilities().hyperlinks` inline at the
link-render site, mirroring pi-tui's `markdown.ts:492`. Detection is
env-based and conservative: tmux / screen force the flag off
(because both multiplexers filter or rewrap OSC 8); Kitty / Ghostty /
WezTerm / iTerm2 / VS Code / Alacritty turn it on; unknown terminals
default off. Hosts that want to override the detection should call
`aj_tui::capabilities::set_capabilities`. There is no per-theme
override — the cap cache is the single source of truth.

### Typed `InputEvent` input path

Input arrives at components as pre-parsed typed events via crossterm.
Examples consequently don't exercise byte-stream parsing (DECSET
responses, raw DCS queries, cell-size replies, etc.); those are
handled by crossterm at the terminal boundary.

### Container surface on `Tui`

Pi-tui's `TUI extends Container`, so callers reach the child list
directly off the `tui` instance: `tui.addChild(c)`, `tui.removeChild(c)`,
`tui.children[i]`. Rust can't `extend` a struct, but the same call
shape falls out of forwarding methods on [`Tui`] that delegate to a
private root container: `tui.add_child(...)`, `tui.insert_child(...)`,
`tui.remove_child_by_ref(...)`, `tui.clear()`, `tui.len()`,
`tui.is_empty()`, `tui.last_index()`, `tui.get(i)`, `tui.get_mut(i)`,
`tui.get_as::<T>(i)`, `tui.get_mut_as::<T>(i)`. These are the only
public surface for child management; the underlying container is not
exposed.

## Per-example notes

### `chat_simple`

- Async `#[tokio::main]` loop that selects over [`Tui::next_event`] and a
  `tokio::time::sleep_until` modeling the bot's 1s "thinking" delay.
  The editor's autocomplete worker uses the [`RenderHandle`] auto-wired
  when the editor is added via `tui.add_child(...)`, so the popup
  repaints the moment results arrive — no per-frame polling.
- Response selection uses a small xorshift PRNG seeded from the
  monotonic clock on first call, so the example doesn't pull in a
  dependency just to pick one of eight canned responses.
- Loader is styled cyan (spinner) + dim (message) via the
  required-at-construction style closures on `Loader::new`.
- Slash-command autocomplete is wired through
  `CombinedAutocompleteProvider` with the two commands `/delete`
  and `/clear`.

### `key_tester`

- Displays the parsed-event representation (from the typed
  `InputEvent`: `KeyCode`, modifier bits, kind) rather than a hex
  dump of raw bytes. Those are the same information at different
  abstraction levels.
- Footer lists the keys the example is most useful for: Shift+Enter,
  Alt+Enter, Alt/Option+Backspace, Cmd/Ctrl+Backspace, plain
  Backspace.

### `viewport_overwrite_repro`

- Drives pre-tool, tool-output, and post-tool streaming phases with
  counts and sleeps sized to push content past the viewport on a
  small (8-12 row) terminal.
- Sleeps via `tokio::time::sleep_until` inside a `drive_for` helper
  that pumps [`Tui::next_event`] through the sleep window. The
  example is about the render engine's behavior under coalesced
  writes, so the tokio runtime stays out of the way — it's just the
  delivery vehicle for the 16 ms render throttle.

### `settings_demo`

- `SettingsList` inside a top-pinned overlay (80% width, 20 rows
  max, `row: Absolute(5)` with `TopCenter` anchor for horizontal
  centering).
- The top-pinned layout is deliberate. A search-enabled settings
  list shrinks and grows on every keystroke (zero matches → "No
  matching settings" is much shorter than the full list); a
  Center-anchored overlay re-centers on every render, which reads
  as the whole list visually jumping up or down as the user types.
  Pinning the top row keeps the first row of content anchored and
  only the bottom pulls in as the list shrinks — the same feel as
  host apps that render settings inline in their root container
  where the editor would normally live. The `explicit_row_pins_...`
  and `center_anchor_shifts_...` tests in `tests/overlay_options.rs`
  lock the two behaviors in side by side.
- Four items: two-value cycle (`Theme`, `Confirm Exit`), five-value
  cycle (`Verbosity`), and a submenu-backed `Editor` item that opens
  a nested `SelectList` picker.
- Descriptions on every item.
- Search enabled (type-to-filter on label). The search input renders
  with a `"> "` prompt so its column aligns with the `"→ "` / `"  "`
  gutter on item rows below it.
- Wires the submenu `done` callback through an
  `Rc<RefCell<Option<SubmenuDoneCallback>>>` so the `SelectList`'s
  `on_select` and `on_cancel` both feed the same done slot; whichever
  fires first wins and the slot is consumed.

## Verification status

- **Compile**: all four examples (`chat_simple`, `key_tester`,
  `viewport_overwrite_repro`, `settings_demo`) build with `cargo
  build -p aj-tui --examples` against the current workspace.
- **Manual smoke**: requires a real TTY (the examples exit
  immediately with a non-TTY stdin because crossterm's `EventStream`
  can't acquire raw mode). When running against a terminal, each
  example's per-example notes above describe what the user should
  see. Run with:
  ```
  cargo run -p aj-tui --example chat_simple
  cargo run -p aj-tui --example key_tester
  cargo run -p aj-tui --example settings_demo
  cargo run -p aj-tui --example viewport_overwrite_repro
  ```

## Unresolved deltas

Anything that shows up under real-terminal testing and isn't covered
by the sections above goes here. Each entry should include:

1. Which example.
2. The reproduction (key sequence, terminal size, environment).
3. The observed behavior and the expected behavior.
4. Whether it's known-cosmetic or suspected regression.

No unresolved deltas at the time of writing.
