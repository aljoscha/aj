//! Animated spinner/loader component.

use std::time::Duration;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::component::Component;
use crate::components::text::Text;
use crate::tui::RenderHandle;

/// Default braille spinner frames used when no custom indicator is set.
const DEFAULT_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Default duration each frame is displayed for.
const DEFAULT_FRAME_INTERVAL: Duration = Duration::from_millis(80);

/// Options controlling the [`Loader`] indicator: its frames, the
/// interval between them, or both. See [`Loader::set_indicator`].
///
/// Semantics:
///
/// - `frames.is_empty()` → render with no spinner glyph at all (the
///   message still appears).
/// - `frames.len() == 1` → render the single frame statically, no
///   animation.
/// - Otherwise → cycle through frames at `interval`.
///
/// `verbatim` signals that the frames already carry their own ANSI
/// styling and should be emitted as-is. When `true`, the loader
/// bypasses `spinner_style` for the frame so the embedded escapes
/// aren't double-styled (which would either no-op redundantly or,
/// worse, leak open SGR state past the spinner cell). The message is
/// unaffected — `message_style` still applies.
#[derive(Clone)]
pub struct LoaderIndicatorOptions {
    pub frames: Vec<String>,
    pub interval: Duration,
    pub verbatim: bool,
}

impl LoaderIndicatorOptions {
    /// Build an options set from frames, keeping the default interval.
    pub fn with_frames<I, S>(frames: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            frames: frames.into_iter().map(Into::into).collect(),
            interval: DEFAULT_FRAME_INTERVAL,
            verbatim: false,
        }
    }

    /// Override the frame interval. Non-positive intervals fall back to
    /// the default.
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = if interval.is_zero() {
            DEFAULT_FRAME_INTERVAL
        } else {
            interval
        };
        self
    }

    /// Mark the frames as already-styled (verbatim). The loader will
    /// emit each frame as-is, without wrapping it in `spinner_style`.
    /// Useful when the caller pre-bakes ANSI escapes into the frame
    /// strings — applying `spinner_style` on top would either no-op
    /// or, worse, leak unclosed SGR state past the spinner cell.
    pub fn with_verbatim(mut self, verbatim: bool) -> Self {
        self.verbatim = verbatim;
        self
    }
}

impl Default for LoaderIndicatorOptions {
    fn default() -> Self {
        Self {
            frames: DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect(),
            interval: DEFAULT_FRAME_INTERVAL,
            verbatim: false,
        }
    }
}

/// An animated spinner with a message.
///
/// The spinner cycles through frames at a configurable interval. The
/// frame index is computed from elapsed wall-clock time inside
/// [`Component::render`], so a render that's *triggered* keeps the
/// animation up to date.
///
/// Layout mirrors pi-tui's `Loader extends Text("", 1, 0)`: the
/// spinner+message body is fed into an embedded [`Text`] with
/// `padding_x = 1, padding_y = 0` and the rendered output is a leading
/// blank row plus the wrapped Text rows. So the spinner sits one space
/// in from the left, each row is right-padded to the terminal width,
/// and a long message wraps at `width - 2` instead of overflowing.
///
/// To make sure renders actually fire — without requiring the
/// surrounding application to ping `request_render` on a timer — the
/// loader spawns its own animation pump on the tokio runtime when its
/// indicator has more than one frame and the loader is active. The
/// pump fires [`RenderHandle::request_render`] once per `interval`,
/// which the `Tui`'s render throttle coalesces with any other pending
/// requests. Dropping the loader (or calling [`Loader::stop`])
/// cancels the pump.
///
/// # Construction
///
/// [`Loader::new`] mirrors pi-tui's required-at-construction shape:
/// callers pass spinner and message style closures up front. Tests and
/// other callers that don't care about styling can use
/// [`Loader::with_identity_styles`], which fills in identity closures.
pub struct Loader {
    message: String,
    spinner_style: Box<dyn Fn(&str) -> String>,
    message_style: Box<dyn Fn(&str) -> String>,
    frames: Vec<String>,
    interval: Duration,
    /// When `true`, frames already carry ANSI styling and should be
    /// emitted unchanged — `spinner_style` is bypassed to avoid double
    /// styling. Set via [`LoaderIndicatorOptions::verbatim`].
    verbatim: bool,
    start_time: Instant,
    active: bool,
    /// Embedded [`Text`] that renders the spinner+message body with
    /// `padding_x = 1, padding_y = 0`. Mirrors pi-tui's `Loader extends
    /// Text("", 1, 0)`. Reused across renders so the Text-internal
    /// cache (keyed on `(text, width)`) works for static-frame loaders.
    body: Text,
    /// Render handle wired in at construction. The animation pump uses
    /// it to wake the event loop on each frame boundary. Standalone
    /// callers without a `Tui` pass [`RenderHandle::detached`].
    render_handle: RenderHandle,
    /// Cancel token for the running animation pump task. `None` when
    /// no pump is running (loader stopped, or frames don't warrant
    /// animation).
    animation_cancel: Option<CancellationToken>,
}

