# Spec A: `AsyncApp` in `vaxis`

## Status: proposal (not started)

Companion to `docs/aj-next-vaxis-plan.md`. This spec adds an async driver for the
`vxfw` widget framework to the `vaxis` crate, so an async (tokio) host like
`aj-next` can drive a widget tree from its own `select!` loop while reusing all
of `vxfw`'s subtle machinery (layout, mouse hit-testing, focus, tick scheduling,
diff rendering).

The existing `vxfw::App` (in `src/vaxis/src/vxfw/app.rs`) stays as-is. It is the
faithful, synchronous, thread-based runtime. `AsyncApp` is a sibling that shares
`App`'s internals.

## Why not `App::run`

`App::run` owns a synchronous frame loop: it paces frames with
`std::thread::sleep`, drains input from a threaded `Loop` via `try_event`, and
blocks up to a second in `query_terminal` for capability detection. An async host
cannot use it, for two reasons:

1. **It owns the loop.** `aj-next` must `select!` terminal input against many
   other sources: the `AgentEvent` channel, turn-completion joins, background
   task wakes, the footer/git tick, the theme-watcher channel. A closed frame
   loop cannot host those arms.
2. **It blocks.** `std::thread::sleep` and the blocking `query_terminal` would
   stall the tokio runtime.

Global-chord interception is not a reason here. vxfw's capture phase already lets
a root-level widget pre-empt the focused widget in-tree (Spec F), independent of
the runtime, so global chords need no host-loop hook.

So `AsyncApp` is a **host-driven driver**, not a batteries-included loop. The
host owns the `select!`. `AsyncApp` owns the terminal and the vxfw engine, and
exposes them behind a narrow API so the host never re-implements the subtle
mouse/focus/tick/render logic. That subtle logic is exactly what we do not want
a second copy of, which is why it lives in `vaxis`.

## Design: extract a shared core, add an async driver

`App`'s internals split cleanly into "loop style" (threaded, synchronous) and
"engine" (terminal + vxfw dispatch + render). The engine is loop-agnostic. We
extract it so both runtimes share it.

### The shared engine

Introduce `AppCore` (crate-internal to `vxfw`) holding what the engine needs:

```
struct AppCore {
    vx: Vaxis,
    tty: Box<dyn Tty>,
    timers: Vec<Tick>,            // sorted by deadline, soonest last
    wants_focus: Option<WidgetRef>,
}
```

The following move onto `AppCore` (or become free functions over it), unchanged
in behavior from today's `App`:

- `do_layout(root) -> Surface`, `render(surface, focused)`, `handle_command(cmds)`,
  `check_timers(ctx)`, `add_tick(tick)`.
- `MouseHandler` and `FocusHandler`, whose methods currently take `&mut App` only
  to reach `handle_command`. They take `&mut AppCore` instead.
- The dispatch helpers (`dispatch_event`, `dispatch_capture`, `local_mouse_event`,
  `diff_hit_lists`, `surface_point`, `reset_event_state`).

`App` becomes `AppCore` + the threaded `Loop` + the synchronous `frame_loop`. Its
public API and behavior are unchanged. `AsyncApp` becomes `AppCore` + the async
input receiver + the driver methods below. This is a refactor of `App`, not a
rewrite, and `App`'s existing test (`timer_consume_does_not_leak_to_the_next_event`)
plus the widget tests keep it honest.

### The `LoopEvent` type

`LoopEvent` (today private in `app.rs`) is the `Send` subset of the vxfw `Event`
that the reader produces (it drops `Event::App`, which holds a non-`Send` `Rc`).
Both runtimes need it: `async_input` is generic over `E: FromEvent`, and
`LoopEvent` already implements `FromEvent`. Promote it to a crate-internal module
shared by `App` and `AsyncApp`. `AsyncApp` does not need `Event::App`: host
events (AgentEvents) arrive on the host's own channel and never travel through
the input reader.

## `AsyncApp` API

