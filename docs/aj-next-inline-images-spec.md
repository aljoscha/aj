# Spec: `aj-next` inline images

## Status: accepted, not started

Companion to `docs/aj-next-vaxis-plan.md`, track **9-Chrome**. This spec covers
rendering tool-result images inline in the `aj-next` transcript, using the
`vaxis` crate's native graphics support instead of porting `aj-tui`'s image
component. It also introduces a runtime terminal-capability seam that the
existing hyperlink gate can later migrate onto.

`aj-next` today renders every image as a dim text placeholder
(`tool_cell.rs:397-413`, `[image: <mime> · WxH]`), with the comment "inline
terminal images are deferred". This spec un-defers it.

## Goal and scope

Render an image graphically, inline in the transcript, where the terminal
supports it, matching the behavior a user gets from `aj` classic. Fall back to
the current text placeholder everywhere else.

**In scope:**

- Tool-result images. These are the images `read_file` produces when it opens
  an image file (`aj-tools`), carried as a `UserContent::Image` in the tool
  entry's content with display metadata in `ToolDetails::Image`. This is the
  one path `aj` classic renders graphically, so it is the reference bar.
- Kitty graphics protocol, gated on a real runtime capability probe.
- The existing `show_image_in_terminal` config flag as the user-facing on/off.
- Graceful text fallback when the capability, the config, or the protocol
  support is absent.

**Explicitly out of scope (see Open decisions and Non-goals):**

- iTerm2 and sixel protocols. `vaxis` implements only kitty graphics.
- User-message images (the CLI `@file` path). `aj` classic renders these as
  text too, and the wire type lacks the dimensions we would need.
- Markdown `![alt](url)` images. Both frontends render these as an alt-text
  link, unchanged here.
- Print and export image handling. Those paths are already defined elsewhere
  and are not affected.

## Reference behavior (the bar)

`aj` classic renders images through `aj-tui`'s `components/image.rs`. The
salient facts that set our bar:

- It draws **tool-result images only**. User-message content and markdown
  images are text. The wiring lives in the tool-execution component
  (`aj/src/modes/interactive/components/tool_execution.rs`), driven by
  `ToolDetails::Image` and gated on `show_image_in_terminal`, the detected
  terminal protocol, and (for kitty) a PNG-only constraint.
- It supports **kitty and iTerm2**, chosen by env-based detection, with
  tmux/screen forcing images off.
- The cell footprint is capped at `(40, 20)` and derived from the image's
  displayed dimensions.
- Absent inline support it prints `[image: <name> · <mime> · WxH]`.

We meet this bar for kitty terminals and fall back to text otherwise. Whether we
also want iTerm2 is Open decision 1.

## What `vaxis` gives us

`vaxis` has complete, tested kitty-graphics support at the low level, and a real
capability probe. It has no widget-framework (`vxfw`) integration. The
consequences drive the whole design.

### Capability probe

`Vaxis` sends a kitty-graphics query in its startup probe batch and folds the
APC reply into `caps.kitty_graphics: bool`, snapshotted for lock-free reads once
device attributes arrive. This is the same mechanism `caps.rgb` uses, and
`aj-next` already reads `caps.rgb` right after `app.init(..)`. The image gate
reads `caps.kitty_graphics` at the same seam.

On a terminal with no kitty support the probe simply leaves the flag false, and
every transmit method returns `Error::NoGraphicsCapability` up front, so the
fallback path is well-defined.

### The `Image` API and the cell-placement model

- Transmission (`Vaxis::load_image` / `transmit_image`) allocates a
  terminal-assigned id, uploads the bytes once (re-encoding to PNG), and returns
  an `Image` handle carrying the id and pixel dimensions. It requires `&mut
  Vaxis` and a writer.
- A `Cell` carries `image: Option<Placement>`, where `Placement { img_id,
  options }` is `Copy`. Writing a placement cell into a surface is how an image
  gets drawn.
- The renderer never skips an image-bearing cell in its diff, and it clears all
  placements at the top of every frame then re-emits them. So **image bytes are
  transmitted once and live in terminal memory by id, while the placement is
  re-emitted on every painted frame** at the cell's current position.

This last point is the crux: because the transcript recomposes its visible slice
and the app re-blits from scratch every frame, a placement baked into an entry's
cell buffer moves with the text as the user scrolls, with no extra bookkeeping.
The only per-frame cost is re-emitting the placement command, which references an
already-uploaded id and is cheap.

### The `vxfw` gap

There is no image support in `vxfw`. A `Widget::draw` receives only a
`DrawContext` (constraints, `cell_size` in pixels-per-cell, width method). It has
no handle to `Vaxis` or a writer, so it **cannot transmit**. It can only
**place** an already-transmitted image by writing a `Cell { image:
Some(Placement { img_id, .. }), .. }` into its returned surface.

This splits the feature cleanly: transmission and id lifecycle live in the host
loop, and the widget layer only places.

## Architecture

### 1. A runtime terminal-capability seam

