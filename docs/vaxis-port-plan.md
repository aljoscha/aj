# libvaxis Rust Port Plan

## Goal

Port the Zig TUI library [libvaxis](https://github.com/rockorager/libvaxis)
(pinned at the `v0.6.0`-era `main`, `minimum_zig_version 0.16.0`) into this
workspace as faithfully as possible. No corners cut. We port the full library,
every test, and every example.

The end goal is for `vaxis` to **replace `aj-tui` as the TUI backend for the
`aj` binary**, which is an async (tokio) application. So the port must integrate
cleanly with async Rust. We still want the full faithful port regardless of that
use case: the library, its threaded `Loop`, and the vxfw `App` are ported as-is
(with their tests), and the async integration is a layer on top, not a
replacement for the faithful pieces.

The work is driven test-first. For each module we translate the upstream Zig
tests into Rust tests, watch them fail (red), then port the implementation until
they pass (green).

This document is the map. It records the architecture we found, the design
decisions the Zig-to-Rust gap forces on us, the dependency-ordered phases, and
the per-phase test inventory. It does not contain code.

## Non-goals (for this effort)

- Rewiring the `aj` binary onto vaxis. Replacing `aj-tui` is the motivating end
  goal and the port is designed to make it possible (see "Async integration"),
  but the actual swap in the `aj` binary is a separate follow-on effort. The
  deliverable here is the faithful `vaxis` crate plus its async seam.
- Improving on or "fixing" upstream behavior. Where upstream has quirks or
  latent bugs we reproduce them and leave a `NOTE` comment. Genuine
  simplifications (e.g. unifying two near-identical wrap engines) are flagged
  here as decisions to workshop, not taken silently.

## Source of truth

The reference checkout lives at `/tmp/libvaxis` during development. The upstream
layout, sizes, and test counts we are matching:

- `src/` is one Zig module, ~18.4k lines, 170 `test` blocks total.
- Core wire layer: `Cell`, `Screen`, `InternalScreen`, `Window`, `gwidth`,
  `unicode`, `GraphemeCache`, `ctlseqs`.
- Input layer: `Parser` (37 tests, the heaviest), `Key`, `Mouse`, `event`.
- Runtime: `Vaxis` (1.5k lines), `tty`, `Image`, `Loop`, `queue`, `main`.
- `vxfw/`: the retained-mode widget framework (`App` plus 16 widgets).
- `widgets/`: the legacy immediate-mode widgets, including a self-contained
  PTY-backed terminal emulator under `widgets/terminal/`.
- `examples/`: 14 example programs (9 legacy, 5 vxfw).

A `refAllDecls` test appears at the end of most files. It is a compile-coverage
smoke test with no runtime assertions. Rust gets that coverage from `cargo
check`, so we do not port `refAllDecls`. We keep the *convention* it enforces
(see the doctest meta-test in Phase 8).

## Architecture, in one breath

vaxis splits cleanly into layers, and the layering dictates our port order.

1. A cell/style/color value model (`Cell`) and a flat cell buffer (`Screen`),
   viewed through cheap, clipped `Window` handles that also host the text
   layout/print engine.
2. A second cell buffer (`InternalScreen`) that owns its grapheme bytes and
   serves as the previous-frame snapshot for diff rendering.
3. A pure input parser (`Parser`) that decodes terminal bytes into `Event`s,
   plus the `Key`/`Mouse` value types.
4. A thread-safe bounded queue (`queue`) and a threaded event `Loop` that reads
   the tty, parses, and pushes events.
5. The OS boundary (`tty`): raw mode, window size, SIGWINCH, the buffered
   writer. Comptime-dispatched across posix/windows/test backends.
6. The `Vaxis` runtime: owns both screen buffers and the capability set, runs
   capability detection, and diff-renders to a writer. Plus `Image` (kitty
   graphics) transmission and geometry.
7. `vxfw`: a Flutter-style retained widget tree. A `Widget` vtable
   (`userdata` + draw/event/capture function pointers), a constraint-based
   `DrawContext`, a `Surface`/`SubSurface` layout tree allocated from a
   per-frame arena, an `EventContext` command bus, and an `App` runtime with
   focus/mouse/tick handling and capture/target/bubble event propagation.
8. Legacy immediate-mode `widgets/`, drawn directly onto a `Window`, including
   the embedded terminal emulator.

The single most important structural property to preserve: **the renderer
writes into an abstract buffered writer, and the TTY (fd + raw mode + signals)
is a separate object.** That seam is what makes the renderer unit-testable
against an in-memory buffer and is the backbone of our test strategy.

## Async integration

The `aj` binary is async (tokio) and `select!`s terminal input against agent
events. vaxis must slot into that without us giving up the faithful threaded
pieces. Three facts make this clean:

1. **The input `Parser` is a pure function** (`parse(bytes) -> (Option<Event>,
   n_consumed)`), with the partial-sequence resync owned by the caller. Byte
   decoding is therefore decoupled from "who reads the bytes and where events
   go." We factor the byte-pump (read, resync, parse, fold capability events,
   copy `Key.text`, post) into a reusable core that both the faithful threaded
   `Loop` and an async front-end share.