```
pub struct AsyncApp { /* AppCore + input receiver + resize source + handlers */ }

pub struct Frame { pub quit: bool }   // outcome of one dispatch step

impl AsyncApp {
    /// Build over a runtime, a writer-side tty, and a read source (a
    /// separately opened terminal fd, see "The read source" below).
    pub fn new(vx: Vaxis, tty: Box<dyn Tty>, source: OwnedFd) -> AsyncApp;

    /// Terminal setup + first layout. Async so capability detection does not
    /// block the runtime. Enters the alt screen, turns on mouse and bracketed
    /// paste, subscribes to color-scheme updates, spawns the async reader,
    /// posts Init + FocusIn, and draws the first frame.
    pub async fn init(&mut self, root: WidgetRef, opts: Options) -> Result<(), Error>;

    /// Await the next terminal input event (key, mouse, paste, focus, resize,
    /// color report). Returns None when the reader has ended.
    pub async fn next_input(&mut self) -> Option<Event>;

    /// Run one input event through the vxfw engine: mouse hit-test + enter/leave
    /// + capture/target/bubble, or focus-path dispatch, or an internal resize.
    /// Applies the resulting commands and any focus change. Sets the per-frame
    /// redraw latch as widgets request it. Returns whether a handler asked to
    /// quit.
    pub fn handle_input(&mut self, event: Event) -> Frame;

    /// Dispatch a host event to the focused widget as Event::App(UserEvent).
    /// Optional path for hosts that want widget-targeted app events; aj-next's
    /// AgentEvents go through the reducer instead, so this is rarely used.
    pub fn post_app_event(&mut self, event: UserEvent) -> Frame;

    /// The soonest pending tick deadline, for the host's timer select arm.
    pub fn next_tick_deadline(&self) -> Option<Instant>;

    /// Fire every due tick (delivering Event::Tick) and apply commands.
    pub fn fire_due_timers(&mut self) -> Frame;

    /// Mark the frame dirty; the next render_if_needed draws.
    pub fn request_redraw(&mut self);
    pub fn needs_redraw(&self) -> bool;

    /// Lay out the root, update mouse/focus against the fresh surface, and
    /// diff-render to the tty. Clears the redraw latch. The root pulls host
    /// state (the model) through its own Rc handles during layout.
    pub fn render(&mut self, root: &WidgetRef) -> Result<(), Error>;

    /// render() only when needs_redraw(); the common per-iteration call.
    pub fn render_if_needed(&mut self, root: &WidgetRef) -> Result<(), Error>;

    /// Escape hatches for host commands vxfw's Command enum does not cover
    /// (kitty graphics, custom sequences): a scoped writer, and the runtime.
    pub fn vaxis(&mut self) -> &mut Vaxis;
    pub fn with_writer<R>(&mut self, f: impl FnOnce(&mut dyn Write) -> R) -> R;

    /// Restore the terminal: stop the reader, leave the alt screen, disable
    /// mouse and paste, show the cursor, flush.
    pub async fn shutdown(self);
}
```

`Frame` carries only `quit`. The redraw latch lives on `AsyncApp` (it persists
across a whole iteration and clears on render), matching `App`'s per-frame
semantics.

## Terminal setup: async capability detection

`App::run` calls the blocking `query_terminal`, which parks up to a second
waiting for the DA1 response. `AsyncApp::init` must not block the runtime, so
the blocking call splits into three steps:

1. `vx.query_terminal_send(writer)` to emit the capability-probe batch
   (already public).
2. Await the DA1 handshake with a timeout, off the executor:
   `Shared::wait_for_da1` becomes `pub(crate)` and `init` runs it on the
   blocking pool (`spawn_blocking` over the `Arc<Shared>` clone). The async
   reader already folds capability responses into `Shared` and fires the
   condvar via `notify_da1`, so no new wake mechanism is needed. DA1 never
   reaches the event channel (`InputCore::dispatch` intercepts it before the
   sink), so awaiting the channel instead would not wake. Parking one
   blocking-pool thread for at most a second, once at startup, is exactly what
   the blocking pool is for.