Introduce a small `TerminalCaps` value threaded through the transcript's styling
path, alongside the theme. Today `aj-next` gates hyperlinks on a compile-time
`terminal.rs` constant (`TERMINAL_HYPERLINKS: bool = true`) with a TODO to "thread
a real capability once vaxis detects hyperlink support". We generalize that seam:

- `TerminalCaps { images: bool, hyperlinks: bool, .. }` is read once after
  `app.init(..)`, populated from `app.vaxis().caps`.
- `images` comes from the real `caps.kitty_graphics` probe.
- `hyperlinks` stays optimistic for now. `vaxis` does not probe OSC 8 support,
  so we cannot make it a real capability yet. It keeps its current default and
  moves from a compile-time constant to this runtime struct, so the day `vaxis`
  grows a hyperlink probe it becomes a one-line change here. This resolves the
  intent of the `terminal.rs:9` TODO (establish the runtime-capability seam)
  without inventing a probe `vaxis` does not have. We record that limitation in
  a `NOTE`.

`TerminalCaps` is threaded into `TranscriptStyles` the same way the theme is, via
`restyle()`, so a capability change would re-flow rendering. In practice caps are
fixed for the session.

### 2. Transmission and id lifecycle live in the host

Transmission needs `&mut Vaxis` and a writer together. `AsyncApp` exposes those
as separately-borrowed fields (`vaxis()` and `with_writer`), so neither alone
suffices. Add a thin method to `vxfw::AsyncApp`:

```rust
/// Transmit an image into the terminal's graphics store, returning a handle
/// whose id can be placed into a surface cell. Requires the kitty graphics
/// capability, returning `Error::NoGraphicsCapability` otherwise.
pub fn load_image(&mut self, source: Source) -> Result<Image, Error>;

/// Delete a transmitted image from the terminal's graphics store.
pub fn free_image(&mut self, id: u32);
```

Internally these borrow both `AppCore` fields at once
(`self.core.vx.load_image(&mut self.core.tty.writer(), source)`), which the
public API cannot express. This is a genuine framework addition, not a
workaround, and it is the single seam through which `aj-next` transmits.

### 3. The image store

The host owns an `ImageStore` keyed by `(AgentId, EntryId)`:

```rust
struct Transmitted { img_id: u32, px: (u32, u32) }
// (AgentId, EntryId) -> Transmitted, plus a pending set (see below).
```

Its contract:

- One transmitted id per tool-result-image entry. The key matches the transcript
  render cache's key, so the two stay aligned.
- Ids are freed and the store cleared on session switch (`reset_to_tail` /
  rebind). Within a session, a transmitted id is kept even after its entry
  scrolls off screen, so scrolling back does not re-transmit.
- Terminal memory is therefore bounded by the images a user has actually
  scrolled into view during the session, each already resized to the inline
  budget by `read_file`. An LRU cap is a possible future tightening, noted but
  not built.

### 4. Lazy, draw-driven transmission

Transmission is driven by the draw, not by the event that adds the entry. When
the transcript builder is about to draw a tool-result-image entry:

- If the store has a transmitted id, it computes the footprint and writes the
  placement cell (see 5).
- If not, it records the `(AgentId, EntryId)` key in a shared pending set and
  reserves the image's rows filled with a neutral placeholder. The footprint is
  known from `ToolDetails::Image`'s displayed dimensions without transmitting,
  so **reserving the rows before the id arrives avoids a height shift** when the
  image pops in one frame later.

After `app.render(..)`, the host drains the pending set. For each key it reads
the image bytes from the chat model entry (base64 in the `UserContent::Image`),
base64-decodes them, calls `app.load_image(Source::Mem(bytes))`, stores the
returned id and pixel size, and requests a redraw. The next frame places the
image.

We choose lazy over eager (transmit-on-event) for three reasons:

- It handles replayed sessions uniformly. Continuing a session reconstructs tool
  entries without replaying the live event, so an eager transmit-on-event would
  need a separate scan of loaded entries. The draw-driven path treats live,
  replayed, and scrolled-into-view images identically.
- It bounds terminal graphics memory to images actually viewed.
- It matches existing off-loop delivery idioms (autocomplete delivery, the
  prompt-history bootstrap), so the one-frame-late first paint is consistent
  with patterns already in the codebase and invisible in practice.

The shared pending set is an `Rc<RefCell<..>>` handed to the builder, mirroring
how the render cache is shared in.

### 5. Placement in the tool cell

The tool cell (`tool_cell.rs`) replaces the text-only `ToolDetails::Image` arm
with a capability-and-config-gated branch:

- If `caps.images` and `show_image_in_terminal` are both true, and the store has
  a transmitted id for this entry, reserve a footprint of `min(natural_cells,
  (40, 20))` computed from `ToolDetails`'s displayed dimensions and
  `DrawContext::cell_size` (pixels-per-cell), preserving aspect ratio. Write the
  placement cell into the reserved area with `DrawOptions { scale: Contain, size:
  reserved, .. }` so the image fits the cells. The `(40, 20)` cap matches the
  reference.
- If the id is not yet transmitted but the gates pass, reserve the same
  footprint with a neutral placeholder and record the pending key (see 4).
