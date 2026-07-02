//! The status chrome's loader line: a braille spinner plus a message,
//! shown while the viewed agent works.
//!
//! The widget renders from two shared cells at draw time: the
//! [`ChatState`] (for the active view and compaction phase) and a
//! [`StatusState`] mirror of the lifecycle bits the widgets can't
//! reach directly. The host's select loop owns the `AgentLifecycle`
//! (the reducer and the turn-join arm both mutate it), so instead of
//! sharing it we mirror the three derived bits into `StatusState`
//! once per loop iteration. Single writer, read-only widgets, one
//! sync point right before each render.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Instant;

use aj_agent::events::CompactionPhase;
use aj_app::chat::ChatState;
use aj_app::keybindings::fixed_keys;
use vaxis::vxfw::{DrawContext, Event, EventContext, RichText, Size, Surface, TextSpan, Widget};

use crate::transcript::TranscriptStyles;

/// Spinner frames, matching `aj`'s braille set and 80ms cadence (a
/// deliberate parity decision over vxfw's `Spinner` frames).
const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Milliseconds each frame is displayed for.
pub(crate) const FRAME_INTERVAL_MS: u32 = 80;

/// Name of the host-posted [`vaxis::vxfw::UserEvent`] that wakes the
/// loader when the viewed agent turns busy, so it can arm its tick
/// chain (widgets can only schedule ticks from an event handler).
pub(crate) const STATUS_WAKE_EVENT: &str = "aj-next.status.wake";

/// Lifecycle bits the status chrome reads at draw time, mirrored from
/// the host-owned `AgentLifecycle` once per loop iteration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StatusState {
    /// Whether the viewed agent has an open turn (`AgentStart`
    /// without a matching `AgentEnd`).
    pub(crate) running: bool,
    /// Whether the viewed agent has an in-flight host-driven
    /// compaction. Compaction doesn't bracket itself with
    /// `AgentStart`/`AgentEnd`, so this is a separate bit.
    pub(crate) compacting: bool,
    /// Count of running `Sub(_)` agents, for the footer's activity
    /// indicator.
    pub(crate) sub_agents_running: usize,
}

impl StatusState {
    /// Whether the viewed agent should show the loader.
    pub(crate) fn busy(&self) -> bool {
        self.running || self.compacting
    }
}

/// The loader line widget: zero height while idle, one leading blank
/// row plus ` {spinner} {message}` while the viewed agent is busy.
pub(crate) struct StatusLine {
    /// Weak self-reference so tick commands can target this widget.
    /// Captured at construction with [`Rc::new_cyclic`], like vxfw's
    /// `Spinner`.
    me: Weak<RefCell<StatusLine>>,
    chat: Rc<RefCell<ChatState>>,
    status: Rc<RefCell<StatusState>>,
    styles: Rc<TranscriptStyles>,
    /// Wall-clock origin of the current busy stretch. The frame index
    /// derives from elapsed time, like `aj`'s loader, so the
    /// animation speed doesn't depend on tick delivery jitter.
    /// Cleared when the agent goes idle so the next stretch restarts
    /// at frame zero.
    started: Option<Instant>,
    /// Whether a tick targeting this widget is in flight. Guards
    /// against stacking multiple tick chains when wake events and
    /// pending ticks interleave.
    tick_armed: bool,
}

impl StatusLine {
    pub(crate) fn new(
        chat: Rc<RefCell<ChatState>>,
        status: Rc<RefCell<StatusState>>,
        styles: Rc<TranscriptStyles>,
    ) -> Rc<RefCell<StatusLine>> {
        Rc::new_cyclic(|me| {
            RefCell::new(StatusLine {
                me: Weak::clone(me),
                chat,
                status,
                styles,
                started: None,
                tick_armed: false,
            })
        })
    }

    /// The loader message for the current activity. Compaction labels
    /// win over the default because a compacting agent may also be
    /// mid-turn (auto-compaction runs inside the turn ladder).
    fn message(&self) -> String {
        let status = self.status.borrow();
        if status.compacting {
            let chat = self.chat.borrow();
            let label = match chat.compaction_phase(chat.active_view()) {
                None => "Compacting context…",
                Some(CompactionPhase::Summarizing) => "Compacting: summarizing earlier context…",
                Some(CompactionPhase::SummarizingTurnPrefix) => {
                    "Compacting: summarizing split turn…"
                }
                Some(CompactionPhase::Saving) => "Compacting: saving…",
            };
            label.to_string()
        } else {
            format!("Working… ({} to cancel)", fixed_keys::CTRL_C)
        }
    }

