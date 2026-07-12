//! The generic keymap engine: activators, single and sequence bindings, and
//! the [`KeymapController`] widget that matches them during dispatch.
//!
//! A [`Keymap`] compiles a list of [`Entry`] bindings, each mapping an
//! [`Activator`] chord (or a sequence of them) to an action `A` under a
//! context predicate. The [`KeymapController`] wraps a child widget and sits
//! on the focus path, matching keys in two phases:
//!
//! - [`BindingPhase::Capture`] singles and every sequence pre-empt the focused
//!   widget (matched in `capture_event`, before the target sees the key).
//! - [`BindingPhase::Bubble`] singles are shadowable: the focused widget sees
//!   the key first and the controller only fires when it declined (matched in
//!   `handle_event` on the way back up).
//!
//! Both types are generic over the action `A` and a host context `C`, so the
//! framework carries no application vocabulary. The host supplies the context
//! as an `Rc<RefCell<C>>` at construction and mutates it as its state changes.
//! Entry predicates are plain `fn(&C) -> bool`, keeping the keymap pure data.
//!
//! ```ignore
//! #[derive(Clone)]
//! enum Action { Cancel, Quit }
//! struct Ctx { running: bool }
//!
//! let ctrl_c = Activator::new(u32::from('c'), Modifiers::CTRL);
//! let keymap = Keymap::new(vec![
//!     Entry::single(ctrl_c, Action::Cancel, BindingPhase::Capture)
//!         .with_enabled(|cx: &Ctx| cx.running),
//!     Entry::sequence(vec![ctrl_c, ctrl_c], Action::Quit),
//! ]);
//! let controller = KeymapController::new(
//!     keymap,
//!     Rc::clone(&ctx),
//!     child,
//!     Box::new(|ectx, action| match action {
//!         Action::Cancel => { /* cancel the turn */ }
//!         Action::Quit => ectx.quit = true,
//!     }),
//! );
//! ```
//!
//! An in-flight sequence times out via a self-scheduled [`Command::Tick`]. The
//! controller exposes the pending chord through
//! [`pending_sequence`](KeymapController::pending_sequence) so the host can
//! render a hint. The controller does not draw one itself.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use crate::key::{Key, Modifiers};
use crate::vxfw::{
    Command, DrawContext, Event, EventContext, Surface, Tick, Widget, WidgetRef, draw_widget,
};

/// A key chord: a codepoint plus the exact modifier set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activator {
    /// The target codepoint (see the constants on [`Key`]).
    pub key: u32,
    pub mods: Modifiers,
}

impl Activator {
    /// Builds an activator for `key` under `mods`.
    pub fn new(key: u32, mods: Modifiers) -> Activator {
        Activator { key, mods }
    }

    /// Whether the pressed `key` activates this chord.
    ///
    /// Delegates to [`Key::matches`], so exact, text-equivalent, and
    /// shifted-codepoint presses all count (an activator for `:` accepts
    /// shift+`;` on a US layout).
    pub fn accepts(&self, key: &Key) -> bool {
        key.matches(self.key, self.mods)
    }

    /// The activator describing what `key` pressed, for hint display.
    fn from_key(key: &Key) -> Activator {
        Activator {
            key: key.codepoint,
            mods: key.mods,
        }
    }
}

/// What a binding is activated by: one chord or a multi-key sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    Single(Activator),
    /// A leader / multi-key sequence. Must be non-empty.
    Sequence(Vec<Activator>),
}

/// The dispatch phase a single binding matches in.
///
/// `Capture` singles pre-empt the focused widget, `Bubble` singles are
/// shadowable by it. Sequences always match in the capture phase (a focused
/// text field would otherwise swallow the first key of every sequence), so
/// this choice only applies to [`Binding::Single`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingPhase {
    Capture,
    Bubble,
}

/// One compiled binding: an activation, the action it fires, the phase it
/// matches in, and a context predicate gating it.
pub struct Entry<A, C> {
    pub binding: Binding,
    pub action: A,
    /// Ignored for sequence bindings, which always match in capture.
    pub phase: BindingPhase,
    /// A disabled entry matches nothing: it neither fires as a single nor
    /// starts or advances as a sequence.
    pub enabled: fn(&C) -> bool,
}

