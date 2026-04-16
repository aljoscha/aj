//! Animated spinner/loader component.

use std::time::Duration;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::component::Component;
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
#[derive(Clone)]
pub struct LoaderIndicatorOptions {
    pub frames: Vec<String>,
    pub interval: Duration,
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
}

impl Default for LoaderIndicatorOptions {
    fn default() -> Self {
        Self {
            frames: DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect(),
            interval: DEFAULT_FRAME_INTERVAL,
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
/// To make sure renders actually fire — without requiring the
/// surrounding application to ping `request_render` on a timer — the
/// loader spawns its own animation pump on the tokio runtime when it
/// joins a [`crate::tui::Tui`] tree (via
/// [`Component::set_render_handle`]). The pump fires
/// [`RenderHandle::request_render`] once per `interval`, which the
/// `Tui`'s render throttle coalesces with any other pending requests.
/// Dropping the loader (or calling [`Loader::stop`]) cancels the
/// pump.
pub struct Loader {
    message: String,
    spinner_style: Option<Box<dyn Fn(&str) -> String>>,
    message_style: Option<Box<dyn Fn(&str) -> String>>,
    frames: Vec<String>,
    interval: Duration,
    start_time: Instant,
    active: bool,
    /// Render handle wired in by the `Tui` when the loader joins the
    /// component tree. The animation pump uses it to wake the event
    /// loop on each frame boundary.
    render_handle: Option<RenderHandle>,
    /// Cancel token for the running animation pump task. `None` when
    /// no pump is running (no render handle yet, loader stopped, or
    /// frames don't warrant animation).
    animation_cancel: Option<CancellationToken>,
}

impl Loader {
    /// Create a new loader with the given message.
    pub fn new(message: &str) -> Self {
        Self {
            message: message.to_string(),
            spinner_style: None,
            message_style: None,
            frames: DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect(),
            interval: DEFAULT_FRAME_INTERVAL,
            start_time: Instant::now(),
            active: true,
            render_handle: None,
            animation_cancel: None,
        }
    }

    /// Set the message text.
    pub fn set_message(&mut self, message: &str) {
        self.message = message.to_string();
    }

    /// Set the style function for the spinner character.
    pub fn set_spinner_style(&mut self, style_fn: Box<dyn Fn(&str) -> String>) {
        self.spinner_style = Some(style_fn);
    }

    /// Set the style function for the message text.
    pub fn set_message_style(&mut self, style_fn: Box<dyn Fn(&str) -> String>) {
        self.message_style = Some(style_fn);
    }

    /// Start the animation (resets the timer).
    pub fn start(&mut self) {
        self.start_time = Instant::now();
        self.active = true;
        self.restart_animation_pump();
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
            }
            None => {
                self.frames = DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect();
                self.interval = DEFAULT_FRAME_INTERVAL;
            }
        }
        self.start_time = Instant::now();
        // The new frame set / interval may have changed whether the
        // loader is animating (e.g. swapping from the default spinner
        // to a single static frame, or vice versa). Re-evaluate.
        self.restart_animation_pump();
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
        let frame_idx = ((elapsed / interval) as usize) % self.frames.len();
        self.frames[frame_idx].as_str()
    }

    /// Cancel any running animation-pump task. No-op when no task is
    /// running.
    fn cancel_animation_pump(&mut self) {
        if let Some(token) = self.animation_cancel.take() {
            token.cancel();
        }
    }

    /// (Re)start the animation pump if conditions warrant: a render
    /// handle is wired in, the loader is active, and the indicator
    /// has more than one frame (i.e. it actually animates).
    ///
    /// Called from every state-changing path: `set_render_handle`,
    /// `start`, `stop`, `set_indicator`. Cheap when nothing changes
    /// because [`tokio::runtime::Handle::try_current`] is fast and
    /// the task is small.
    fn restart_animation_pump(&mut self) {
        self.cancel_animation_pump();

        let Some(handle) = self.render_handle.clone() else {
            return;
        };
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

    fn set_render_handle(&mut self, handle: RenderHandle) {
        self.render_handle = Some(handle);
        self.restart_animation_pump();
    }

    fn render(&mut self, _width: usize) -> Vec<String> {
        let frame = self.current_frame();
        let styled_msg = match &self.message_style {
            Some(f) => f(&self.message),
            None => self.message.clone(),
        };

        // Hidden-indicator shape: render the message on its own with no
        // leading glyph or separator space. Tests assert on this exact
        // shape, so don't pad with a blank character.
        if frame.is_empty() {
            return vec![String::new(), styled_msg];
        }

        let styled_frame = match &self.spinner_style {
            Some(f) => f(frame),
            None => frame.to_string(),
        };

        vec![String::new(), format!("{} {}", styled_frame, styled_msg)]
    }
}