3. `vx.query_terminal_finish(writer)`, a new method carrying the post-wait
   tail of `query_terminal`: `set_queries_done(true)`, snapshot
   `caps = shared.detected()`, sync `screen.width_method`, then
   `enable_detected_features`. `query_terminal` itself becomes
   send + wait + finish. The `set_queries_done(true)` step matters most on
   **timeout**: without it the input parser keeps treating real keypresses
   (e.g. F3) as probe replies and swallows them.

The rest of setup mirrors `App::run`: `resize` from a live `tty.get_winsize()`
before first layout (the async reader posts no initial winsize event),
`enter_alt_screen`, `set_bracketed_paste(true)`,
`subscribe_to_color_scheme_updates`, force `caps.sgr_pixels = false`, then
`set_mouse_mode(true)`. The first frame draws unconditionally.

## The read source: a fresh fd, not a dup

`async_input` sets `O_NONBLOCK` on its source fd. A fd obtained via `dup(2)`
(what `PosixTty::dup_reader` does) shares file status flags with the writer
through the common open file description, so the writer would silently turn
non-blocking and large frames would start failing with `WouldBlock` once the
kernel tty buffer fills (easy over ssh). The threaded loop never hits this
because its reads stay blocking.

So the read source for `AsyncApp` must be a separate open file description:
add `PosixTty::open_reader(&self) -> io::Result<OwnedFd>` doing a fresh
`open("/dev/tty", O_RDONLY)`. Raw mode lives in the termios of the terminal
itself, not the description, so the new fd sees the raw mode `PosixTty`
installed. Tests keep passing pipe/PTY fds directly, as `async_reader`'s tests
already do. `OwnedFd` stays the parameter type: `AsyncFd` takes ownership and
close-on-drop gives clean teardown.

## Resize: fix the live-winsize gap

`App::run` wires a **fixed-snapshot** winsize source for the out-of-band SIGWINCH
path (it re-reports the initial size on every resize). `AsyncApp` does it
correctly:

- **In-band (preferred).** After detection, if `shared.in_band_resize()` is true
  (DEC mode 2048), the terminal reports resizes as `Event::Winsize` through the
  async input receiver. `handle_input` handles `Event::Winsize(ws)` by calling
  `vx.resize(writer, ws)` and setting redraw. No signal handling needed.
- **Out-of-band.** Otherwise, `AsyncApp` owns a
  `tokio::signal::unix::signal(SignalKind::window_change())` stream and exposes it
  so the host can add a select arm, or (simpler) `AsyncApp` folds SIGWINCH into
  `next_input` and yields a synthesized `Event::Winsize` after a **live**
  `tty.get_winsize()` ioctl. Either way the reported size is the real current
  size, not a stale snapshot.

Recommendation: fold SIGWINCH into `next_input` so the host sees a single input
stream and resize "just works" on both paths. This is the concrete live-resize
fix the plan promised.