impl<A, C> Entry<A, C> {
    /// A single-chord binding, enabled unconditionally.
    pub fn single(activator: Activator, action: A, phase: BindingPhase) -> Entry<A, C> {
        Entry {
            binding: Binding::Single(activator),
            action,
            phase,
            enabled: |_| true,
        }
    }

    /// A sequence binding, enabled unconditionally. Sequences are always
    /// capture-phase, so no phase parameter.
    pub fn sequence(activators: Vec<Activator>, action: A) -> Entry<A, C> {
        Entry {
            binding: Binding::Sequence(activators),
            action,
            phase: BindingPhase::Capture,
            enabled: |_| true,
        }
    }

    /// Replaces the entry's context predicate.
    pub fn with_enabled(mut self, enabled: fn(&C) -> bool) -> Entry<A, C> {
        self.enabled = enabled;
        self
    }
}

/// The outcome of feeding one key to an in-flight (or starting) sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeqStep<A> {
    /// The key completed a sequence, firing this action.
    Matched(A),
    /// The key extended at least one candidate sequence without completing
    /// any.
    Progress,
    /// The key fits no candidate sequence.
    None,
}

/// A compiled set of bindings, queried by the [`KeymapController`].
///
/// NOTE: The sequence entries are indexed at construction so the sequence
/// queries probe only them. A hash index keyed on activators does not work
/// here because [`Key::matches`] is loose (text and shifted-codepoint
/// equivalence), so an incoming key can match activators that are not
/// byte-equal to it. Candidates must be probed, not looked up.
pub struct Keymap<A, C> {
    entries: Vec<Entry<A, C>>,
    /// Indices into `entries` of the [`Binding::Sequence`] entries.
    sequence_idxs: Vec<usize>,
}

impl<A: Clone, C> Keymap<A, C> {
    /// Compiles `entries`. Panics on an empty sequence binding.
    pub fn new(entries: Vec<Entry<A, C>>) -> Keymap<A, C> {
        let mut sequence_idxs = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            if let Binding::Sequence(seq) = &entry.binding {
                assert!(!seq.is_empty(), "a sequence binding must be non-empty");
                sequence_idxs.push(i);
            }
        }
        Keymap {
            entries,
            sequence_idxs,
        }
    }

    /// The action of the first enabled single binding in `phase` that accepts
    /// `key`, if any. Entry order breaks ties.
    pub fn match_single(&self, key: &Key, phase: BindingPhase, cx: &C) -> Option<&A> {
        self.entries.iter().find_map(|entry| match &entry.binding {
            Binding::Single(act)
                if entry.phase == phase && (entry.enabled)(cx) && act.accepts(key) =>
            {
                Some(&entry.action)
            }
            _ => None,
        })
    }

    /// Whether `key` begins any enabled sequence.
    pub fn starts_sequence(&self, key: &Key, cx: &C) -> bool {
        !matches!(self.advance_sequence(&[], key, cx), SeqStep::None)
    }

    /// Feeds `key` to the sequences reachable from `prefix` (the keys matched
    /// so far, empty when starting).
    ///
    /// Only entries enabled right now are candidates, and the whole prefix is
    /// re-checked against each candidate, so a predicate flipping mid-sequence
    /// drops that entry from the running.
    ///
    /// A completion wins immediately: if `key` finishes one sequence while
    /// extending a longer one that shares the prefix, the finished one fires
    /// and the longer one is unreachable. Entry order breaks ties among
    /// sequences completing at the same step.
    pub fn advance_sequence(&self, prefix: &[Key], key: &Key, cx: &C) -> SeqStep<A> {
        let mut progressed = false;
        for &i in &self.sequence_idxs {
            let entry = &self.entries[i];
            if !(entry.enabled)(cx) {
                continue;
            }
            let Binding::Sequence(seq) = &entry.binding else {
                unreachable!("sequence_idxs only indexes sequence bindings");
            };
            if seq.len() <= prefix.len() {
                continue;
            }
            let prefix_ok = seq.iter().zip(prefix).all(|(act, k)| act.accepts(k));
            if !prefix_ok || !seq[prefix.len()].accepts(key) {
                continue;
            }
            if seq.len() == prefix.len() + 1 {
                return SeqStep::Matched(entry.action.clone());
            }
            progressed = true;
        }
        if progressed {
            SeqStep::Progress
        } else {
            SeqStep::None
        }
    }
}