    /// The spinner glyph for the elapsed time since `started`.
    fn frame(started: Instant) -> &'static str {
        let elapsed = started.elapsed().as_millis();
        let interval = u128::from(FRAME_INTERVAL_MS);
        // Modulo first so the index is bounded by `FRAMES.len()` and
        // always fits in `usize`.
        let n = u128::try_from(FRAMES.len()).unwrap_or(u128::MAX);
        let idx = usize::try_from((elapsed / interval) % n).unwrap_or(0);
        FRAMES[idx]
    }

    /// Schedule the next animation tick if the loader is visible and
    /// none is pending, and latch a redraw so the new frame paints.
    fn arm_tick(&mut self, ctx: &mut EventContext) {
        if !self.status.borrow().busy() {
            return;
        }
        ctx.redraw = true;
        if self.tick_armed {
            return;
        }
        self.tick_armed = true;
        ctx.tick(
            FRAME_INTERVAL_MS,
            self.me.upgrade().expect("loader self-reference is live"),
        );
    }
}

impl Widget for StatusLine {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        if !self.status.borrow().busy() {
            // Idle agent: no rendered rows. The slot collapses to
            // zero height so the chat sits flush above the pending
            // box and editor.
            self.started = None;
            return Surface::with_size(Size {
                width: ctx.max.width.unwrap_or(0),
                height: 0,
            });
        }
        // The busy stretch's clock starts at the first busy draw,
        // which the event batch that flipped the state already
        // triggered.
        let started = *self.started.get_or_insert_with(Instant::now);
        let span = |text: String, style| TextSpan {
            text,
            style,
            ..TextSpan::default()
        };
        // One leading blank row, then ` {spinner} {message}`, the
        // exact shape of `aj`'s loader slot.
        let spans = vec![
            span("\n ".to_string(), self.styles.text),
            span(Self::frame(started).to_string(), self.styles.accent),
            span(format!(" {}", self.message()), self.styles.dim),
        ];
        RichText::new(spans).draw(ctx)
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            // The host posts a wake on the idle-to-busy edge; the
            // Shell forwards it here so the tick chain starts.
            Event::App(user) if user.name == STATUS_WAKE_EVENT => self.arm_tick(ctx),
            Event::Tick => {
                self.tick_armed = false;
                // Repaint even when the agent just went idle so the
                // final frame clears, then re-arm only while busy.
                ctx.redraw = true;
                self.arm_tick(ctx);
            }
            _ => {}
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use aj_agent::events::{AgentEvent, AgentId, AgentSettings, CompactionReason};
    use aj_app::chat::reduce;
    use aj_app::session::AgentLifecycle;
    use aj_app::theme::Theme;
    use vaxis::vxfw::Command;

    use super::*;

    fn chat() -> Rc<RefCell<ChatState>> {
        Rc::new(RefCell::new(ChatState::new(
            AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            0,
            Arc::new(Vec::new()),
        )))
    }

