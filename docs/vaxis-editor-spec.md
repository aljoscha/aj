# Spec B: `vxfw` multi-line editor

## Status: implemented

Companion to `docs/aj-next-vaxis-plan.md`. This spec ports the `aj-tui` prompt
editor to a new `vxfw` widget in the `vaxis` crate (decision: approach a+b, a new
vxfw multi-line editor whose editing logic is ported from the already-mostly-pure
`aj-tui` modules). `aj-next` uses it as its prompt input.

This is the largest single widget in the port and the main schedule risk, so it
is phased. We preserve the full capability of the `aj-tui` `Editor`. We do not
ship a cut-down input and call it done.

**Not built yet.** `aj-next` currently runs on a stopgap single-line
`vxfw::TextField` as its prompt input (`Shell.editor: Rc<RefCell<TextField>>`).
`TextArea` (this spec) does not exist, so multi-line editing, history, paste
markers, jump mode, and autocomplete are all missing from `aj-next` today.
Building `TextArea` and swapping the shell's `TextField` for it is outstanding
work, tracked by phases B1 to B3 below.

## Where it lives

The widget lives in `vaxis::vxfw` as `TextArea` (the multi-line sibling of the
existing single-line `TextField`). The generic, reusable editing logic lives with
it in `vaxis`. The app-specific pieces (which files/commands/symbols to complete,
the command catalog, the palette trigger wiring) live in `aj-next`.

Naming: `TextArea` reads as the multi-line counterpart to `TextField`. `Editor`
is an alternative but carries app-specific connotations. See decision B-1.

## The existing landscape

`vxfw::TextField` (`src/vaxis/src/vxfw/text_field.rs`) is a single-line input: a
gap buffer, readline editing (Ctrl-A/E/K/U/W, Alt-B/F/D/Backspace, word motions
by Unicode general category), horizontal scroll with ellipsis, `on_change` /
`on_submit: Option<Box<dyn FnMut(&mut EventContext, &str)>>`, and it sets
`surface.cursor`. `TextArea` follows the same conventions (callbacks, cursor,
`wants_events`, `draw -> Surface`) but is structurally different: a multi-line
document with vertical movement and wrapping. It is a fresh widget, not an
extension of `TextField`, but the two share one word-motion engine with a
pluggable classifier (see "Word motion" below and decision B-2).

## Full feature inventory to preserve

The `aj-tui` `Editor` (`src/aj-tui/src/components/editor.rs`, ~4000 lines) is
rich. We port all of it, phased:

1. Multi-line document (`lines: Vec<String>`), cursor as `(line, byte col)`.
2. Width-aware word wrapping for display.
3. Emacs-style editing (Ctrl-A/E/K/U/W, Alt-B/F/D, Ctrl-B/F/P/N, etc.).
4. Kill ring with accumulate/prepend and yank-pop rotate.
5. Undo (buffer snapshots, coalesced by `LastAction`).
6. Vertical movement with a sticky preferred column, atomic-segment snap, and the
   `snapped_from_cursor_col` intent tracking.
7. History (up/down at the first/last line), `seed_history`, `HISTORY_LIMIT`.
8. Large-paste markers (`[paste #N +M lines]`), a `pastes` map + counter, and
   `expanded_text` that splices markers back to their literal content.
9. Char-jump mode (`Ctrl-]` forward, `Ctrl-Alt-]` backward, scanning across
   logical lines).
10. Autocomplete: an inline popup driven by a provider pipeline, triggered by `@`
    (files), `/` (commands), `#` (symbols), and Tab (force), with async result
    delivery and a streaming-session fast path.
11. A border (top/bottom rules) with a themed color and an optional inlaid
    top-bar label plus the `up N more` scroll indicator.