/// Default milliseconds an in-flight sequence waits for its next key.
pub const DEFAULT_SEQUENCE_TIMEOUT_MS: u64 = 2000;

/// An in-flight sequence: what was pressed so far and when it disarms.
struct SequenceState {
    /// The keys consumed so far, replayed against the candidate sequences on
    /// each advance. We store the pressed `Key`s rather than activators
    /// because matching is loose: one key can satisfy differently-spelled
    /// activators in different candidate entries, so there is no single
    /// canonical activator to record per step.
    keys: Vec<Key>,
    /// What was pressed, as activators, for the host's hint rendering.
    pressed: Vec<Activator>,
    /// When the sequence disarms. Each extension schedules a fresh tick and
    /// moves this forward, and the tick handler compares against it so a
    /// stale tick from an earlier arm cannot disarm the extended sequence.
    deadline: Instant,
}

/// A wrapper widget that matches keys against a [`Keymap`] and fires actions
/// through a host-supplied handler.
///
/// Place it on the focus path as an ancestor of the widgets whose keys it
/// should pre-empt or backstop (typically at the root, wrapping the layout).
/// It draws nothing of its own: `draw` delegates to the child and the surface
/// is re-stamped with the controller's identity by the caller's
/// [`draw_widget`].
pub struct KeymapController<A, C> {
    /// A weak self-reference so the controller can schedule ticks targeting
    /// itself, captured at construction with [`Rc::new_cyclic`].
    me: Weak<RefCell<KeymapController<A, C>>>,
    keymap: Keymap<A, C>,
    /// The host context the entry predicates evaluate against. Shared so the
    /// host mutates it in place as its state changes.
    context: Rc<RefCell<C>>,
    in_flight: Option<SequenceState>,
    on_action: Box<dyn FnMut(&mut EventContext, &A)>,
    child: WidgetRef,
    /// Milliseconds an in-flight sequence waits for its next key before
    /// disarming.
    pub timeout_ms: u64,
}

impl<A: Clone + 'static, C: 'static> KeymapController<A, C> {
    /// Builds a controller wrapping `child`, behind an `Rc` so it can target
    /// its own timeout ticks.
    pub fn new(
        keymap: Keymap<A, C>,
        context: Rc<RefCell<C>>,
        child: WidgetRef,
        on_action: Box<dyn FnMut(&mut EventContext, &A)>,
    ) -> Rc<RefCell<KeymapController<A, C>>> {
        Rc::new_cyclic(|me| {
            RefCell::new(KeymapController {
                me: Weak::clone(me),
                keymap,
                context,
                in_flight: None,
                on_action,
                child,
                timeout_ms: DEFAULT_SEQUENCE_TIMEOUT_MS,
            })
        })
    }

    /// The chord pressed so far of an in-flight sequence, for the host to
    /// render as a hint. `None` when no sequence is in flight.
    pub fn pending_sequence(&self) -> Option<&[Activator]> {
        self.in_flight.as_ref().map(|s| s.pressed.as_slice())
    }

    /// The controller's own `WidgetRef`, used to target self-scheduled ticks.
    fn widget(&self) -> WidgetRef {
        self.me
            .upgrade()
            .expect("controller self-reference is live")
    }

    /// Fires `action` through the handler and consumes the event.
    fn fire(&mut self, ctx: &mut EventContext, action: &A) {
        (self.on_action)(ctx, action);
        ctx.consume_event();
    }