2. **`Event` is `Send + 'static`** under the D1 ownership model (A or B), so it
   flows through a `tokio::sync::mpsc` channel and across `spawn_blocking`
   boundaries with no lifetime trouble.
3. **Rendering is synchronous and cheap**: a diff over the cell buffer followed
   by a small buffered write + flush to the fd. It runs inline on the app task.
   `spawn_blocking` is available if a pathological full repaint ever needs it.

Concretely we ship two input front-ends over the shared byte-pump core:

- **Threaded `Loop` + `Queue`** (faithful, fully tested). Ported verbatim with
  all upstream tests. Usable standalone and from sync code.
- **Async front-end** for the aj use case: register `/dev/tty` with the tokio
  reactor via `AsyncFd`, read when readable, feed the shared parser core, and
  emit `Event`s on a `tokio::sync::mpsc`. No reader thread. (A trivial bridge
  thread forwarding `queue.pop()` into an mpsc is the fallback if we ever want
  to reuse the threaded `Loop` verbatim under async, but `AsyncFd` is the
  idiomatic path.)

The vxfw `App` is the one piece tightly coupled to a run loop and to `std.Io`
timers (`Tick`). We keep the faithful `App` (tested), and add an async driver
that paces frames and schedules `Tick` deadlines on tokio timers, so an async
host can `select!` vaxis input, ticks, and its own events. For the eventual aj
integration we will most likely drive the vxfw widget tree from aj's existing
async loop rather than calling `App::run`, using `Vaxis` (render) plus the async
input front-end directly. Signals (SIGWINCH/SIGCHLD) route through `signal-hook`
into the same channel rather than doing work in the handler (D7), which is
exactly what an async host wants.

Net: the faithful threaded and `App`-driven paths exist and are tested, and the
async paths reuse the same parser, renderer, and `Vaxis` runtime underneath.

## Crate and module structure

We mirror upstream's single-module design with a single library crate plus two
small supporting crates that exist only because Rust forces them to be separate.

```
src/vaxis/                  # the library; modules mirror libvaxis src/ layout
  Cargo.toml
  src/
    lib.rs                  # = main.zig: re-exports, Winsize, panic/recover
    cell.rs                 # Cell, Segment, Character, Style, Color, Scale, ...
    screen.rs               # Screen (front buffer)
    internal_screen.rs      # InternalScreen (back buffer, owns graphemes)
    window.rs               # Window + the print/wrap engine
    gwidth.rs               # display-width measurement
    unicode.rs              # grapheme iterator wrapper
    grapheme_cache.rs       # GraphemeCache (see ownership decision below)
    ctlseqs.rs              # control-sequence constants + typed encoders
    parser.rs               # input escape-sequence parser
    key.rs                  # Key, Modifiers, KittyFlags, codepoint constants
    mouse.rs                # Mouse, Button, Modifiers, Shape, Type
    event.rs                # Event union
    queue.rs                # bounded thread-safe ring buffer
    loop.rs                 # threaded event loop (`r#loop` or `event_loop`)
    tty/                    # posix.rs, windows.rs, test_backend.rs, mod.rs
    vaxis.rs                # the Vaxis runtime + renderer
    image.rs                # Image geometry + kitty transmission
    widgets/                # legacy widgets (mod.rs + one file per widget)
      terminal/             # the embedded emulator
    vxfw/                   # the widget framework (mod.rs + App + widgets)
  examples/                 # all 14 examples as <name>.rs
  tests/                    # cross-module integration tests

src/vaxis-ucd/              # build-time Unicode-table generator (see below)
src/vaxis-derive/           # proc-macro crate for the Table row derive
```

Rationale for one crate, not many: upstream is one module with internal cycles
(`main` imports `Vaxis` and `tty`, both import back into `main` for shared
types like `Winsize`/`Event`). Splitting into many crates would force us to
break those cycles into artificial public boundaries that upstream does not
have, working against faithfulness. A single crate with a module tree keeps the
mapping one-to-one. We break the one real cycle by putting shared leaf types
(`Winsize`, `Event`, `Cell`) in low modules that everything else depends on,
which Rust requires anyway.

The two supporting crates are unavoidable in Rust:

- `vaxis-ucd`: Unicode data generation (proc-macro/build-script-style codegen
  cannot live in the same crate as its consumer cleanly, and the generator
  pulls in UCD-parsing deps we do not want in the runtime crate).
- `vaxis-derive`: the `#[derive(TableRow)]` proc-macro that replaces Zig's
  comptime struct reflection in the `Table` widget. Proc-macros must be their
  own crate.

## Dependency mapping: Zig to Rust