Note: widgets never receive `Event::Winsize`. Both runtimes consume it (resize
plus redraw) before focus dispatch, and widgets learn the size purely through
layout. The stale `Event::Winsize` doc comment in `vxfw.rs` ("Always delivered
once when the App starts.") gets fixed as part of this work.

## Teardown

Simpler than `App::run`. The threaded loop needs a device-status-report to
unblock its blocking `read` before `stop` can join. The async reader instead
selects on a `shutdown` `Notify` (see `async_reader::reader`), so `shutdown`
just calls `AsyncInput::shutdown()` and awaits `join()`, then `vx.reset_state`
and flush. No DSR dance.

## The host loop (what `aj-next` writes)

`AsyncApp` keeps the subtle engine. The host owns the multiplexing:

```
app.init(root.clone(), Options::default()).await?;
loop {
    tokio::select! {
        biased;
        Some(ev) = app.next_input() => {
            // No host-side pre-interception. Global chords live in the
            // KeymapController widget near the root and are matched in the
            // capture/bubble phases (Spec F); handle_input drives that walk.
            let frame = app.handle_input(ev);
            if frame.quit { break; }
        }
        Some(agent_ev) = agent_rx.recv() => {
            reduce(&mut chat_state, &mut core.lifecycle, &agent_ev);  // Spec C
            app.request_redraw();
        }
        _ = sleep_until_next_deadline(&app) => { app.fire_due_timers(); }
        _ = footer_tick.tick() => { host.refresh_footer(); app.request_redraw(); }
        Some(done) = turns.join_next() => { host.on_turn_done(done); app.request_redraw(); }
        Some(theme) = theme_rx.recv() => { host.replace_theme(theme); app.request_redraw(); }
        // ... task wakes, etc.
    }
    app.render_if_needed(&root)?;
}
app.shutdown().await;
```

The root widget and its children hold `Rc<RefCell<ChatState>>` (and theme, config)
handles, so `render` re-lays-out from the freshly-reduced model with no extra
plumbing.

Tick borrow note: `next_tick_deadline()` + a `tokio::time::sleep_until` computed
each iteration avoids holding a `&self` future across a `&mut self` call inside
the `select!`. `sleep_until_next_deadline` returns a future that resolves far in
the future when there are no timers.

## Engine-fidelity notes

Subtleties of `App`'s loop that `AsyncApp` must reproduce, not simplify:

- The `EventContext` redraw latch is per-frame, not per-event. `AsyncApp` keeps
  the latch across `handle_input` / `fire_due_timers` / `post_app_event` calls
  and clears it only when `render` draws. Per-event state (`consume_event`,
  `phase`) resets after every dispatch, and commands drain after every
  dispatch, including after applying `wants_focus`.
- `wants_focus` applies at two points: after each input batch and again after
  `update_mouse` during render. `handle_input` covers the first, `render` the
  second.
- `render` reproduces the double-layout dance: layout, `update_mouse` (which
  may set redraw or request focus), re-apply `wants_focus`, re-layout if
  redraw got re-set, then store the surface as `last_frame`, update the focus
  path, and diff-render.
- `AsyncApp` futures are `!Send` (`WidgetRef` is `Rc<RefCell<...>>`). Fine on a
  top-level `block_on` or a `LocalSet`. Document it.
- Terminal restore is the host's job via `shutdown()`. On an unclean drop, the
  reader task aborts and `PosixTty::Drop` restores termios, but alt screen,
  mouse, and kitty-keyboard state stay on (the panic hook covers panics).
  Document that hosts must call `shutdown`.

## Tests

- An async smoke test that feeds bytes through a pipe (mirroring
  `async_reader`'s tests) into an `AsyncApp` over a `TestTty`, asserting a key
  reaches a widget and a redraw is requested.
- A resize test: deliver an `Event::Winsize` and assert the screen resized and a
  redraw was latched.
- A tick test: schedule a `Command::Tick`, advance time, assert `fire_due_timers`
  delivers `Event::Tick` to the target.
- The refactor keeps `App`'s existing tests green (the shared-core extraction is
  behavior-preserving).

## Decisions

- **A-1. Driver vs batteries-included. Resolved: driver now.** Ship the
  host-driven driver. Add a thin `AsyncApp::run(root, opts, handler)` convenience
  later only if an example wants it.
- **A-2. SIGWINCH folding. Resolved: fold.** Fold out-of-band SIGWINCH into
  `next_input` as a synthesized `Event::Winsize` after a live `get_winsize`, so
  the host sees one input stream and resize works on both the in-band and
  out-of-band paths.
- **A-3. Input dispatch seam. Resolved: native capture/bubble, no host
  interception.** Global chords live in a root-level `KeymapController` widget,
  matched in vxfw's capture phase (pre-empting chords and leader sequences) and
  bubble phase (shadowable shortcuts). The host loop just calls `handle_input`.
  The keymap, the full leader-sequence engine, and the dispatch-debug aids are
  specified in Spec F (`docs/vaxis-input-keymap-spec.md`).

## Out of scope

The threaded `App` and `Loop` are untouched. Windows tty support stays a stub, as
in the rest of the crate. Image transmission (`vaxis::image`) is reachable through
`vaxis()`/`with_writer` but this spec does not add a vxfw image widget.