    /// Extends (or starts) the in-flight sequence with `key`, rescheduling
    /// the timeout tick. Consumes the event and requests a redraw so a hint
    /// can update.
    fn extend_in_flight(&mut self, ctx: &mut EventContext, key: &Key) {
        let deadline = Instant::now() + Duration::from_millis(self.timeout_ms);
        let widget = self.widget();
        let state = self.in_flight.get_or_insert_with(|| SequenceState {
            keys: Vec::new(),
            pressed: Vec::new(),
            deadline,
        });
        state.keys.push(key.clone());
        state.pressed.push(Activator::from_key(key));
        state.deadline = deadline;
        ctx.add_cmd(Command::Tick(Tick { deadline, widget }));
        ctx.consume_and_redraw();
    }

    /// Drops the in-flight sequence, requesting a redraw so a hint clears.
    fn disarm(&mut self, ctx: &mut EventContext) {
        if self.in_flight.take().is_some() {
            ctx.redraw = true;
        }
    }

    /// Runs `key` through the in-flight sequence, if any. Returns true when
    /// the key was consumed (completion or progress). On no match the
    /// sequence resets and the key falls through to normal matching.
    fn advance_in_flight(&mut self, ctx: &mut EventContext, key: &Key) -> bool {
        if self.in_flight.is_none() {
            return false;
        }
        let step = {
            let cx = self.context.borrow();
            let state = self.in_flight.as_ref().expect("checked above");
            self.keymap.advance_sequence(&state.keys, key, &cx)
        };
        match step {
            SeqStep::Matched(action) => {
                self.disarm(ctx);
                self.fire(ctx, &action);
                true
            }
            SeqStep::Progress => {
                self.extend_in_flight(ctx, key);
                true
            }
            SeqStep::None => {
                self.disarm(ctx);
                false
            }
        }
    }
}

impl<A: Clone + 'static, C: 'static> Widget for KeymapController<A, C> {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // The caller's draw_widget re-stamps the returned surface with the
        // controller's identity, replacing the child's stamp at this level
        // while descendants keep their own identities.
        draw_widget(&self.child, ctx)
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else {
            return;
        };
        // A bare modifier press (kitty reports these as their own key events)
        // must not reset an in-flight sequence: pressing ctrl on the way to
        // the next chord is not a mismatch.
        if key.is_modifier() {
            return;
        }

        if self.advance_in_flight(ctx, key) {
            return;
        }

        // Ambiguity resolution for an activator that is both a capture-phase
        // single and a sequence prefix (the cancel/quit ladder): the enabled
        // single wins and the sequence does not arm. The single's predicate
        // is how the data expresses which one applies, so when the predicate
        // declines, the sequence gets its turn to start.
        let single = {
            let cx = self.context.borrow();
            self.keymap
                .match_single(key, BindingPhase::Capture, &cx)
                .cloned()
        };
        if let Some(action) = single {
            self.fire(ctx, &action);
            return;
        }