| Upstream (Zig) | Rust replacement | Notes |
|---|---|---|
| `std.mem.Allocator` threading | ownership + `Drop` | `init(alloc)`/`deinit(alloc)` collapse into `new`/`Drop`; allocators disappear from signatures |
| `std.Io` async (`Future`, `concurrent`, `await`) | `std::thread` + `JoinHandle` | the reader loop and terminal emulator become OS threads |
| `std.Io.Mutex` / `std.Io.Condition` | `std::sync::{Mutex, Condvar}` | the queue's two-condvar design ports directly |
| `std.Io.futexWaitTimeout` (DA1 handshake) | `Condvar` + `Mutex<bool>` or `mpsc` recv-timeout | capability-detection wakeup |
| `std.atomic.Value(T)` | `std::sync::atomic::*` | preserve memory orderings from the tests |
| `uucode` (Unicode props) | `vaxis-ucd` generated tables | east_asian_width, grapheme_break, general_category, is_emoji_presentation |
| `zigimg` | `image` crate (already a workspace dep) | decode + PNG/RGB/RGBA encode |
| `std.base64` | `base64` crate (already a dep) | OSC 52 paste, kitty payloads |
| posix termios/ioctl/sigaction | `nix` (`term`, `fs`, `ioctl`, `signal`) | port `tty.zig` faithfully, not via crossterm |
| `openpty`/`fork`/`exec`/`waitpid` | `nix` (`pty`, `process`, `unistd`) + `Command::pre_exec` | terminal emulator child spawn |
| SIGWINCH/SIGCHLD C handlers | `signal-hook` or self-pipe into the queue | avoid real work inside a signal handler |
| Windows console API | `windows-sys` | the Windows tty backend, staged separately |
| `std.StaticStringMap` | `phf` or a `match` | `Key.name_map` |
| comptime struct reflection (`Table`) | `vaxis-derive` `#[derive(TableRow)]` | preserves "columns from the type" |
| `MultiArrayList` | parallel `Vec`s or `Vec<Grapheme>` | `TextView.Buffer` |
| saturating `+|` / `-|` | `saturating_add` / `saturating_sub` | pervasive, do not miss any |
| `i17` offsets | `i32` | window/subsurface origins can be negative |
| `u21` codepoints | `u32` newtype, not `char` | `multicodepoint = 1_114_113` exceeds `char::MAX` |

New workspace dependencies to add: `nix` (extend features to `term`, `fs`,
`ioctl`, `pty`, `process`, plus existing `signal`), `signal-hook`, `phf`
(+ `phf_macros`), `windows-sys` (target-gated), a small-string type for grapheme
storage (`compact_str` or `smol_str`, see D1), and for `vaxis-ucd` a UCD parser
dependency (candidate: `ucd-parse`, or vendored UCD data files with a small
hand-rolled parser). `image`, `base64`, and `tokio` already exist in the
workspace.

## Key design decisions to workshop before/while coding

These are the points where the Zig-to-Rust gap forces a real choice. Per our
guidance we surface them rather than silently picking. Each has a recommended
default so work can proceed, but they are worth an explicit thumbs-up.

### D1. Grapheme byte ownership and the perf question (the #1 decision, blocks Phase 1)

Upstream `Cell.char.grapheme` is a borrowed `[]const u8`. The front `Screen`
borrows (into the `Segment` text the caller passes to `print`, or into the
`GraphemeCache`, an 8 KiB ring reused each frame). The back `InternalScreen`
owns bytes in an arena. `Key.text` borrows the parser's transient scratch and is
copied into the cache before crossing the loop's thread boundary. The net effect
is **zero heap allocation per frame**: cells are 16-byte fat pointers into
reused storage.

A literal zero-copy port would thread a lifetime through `Cell`, `Screen`,
`Window`, `Surface`, and `Event`. That lifetime cannot cross the channel between
the input source and the app, which also breaks the async integration goal (a
borrowed `Event` is not `Send + 'static`). So the literal translation is a dead
end for our use case.

The key realization: **owning a grapheme does not mean heap-allocating it.** A
grapheme is almost always 1 byte (ASCII) and essentially never more than ~24.
So the real options all avoid per-frame allocation:

- **Option A, inline small string (SSO).** Store the grapheme in a 24-byte
  `CompactString`/`SmolStr` kept inline. Zero heap allocation for any grapheme
  that fits inline, which is effectively all of them (heap only for rare
  >24-byte clusters). The renderer reads bytes inline with no pointer-chase, so
  the per-frame read path is at least as fast as upstream's borrowed pointer.
  Cost: ~8 bytes/cell over a fat pointer. Cells, `Key.text`, and `Event` are
  `Send + 'static`. `GraphemeCache` becomes vestigial (kept as a thin no-op for
  API parity, or dropped).