impl Loader {
    /// Create a new loader with the given styling closures and message.
    ///
    /// Mirrors pi-tui's `new Loader(ui, spinnerColorFn, messageColorFn,
    /// message, indicator?)` shape: the styling closures are required
    /// at construction. Use [`Loader::with_identity_styles`] for the
    /// common test/no-styling case to avoid the per-site
    /// `Box::new(|s| s.to_string())` boilerplate.
    ///
    /// `handle` wakes the `Tui`'s render loop on each animation tick.
    /// Callers attached to a `Tui` pass `tui.handle()`; standalone
    /// callers pass [`RenderHandle::detached`] (the loader still
    /// functions as a renderable component, but its animation pump
    /// becomes a no-op because there's nothing to wake).
    pub fn new(
        handle: RenderHandle,
        spinner_style: Box<dyn Fn(&str) -> String>,
        message_style: Box<dyn Fn(&str) -> String>,
        message: &str,
    ) -> Self {
        // Mirror pi-tui's `super("", 1, 0)`: padding_x = 1, padding_y =
        // 0 so we don't sandwich the spinner row in blank rows on top
        // of our own leading blank.
        let body = Text::new("", 1, 0);
        let mut loader = Self {
            message: message.to_string(),
            spinner_style,
            message_style,
            frames: DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect(),
            interval: DEFAULT_FRAME_INTERVAL,
            verbatim: false,
            start_time: Instant::now(),
            active: true,
            body,
            render_handle: handle,
            animation_cancel: None,
        };
        // The default braille indicator has > 1 frame, so kick the
        // pump now (mirrors pi-tui's `setIndicator(default)` at the
        // tail of its constructor).
        loader.restart_animation_pump();
        // Mirror pi-tui's `setIndicator` → `start` → `updateDisplay`
        // chain at the tail of the constructor: a freshly-constructed
        // loader synchronously asks for a render so its first frame
        // appears even on an idle Tui that has `set_initial_render(false)`.
        loader.request_repaint();
        loader
    }

    /// Construct a loader with identity styling closures (no extra
    /// styling applied to spinner or message). Convenience for tests
    /// and other callers that don't need themed output.
    pub fn with_identity_styles(handle: RenderHandle, message: &str) -> Self {
        Self::new(
            handle,
            Box::new(|s| s.to_string()),
            Box::new(|s| s.to_string()),
            message,
        )
    }

    /// Set the message text.
    pub fn set_message(&mut self, message: &str) {
        self.message = message.to_string();
        // Mirror pi-tui's `updateDisplay` → `requestRender`: a message
        // change must paint immediately, even on a static or empty-
        // frame loader where no animation pump exists to coalesce the
        // change into the next tick.
        self.request_repaint();
    }

    /// Start the animation (resets the timer).
    pub fn start(&mut self) {
        self.start_time = Instant::now();
        self.active = true;
        self.restart_animation_pump();
        // Pi's `start` calls `updateDisplay` which requests a render.
        // Without this, a `start` on an idle Tui with `initial_render =
        // false` and a static indicator wouldn't paint until the next
        // input arrives.
        self.request_repaint();
    }

    /// Stop the animation.
    pub fn stop(&mut self) {
        self.active = false;
        self.cancel_animation_pump();
    }

    /// Returns whether the loader is currently animating.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Configure the loader's indicator (spinner frames and interval).
    ///
    /// `Some(options)` applies the override; `None` restores the default
    /// braille spinner. Reset the timer so a swap mid-flight doesn't
    /// jump to an arbitrary frame of the new sequence.
    ///
    /// Semantics:
    ///
    /// - Empty `options.frames` renders with no spinner glyph (just the
    ///   message).
    /// - Single-element `options.frames` renders statically, no cycle.
    /// - Multi-element `options.frames` cycles at `options.interval`.
    pub fn set_indicator(&mut self, options: Option<LoaderIndicatorOptions>) {
        match options {
            Some(opts) => {
                self.frames = opts.frames;
                self.interval = if opts.interval.is_zero() {
                    DEFAULT_FRAME_INTERVAL
                } else {
                    opts.interval
                };
                self.verbatim = opts.verbatim;
            }
            None => {
                self.frames = DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect();
                self.interval = DEFAULT_FRAME_INTERVAL;
                // Restoring the default also clears the verbatim flag —
                // the built-in braille spinner is plain text and the
                // caller's `spinner_style` should style it.
                self.verbatim = false;
            }
        }
        self.start_time = Instant::now();
        // The new frame set / interval may have changed whether the
        // loader is animating (e.g. swapping from the default spinner
        // to a single static frame, or vice versa). Re-evaluate.
        self.restart_animation_pump();
        // Indicator swap is visible on the next paint regardless of
        // whether the new indicator animates. Mirror pi's
        // `setIndicator → start → updateDisplay → requestRender`
        // chain so a swap to a static or empty indicator surfaces
        // immediately rather than waiting for an unrelated render
        // trigger.
        self.request_repaint();
    }