        let step = {
            let cx = self.context.borrow();
            self.keymap.advance_sequence(&[], key, &cx)
        };
        match step {
            // A one-chord "sequence" completes immediately, degenerating to a
            // capture single.
            SeqStep::Matched(action) => self.fire(ctx, &action),
            SeqStep::Progress => self.extend_in_flight(ctx, key),
            SeqStep::None => {}
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::Tick => {
                // Only a tick at or past the current deadline disarms. A
                // stale tick from before an extension fires while now is
                // still short of the moved deadline and is ignored.
                if let Some(state) = &self.in_flight {
                    if Instant::now() >= state.deadline {
                        self.disarm(ctx);
                    }
                }
            }
            Event::KeyPress(key) => {
                // The bubble phase: the focused widget declined the key.
                // (Also reached at-target if the controller itself is
                // focused, where firing is equally correct: no descendant
                // had a claim on the key.)
                let action = {
                    let cx = self.context.borrow();
                    self.keymap
                        .match_single(key, BindingPhase::Bubble, &cx)
                        .cloned()
                };
                if let Some(action) = action {
                    self.fire(ctx, &action);
                }
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
    use std::time::Duration;

    use super::*;
    use crate::tty::TestTty;
    use crate::vaxis::{Options as VaxisOptions, Vaxis};
    use crate::vxfw::app_core::{AppCore, FocusHandler, reset_event_state};
    use crate::vxfw::to_widget_ref;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Act {
        Cancel,
        Quit,
        Palette,
        Newline,
    }

    struct Ctx {
        running: bool,
    }

    fn ctrl_c() -> Activator {
        Activator::new(u32::from('c'), Modifiers::CTRL)
    }

    /// A plain character press, with text, the way the parser reports it.
    fn key(c: char) -> Key {
        Key {
            codepoint: u32::from(c),
            text: Some(c.to_string().into()),
            ..Default::default()
        }
    }

    fn ctrl(c: char) -> Key {
        Key {
            codepoint: u32::from(c),
            mods: Modifiers::CTRL,
            ..Default::default()
        }
    }

    #[test]
    fn activator_accepts_exact_shifted_and_text() {
        // Exact codepoint + modifier match.
        assert!(ctrl_c().accepts(&ctrl('c')));
        assert!(!ctrl_c().accepts(&key('c')));

        // Text match: caps/num lock and shift are consumed by Key::matches.
        let a = Key {
            codepoint: u32::from('a'),
            mods: Modifiers::NUM_LOCK,
            text: Some("a".into()),
            ..Default::default()
        };
        assert!(Activator::new(u32::from('a'), Modifiers::empty()).accepts(&a));

        // Shifted codepoint: an activator for ':' accepts shift+';'.
        let shifted_semicolon = Key {
            codepoint: u32::from(';'),
            shifted_codepoint: Some(u32::from(':')),
            mods: Modifiers::SHIFT,
            text: Some(":".into()),
            ..Default::default()
        };
        assert!(Activator::new(u32::from(':'), Modifiers::empty()).accepts(&shifted_semicolon));
    }

    /// The module doctest: keymap queries respect phase and predicates.
    #[test]
    fn keymap() {
        let map: Keymap<Act, Ctx> = Keymap::new(vec![
            Entry::single(
                Activator::new(u32::from('p'), Modifiers::CTRL),
                Act::Palette,
                BindingPhase::Bubble,
            ),
            Entry::single(ctrl_c(), Act::Cancel, BindingPhase::Capture)
                .with_enabled(|cx| cx.running),
        ]);

        let running = Ctx { running: true };
        let idle = Ctx { running: false };

        // Phase filtering: the bubble binding does not match in capture.
        assert_eq!(
            map.match_single(&ctrl('p'), BindingPhase::Bubble, &idle),
            Some(&Act::Palette)
        );
        assert_eq!(
            map.match_single(&ctrl('p'), BindingPhase::Capture, &idle),
            None
        );

        // Predicate gating: cancel only matches while running.
        assert_eq!(
            map.match_single(&ctrl('c'), BindingPhase::Capture, &running),
            Some(&Act::Cancel)
        );
        assert_eq!(
            map.match_single(&ctrl('c'), BindingPhase::Capture, &idle),
            None
        );
    }

    #[test]
    fn sequence_advance_transitions() {
        let map: Keymap<Act, Ctx> = Keymap::new(vec![
            Entry::sequence(
                vec![
                    Activator::new(u32::from('g'), Modifiers::empty()),
                    Activator::new(u32::from('q'), Modifiers::empty()),
                ],
                Act::Quit,
            ),
            Entry::sequence(
                vec![
                    Activator::new(u32::from('g'), Modifiers::empty()),
                    Activator::new(u32::from('n'), Modifiers::empty()),
                ],
                Act::Newline,
            )
            .with_enabled(|cx| cx.running),
        ]);
        let idle = Ctx { running: false };
        let running = Ctx { running: true };

        // Prefix start.
        assert!(map.starts_sequence(&key('g'), &idle));
        assert!(!map.starts_sequence(&key('q'), &idle));
        assert_eq!(
            map.advance_sequence(&[], &key('g'), &idle),
            SeqStep::Progress
        );

        // Completion from a one-key prefix.
        let prefix = vec![key('g')];
        assert_eq!(
            map.advance_sequence(&prefix, &key('q'), &idle),
            SeqStep::Matched(Act::Quit)
        );

        // A disabled entry cannot complete, an enabled one can.
        assert_eq!(
            map.advance_sequence(&prefix, &key('n'), &idle),
            SeqStep::None
        );
        assert_eq!(
            map.advance_sequence(&prefix, &key('n'), &running),
            SeqStep::Matched(Act::Newline)
        );

        // A key that fits nothing.
        assert_eq!(
            map.advance_sequence(&prefix, &key('x'), &idle),
            SeqStep::None
        );
    }

    #[test]
    fn completion_wins_over_a_longer_shared_prefix() {
        // The longer sequence comes first so a mere entry-order tie-break
        // cannot explain the outcome: the completed one must win because it
        // completed.
        let map: Keymap<Act, Ctx> = Keymap::new(vec![
            Entry::sequence(
                vec![
                    Activator::new(u32::from('g'), Modifiers::empty()),
                    Activator::new(u32::from('q'), Modifiers::empty()),
                    Activator::new(u32::from('x'), Modifiers::empty()),
                ],
                Act::Newline,
            ),
            Entry::sequence(
                vec![
                    Activator::new(u32::from('g'), Modifiers::empty()),
                    Activator::new(u32::from('q'), Modifiers::empty()),
                ],
                Act::Quit,
            ),
        ]);
        let idle = Ctx { running: false };

        // 'q' extends the three-key sequence but completes the two-key one.
        // The completion fires eagerly rather than waiting out the timeout
        // to disambiguate, making the longer sequence unreachable.
        let prefix = vec![key('g')];
        assert_eq!(
            map.advance_sequence(&prefix, &key('q'), &idle),
            SeqStep::Matched(Act::Quit)
        );
    }

    /// A leaf standing in for the editor: records every key it sees and
    /// optionally consumes it.
    struct Leaf {
        seen: Vec<u32>,
        consume: bool,
    }

    impl Widget for Leaf {
        fn draw(&mut self, _ctx: &DrawContext) -> Surface {
            Surface::empty()
        }
        fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
            if let Event::KeyPress(k) = event {
                self.seen.push(k.codepoint);
                if self.consume {
                    ctx.consume_event();
                }
            }
        }
        fn wants_events(&self) -> bool {
            true
        }
    }

    /// The controller wrapping a leaf, wired for dispatch through a
    /// [`FocusHandler`] whose focus path is `[controller, leaf]`.
    struct Rig {
        controller: Rc<RefCell<KeymapController<Act, Ctx>>>,
        leaf: Rc<RefCell<Leaf>>,
        context: Rc<RefCell<Ctx>>,
        fired: Rc<RefCell<Vec<Act>>>,
        focus: FocusHandler,
    }

    impl Rig {
        fn new(keymap: Keymap<Act, Ctx>, leaf_consumes: bool) -> Rig {
            let context = Rc::new(RefCell::new(Ctx { running: false }));
            let fired: Rc<RefCell<Vec<Act>>> = Rc::new(RefCell::new(Vec::new()));
            let leaf = Rc::new(RefCell::new(Leaf {
                seen: Vec::new(),
                consume: leaf_consumes,
            }));
            let sink = Rc::clone(&fired);
            let controller = KeymapController::new(
                keymap,
                Rc::clone(&context),
                to_widget_ref(Rc::clone(&leaf)),
                Box::new(move |_ctx, action| sink.borrow_mut().push(*action)),
            );
            let controller_ref = to_widget_ref(Rc::clone(&controller));
            let leaf_ref = to_widget_ref(Rc::clone(&leaf));
            let mut focus = FocusHandler::init(Rc::clone(&controller_ref));
            focus.focused = Rc::clone(&leaf_ref);
            focus.path_to_focused = vec![controller_ref, leaf_ref];
            Rig {
                controller,
                leaf,
                context,
                fired,
                focus,
            }
        }

        /// Dispatches a key press along the focus path, returning the drained
        /// commands for the caller to apply (or drop).
        fn press(&mut self, k: Key) -> Vec<Command> {
            let mut ctx = EventContext::new();
            self.focus.handle_event(&mut ctx, &Event::KeyPress(k));
            reset_event_state(&mut ctx);
            std::mem::take(&mut ctx.cmds)
        }
    }

    fn ladder_keymap() -> Keymap<Act, Ctx> {
        Keymap::new(vec![
            Entry::single(ctrl_c(), Act::Cancel, BindingPhase::Capture)
                .with_enabled(|cx| cx.running),
            Entry::sequence(vec![ctrl_c(), ctrl_c()], Act::Quit),
        ])
    }

    #[test]
    fn bubble_binding_is_shadowed_by_a_consuming_leaf() {
        let map = Keymap::new(vec![Entry::single(
            Activator::new(u32::from('p'), Modifiers::CTRL),
            Act::Palette,
            BindingPhase::Bubble,
        )]);

        // Consuming leaf: the key stops at the target, the binding never
        // fires.
        let mut rig = Rig::new(map, true);
        rig.press(ctrl('p'));
        assert_eq!(rig.leaf.borrow().seen, vec![u32::from('p')]);
        assert!(rig.fired.borrow().is_empty());

        // Declining leaf: the key bubbles up and the binding fires.
        let map = Keymap::new(vec![Entry::single(
            Activator::new(u32::from('p'), Modifiers::CTRL),
            Act::Palette,
            BindingPhase::Bubble,
        )]);
        let mut rig = Rig::new(map, false);
        rig.press(ctrl('p'));
        assert_eq!(rig.leaf.borrow().seen, vec![u32::from('p')]);
        assert_eq!(*rig.fired.borrow(), vec![Act::Palette]);
    }

    #[test]
    fn capture_binding_preempts_the_leaf() {
        let map = Keymap::new(vec![Entry::single(
            Activator::new(u32::from('p'), Modifiers::CTRL),
            Act::Palette,
            BindingPhase::Capture,
        )]);
        let mut rig = Rig::new(map, true);
        rig.press(ctrl('p'));
        // Fired in capture, so the leaf (which would have consumed it) never
        // saw the key.
        assert_eq!(*rig.fired.borrow(), vec![Act::Palette]);
        assert!(rig.leaf.borrow().seen.is_empty());
    }

    #[test]
    fn sequence_keys_are_consumed_in_capture() {
        let map = Keymap::new(vec![Entry::sequence(
            vec![
                Activator::new(u32::from('g'), Modifiers::empty()),
                Activator::new(u32::from('q'), Modifiers::empty()),
            ],
            Act::Quit,
        )]);
        let mut rig = Rig::new(map, true);

        // The prefix arms the sequence and is consumed before the leaf.
        rig.press(key('g'));
        assert!(rig.leaf.borrow().seen.is_empty());
        assert!(rig.fired.borrow().is_empty());
        assert_eq!(
            rig.controller.borrow().pending_sequence(),
            Some(&[Activator::new(u32::from('g'), Modifiers::empty())][..])
        );

        // The completion fires and is consumed too.
        rig.press(key('q'));
        assert_eq!(*rig.fired.borrow(), vec![Act::Quit]);
        assert!(rig.leaf.borrow().seen.is_empty());
        assert!(rig.controller.borrow().pending_sequence().is_none());
    }

    #[test]
    fn mismatch_resets_the_sequence_and_falls_through() {
        let map = Keymap::new(vec![Entry::sequence(
            vec![
                Activator::new(u32::from('g'), Modifiers::empty()),
                Activator::new(u32::from('q'), Modifiers::empty()),
            ],
            Act::Quit,
        )]);
        let mut rig = Rig::new(map, false);

        rig.press(key('g'));
        assert!(rig.controller.borrow().pending_sequence().is_some());

        // 'x' fits no candidate: the sequence resets and the key falls
        // through to the leaf.
        rig.press(key('x'));
        assert!(rig.controller.borrow().pending_sequence().is_none());
        assert_eq!(rig.leaf.borrow().seen, vec![u32::from('x')]);
        assert!(rig.fired.borrow().is_empty());
    }

    #[test]
    fn timeout_tick_disarms_the_sequence() {
        let map = Keymap::new(vec![Entry::sequence(
            vec![
                Activator::new(u32::from('g'), Modifiers::empty()),
                Activator::new(u32::from('q'), Modifiers::empty()),
            ],
            Act::Quit,
        )]);
        let mut rig = Rig::new(map, true);
        // A zero timeout makes the scheduled tick due immediately.
        rig.controller.borrow_mut().timeout_ms = 0;

        let cmds = rig.press(key('g'));
        assert!(rig.controller.borrow().pending_sequence().is_some());

        // Route the scheduled tick through the real timer machinery.
        let mut core = AppCore::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
        );
        let mut cmds = cmds;
        core.handle_command(&mut cmds);
        assert_eq!(core.timers.len(), 1);
        std::thread::sleep(Duration::from_millis(1));
        let mut ctx = EventContext::new();
        core.check_timers(&mut ctx);

        // The deadline passed, so the tick disarmed the sequence (and asked
        // for a redraw to clear a hint).
        assert!(rig.controller.borrow().pending_sequence().is_none());
        assert!(ctx.redraw);

        // The next 'q' is just a key again, not a completion.
        rig.press(key('q'));
        assert!(rig.fired.borrow().is_empty());
    }