- **Option B, grapheme interning (max fidelity, min memory).** The literal
  evolution of upstream's `GraphemeCache`: `Cell` holds a 4-byte `GraphemeId`,
  bytes live deduplicated in a per-`Screen` interner. Cells become `Copy` and
  smaller than upstream's, memory is minimal and deduped, and graphemes reused
  across frames cost nothing to re-store. Reading a cell's bytes is one indexed
  lookup. This is the most faithful to upstream's "cells are cheap handles,
  bytes live in shared storage" architecture. Still fully `Send + 'static`.

- **Option C, `Cow<'frame, str>` + frame arena.** The literal transliteration:
  borrow when the source outlives the frame, own into an arena otherwise. Keeps
  zero-copy on the `print(segment)` path, but the lifetime infects the whole
  type stack and `Event` cannot cross a channel. Rejected because it conflicts
  with the async goal.

Recommended: **Option A by default** (simplest, no lifetimes, zero per-frame
allocation, async-friendly), with the `Cell` grapheme exposed through a small
accessor so we can switch to **Option B** behind the same API if profiling ever
shows memory pressure or we want exact architectural fidelity. Either way,
back-buffer cells own, and equality is an explicit asymmetric `back.eql(front)`
method (it ignores `scale`/`image` and short-circuits on the default flag), not
a derived `PartialEq`. The `Event::paste` payload (arbitrary clipboard length,
rare) is a plain owned `String`/`Vec<u8>`.

This is the single biggest deviation from a literal transliteration, but it
preserves observable behavior and upstream's per-frame allocation profile.

### D2. The vxfw `Widget` vtable and identity (blocks Phase 8)

Upstream `Widget` is a hand-rolled fat pointer: `userdata: *anyopaque` plus
optional `captureHandler`/`eventHandler` and a required `drawFn`. `Widget.eql`
is pointer identity over `(userdata, drawFn)`, used pervasively for focus
tracking, mouse enter/leave diffing, and cursor rendering. Handlers mutate
widget state through the shared `userdata` pointer, so the *same* widget is
reached immutably during draw and mutably during event dispatch within one
frame.

Recommended: a `trait Widget { fn draw(&self, ctx) -> Surface; fn
handle_event(&mut self, ctx, ev) {} fn capture_event(&mut self, ctx, ev) {} fn
wants_events(&self) -> bool { false } }`. Optional handlers become default
no-op methods; `wants_events()` replaces the `eventHandler != null` checks in
hit-test/focus. Identity becomes a `WidgetId` (stable per instance) or
`Rc::ptr_eq`. Widgets are owned by the application behind `Rc<RefCell<W>>`;
`Surface` holds a cloned `WidgetRef`. The App loop runs draw, then events, then
draw, so immutable-draw and mutable-event borrows never overlap and `RefCell`
will not double-borrow if we respect the phase ordering.

### D3. Per-frame arena vs cross-frame survival (Phase 8)

`Surface`/`SubSurface`/cell buffers are arena-allocated and dropped each frame,
**except** `MouseHandler.last_frame`, which must survive into the next frame for
hit-testing. Options: ping-pong two frame arenas, or extract only the
hit-relevant skeleton (sizes, origins, widget ids, handler flags, no cell
buffers) into an owned `HitTree` that outlives the arena.

Recommended: own the `Surface` tree with plain `Vec`s for the first cut
(simplest types, no lifetime threading), measure, and only introduce a bump
arena (`bumpalo`) if 60 fps allocation churn shows up in a profile. The
`HitTree` extraction is the clean way to let last-frame data survive.

### D4. Unicode tables: generate, do not snapshot (blocks Phase 1)

uucode generates its tables from the UCD. Our guidance says do the same. So we
build `vaxis-ucd`: a generator that consumes UCD data (`UnicodeData.txt`,
`EastAsianWidth.txt`, `emoji-data.txt`, `GraphemeBreakProperty.txt`) and emits
Rust tables for exactly the four properties vaxis uses: `east_asian_width`,
`general_category`, `is_emoji_presentation`, `grapheme_break`. vaxis
deliberately reimplements width on top of these (it does not call a wcwidth
lib), and its gwidth tests pin emoji/VS/flag/keycap behavior, so we must own the
tables to match.

Tradeoff to workshop: we could instead lean on existing generated crates
(`unicode-properties`/`unicode-general-category` for categories and EAW,
`unicode-segmentation` for UAX#29 grapheme breaks, plus an emoji-presentation
table). That is less code but ties us to those crates' Unicode version, which
may differ from uucode's and could shift a couple of gwidth edge cases. The
no-corners default is the generator; the pragmatic alternative is the crates.
Recommendation: build the generator, and use the crates as a cross-check oracle
in tests.

### D5. `Table` reflection replacement (Phase 10)

`Table.drawTable(anytype)` uses comptime reflection over an arbitrary row
struct's fields: field names become headers, field types drive cell formatting,
and it sniffs `ArrayList`/`MultiArrayList`/slice container kinds by type name.
No 1:1 Rust analog.

