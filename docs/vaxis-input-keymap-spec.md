# Spec F: input dispatch, keymap, and leader sequences

## Status: implemented

Companion to `docs/aj-next-vaxis-plan.md`. This spec defines how `aj-next` routes
keyboard input: how global chords coexist with focused-widget input, a keymap
with single-key bindings and multi-key leader sequences, and the dispatch-debug
aids that make "which widget ate my key" answerable.

It resolves decision A-3 in `docs/vaxis-async-app-spec.md`. The three pieces live
in three crates:

- **`vaxis` / `vxfw`** owns the mechanism: the capture/target/bubble dispatch
  (already present), the activator and sequence matcher, the generic
  `KeymapController<A>` widget, and the debug aids (keystroke log + focus
  inspector). Reusable by any vaxis app.
- **`aj-app`** owns the data: the `AjAction` set, the default bindings (single
  keys and sequences), and loading/merging user config into a compiled
  `Keymap<AjAction>` (per Spec D, the keybinding data moves to `aj-app`).
- **`aj-next`** owns the effects: the handler that turns an `AjAction` into a host
  action (spawn/cancel a turn, open an overlay, quit), and wiring the controller
  into the root widget.

## The mechanism we already have

`vxfw` dispatches a key event along the focus path in three phases: capture
(root to target), at-target, then bubble (target back to root), and any handler
stops propagation with `EventContext::consume_event`. Mouse events walk the
hit-test path the same way. So a widget high in the tree can either pre-empt the
focused widget (handle in `capture_event`) or backstop it (handle in
`handle_event`, the bubble phase). This is the DOM-style model, and it is exactly
what we need. No host-loop pre-interception, and no per-chord "the editor never
sees this key" special-casing.

## The two philosophies, split by phase

The classic tension is whether the app or the focused widget gets a key first.
We resolve it the way the phase model already suggests, routing on the binding:

- **Shadowable shortcuts** (most single-key and modified chords) are handled in
  the **bubble** phase. The focused widget (editor, overlay, chat) sees the key
  first at-target. If it declines, the key bubbles up to the `KeymapController`
  near the root, which fires the bound action. This is what lets the editor
  legitimately shadow a global key while it is focused.
- **Pre-empting chords** (the cancel/quit ladder, close-all-overlay, and every
  leader prefix) are handled in the **capture** phase, so the controller sees
  them before the focused widget and consumes them. A leader prefix must
  pre-empt, otherwise the focused text field would swallow the first key of the
  sequence.

Each binding declares its phase (default bubble; capture for the pre-empting
set). Sequences are always capture.

## `KeymapController<A>`: the root chord widget

A generic `vxfw` widget placed as an ancestor of the editor, chat, and overlays
in the focus tree. It is generic over the action type `A` so `vaxis` carries no
`aj` vocabulary.

```
pub struct KeymapController<A, C> {
    keymap: Keymap<A, C>,              // compiled bindings -> action
    context: Rc<RefCell<C>>,           // host state the predicates read
    in_flight: Option<SequenceState>,  // the current leader sequence, if any
    on_action: Box<dyn FnMut(&mut EventContext, &A)>,
    child: WidgetRef,
}

struct SequenceState {
    pressed: Vec<Key>,                 // the keys pressed so far
    deadline: Instant,                 // scheduled via a vxfw Tick
}
```

The controller is generic over a host context type `C`, with predicates as
plain `fn(&C) -> bool`, so vaxis carries no aj vocabulary. The state stores
the pressed keys rather than matched activators. Activator matching is loose
(text and shifted-codepoint equivalence), so one press can satisfy
differently-spelled activators across candidate entries. Storing the keys and
replaying the whole prefix on each advance keeps every candidate sequence
alive.

Behavior:

- `capture_event(key)`:
  - If a sequence is in flight, try to advance it with `key`. On a full match,
    fire the action, reset, consume. On a partial match, extend the prefix,
    reschedule the timeout Tick, consume. On no match, reset the sequence, then
    fall through so the key is dispatched normally (it may start a new sequence
    or reach the focused widget).
  - Else, if `key` matches a capture-phase (pre-empting) single binding whose
    context predicate is satisfied, fire the action, consume.
  - Else, if `key` starts an enabled sequence (is a registered prefix), begin
    the sequence, schedule the timeout Tick, consume.
- `handle_event(key)` (bubble): the focused widget declined, so match `key`
  against bubble-phase bindings whose predicate is satisfied and fire, consume.
- On the timeout Tick: cancel the in-flight sequence and clear its hint.

An enabled capture single shadows a sequence start on the same activator. This
is what lets the ladder's Cancel binding pre-empt the quit sequence while a
turn runs, with the single's predicate selecting which applies.

A completion wins immediately: if a key finishes one sequence while extending
a longer one that shares the prefix, the finished sequence fires at once
rather than waiting out the timeout to disambiguate, and the longer sequence
is unreachable. Entry order breaks ties among sequences completing at the same
step.

The in-flight sequence renders a small hint (the partial chord and what it can
complete to), matching the affordance users expect from a leader system.

## Activators, sequences, and the keymap

```
pub struct Activator { pub key: KeyCode, pub mods: Modifiers }
impl Activator { pub fn accepts(&self, ev: &KeyEvent) -> bool { /* key + shift/ctrl/alt/super */ } }

pub enum Binding {
    Single(Activator),
    Sequence(Vec<Activator>),   // leader / multi-key
}

pub struct Entry<A> {
    pub binding: Binding,
    pub action: A,
    pub phase: Phase,           // Capturing (pre-empt) or Bubbling (shadowable)
    pub enabled: fn(&Ctx) -> bool,  // context predicate, e.g. only when no overlay is open
}

pub struct Keymap<A> { entries: Vec<Entry<A>>, /* prefix index for sequences */ }

pub enum SeqStep<A> { Matched(A), Progress, None }
```

`Keymap<A>` answers three queries the controller needs: does this key start a
sequence, does an in-flight prefix advance or complete, and does a lone key match
a single binding in a given phase. The prefix index makes the sequence lookups
cheap.

**Leader expansion.** A configurable leader activator lets a binding be written
as `<leader> x`, which the compiler expands into `Sequence([leader, x])`. The
leader is data, so a user can rebind it.

**Compilation (in `aj-app`).** `aj-app` merges the default bindings with the
user's `[keybindings]` config (`install_keybindings`), deduplicating, and
rejecting entries that name an unknown action, fail to parse, or collide with
another global binding. The frontend then compiles the effective bindings into
its `Keymap<AjAction>` and hands it to the controller at construction.

**Context predicates.** The `enabled` predicate centralizes the context-
sensitivity that `aj` scatters as `if overlay.is_open()` checks (palette-open is
inert while a modal is up, close-all only applies while an overlay is open). It
also drives the palette and hint UIs, which gray out disabled actions instead of
hardcoding availability.

**Hint labels are resolved, never hardcoded.** Any keyboard shortcut printed in
the UI (expand hints, footer hints, pending-box hints, overlay subtitles, the
help screen) must resolve through the keybinding data (`aj-app`'s
`action_shortcut`, which returns the user's `[keybindings]` override or the
built-in default), formatted by the shared `format_keybinding`. `aj` classic
works this way via `format_action_shortcut`, and `aj-next` does too: a rebound
action relabels every hint automatically, and no string literal can drift from
the actual binding. The only exceptions are the `fixed_keys` labels (Ctrl+C,
Ctrl+Y), which are deliberately not rebindable and have named constants for
their labels.

## Actions, not per-node intents