    #[test]
    fn stale_tick_does_not_disarm_an_extended_sequence() {
        let map = Keymap::new(vec![Entry::sequence(
            vec![
                Activator::new(u32::from('g'), Modifiers::empty()),
                Activator::new(u32::from('g'), Modifiers::empty()),
                Activator::new(u32::from('q'), Modifiers::empty()),
            ],
            Act::Quit,
        )]);
        let mut rig = Rig::new(map, true);

        rig.press(key('g'));
        rig.press(key('g'));

        // A tick from the first arm fires now, while the extension moved the
        // deadline well into the future. The sequence must stay armed.
        let mut ctx = EventContext::new();
        rig.controller
            .borrow_mut()
            .handle_event(&mut ctx, &Event::Tick);
        assert!(rig.controller.borrow().pending_sequence().is_some());

        rig.press(key('q'));
        assert_eq!(*rig.fired.borrow(), vec![Act::Quit]);
    }

    /// The cancel/quit ladder from the spec: while a turn runs, ctrl+c fires
    /// Cancel (predicated capture single) and does not arm. While idle, the
    /// first ctrl+c arms the ctrl+c ctrl+c sequence and the second quits. The
    /// timeout disarms.
    #[test]
    fn ctrl_c_ladder() {
        // Running: the enabled single wins over the sequence prefix, fires
        // Cancel, and nothing arms.
        let mut rig = Rig::new(ladder_keymap(), true);
        rig.context.borrow_mut().running = true;
        rig.press(ctrl('c'));
        assert_eq!(*rig.fired.borrow(), vec![Act::Cancel]);
        assert!(rig.controller.borrow().pending_sequence().is_none());
        assert!(rig.leaf.borrow().seen.is_empty());

        // Idle: the single's predicate declines, so the sequence arms, and
        // the second ctrl+c completes it.
        let mut rig = Rig::new(ladder_keymap(), true);
        rig.press(ctrl('c'));
        assert!(rig.fired.borrow().is_empty());
        assert!(rig.controller.borrow().pending_sequence().is_some());
        rig.press(ctrl('c'));
        assert_eq!(*rig.fired.borrow(), vec![Act::Quit]);
        assert!(rig.controller.borrow().pending_sequence().is_none());

        // Idle, timed out: the disarmed sequence starts over instead of
        // quitting.
        let mut rig = Rig::new(ladder_keymap(), true);
        rig.controller.borrow_mut().timeout_ms = 0;
        let mut cmds = rig.press(ctrl('c'));
        let mut core = AppCore::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
        );
        core.handle_command(&mut cmds);
        std::thread::sleep(Duration::from_millis(1));
        let mut ctx = EventContext::new();
        core.check_timers(&mut ctx);
        assert!(rig.controller.borrow().pending_sequence().is_none());
        rig.press(ctrl('c'));
        assert!(rig.fired.borrow().is_empty(), "re-armed, did not quit");
        assert!(rig.controller.borrow().pending_sequence().is_some());
    }
}