- Otherwise, keep the current `[image: <mime> · WxH]` dim text placeholder
  unchanged.

Unlike `aj` classic's kitty PNG-only constraint, `vaxis` re-encodes to PNG on
transmit, so we accept any format the `image` crate decodes (JPEG and so on).
This slightly exceeds the reference and costs one decode at transmit time.

### 6. Scroll, redraw, and the render cache

No extra work is needed for scrolling. The transcript recomposes its visible
slice each frame and the app re-blits from scratch, so a placement cell in an
entry's surface follows the text. The render cache stores full surfaces
including placement cells, and `Placement` is `Copy`, so cache hits replay the
placement with no re-transmission.

One characteristic to record in a `NOTE`: during a streaming turn the transcript
repaints up to 60 times a second, and the renderer clears and re-emits every
on-screen placement on each paint. On kitty terminals the image data stays
resident by id, so this is a placement re-emit, not a re-upload, and it is
visually stable. This is inherent to the `vaxis` render model, not something we
introduce, but it is worth knowing when reasoning about images visible during
streaming.

## End-to-end data flow (a tool-result image)

1. The user pastes an image. The clipboard path writes a tempfile and inserts
   its path into the editor. On submit the agent calls `read_file`, which
   resizes the image to the inline budget and returns a `UserContent::Image`
   (base64) plus a `ToolDetails::Image` (mime, original and displayed
   dimensions).
2. The tool entry lands in the chat model. The transcript draws it. The store
   has no id yet, so the tool cell reserves the footprint from the displayed
   dimensions and records the `(AgentId, EntryId)` pending key.
3. After the frame, the host drains pending, base64-decodes the entry's image
   bytes, calls `app.load_image(Source::Mem(..))`, stores the id and pixel size,
   and requests a redraw.
4. The next frame, the tool cell finds the id in the store and writes the
   placement cell into the reserved footprint. The image appears.
5. As the user scrolls, the placement re-emits at the entry's current row for
   free.
6. On session switch, the host frees every transmitted id and clears the store.

## Decisions

**Decision 1: kitty-only.** `vaxis` implements only kitty graphics. `aj` classic
also supports iTerm2 (OSC 1337), but `aj-next` targets kitty only. An iTerm2
terminal that does not speak kitty falls back to text. We accept this as the
target set (kitty, WezTerm, Ghostty, Konsole all speak the kitty protocol) and
document the loss. Adding iTerm2 to `vaxis` was rejected: its OSC 1337 is an
inline escape that does not fit `vaxis`'s cell-placement and diff model and would
deviate from the faithful libvaxis port.

**Decision 2: lazy transmission.** Transmit an image when it is first drawn,
driven by a shared pending set the host drains after each frame. Chosen over
eager (transmit when the entry is added) for uniform replay handling and bounded
terminal memory, at the cost of a one-frame-late first paint that is invisible in
practice and consistent with existing off-loop delivery idioms.

**Decision 3: user-message images deferred.** Out of scope, matching the
reference. Rendering the CLI `@file` case graphically would exceed `aj` classic
and requires recovering pixel dimensions the wire type does not carry (a base64
decode at the display layer). A possible future extension, not built here.

## Non-goals

- iTerm2 and sixel, unless Decision 1 goes to 1b.
- User-message and markdown images.
- Print and export changes.
- An LRU bound on the image store. Recorded as a future tightening.
- A hyperlink capability probe. `vaxis` does not offer one, so the hyperlink gate
  stays optimistic on the new runtime seam.

## Testing strategy

- **Capability gating.** With `caps.images` false, the tool cell emits the text
  placeholder and never records a pending key or transmits. Mirrors the existing
  fallback-path tests.
- **Config gating.** With `show_image_in_terminal` false, same as above even
  when `caps.images` is true.
- **Placement.** With a transmitted id in the store, the tool cell's surface
  carries a `Placement` cell with that id in the reserved footprint. Assert the
  footprint respects the `(40, 20)` cap and the aspect ratio.
- **Reserve-before-transmit.** An entry whose id is not yet in the store reserves
  the same footprint (no height shift) and records the pending key. A follow-up
  frame with the id placed asserts the placement lands in the already-reserved
  area.
- **Lifecycle.** Session switch frees ids and clears the store. Scrolling an
  entry off and back does not re-transmit (the id stays in the store).
- **`AsyncApp::load_image`.** A unit test against a graphics-capable test
  `Vaxis` asserts the transmit method allocates an id and writes the kitty
  transmit sequence, reusing the existing `vaxis` image test scaffolding.

## Where it lives

- `vaxis::vxfw::AsyncApp`: `load_image` / `free_image` (the transmit seam).
- `aj-next`: `TerminalCaps` and the runtime capability read after `init`; the
  `ImageStore` and pending-set plumbing in the host loop; the tool-cell
  placement branch; the `terminal.rs` hyperlink flag migration.
- `aj-app`: no new types expected. `ToolDetails::Image` and `UserContent::Image`
  already carry everything the renderer needs.