12. A bounded visible-row window with scrolling.
13. Horizontal padding.
14. `on_submit` / `on_change`, submit-vs-newline (Enter vs Shift-Enter / `\`+Enter),
    a submitted-text poll, `disable_submit`, and a palette trigger (`/` at an
    empty start-of-message).
15. `set_text` / `text` / `expanded_text` / `cursor` / `lines` /
    `insert_text_at_cursor`.
16. A theme (border color, autocomplete popup styling).

## What ports as-is, what is rewritten, and the autocomplete boundary

### Ports as-is (pure logic, no TUI dependency)

Small pure modules move into `vaxis` alongside `TextArea`:

- `kill_ring::KillRing` (push with prepend/accumulate, peek, rotate).
- `undo_stack::UndoStack<S>` (generic LIFO snapshots).
- The word-motion engine and its classifiers (see "Word motion" below).

The document-editing algorithms (insert/delete, word motions, kill/yank, the
sticky-column vertical-move math, jump-mode scanning, paste-marker bookkeeping,
history navigation) port largely verbatim. They operate on `Vec<String>` + cursor
and carry no rendering.

### Rewritten (rendering and input)

- **Rendering.** `aj-tui` renders to `Vec<Line>` of ANSI strings. `TextArea`
  draws into a `Surface` (cells with `vaxis::cell::Style`). The border is drawn
  with cells (or via the `vxfw::Border` widget) using a structured border color,
  not an ANSI closure. The caret is reported through `surface.cursor`
  (`CursorState`), not an embedded marker.
- **Wrapping.** `aj-tui`'s `word_wrap` is ANSI-aware because its buffers can carry
  escapes. `TextArea`'s buffer is plain user text, so it uses `vxfw`'s
  width-aware wrapping primitives (`DrawContext::string_width` /
  `grapheme_iterator`, the same measurement the `Text` widget uses) rather than
  porting `word_wrap`. This keeps one wrap engine in `vaxis`.
- **Input.** `aj-tui` dispatches typed crossterm `InputEvent`/`Key`. `TextArea`
  handles `vxfw::Event` with `key.matches(codepoint, mods)` (the vaxis matching
  API), inside `handle_event`. Paste arrives as `Event::Paste`.
- **Theme.** `EditorTheme` in `vaxis` holds structured `Style`/`Color` values
  (border, popup) instead of `Arc<dyn Fn(&str) -> String>` ANSI closures.
  `aj-next` builds it from the shared palette (Spec D's structured colors).

### The autocomplete boundary

`aj-tui`'s `autocomplete.rs` (~1650 lines) splits into a generic mechanism and
app-specific providers:

- **Into `vaxis::vxfw` (the mechanism):** the provider traits (`AutocompleteProvider`,
  `AutocompleteSession`) and data types (`AutocompleteItem`,
  `AutocompleteSuggestions`, `CompletionApplied`, `SuggestOpts`, `SessionStatus`),
  plus `TextArea`'s trigger detection, prefix tracking, the inline popup
  (rendered with a small `vxfw` list rather than `aj-tui`'s `SelectList`), and the
  accept/apply flow.
- **Into `aj-next` (the providers):** the concrete completions, which are
  app-specific. `@` file completion (the fuzzy filesystem walk, currently
  `FuzzyFileSession` / `CombinedAutocompleteProvider`, using `ignore` + `nucleo`),
  `/` command completion (from the shared command catalog), and `#` symbol
  completion. `aj-next` composes these against the `vaxis` provider trait.

The fuzzy file walk is arguably reusable enough to live in `vaxis`, but composing
it with the command/symbol providers is app knowledge, so the composition stays in
`aj-next`. See decision B-3.

## Word motion: a shared, pluggable classifier

Both `TextField` and `TextArea` move and delete by word, but with different
default feels. `TextField` uses a readline two-class model where a run of
punctuation is a boundary you skip. `TextArea` inherits aj's three-class model
where a run of punctuation is its own word you land on. For example, word-right
over `foo...bar` from just after `foo`: the readline model jumps to the end of
`bar` (skipping the dots), the three-class model stops between the dots and
`bar`. Rather than fork two engines, we share one and make the classification
pluggable.

Shape (lives in `vaxis`):

- `enum CharClass { Separator, Punctuation, Word }`. `Separator` is the leading
  run a word jump always skips first. `Punctuation` and `Word` are landable
  classes: a jump stops when the class changes.
- `trait WordClassifier { fn classify(&self, grapheme: &str) -> CharClass; }` (a
  boxed `Fn(&str) -> CharClass` is the lighter alternative). Grapheme-based, so
  combining marks and ZWJ sequences stay with their base.
- One engine over any classifier: `word_left` / `word_right`, plus the two-phase
  `skip_separators` / `skip_class` helpers so `TextArea` can splice its
  paste-marker handling between the separator skip and the class skip exactly as
  aj does today. The rule is: skip a run of `Separator`, then skip a maximal run
  of the class of the next non-separator unit.

Two built-in classifiers reproduce both default feels through this one engine:

- **`ReadlineWords` (two-class, `TextField`'s default).** `Word` = letters,
  digits, marks, connector punctuation, and `_` (by Unicode General Category).
  Whitespace and all punctuation map to `Separator`, so the engine skips
  punctuation with whitespace: the readline feel.
- **`EmacsWords` (three-class, `TextArea`'s default).** Whitespace maps to
  `Separator`, aj's ASCII-punctuation bag maps to `Punctuation`, everything else
  to `Word`, so the engine stops on punctuation runs: aj's current feel.

Each widget sets its default and exposes a setter, so an app or a user can plug
in a custom classifier without forking the widget.

Adopting the shared engine for `TextField` is gated on its ported word-motion
tests staying green: the engine plus `ReadlineWords` must be behavior-identical
to its current codepoint logic for normal text. If any ported test shifts, we
keep `TextField` on its own faithful engine and make only `TextArea` pluggable,
so the libvaxis port stays faithful.

## Widget API

Matching `vxfw` conventions (`Rc<RefCell<TextArea>>`, callbacks take
`&mut EventContext`):

```
pub struct TextArea { /* document, cursor, kill ring, undo, autocomplete, ... */ }

impl TextArea {
    pub fn new() -> Rc<RefCell<TextArea>>;

    // Content.
    pub fn text(&self) -> String;                 // logical lines joined by \n
    pub fn expanded_text(&self) -> String;        // paste markers spliced back
    pub fn set_text(&mut self, text: &str);       // cursor to end
    pub fn insert_at_cursor(&mut self, text: &str);  // one undo unit
    pub fn cursor(&self) -> (usize, usize);
    pub fn clear(&mut self);

    // Submission.
    pub on_change: Option<Box<dyn FnMut(&mut EventContext, &str)>>,
    pub on_submit: Option<Box<dyn FnMut(&mut EventContext, &str)>>,
    pub fn set_submit_enabled(&mut self, enabled: bool);   // disable_submit inverse

    // History.
    pub fn seed_history(&mut self, entries: &[String]);
    pub fn add_to_history(&mut self, text: &str);

    // Presentation.
    pub fn set_theme(&mut self, theme: EditorTheme);
    pub fn set_padding_x(&mut self, cols: usize);
    pub fn set_top_bar_label(&mut self, label: Option<String>);
    pub fn set_border_color(&mut self, color: Color);      // thinking/bash-mode tint

    // Autocomplete.
    pub fn set_autocomplete_provider(&mut self, provider: Arc<dyn AutocompleteProvider>);
    pub fn set_autocomplete_max_visible(&mut self, max: usize);
    pub fn on_palette_trigger: Option<Box<dyn FnMut(&mut EventContext)>>,  // `/` at empty start
    pub fn is_showing_autocomplete(&self) -> bool;
}

impl Widget for TextArea {
    fn draw(&mut self, ctx: &DrawContext) -> Surface;         // sets surface.cursor
    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event);
    fn wants_events(&self) -> bool { true }
}
```

`aj-tui`'s `EditorComponent` trait (the `Box<dyn EditorComponent>` pluggable-editor
abstraction) is aj-tui-specific and is not ported. `aj-next` holds a concrete
`Rc<RefCell<TextArea>>`. If pluggable editors are ever wanted, aj-next defines its
own trait then.

**Editing-chord descriptor table.** `TextArea` also exposes its fixed editing
chords as a static descriptor list (key label, description, display group), an
associated `fn bindings() -> &'static [ChordDoc]` rather than per-instance state.
The help screen (Spec E) renders its "Editor shortcuts" section from this table,
so the editor's own chords are the single source of truth for both the handler and
the documentation and cannot drift. These chords are fixed and not rebindable, so
the table is a plain constant, unlike the global chords the help resolves through
the `Keymap` (Spec F).

## Height and scroll: constraint-based, not terminal-driven

`aj-tui`'s `Editor` reads terminal rows off a `RenderHandle` to auto-size its
visible window (`max(5, floor(rows * 0.3))`). In `vxfw` the editor sizes to the
`DrawContext` constraints it is given, so `aj-next`'s layout owns the height
budget (via a `SizedBox` / flex around the editor) and `TextArea` scrolls its
document within the height it receives. This drops the `RenderHandle` dependency
entirely and moves the sizing policy to the layout, where it belongs.

The border's `up N more` indicator and the top-bar label render into the top rule
exactly as today, driven by the internal scroll offset.

## Async autocomplete under the host-driven loop

`aj-tui`'s editor spawns `spawn_blocking` walks and drains a results channel at the
top of `render`/`handle_input`, using a `RenderHandle` to wake the driver. Under
`AsyncApp` (Spec A) the host owns the `select!`, so the wake is the host's job:

- `TextArea` keeps the async pipeline (cancel token, request id, streaming
  session) but exposes its results receiver (or a `pump_autocomplete()` method) to
  the host.
- `aj-next` adds a `select!` arm on that receiver: when a delivery arrives, it
  calls `pump_autocomplete()` and `app.request_redraw()`.

This mirrors how AgentEvents and other host sources integrate in Spec A: one more
arm, no reader thread inside the widget.

## Image paste stays in the host

The `aj-tui` editor does not itself read the clipboard. Image paste is a host
chord (`Ctrl+V` -> clipboard -> tempfile -> insert the path), and the editor only
needs `insert_at_cursor(path)`. That stays true here: `aj-next`'s loop handles the
chord and calls `insert_at_cursor`. `TextArea` carries no clipboard code.

## Phasing

### Phase B1: core multi-line editing

Document model, cursor movement (horizontal + vertical with sticky column and the
atomic-segment snap), insert/delete, word motions, kill ring, undo, width-aware
wrapping via `vxfw`, the border with themed color and top-bar label, the bounded
scroll window, padding, `on_change`/`on_submit`, submit-vs-newline, history
up/down, and `set_text`/`text`/`cursor`/`insert_at_cursor`. Ports `KillRing`,
`UndoStack`, `word_boundary` into `vaxis`. No autocomplete, no paste markers, no
jump mode. Gate: a usable multi-line prompt in an example.

### Phase B2: paste markers and jump mode

Large-paste markers + `expanded_text`, the `pastes` map + counter, and char-jump
mode (`Ctrl-]` / `Ctrl-Alt-]`). Gate: pasting a large block collapses to a marker
and `expanded_text` restores it.

### Phase B3: autocomplete

The provider traits + data types + inline popup mechanism into `vxfw`; the concrete
`@`/`/`/`#` providers into `aj-next`; the async delivery wired through the host's
`select!`. The palette trigger (`/` at empty start) fires `on_palette_trigger`.
Gate: `@`-file and `/`-command completion work end to end in `aj-next`.

## Testing

`vxfw` widgets carry a doctest per module (the framework's meta-test enforces it),
so `TextArea` gets one. Beyond that, port the behavioral coverage from the
`aj-tui` editor and its supporting modules:

- kill ring, undo, and word-boundary unit tests move with those modules.
- editing tests (insert/delete, word motions, vertical move with sticky column,
  wrapping at a given width) as `TextArea` unit tests driving `handle_event` with
  synthesized `vxfw::Event`s and asserting `text()` / `cursor()`.
- paste-marker round-trip (`expanded_text`), jump-mode landing positions.
- autocomplete trigger/prefix/accept against a stub provider (no filesystem).

## Decisions

- **B-1. Widget name. Resolved: `TextArea`** (pairs with `TextField`).
- **B-2. Word motion. Resolved: separate defaults, one pluggable engine.**
  `TextField` and `TextArea` keep distinct default feels (readline two-class and
  emacs three-class) but share one grapheme-based engine parameterized by a
  pluggable `WordClassifier` (see "Word motion"). Adopting the shared engine in
  `TextField` is gated on its ported tests staying green; otherwise only
  `TextArea` is pluggable and `TextField` stays as the faithful port.
- **B-3. Autocomplete provider placement. Resolved: `vaxis` stays lean.** The
  provider traits, data types, and inline-popup mechanism go into `vxfw`. Every
  concrete provider, including the fuzzy-file walk and its `ignore`/`nucleo`
  dependencies, lives in `aj-next`. `vaxis` takes no fuzzy-matching or
  filesystem-walk dependency.
- **B-4. Autocomplete popup. Resolved: purpose-built.** A small inline popup
  owned by the editor and drawn into its surface, not a reused `ListView`, since
  it renders within the editor and routes keys inline.