Recommended: `vaxis-derive`'s `#[derive(TableRow)]` generates `fn headers() ->
&'static [&'static str]` from field names and `fn cell(&self, col) ->
Cow<str>` from field types (string fields direct, enums via their name,
options unwrapped to `-`, everything else via `Display`/`Debug`). This
preserves the "columns from the type" capability rather than downgrading to
stringly-typed rows. The container-kind sniffing collapses to accepting
`&[R: TableRow]`.

### D6. Concurrency primitives (Phases 4, 7, 11)

The threaded `Loop`, the `queue`, and the terminal emulator's reader thread all
ride on `std.Io`. We map to `std::thread` + `std::sync`. The queue's two-condvar
bounded ring and its spurious-wakeup `while` loops are correctness-load-bearing
(its tests deliberately fire spurious signals and assert blocking durations), so
we port it as a real mutex+condvar structure, not a swap for
`crossbeam::channel` (which would lose the `poll`/`drain`/external-lock surface
the render loop needs). The DA1 capability handshake (`futexWaitTimeout` woken
by the reader thread) becomes a `Condvar` + `AtomicBool`.

### D7. Signals (Phases 7, 11)

Upstream runs real work inside the SIGWINCH and SIGCHLD handlers (takes a mutex,
invokes callbacks, calls `waitpid`). That is not async-signal-safe and is
hostile to a safe Rust port. Recommended: route signals through `signal-hook`
(or a self-pipe) so the handler only flags/writes a byte, and do the real work
(ioctl `TIOCGWINSZ`, posting the resize event, reaping children) on a normal
thread. Honor the in-band-resize switch that disables the SIGWINCH path once DEC
mode 2048 resizes start arriving as parsed events.

### D8. Deviations to reconcile (raise, do not auto-fix)

The investigations found genuine upstream asymmetries and dead code. We
reproduce them with a `NOTE`, or unify them only with explicit sign-off:

- `FlexColumn` vs `FlexRow` distribute flex space differently (different
  measurement set, different formula, different last-element handling,
  saturating vs plain subtraction). Likely organic divergence. Reproduce
  exactly, leave a `NOTE`.
- `Text` and `RichText` carry parallel-but-separate wrap engines (byte-based vs
  cell-based). Candidate for unification, which would be an improvement, so it
  needs sign-off rather than a silent merge.
- `ListView` and `ScrollView` share ~70% of their code but with deliberate
  differences (default `draw_cursor`, indicator const-vs-field, child
  constraints, child trimming, the horizontal axis). Factor a shared engine
  only with sign-off; otherwise port both faithfully.
- `Text.SoftwrapIterator` has dead `consumeLF`/`consumeCR` methods referencing a
  non-existent field; they never get called. Do not port them.
- `DrawContext.width_method` is a file-scoped mutable global. Do not replicate
  global mutability; make it a `DrawContext` field.
- `widgets/ScrollView.readCell` has a latent bug (returns unit from an optional
  fn on the out-of-bounds path). Decide per-case: reproduce or fix-with-NOTE.
- `terminal/key.zig` hits `unreachable` for kitty-flag encoding (upstream is
  itself incomplete here). Mirror the limitation or extend, but call it out.

## TDD methodology (red/green)

The unit of work is a module. For each module:

1. **Red.** Translate every upstream `test "..."` block in that file into a Rust
   `#[cfg(test)] mod tests` with one `#[test]` per upstream test, named
   faithfully (`parse: single xterm keypress` becomes
   `parse_single_xterm_keypress`). Translate the inputs and expected values
   verbatim. Add the public type/function signatures as `todo!()` stubs so the
   test module compiles and the tests fail (or, where signatures do not exist
   yet, let it fail to compile, which is the reddest red).
2. **Green.** Port the implementation until every test in the module passes.
3. **Refactor/verify.** `cargo fmt`, `cargo check`, `cargo clippy`, and
   `cargo test -p vaxis <module>::`. Only then move to the next module.

Conventions:

- Drop `refAllDecls` tests. `cargo check` is the equivalent. The doctest-meta
  convention is kept (Phase 8).
- The Parser, gwidth, Key, queue, and Window print tests are the de-facto specs
  for those modules. Port their assertions character for character. The reports
  in this effort already enumerate the exact inputs and expected outputs.
- Concurrency tests (queue) port their timing harness too: `io.concurrent` to
  `std::thread::spawn`, `io.sleep` to `thread::sleep`, atomics with the same
  orderings. The "blocked push took > 5 ms" and spurious-wakeup assertions are
  the point of those tests and must survive.
- Renderer/runtime tests assert against captured writer output, so the in-memory
  tty backend must exist before Phase 6. Build it in Phase 5 (it is also the
  upstream `TestTty`).

A running tally of ported vs upstream tests per module is the progress metric.
Target: all 170 upstream behavioral tests (minus `refAllDecls`) green, plus new
smoke tests for the untested modules (tty, the terminal emulator, most legacy
widgets) and the examples compiling.