amp decouples key to intent to action by walking `Actions` widgets up the focus
path, so different ancestors can service the same intent differently. `aj`'s
actions are global, and the phase model already gives us context-sensitivity (an
overlay consuming Esc at-target before it reaches the controller, plus the
`enabled` predicate). So we adopt the keymap-to-action-callback model with a
single global handler and skip the per-node intent indirection. If a future
action genuinely needs per-focus-node behavior, it is handled by the focused
widget at-target rather than by adding an intent layer.

## The cancel/quit ladder

`aj`'s Ctrl+C behavior is a small state machine: while a turn runs, Ctrl+C
cancels it; while idle, the first Ctrl+C arms "press again to quit" and the
second quits, with a timeout that disarms. We express this on the sequence
engine plus a context predicate, preserving the exact semantics:

- Ctrl+C is a leader. While a turn runs, its action cancels the turn (predicate:
  turn running) and does not arm.
- While idle, the first Ctrl+C starts the sequence (the "press again to quit"
  hint is the in-flight hint), and a second Ctrl+C completes `ctrl+c ctrl+c` to
  quit. The timeout disarms.

Quit flows out through `Frame.quit` (the controller's action requests it), so the
host loop sees it the same as any other quit.

## Modality

An open overlay moves focus into itself, so it handles its keys at-target and
consumes them. Because the controller sits above the overlay, its capture-phase
chords still run first. That is correct for the chords that should work under a
modal (close-all) and wrong for those that should not (palette-open). The
`enabled` predicate resolves this: context-sensitive chords check "is a modal
open" and decline. This keeps modality decisions in the keymap data rather than
in scattered host conditionals. Mouse is blocked under a modal by a full-size
hit-test-consuming region behind the overlay (see Spec E).

## Debug aids

Not optional. They make dispatch observable, which is worth a lot when a key goes
to the wrong widget.

**Keystroke log (in `vxfw`).** A ring buffer (last ~100 events) on the focus
handler recording, per key: the key, the full focus path (widget debug labels),
the per-node handled flag, and which controller action (if any) fired. The
dispatch walk already knows the path and consumption, so it records as it goes.

**`Widget::debug_label` (in `vxfw`).** A trait method for the labels the log and
inspector show. The default returns `std::any::type_name::<Self>()`, so every
widget has a usable label for free and interesting ones override it.

**Focus inspector overlay (in `vxfw`).** A widget that renders the current focus
tree and the recent keystroke log with handled markers (a check or cross per node
per key). Toggled by a binding. It is a dev tool, drawn like any overlay.

`aj-next` binds the inspector toggle and otherwise gets the log and labels for
free.

## Phasing

Aligns with the plan's phases. The controller and keymap land with the input
work; the debug aids land alongside.

- **F1:** the activator/sequence/`Keymap<A>` types and `KeymapController<A>` in
  `vxfw`, the `AjAction` set and default bindings (single + sequences + leader)
  and config merge in `aj-app`, and the `aj-next` action handler. Port `aj`'s
  chords with behavior parity, including the cancel/quit ladder.
- **F2:** the debug aids: `Widget::debug_label`, the keystroke ring buffer on the
  focus handler, and the focus-inspector overlay.

## Decisions

- **F-1. Dispatch mechanism. Resolved: vxfw capture/bubble, no host
  interception.** Global chords are matched by a root `KeymapController` in the
  capture phase (pre-empting chords and leader prefixes) and the bubble phase
  (shadowable shortcuts). The host loop only calls `handle_input`.
- **F-2. Leader sequences. Resolved: full engine.** Multi-key sequences with a
  timeout and an in-flight hint, plus configurable leader expansion, not just a
  fixed cancel/quit ladder. The ladder is expressed on this engine.
- **F-3. Intent indirection. Resolved: single global action handler.** No
  per-focus-node `Actions`/intent layer. Context-sensitivity comes from at-target
  consumption and the `enabled` predicate.
- **F-4. Debug aids. Resolved: first-class, not optional.** Keystroke ring buffer,
  `Widget::debug_label`, and a focus-inspector overlay, all in `vxfw`.