    fn styles() -> Rc<TranscriptStyles> {
        Rc::new(TranscriptStyles::from_theme(
            &Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor),
        ))
    }

    fn loader(status: StatusState) -> (Rc<RefCell<StatusLine>>, Rc<RefCell<StatusState>>) {
        let status = Rc::new(RefCell::new(status));
        let line = StatusLine::new(chat(), Rc::clone(&status), styles());
        (line, status)
    }

    fn rows(line: &Rc<RefCell<StatusLine>>) -> Vec<String> {
        let surface = line
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(80, None));
        crate::test_support::rows(&surface)
    }

    #[test]
    fn idle_loader_takes_zero_height() {
        let (line, _) = loader(StatusState::default());
        let surface = line
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(80, None));
        assert_eq!(surface.size.height, 0);
    }

    #[test]
    fn running_loader_shows_spinner_and_default_label() {
        let (line, _) = loader(StatusState {
            running: true,
            ..StatusState::default()
        });
        let r = rows(&line);
        assert_eq!(r[0], "", "leading blank row");
        assert_eq!(r[1], " ⠋ Working… (Ctrl+C to cancel)", "{r:?}");
    }

    /// Compaction relabels the loader: the starting phase, then each
    /// reported `CompactionProgress` phase.
    #[test]
    fn compaction_labels_follow_the_reported_phase() {
        let (line, _) = loader(StatusState {
            compacting: true,
            ..StatusState::default()
        });
        assert!(rows(&line)[1].ends_with("Compacting context…"));

        let mut life = AgentLifecycle::default();
        for (phase, label) in [
            (
                CompactionPhase::Summarizing,
                "Compacting: summarizing earlier context…",
            ),
            (
                CompactionPhase::SummarizingTurnPrefix,
                "Compacting: summarizing split turn…",
            ),
            (CompactionPhase::Saving, "Compacting: saving…"),
        ] {
            let _ = reduce(
                &mut line.borrow().chat.borrow_mut(),
                &mut life,
                AgentEvent::CompactionProgress {
                    agent_id: AgentId::Main,
                    reason: CompactionReason::Manual,
                    phase,
                },
            );
            let r = rows(&line);
            assert!(r[1].ends_with(label), "{phase:?}: {r:?}");
        }

        // `CompactionEnd` clears the phase (via the reducer) and the
        // busy bit (via the host's status sync), so the label resets
        // for the next turn.
        let _ = reduce(
            &mut line.borrow().chat.borrow_mut(),
            &mut life,
            AgentEvent::CompactionEnd {
                agent_id: AgentId::Main,
                reason: CompactionReason::Manual,
                tokens_before: 100,
                tokens_after: 50,
                summary: Some("s".into()),
                error: None,
            },
        );
        line.borrow_mut().status.borrow_mut().compacting = false;
        line.borrow_mut().status.borrow_mut().running = true;
        assert!(rows(&line)[1].contains("Working…"));
    }

    /// The frame index derives from elapsed wall time, so backdating
    /// the start instant advances the glyph deterministically.
    #[test]
    fn frame_advances_with_elapsed_time() {
        let (line, _) = loader(StatusState {
            running: true,
            ..StatusState::default()
        });
        let _ = rows(&line); // first draw pins `started`
        let two_frames = Duration::from_millis(u64::from(FRAME_INTERVAL_MS) * 2 + 10);
        line.borrow_mut().started = Some(Instant::now() - two_frames);
        let r = rows(&line);
        assert!(
            r[1].starts_with(" ⠹ "),
            "third frame after 2 intervals: {r:?}"
        );
    }

    /// A wake arms exactly one tick chain; each tick re-arms while
    /// busy and the chain dies once the agent goes idle.
    #[test]
    fn wake_and_ticks_drive_the_animation_pump() {
        let (line, status) = loader(StatusState {
            running: true,
            ..StatusState::default()
        });
        let wake = Event::App(vaxis::vxfw::UserEvent {
            name: STATUS_WAKE_EVENT.to_string(),
            data: None,
        });

        let mut ctx = EventContext::new();
        line.borrow_mut().handle_event(&mut ctx, &wake);
        assert!(ctx.redraw);
        assert_eq!(ctx.cmds.len(), 1);
        assert!(matches!(ctx.cmds[0], Command::Tick(_)));

        // A second wake while a tick is pending must not stack a
        // second chain.
        let mut ctx = EventContext::new();
        line.borrow_mut().handle_event(&mut ctx, &wake);
        assert!(ctx.cmds.is_empty(), "no duplicate tick chain");

        // The pending tick fires: re-arm while busy.
        let mut ctx = EventContext::new();
        line.borrow_mut().handle_event(&mut ctx, &Event::Tick);
        assert!(ctx.redraw);
        assert_eq!(ctx.cmds.len(), 1, "tick re-arms while busy");

        // Idle: the tick still repaints (to clear the line) but does
        // not re-arm.
        status.borrow_mut().running = false;
        let mut ctx = EventContext::new();
        line.borrow_mut().handle_event(&mut ctx, &Event::Tick);
        assert!(ctx.redraw, "final clearing repaint");
        assert!(ctx.cmds.is_empty(), "chain ends when idle");
    }
}