## Phased port order

Ordered by the dependency graph, leaves first. Each phase lists its modules and
the upstream tests to port (the red set).

### Phase 0: Scaffolding

- Create the `vaxis`, `vaxis-ucd`, `vaxis-derive` crates and add to the
  workspace. Add new workspace deps (`nix` features, `signal-hook`, `phf`,
  `windows-sys`, UCD parser).
- Stand up `lib.rs` with empty modules and the `Winsize` type.
- Decide D1 (grapheme ownership) and D4 (Unicode tables) up front, since they
  block Phase 1.
- Tests: none yet. Gate: `cargo check` passes on the empty skeleton.

### Phase 1: Leaf primitives

Modules: `ctlseqs`, `vaxis-ucd` + `gwidth` + `unicode`, `cell`, `mouse`, `key`,
`event`, `grapheme_cache`.

- `ctlseqs`: port the constant table verbatim for literal sequences; for
  parameterized sequences write typed encoder functions (Rust `format!` syntax,
  not Zig's `{d}`/`{x:0>2}`/`{f}`). This is the spec for all wire output.
- `gwidth`: port `Method`, `eawToWidth`, and `gwidth`. Carry over the hardcoded
  zero-width codepoint list and the VS16/VS15/emoji-presentation/regional-
  indicator/keycap/skin-tone rules verbatim.
- `unicode`: thin grapheme-iterator adapter yielding `{start, len}` with a
  `.bytes(str)` accessor (back it with the generated grapheme-break table).
- `cell`: `Cell`, `Segment`, `Character`, `CursorShape`, `Hyperlink`, `Scale`,
  `Style` (+ `Underline`), `Color` (+ `Kind`, `Report`, `Scheme`,
  `rgbFromUint`, `rgbFromSpec`). Apply D1 to the grapheme field.
- `mouse`: `Mouse`, `Shape`, `Button` (sparse `repr(u8)` + `TryFrom`),
  `Modifiers` (u3), `Type`.
- `key`: `Key`, `Modifiers` (u8 bitflags, exact bit order), `KittyFlags` (u5),
  the ~120 codepoint constants, `name_map` (phf), and the `matches*` family.
- `event`: the `Event` enum (owned `paste`, owned/copied `Key.text` per D1).

Red set: `gwidth` ~11 behavioral tests, `cell` `rgbFromSpec`, `key` 5 `matches`
tests. (`mouse`, `event`, `unicode`, `ctlseqs`, `grapheme_cache` are
refAllDecls-only upstream, so add targeted new unit tests where behavior is
non-trivial, e.g. `Button` enum round-trip, `rgbFromUint`.)

### Phase 2: Input parser

Module: `parser`.

- Port the lookahead/recursive-descent dispatcher (not a byte FSM; the upstream
  `State` enum is a red herring). Ground state, alt-combos, SS3, CSI (legacy
  keys, kitty keyboard, mouse SGR + X10, focus, DA1/DSR/DECRPM/XTWINOPS
  capability probes), OSC (4/10/11/12 color reports, 52 paste), and DCS/SOS/PM/
  APC skipping.
- Preserve the `(event: Option, n: usize)` resync contract exactly (`n==0`
  means incomplete, retry from the same offset).
- Reproduce the kitty modifier `@bitCast(mask - 1)` into `Modifiers`, the
  shift-synthesis rule, and saturating signed mouse coords.

Red set: all 36 behavioral Parser tests (the full table with input bytes and
expected parse results is in the input-layer report).

### Phase 3: Screen and Window

Modules: `screen`, `internal_screen`, `window`.

- `screen`: flat row-major `Vec<Cell>`, bounds-checked write/read, cursor and
  shape state, width method.
- `internal_screen`: the back buffer that owns grapheme/uri bytes, plus the
  asymmetric `eql(Cell)` used by the diff.
- `window`: the clipped view (resolve D-level borrow questions: `Window` carries
  offsets + a screen handle; route mutation through the shared screen). Port the
  child/border logic and the full print engine (grapheme/word/none wrap).

Red set: `internal_screen` out-of-bounds test, `window` size/offset tests (5),
and the two big print tests (`print: grapheme`, `print: word`) which are the
wrap-engine spec (~25 `(col,row,overflow)` cases).

### Phase 4: Queue

Module: `queue`.

- The bounded thread-safe two-mirror ring buffer with two condvars, blocking
  `push`/`pop`, non-blocking `try_*`, `poll`, and the external-lock `drain`
  surface. Preserve the `while`-loop-on-condition (spurious-wakeup safety).

Red set: 7 behavioral queue tests, including the timing-sensitive
"Fill, block, fill, block" and "Drain, block, drain, block" and the
2-readers/2-writers cases. Port the threaded harness.

### Phase 5: TTY

Module: `tty` (`posix`, `test_backend`, and a `windows` stub gated off).

- `posix`: `/dev/tty`, raw mode via `nix` termios (the exact flag set from the
  report), `TIOCGWINSZ`, SIGWINCH via signal-hook (D7). Buffered writer
  separate from the runtime.
- `test_backend`: in-memory `Vec<u8>` writer plus a pipe-backed reader and a
  fixed 40x80 winsize, so renderer tests are hermetic. This mirrors upstream
  `TestTty` and is required by Phase 6.
- The `global_tty` + panic recovery becomes a `static` plus a
  `std::panic::set_hook` that writes the reset string. (Best-effort, exactly as
  upstream.)

Red set: refAllDecls-only upstream. Add new unit tests: raw-mode round-trip
(enter/restore termios), winsize parse, and the test-backend writer capture.

### Phase 6: Vaxis runtime and Image

Modules: `vaxis`, `image`.

- `vaxis`: `Capabilities`, `Options`, the `state` toggles, the ordered
  `reset_state` teardown, `resize`, `window`, the capability-detection flow
  (`queryTerminalSend` batch, `queryTerminal` blocking on the handshake,
  `enableDetectedFeatures` with env overrides), and the diff `render` (the
  ~470-line heart: width/wrap/skip logic, cursor repositioning,
  image placement, SGR style diffing, hyperlinks, scaled text). Plus all the
  per-feature emit methods (alt screen, mouse modes, kitty keyboard, sync,
  titles, notifications, clipboard, colors, cursor/mouse shapes) and
  `prettyPrint`.
- `image`: `Image{id,width,height}`, `DrawOptions`, `Placement`, the
  scaling/`draw`/`cell_size` geometry, and kitty-graphics transmission
  (`transmitLocalImagePath`, `transmitPreEncodedImage`, `transmitImage` via the
  `image` crate, `loadImage`, `freeImage`, the 4096-byte chunking and format
  codes 24/32/100). Kitty only. No sixel/iTerm path exists upstream.

Red set: `render: no output when no changes` (asserts zero bytes emitted on a
clean render). Add new tests against the in-memory backend: a single-cell write
emits the expected bytes, the diff skips unchanged cells, `reset_state` emits
the documented teardown order.

### Phase 7: Loop and async front-end

Module: `loop` (the threaded loop), plus a shared `input` core and an `async`
front-end.

- Factor the **byte-pump core** first: read bytes, run the partial-sequence
  resync (`copy_within` for the overlapping shift), parse with the ported
  `Parser`, fold capability events into `Vaxis.caps`, copy `Key.text` per D1,
  wake the DA1 handshake, and produce user-typed events. The subset/superset
  event typing (`@hasField`) becomes a `TryFrom<InternalEvent> ->
  Option<UserEvent>` conversion trait per user event type. This core is sink-
  and source-agnostic.
- **Threaded `Loop`** (faithful): a `std::thread` reader driving the core,
  pushing onto the `Queue`, with the SIGWINCH bridge per D7. Ported verbatim.
- **Async front-end**: an `AsyncFd<File>` over `/dev/tty` driving the same core
  and emitting on a `tokio::sync::mpsc`. No reader thread. This is the path the
  aj integration will use.

Red set: the `Loop` smoke test (custom user event type with a field absent from
the internal event, proving the decoupling). Add new tests for the resync logic,
the conversion trait, and an async-front-end test that feeds bytes through a
pipe and asserts the decoded `Event` stream.

### Phase 8: vxfw core and App

Modules: `vxfw` (core types) and `vxfw::App`.

- Core: `Event`, `Command`, `Tick` (descending-sorted deadlines),
  `EventContext` (per-frame redraw latch vs per-event consume/phase),
  `DrawContext` (constraints, `withConstraints`, width method as a field per
  D8), `Size`/`MaxSize`, the `Widget` trait + identity (D2), `FlexItem`,
  `Point`/`RelativePoint` (i32), `HitResult`, `CursorState`, `Surface`
  (buffer contract, `writeCell` clips/`readCell` asserts, `trimHeight`,
  `hitTest`, `render`, z-sort), `SubSurface` (`containsPoint`).
- `App`: the frame loop (sleep, timers, event drain, layout, mouse update,
  render), `MouseHandler` (capture/target/bubble with local-coord translation,
  enter/leave diffing), `FocusHandler` (focus path, focusable assertion). The
  cross-frame `last_frame` survival per D3.
- The doctest meta-test: a Rust test that walks `src/vxfw/`, and for each widget
  module asserts a doctest (`#[test] fn <module>()` or `_doctest`) and a
  compiles marker exist. Keeps the upstream convention without parsing an AST.

Red set: `SubSurface: containsPoint`, `Surface: satisfiesConstraints`,
`App` "timer consume does not leak to the next event", and the doctest
meta-test (reframed).

### Phase 9: vxfw widgets

Order (foundation first): `Text` (every other widget's test builds on it) ->
`Center`, `Padding`, `SizedBox`, `Border`, `Spinner` -> `FlexColumn`,
`FlexRow` -> `RichText` -> `Button` -> `ListView`, `ScrollView` ->
`ScrollBars`, `SplitView` -> `TextField`.

Each widget: port its `test <Widget>` doctest (red), implement (green). Note the
heavier test files: `Text` (10: SoftwrapIterator + LineIterator + Text),
`RichText` (3), `ListView` (7, incl. `jumpToItem`/`scrollToBottom` with call-
count assertions and ASCII-diagram scroll scenarios), `ScrollView` (3),
`TextField` (17, incl. the full word-motion/kill battery and the gap-buffer
test). Reproduce the D8 asymmetries with `NOTE`s.

Red set: the per-widget doctests, ~50 tests total across the framework widgets.

### Phase 10: legacy widgets

Order: `alignment`, `Scrollbar`, `widgets::ScrollView`, `LineNumbers`
(trivial, infallible) -> `TextInput` (the only legacy widget with tests, 16 of
them; validates the gap buffer and Unicode word logic) -> `TextView`,
`CodeView` (the shared `Buffer` + per-byte styling) -> `View` (oversized
off-screen surface) -> `Table` (needs D5's `#[derive(TableRow)]`).

Red set: the 16 `TextInput` tests (word motions across separators, path
separators, hyphens, dots, underscores-as-word-chars, non-ASCII like `café`/
`über`/em-dash/ideographic space, the ZWJ-emoji insert assertion, and the
low-level `Buffer` test). The rest of the legacy widgets are untested upstream,
so add smoke/golden tests.

### Phase 11: terminal emulator

Modules under `widgets/terminal/`: `ansi` -> `Parser` (the output/VT parser,
distinct from the input `Parser`) -> `Screen` (the emulator's own grid with
per-cell owned graphemes and the full SGR machine) -> `key` (key-to-PTY
encoding) -> `Pty` (`nix::pty`) -> `Command` (child spawn via
`Command::pre_exec` doing `setsid`/`TIOCSCTTY`/`dup2`, SIGCHLD reaping per D7)
-> `Terminal` (the orchestrator: reader thread, triple-buffered screens behind a
mutex, `dirty` flag, sync-mode interplay, event queue).

Linux-first (upstream `@compileError`s elsewhere for `Pty`/`Command`). No
upstream tests. Add smoke tests: spawn a trivial child (`printf`), pump output,
assert screen contents and the `exited` event. Keep the post-fork-pre-exec code
allocation-free and async-signal-safe.

### Phase 12: examples

Port all 14 examples to `vaxis/examples/<name>.rs`:

- Legacy: `cli`, `main`, `text_input`, `text_view`, `table`, `view`, `vaxis`,
  `image`, `vt`.
- vxfw: `counter`, `list_view`, `fuzzy`, `split_view`, `scroll`.

Examples that shell out (`fuzzy` runs `fd`, `text_input` spawns `nvim`) port
their process spawning to `std::process`. The comptime-computed world maps in
`view.zig` become `const`/`include_str!` data. Gate to compile in CI; document
which need a real terminal to run.

### Phase 13: integration and parity

- Cross-module integration tests under `vaxis/tests/`.
- A render-parity harness: drive a known widget tree and assert the exact byte
  stream against the in-memory backend, so we can detect renderer regressions.
- Confirm the full `cargo test -p vaxis` suite is green and the example targets
  build.

## Progress tracking

Track per-module: upstream test count, ported test count, green count. The port
is done when:

- All upstream behavioral tests (170 minus `refAllDecls`) are ported and green.
- Untested upstream modules have smoke tests and pass.
- All 14 examples build (and the non-interactive ones run).
- `cargo fmt`, `cargo check`, `cargo clippy --workspace --all-targets` are clean.

## Open questions for the user

1. D1 grapheme ownership: confirm Option A (inline small string, recommended
   default) with Option B (interning) kept behind the same accessor as the
   fallback, vs going straight to B. (Option C / lifetime-threaded zero-copy is
   ruled out by the async goal.)
2. D4 Unicode tables: confirm building the `vaxis-ucd` generator (recommended,
   no corners) vs reusing existing `unicode-*` crates.
3. D8 deviations: confirm "reproduce upstream faithfully with NOTE comments" as
   the default, with unification of the duplicated engines (`Text`/`RichText`,
   `ListView`/`ScrollView`) only on explicit request.
4. Windows tty backend and the Linux-only terminal emulator: confirm Linux-first
   with Windows staged later (matches upstream's own platform gating).
5. Crate shape: confirm one `vaxis` crate plus `vaxis-ucd` and `vaxis-derive`,
   vs a multi-crate split.
6. Async front-end: confirm the `AsyncFd`-based async input source + tokio-timer
   vxfw driver as the integration seam (built alongside the faithful threaded
   `Loop`/`App`, not replacing them).