    /// Current spinner glyph, or an empty string when the indicator is
    /// hidden (empty frames) or static-single-frame.
    ///
    /// Exposed for tests; components don't normally need to inspect
    /// this directly.
    pub fn current_frame(&self) -> &str {
        if self.frames.is_empty() {
            return "";
        }
        if self.frames.len() == 1 || !self.active {
            return self.frames[0].as_str();
        }
        let elapsed = self.start_time.elapsed().as_millis();
        let interval = self.interval.as_millis().max(1);
        // Modulo first so `frame_idx` is bounded by `frames.len()` and
        // always fits in `usize` on every target.
        let n = u128::try_from(self.frames.len()).unwrap_or(u128::MAX);
        let frame_idx = usize::try_from((elapsed / interval) % n).unwrap_or(0);
        self.frames[frame_idx].as_str()
    }

    /// Ask the owning [`Tui`] to schedule a paint.
    ///
    /// Mirrors pi-tui's `updateDisplay → requestRender` chain: any
    /// state change that affects the visible loader (message swap,
    /// indicator swap, style swap, construction, start) calls this
    /// to keep the screen in sync without waiting for the next
    /// animation tick.
    ///
    /// Detached render handles silently drop the request (see
    /// [`RenderHandle::detached`]), so calling this from a standalone
    /// loader (no `Tui`) is a cheap no-op.
    fn request_repaint(&self) {
        self.render_handle.request_render();
    }

    /// Cancel any running animation-pump task. No-op when no task is
    /// running.
    fn cancel_animation_pump(&mut self) {
        if let Some(token) = self.animation_cancel.take() {
            token.cancel();
        }
    }

    /// (Re)start the animation pump if conditions warrant: the loader
    /// is active and the indicator has more than one frame (i.e. it
    /// actually animates).
    ///
    /// Called from every state-changing path: `new`, `start`,
    /// `set_indicator`. (`stop` cancels via `cancel_animation_pump`
    /// directly.) Cheap when nothing changes because
    /// [`tokio::runtime::Handle::try_current`] is fast and the task
    /// is small.
    fn restart_animation_pump(&mut self) {
        self.cancel_animation_pump();

        if !self.active || self.frames.len() <= 1 {
            return;
        }
        // Spawning needs an active tokio runtime. The loader can be
        // constructed and rendered standalone (no Tui), in which case
        // there's no runtime to spawn into; fall back gracefully so
        // we don't panic in that case. Tests that exercise the loader
        // outside a `#[tokio::test]` rely on this fallback.
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };

        let token = CancellationToken::new();
        let child = token.clone();
        let interval = self.interval;
        let handle = self.render_handle.clone();
        rt.spawn(async move {
            // Skip the immediate initial tick so a render isn't
            // requested *before* we've drawn anything; the throttle
            // would still coalesce, but the extra request is wasted.
            let mut t = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = child.cancelled() => break,
                    _ = t.tick() => handle.request_render(),
                }
            }
        });
        self.animation_cancel = Some(token);
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.cancel_animation_pump();
    }
}

impl Component for Loader {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Take an owned copy of the frame so the immutable borrow of
        // `self` ends before we mutably borrow `self.body`.
        let frame = self.current_frame().to_string();
        let styled_msg = (self.message_style)(&self.message);

        // Compose the body string the same way pi-tui's updateDisplay
        // does: `${frame} ${msg}` when there's a frame, else just the
        // message (no leading separator space). Verbatim frames bypass
        // `spinner_style` so the caller's pre-baked ANSI escapes aren't
        // double-wrapped.
        let text = if frame.is_empty() {
            styled_msg
        } else {
            let styled_frame = if self.verbatim {
                frame
            } else {
                (self.spinner_style)(&frame)
            };
            format!("{styled_frame} {styled_msg}")
        };

        // Feed the body into the embedded Text. With `padding_x = 1`
        // and `padding_y = 0`, Text emits one wrapped/padded row per
        // visible line at `width - 2` content width. Prepend our own
        // leading blank row to mirror pi-tui's
        // `["", ...super.render(width)]`.
        self.body.set_text(&text);
        let mut result = Vec::with_capacity(2);
        result.push(String::new());
        result.extend(self.body.render(width));
        result
    }
}
