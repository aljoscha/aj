//! The interactive usage overlay (`/usage`).
//!
//! Unlike the other read-only content pages ([`crate::content_overlay`]),
//! the usage page carries one action: spending an earned rate-limit reset
//! credit. That turns it into an interactive overlay with a small state
//! machine ([`Phase`]) rather than a plain scrollable list.
//!
//! The page opens on a background fetch of every provider's usage report.
//! A provider is *eligible* for the reset action when its report carries a
//! non-zero [`aj_models::usage::ProviderUsage::reset_credits`] and a
//! matching [`RateLimitResetSource`] is configured. When eligible, the
//! footer offers the `aj.usage.reset` chord, which drives the flow: pick a
//! provider (when more than one is eligible), confirm, spend the credit (a
//! `POST` behind the source trait, run off the UI thread), show the
//! outcome, then refetch so the reset windows and the new count show.
//!
//! # Off-thread work and the redraw ping
//!
//! The widget lives on the `!Send` UI thread, so it can't call
//! `AsyncApp::request_redraw`. It spawns the fetch and the consume onto a
//! [`tokio::runtime::Handle`] it holds, moving only `Send` data in (the
//! `AuthStorage`, the `Arc<dyn RateLimitResetSource>`, the idempotency
//! key, the [`oneshot`] sender, and a clone of the redraw ping). Each task
//! sends its result over a `oneshot` and pings the shared redraw sender.
//! The drive loop turns the ping into a repaint, and both receivers are
//! drained at the top of [`draw`](UsageOverlay::draw), which the ping
//! guarantees runs.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use aj_app::keybindings::{ACTION_USAGE_RESET, default_action_shortcut};
use aj_app::usage::{ProviderUsageStatus, UsageOutcome};
use aj_models::auth::AuthStorage;
use aj_models::usage::{RateLimitResetSource, ResetOutcome};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use vaxis::cell::{Segment, Style};
use vaxis::key::{Key, Modifiers};
use vaxis::vxfw::{
    Builder, DrawContext, Event, EventContext, ListView, MaxSize, RelativePoint, RichText,
    ScrollBars, SelectStyles, Size, Source, SubSurface, Surface, Widget, WidgetRef, WidthBasis,
    to_widget_ref,
};

use crate::content_overlay::{ContentStyles, Row, plain, usage_rows};
use crate::keymap::action_matches;
use crate::overlay::{
    OverlayChrome, OverlayPlacement, OverlayStack, close_top, confirm_key_label, subtitle_close,
};
use crate::settings_ui::push_window;

/// Where the overlay is in the reset-credit interaction. `Display` is the
/// read-only usage page. The rest are the steps of spending one credit.
enum Phase {
    /// The read-only usage page.
    Display,
    /// More than one provider is eligible: pick which one to reset.
    SelectProvider,
    /// Confirm spending a credit for `provider_id`.
    Confirm { provider_id: String },
    /// Consume request in flight. `key` is the idempotency key, retained
    /// so a retry after a transient failure reuses it and can't
    /// double-spend.
    Consuming { provider_id: String, key: String },
    /// The consume failed transiently. Offers a retry that reuses `key`.
    Failed {
        provider_id: String,
        key: String,
        message: String,
    },
    /// The consume completed (a reset, or a benign no-op like "nothing to
    /// reset"). `Enter` refetches and returns to `Display`.
    Done { message: String },
}

/// A plain, `Copy` tag of the current [`Phase`], so key dispatch can read
/// the phase without holding a borrow of `self.phase` across the `&mut
/// self` handler it calls.
#[derive(Clone, Copy)]
enum PhaseKind {
    Display,
    SelectProvider,
    Confirm,
    Consuming,
    Failed,
    Done,
}

/// One row of an interactive menu phase: the `value` returned when it is
/// confirmed, the `label` shown, and an optional dim description column.
#[derive(Clone)]
struct MenuItem {
    value: String,
    label: String,
    description: Option<String>,
}

impl MenuItem {
    fn new(value: impl Into<String>, label: impl Into<String>) -> MenuItem {
        MenuItem {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    fn with_description(mut self, description: impl Into<String>) -> MenuItem {
        self.description = Some(description.into());
        self
    }
}

/// The interactive usage overlay: a phase machine over the usage report
/// with the in-overlay reset-credit action.
///
/// Focus sits on this widget while it is the top overlay, so it intercepts
/// every key in its capturing phase and owns the whole state machine.
pub(crate) struct UsageOverlay {
    phase: Phase,
    /// Fetched per-provider statuses, once a fetch has landed.
    statuses: Option<Vec<ProviderUsageStatus>>,
    /// Pending fetch (initial or refetch). `None` once received.
    statuses_rx: Option<oneshot::Receiver<Vec<ProviderUsageStatus>>>,
    /// Set when a fetch's sender vanished, so `Display` shows an error row
    /// instead of wedging in its loading state.
    fetch_failed: bool,
    /// Pending consume result. `Some` only while in `Consuming`.
    consume_rx: Option<oneshot::Receiver<Result<ResetOutcome, String>>>,
    /// Rows of the current interactive menu phase, empty in read-only
    /// phases. The list cursor indexes into this to resolve the confirmed
    /// row's `value`.
    menu_items: Vec<MenuItem>,
    /// The row list, shared with `bars` (which draws it). Rebuilt on every
    /// phase change to match the phase.
    list: Rc<RefCell<ListView>>,
    bars: ScrollBars<ListView>,
    /// Content-column tints for the read-only Display rows, snapshotted at
    /// construction.
    styles: ContentStyles,
    /// The selection-band styles for the interactive menu phases,
    /// snapshotted at construction.
    ///
    /// NOTE: like the read-only content overlays, the styles are baked at
    /// open. A theme hot-reload while the overlay is up does not re-tint
    /// it, it re-tints on reopen.
    chrome_select: SelectStyles,
    auth: AuthStorage,
    reset_sources: Vec<Arc<dyn RateLimitResetSource>>,
    /// Where the fetch and consume tasks are spawned. Held so tests can
    /// pass their own runtime rather than relying on `Handle::current`.
    runtime: tokio::runtime::Handle,
    /// Pinged after an off-thread task lands, so the drive loop repaints
    /// and the next draw drains the receiver.
    redraw: UnboundedSender<()>,
    /// Closes this overlay and restores focus to the parent. Runs inside
    /// key dispatch, where the live [`EventContext`] can move focus.
    on_close: Box<dyn FnMut(&mut EventContext)>,
    /// The window's live subtitle cell. Written at the top of each `draw`
    /// with the current phase hint, so the window chrome renders it in the
    /// border (its draw reads this cell after the child's draw has run).
    footer_source: Rc<RefCell<String>>,
}

impl UsageOverlay {
    /// Build the overlay in its loading state and spawn the initial usage
    /// fetch. The fetch pings `redraw` when it lands so the page repaints
    /// without a keypress.
    pub(crate) fn new(
        auth: AuthStorage,
        reset_sources: Vec<Arc<dyn RateLimitResetSource>>,
        styles: ContentStyles,
        chrome_select: SelectStyles,
        runtime: tokio::runtime::Handle,
        redraw: UnboundedSender<()>,
        on_close: Box<dyn FnMut(&mut EventContext)>,
        footer_source: Rc<RefCell<String>>,
    ) -> UsageOverlay {
        let mut list = ListView::new(Source::Slice(row_widgets(&[loading_row()])));
        list.item_count = Some(1);
        // Document scroll with no visible cursor: the menu phases paint
        // their own selection band via the row builder, and the read-only
        // page has no cursor.
        list.draw_cursor = false;
        let mut bars = ScrollBars::new(list);
        bars.draw_horizontal_scrollbar = false;
        let list = Rc::clone(&bars.view);

        let mut overlay = UsageOverlay {
            phase: Phase::Display,
            statuses: None,
            statuses_rx: None,
            fetch_failed: false,
            consume_rx: None,
            menu_items: Vec::new(),
            list,
            bars,
            styles,
            chrome_select,
            auth,
            reset_sources,
            runtime,
            redraw,
            on_close,
            footer_source,
        };
        overlay.start_fetch();
        overlay
    }

    /// The footer hint for the current phase, surfaced in the window's
    /// border subtitle. Resolved every frame so it tracks the state
    /// machine.
    ///
    /// The read-only Display and terminal Done phases share the same
    /// `subtitle_close` "back  \u{2022}  close" convention every other overlay
    /// uses, resolved through the keybinding data: Esc pops this overlay
    /// (`close_top`, i.e. "back") and the close-all chord tears the whole
    /// stack down. The reset chord and the refresh label resolve through the
    /// binding data too. The interactive menu phases keep their own literal
    /// Esc/Enter wording.
    pub(crate) fn footer_hint(&self) -> String {
        match &self.phase {
            Phase::Display => {
                if self.has_eligible_provider() {
                    let reset = default_action_shortcut(ACTION_USAGE_RESET).unwrap_or_default();
                    format!("{reset} use a reset  \u{2022}  {}", subtitle_close())
                } else {
                    subtitle_close()
                }
            }
            Phase::SelectProvider | Phase::Confirm { .. } | Phase::Failed { .. } => {
                "\u{2191}\u{2193} select \u{00b7} Enter confirm \u{00b7} Esc back".to_string()
            }
            Phase::Consuming { .. } => "resetting\u{2026}".to_string(),
            Phase::Done { .. } => {
                format!(
                    "{} refresh  \u{2022}  {}",
                    confirm_key_label(),
                    subtitle_close()
                )
            }
        }
    }

    // --- Eligibility ---

    /// Providers whose report shows reset credits available and that have a
    /// matching reset source, i.e. the ones we can act on.
    fn eligible_providers(&self) -> Vec<String> {
        let Some(statuses) = self.statuses.as_ref() else {
            return Vec::new();
        };
        statuses
            .iter()
            .filter_map(|status| {
                let UsageOutcome::Usage(usage) = &status.outcome else {
                    return None;
                };
                let available = usage.reset_credits?;
                if available == 0 || !self.has_reset_source(&status.provider_id) {
                    return None;
                }
                Some(status.provider_id.clone())
            })
            .collect()
    }

    fn has_eligible_provider(&self) -> bool {
        !self.eligible_providers().is_empty()
    }

    fn has_reset_source(&self, provider_id: &str) -> bool {
        self.reset_source_for(provider_id).is_some()
    }

    fn reset_source_for(&self, provider_id: &str) -> Option<Arc<dyn RateLimitResetSource>> {
        self.reset_sources
            .iter()
            .find(|source| source.provider_id() == provider_id)
            .cloned()
    }

    /// The reset credits available for `provider_id`, for the confirm
    /// subtitle. `0` if unknown (the confirm still works).
    fn available_for(&self, provider_id: &str) -> u32 {
        self.statuses
            .iter()
            .flatten()
            .find(|status| status.provider_id == provider_id)
            .and_then(|status| match &status.outcome {
                UsageOutcome::Usage(usage) => usage.reset_credits,
                _ => None,
            })
            .unwrap_or(0)
    }

    // --- Flow transitions ---

    /// The reset chord from `Display`: enter the confirm flow, picking a
    /// provider first when more than one is eligible.
    fn begin_reset_flow(&mut self) {
        let mut eligible = self.eligible_providers();
        match eligible.len() {
            0 => {}
            1 => self.set_phase(Phase::Confirm {
                provider_id: eligible.remove(0),
            }),
            _ => self.set_phase(Phase::SelectProvider),
        }
    }

    /// Kick off spending one credit for `provider_id` under idempotency key
    /// `key`. Spawns the request and moves to `Consuming`.
    fn begin_consume(&mut self, provider_id: String, key: String) {
        let Some(source) = self.reset_source_for(&provider_id) else {
            // The action is only offered for providers with a source, so
            // this is defensive.
            self.set_phase(Phase::Failed {
                provider_id,
                key,
                message: "Resetting is not supported for this provider.".to_string(),
            });
            return;
        };

        let (tx, rx) = oneshot::channel();
        let auth = self.auth.clone();
        let redraw = self.redraw.clone();
        let task_key = key.clone();
        self.runtime.spawn(async move {
            let result = source
                .consume_reset_credit(&auth, &task_key)
                .await
                .map_err(|err| err.to_string());
            if tx.send(result).is_ok() {
                // A dropped receiver (the overlay closed) makes this a
                // no-op.
                let _ = redraw.send(());
            }
        });
        self.consume_rx = Some(rx);
        self.set_phase(Phase::Consuming { provider_id, key });
    }

    /// Spawn a fresh usage fetch and route it into `statuses_rx`, shared by
    /// the initial load and the post-reset refresh.
    fn start_fetch(&mut self) {
        let (tx, rx) = oneshot::channel();
        let auth = self.auth.clone();
        let redraw = self.redraw.clone();
        self.runtime.spawn(async move {
            let statuses = aj_app::usage::collect_usage(&auth).await;
            if tx.send(statuses).is_ok() {
                let _ = redraw.send(());
            }
        });
        self.statuses_rx = Some(rx);
        self.fetch_failed = false;
    }

    /// Turn a finished consume into a terminal phase.
    fn apply_consume_result(&mut self, result: Result<ResetOutcome, String>) {
        let Phase::Consuming { provider_id, key } = &self.phase else {
            return; // stale result after the user navigated away
        };
        let (provider_id, key) = (provider_id.clone(), key.clone());
        let next = match result {
            Ok(ResetOutcome::Reset | ResetOutcome::AlreadyRedeemed) => Phase::Done {
                message: "Usage reset.".to_string(),
            },
            Ok(ResetOutcome::NothingToReset) => Phase::Done {
                message: "Nothing to reset right now.".to_string(),
            },
            Ok(ResetOutcome::NoCredit) => Phase::Done {
                message: "No rate-limit resets available.".to_string(),
            },
            Err(err) => Phase::Failed {
                provider_id,
                key,
                message: format!("Couldn't reset usage: {err}"),
            },
        };
        self.set_phase(next);
    }

    fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.rebuild_content();
    }

    // --- Polling (called at the top of `draw`) ---

    /// Drain the initial/refetch fetch receiver.
    fn poll_statuses(&mut self) {
        let Some(rx) = self.statuses_rx.as_mut() else {
            return;
        };
        match rx.try_recv() {
            Ok(statuses) => {
                self.statuses = Some(statuses);
                self.statuses_rx = None;
                self.fetch_failed = false;
                if matches!(self.phase, Phase::Display) {
                    self.rebuild_content();
                }
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.statuses_rx = None;
                self.fetch_failed = true;
                if matches!(self.phase, Phase::Display) {
                    self.rebuild_content();
                }
            }
        }
    }

    /// Drain the consume receiver.
    fn poll_consume(&mut self) {
        let Some(rx) = self.consume_rx.as_mut() else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.consume_rx = None;
                self.apply_consume_result(result);
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.consume_rx = None;
                self.apply_consume_result(Err("reset task ended unexpectedly".to_string()));
            }
        }
    }

    // --- List rebuild ---

    /// Rebuild the inner list to match the current phase: read-only rows
    /// for the display/consuming/done phases, a banded menu for the
    /// interactive ones.
    fn rebuild_content(&mut self) {
        // Compute the phase's rows before touching `self.list`: the two
        // matches only borrow `&self.phase` and end before the mutation.
        let menu_items: Vec<MenuItem> = match &self.phase {
            Phase::SelectProvider => provider_items(&self.eligible_providers(), self),
            Phase::Confirm { provider_id } => {
                confirm_items(provider_id, self.available_for(provider_id))
            }
            Phase::Failed { message, .. } => failed_items(message),
            _ => Vec::new(),
        };
        let rows: Vec<Row> = match &self.phase {
            Phase::Display => self.display_rows(),
            Phase::Consuming { .. } => vec![plain("Resetting\u{2026}")],
            Phase::Done { message } => vec![plain(message.clone())],
            _ => Vec::new(),
        };
        let interactive = matches!(
            self.phase,
            Phase::SelectProvider | Phase::Confirm { .. } | Phase::Failed { .. }
        );

        self.menu_items = menu_items;
        let (source, count) = if interactive {
            (
                Source::Builder(Box::new(MenuRowBuilder {
                    items: self.menu_items.clone(),
                    styles: self.chrome_select.clone(),
                })),
                self.menu_items.len(),
            )
        } else {
            (Source::Slice(row_widgets(&rows)), rows.len())
        };

        let mut list = self.list.borrow_mut();
        list.children = source;
        list.item_count = Some(u32::try_from(count).expect("row count fits u32"));
        list.jump_to_item(0);
    }

    /// Rows for the read-only `Display` phase.
    fn display_rows(&self) -> Vec<Row> {
        if self.statuses_rx.is_some() {
            return vec![loading_row()];
        }
        if self.fetch_failed {
            return vec![plain("Usage fetch failed.")];
        }
        match self.statuses.as_ref() {
            Some(statuses) => usage_rows(statuses, &self.styles),
            None => vec![loading_row()],
        }
    }

    /// The confirmed menu row's `value`, from the list cursor.
    fn selected_value(&self) -> Option<String> {
        let cursor = usize::try_from(self.list.borrow().cursor).ok()?;
        self.menu_items.get(cursor).map(|item| item.value.clone())
    }

    // --- Per-phase key handling ---

    fn phase_kind(&self) -> PhaseKind {
        match &self.phase {
            Phase::Display => PhaseKind::Display,
            Phase::SelectProvider => PhaseKind::SelectProvider,
            Phase::Confirm { .. } => PhaseKind::Confirm,
            Phase::Consuming { .. } => PhaseKind::Consuming,
            Phase::Failed { .. } => PhaseKind::Failed,
            Phase::Done { .. } => PhaseKind::Done,
        }
    }

    fn handle_display_key(&mut self, ctx: &mut EventContext, key: &Key) {
        // The reset chord is overlay-local, matched here at-target rather than
        // through the global keymap. Both the match and the footer label
        // resolve through the shared binding data, so they cannot drift.
        if action_matches(key, ACTION_USAGE_RESET) {
            self.begin_reset_flow();
        } else if key.matches(Key::ESCAPE, Modifiers::empty())
            || key.matches(Key::ENTER, Modifiers::empty())
        {
            (self.on_close)(ctx);
        }
        // Every other key is swallowed: the page is read-only (the wheel
        // still scrolls the body through the list underneath).
        ctx.consume_and_redraw();
    }

    fn handle_select_key(&mut self, ctx: &mut EventContext, key: &Key) {
        if key.matches(Key::ESCAPE, Modifiers::empty()) {
            self.set_phase(Phase::Display);
        } else if key.matches(Key::ENTER, Modifiers::empty()) {
            if let Some(provider_id) = self.selected_value() {
                self.set_phase(Phase::Confirm { provider_id });
            }
        } else {
            self.move_menu_cursor(ctx, key);
        }
        ctx.consume_and_redraw();
    }

    fn handle_confirm_key(&mut self, ctx: &mut EventContext, key: &Key) {
        let Phase::Confirm { provider_id } = &self.phase else {
            return;
        };
        let provider_id = provider_id.clone();
        if key.matches(Key::ESCAPE, Modifiers::empty()) {
            self.set_phase(Phase::Display);
        } else if key.matches(Key::ENTER, Modifiers::empty()) {
            match self.selected_value().as_deref() {
                Some("confirm") => self.begin_consume(provider_id, new_idempotency_key()),
                _ => self.set_phase(Phase::Display),
            }
        } else {
            self.move_menu_cursor(ctx, key);
        }
        ctx.consume_and_redraw();
    }

    fn handle_failed_key(&mut self, ctx: &mut EventContext, key: &Key) {
        let Phase::Failed {
            provider_id,
            key: idempotency_key,
            ..
        } = &self.phase
        else {
            return;
        };
        let (provider_id, idempotency_key) = (provider_id.clone(), idempotency_key.clone());
        if key.matches(Key::ESCAPE, Modifiers::empty()) {
            self.set_phase(Phase::Display);
        } else if key.matches(Key::ENTER, Modifiers::empty()) {
            match self.selected_value().as_deref() {
                // The retry reuses the idempotency key, so it can't
                // double-spend even if the failed attempt reached the
                // server.
                Some("retry") => self.begin_consume(provider_id, idempotency_key),
                _ => self.set_phase(Phase::Display),
            }
        } else {
            self.move_menu_cursor(ctx, key);
        }
        ctx.consume_and_redraw();
    }

    fn handle_done_key(&mut self, ctx: &mut EventContext, key: &Key) {
        if key.matches(Key::ESCAPE, Modifiers::empty()) {
            (self.on_close)(ctx);
        } else if key.matches(Key::ENTER, Modifiers::empty()) {
            // Refetch so the reset windows and the updated count show.
            self.start_fetch();
            self.set_phase(Phase::Display);
        }
        ctx.consume_and_redraw();
    }

    /// Move the menu cursor for Up/Down; other keys are no-ops here.
    fn move_menu_cursor(&self, ctx: &mut EventContext, key: &Key) {
        if key.matches(Key::DOWN, Modifiers::empty()) {
            self.list.borrow_mut().next_item(ctx);
        } else if key.matches(Key::UP, Modifiers::empty()) {
            self.list.borrow_mut().prev_item(ctx);
        }
    }

    #[cfg(test)]
    fn seed_statuses(&mut self, statuses: Vec<ProviderUsageStatus>) {
        self.statuses = Some(statuses);
        self.statuses_rx = None;
        self.fetch_failed = false;
        self.rebuild_content();
    }

    #[cfg(test)]
    fn select_menu_value(&self, value: &str) {
        if let Some(pos) = self.menu_items.iter().position(|item| item.value == value) {
            self.list
                .borrow_mut()
                .jump_to_item(u32::try_from(pos).expect("pos fits u32"));
        }
    }
}

impl Widget for UsageOverlay {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // Poll first: the redraw ping guarantees this draw runs after an
        // off-thread task lands, so draining here is where results are
        // applied.
        self.poll_statuses();
        self.poll_consume();

        // Publish the phase hint into the window's live subtitle cell. The
        // window's draw reads it after this child's draw returns, so it
        // lands in the border chrome this same frame.
        *self.footer_source.borrow_mut() = self.footer_hint();

        let size = ctx.max.size();
        // An opaque full-size surface so a shorter phase can't leave stale
        // cells from a taller previous frame.
        let mut surface = Surface::with_size(size);
        let zero = Size {
            width: 0,
            height: 0,
        };

        let body_height = size.height;
        if body_height > 0 {
            let body_ctx = ctx.with_constraints(
                zero,
                MaxSize {
                    width: Some(size.width),
                    height: Some(body_height),
                },
            );
            surface.children.push(SubSurface {
                origin: RelativePoint { row: 0, col: 0 },
                surface: self.bars.draw(&body_ctx),
                z_index: 0,
            });
        }
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else {
            // Mouse events fall through to the list (wheel scroll); no
            // consume here.
            return;
        };
        match self.phase_kind() {
            PhaseKind::Display => self.handle_display_key(ctx, key),
            PhaseKind::SelectProvider => self.handle_select_key(ctx, key),
            PhaseKind::Confirm => self.handle_confirm_key(ctx, key),
            // The consume is quick and idempotency-keyed, so there's no
            // cancel: swallow every key.
            PhaseKind::Consuming => ctx.consume_and_redraw(),
            PhaseKind::Failed => self.handle_failed_key(ctx, key),
            PhaseKind::Done => self.handle_done_key(ctx, key),
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// Build a titled usage overlay over `deps` and push it as the top
/// overlay. Does not move focus: the host posts the refocus event.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_usage_overlay(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    styles: ContentStyles,
    auth: AuthStorage,
    reset_sources: Vec<Arc<dyn RateLimitResetSource>>,
    runtime: tokio::runtime::Handle,
    redraw: UnboundedSender<()>,
) {
    let on_close: Box<dyn FnMut(&mut EventContext)> = {
        let stack = Rc::clone(stack);
        let editor = Rc::clone(editor);
        Box::new(move |ctx| close_top(&stack, ctx, &editor))
    };
    // The window's subtitle is fed from this shared cell: the overlay writes
    // the current phase hint into it during its draw, and the window renders
    // it in the border afterwards.
    let footer_source = Rc::new(RefCell::new(String::new()));
    let overlay = Rc::new(RefCell::new(UsageOverlay::new(
        auth,
        reset_sources,
        styles,
        chrome.select.clone(),
        runtime,
        redraw,
        on_close,
        Rc::clone(&footer_source),
    )));
    let focus = to_widget_ref(Rc::clone(&overlay));
    // The static subtitle stays empty. The live `footer_source` overrides it,
    // so the border shows the phase-appropriate hint like every other window.
    let window = push_window(
        stack,
        chrome,
        "Usage",
        String::new(),
        to_widget_ref(overlay),
        focus,
        OverlayPlacement::Large,
    );
    window.borrow_mut().subtitle_source = Some(footer_source);
}

/// The single-row "Loading…" seed.
fn loading_row() -> Row {
    plain("Loading usage\u{2026}")
}

/// Build the list-row widgets for a set of read-only rows.
fn row_widgets(rows: &[Row]) -> Vec<WidgetRef> {
    rows.iter()
        .map(|r| {
            let text: WidgetRef = Rc::new(RefCell::new(RichText::new(r.clone())));
            text
        })
        .collect()
}

/// Confirm menu: spending a credit or backing out. Names the provider and
/// what the reset does so the screen stands on its own, and defaults to the
/// reset since that's the reason the user opened it.
fn confirm_items(provider_id: &str, available: u32) -> Vec<MenuItem> {
    vec![
        MenuItem::new("confirm", format!("Use a reset for {provider_id}")).with_description(
            format!("clears the current limits \u{00b7} {available} available"),
        ),
        MenuItem::new("cancel", "Cancel"),
    ]
}

/// Retry menu shown after a transient consume failure. Defaults to "Try
/// again". The retry reuses the idempotency key, so it can't double-spend
/// even if the failed attempt actually reached the server.
fn failed_items(message: &str) -> Vec<MenuItem> {
    vec![
        MenuItem::new("retry", "Try again").with_description(message),
        MenuItem::new("cancel", "Back"),
    ]
}

/// Provider picker rows when several providers are eligible.
fn provider_items(providers: &[String], overlay: &UsageOverlay) -> Vec<MenuItem> {
    providers
        .iter()
        .map(|id| {
            MenuItem::new(id.clone(), id.clone())
                .with_description(format!("{} available", overlay.available_for(id)))
        })
        .collect()
}

/// Builds one banded row per menu index, tinting the row the cursor is on
/// with the selection band. Mirrors the pick-list band: the full inner
/// width fills with `selected_bg` and the text spans sit on the band.
struct MenuRowBuilder {
    items: Vec<MenuItem>,
    styles: SelectStyles,
}

impl Builder for MenuRowBuilder {
    fn item_at_idx(&self, idx: usize, cursor: usize) -> Option<WidgetRef> {
        let item = self.items.get(idx)?;
        Some(build_banded_row(item, idx == cursor, &self.styles))
    }
}

/// Build one menu row as a full-width [`RichText`] whose cells all carry
/// `selected_bg` when `selected`, so the band spans the inner width even
/// past the text.
fn build_banded_row(item: &MenuItem, selected: bool, styles: &SelectStyles) -> WidgetRef {
    let band = selected.then_some(styles.selected_bg);
    let tint = |mut style: Style| -> Style {
        if let Some(bg) = band {
            style.bg = bg;
        }
        style
    };
    let mut spans = vec![Segment {
        text: item.label.clone(),
        style: tint(styles.label),
        ..Segment::default()
    }];
    if let Some(description) = &item.description {
        spans.push(Segment {
            text: "  ".to_string(),
            style: tint(styles.secondary),
            ..Segment::default()
        });
        spans.push(Segment {
            text: description.clone(),
            style: tint(styles.secondary),
            ..Segment::default()
        });
    }
    let mut rich = RichText::new(spans);
    // Single-line rows: long content truncates rather than wrapping.
    rich.softwrap = false;
    // Full inner width so the band (and its fill cells) reach the edge.
    rich.width_basis = WidthBasis::Parent;
    if let Some(bg) = band {
        rich.base_style = Style {
            bg,
            ..Style::default()
        };
    }
    Rc::new(RefCell::new(rich))
}

/// A random 128-bit idempotency key formatted as a UUID v4 string.
/// Uniqueness is all the endpoint needs. The version/variant bits keep it a
/// well-formed UUID for servers that validate the shape.
fn new_idempotency_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use aj_models::usage::{ProviderUsage, UsageError, UsageWindow};
    use async_trait::async_trait;
    use vaxis::cell::Color;

    use super::*;

    /// A leaked test runtime so the widget can carry a valid `Handle` from
    /// a plain `#[test]`. Tasks spawned onto it actually run, which the
    /// consume-flow tests rely on.
    fn runtime_handle() -> tokio::runtime::Handle {
        static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        RT.get_or_init(|| tokio::runtime::Runtime::new().unwrap())
            .handle()
            .clone()
    }

    fn scratch_auth() -> AuthStorage {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("aj-next-usage-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        AuthStorage::with_providers(dir.join("auth.json"), HashMap::new())
    }

    /// Distinct dim/muted tints so a column left at the default fg fails the
    /// tinting assertions.
    fn test_styles() -> ContentStyles {
        ContentStyles {
            dim: Style {
                fg: Color::Index(1),
                ..Style::default()
            },
            muted: Style {
                fg: Color::Index(2),
                ..Style::default()
            },
            heading: Style {
                fg: Color::Index(3),
                bold: true,
                ..Style::default()
            },
        }
    }

    /// A reset source that returns a scripted outcome and records every
    /// idempotency key it receives, so tests don't hit the network and can
    /// assert on retry key reuse.
    struct FakeResetSource {
        provider_id: String,
        outcome: Result<ResetOutcome, String>,
        keys: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl RateLimitResetSource for FakeResetSource {
        fn provider_id(&self) -> &str {
            &self.provider_id
        }
        async fn consume_reset_credit(
            &self,
            _auth: &AuthStorage,
            idempotency_key: &str,
        ) -> Result<ResetOutcome, UsageError> {
            self.keys.lock().unwrap().push(idempotency_key.to_string());
            self.outcome.clone().map_err(UsageError::Fetch)
        }
    }

    fn fake_source_for(
        provider_id: &str,
        outcome: Result<ResetOutcome, String>,
    ) -> Arc<dyn RateLimitResetSource> {
        Arc::new(FakeResetSource {
            provider_id: provider_id.into(),
            outcome,
            keys: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn fake_source(outcome: Result<ResetOutcome, String>) -> Arc<dyn RateLimitResetSource> {
        fake_source_for("openai-codex", outcome)
    }

    fn usage_status(provider_id: &str, reset_credits: Option<u32>) -> ProviderUsageStatus {
        ProviderUsageStatus {
            provider_id: provider_id.into(),
            outcome: UsageOutcome::Usage(ProviderUsage {
                windows: vec![UsageWindow {
                    label: "5h limit".into(),
                    used: 0.96,
                    resets_at: None,
                }],
                notes: vec![],
                reset_credits,
            }),
        }
    }

    fn codex_status(reset_credits: Option<u32>) -> ProviderUsageStatus {
        usage_status("openai-codex", reset_credits)
    }

    /// Build an overlay whose deps carry the given sources, then seed the
    /// statuses directly so eligibility/consume tests don't need a live
    /// fetch. The `on_close` flips `closed` so tests can assert the close
    /// callback fired.
    fn overlay_with(
        statuses: Vec<ProviderUsageStatus>,
        sources: Vec<Arc<dyn RateLimitResetSource>>,
    ) -> (UsageOverlay, Rc<RefCell<bool>>) {
        let closed = Rc::new(RefCell::new(false));
        let on_close: Box<dyn FnMut(&mut EventContext)> = {
            let closed = Rc::clone(&closed);
            Box::new(move |_ctx| *closed.borrow_mut() = true)
        };
        let mut overlay = UsageOverlay::new(
            scratch_auth(),
            sources,
            test_styles(),
            SelectStyles::default(),
            runtime_handle(),
            tokio::sync::mpsc::unbounded_channel().0,
            on_close,
            Rc::new(RefCell::new(String::new())),
        );
        overlay.seed_statuses(statuses);
        (overlay, closed)
    }

    fn key(codepoint: u32, mods: Modifiers) -> Event {
        Event::KeyPress(Key {
            codepoint,
            mods,
            ..Key::default()
        })
    }

    fn send(overlay: &mut UsageOverlay, event: &Event) {
        let mut ctx = EventContext::new();
        overlay.capture_event(&mut ctx, event);
    }

    /// Render the overlay to a flat text blob, so plain `.contains(...)`
    /// assertions work.
    fn body(overlay: &mut UsageOverlay) -> String {
        let ctx = crate::test_support::draw_ctx(120, Some(20));
        let surface = overlay.draw(&ctx);
        crate::test_support::rows(&surface).join("\n")
    }

    /// Render in a bounded loop until `needle` appears, giving a spawned
    /// consume task time to land. Panics on timeout so a broken wiring
    /// fails loudly instead of hanging.
    fn wait_for(overlay: &mut UsageOverlay, needle: &str) -> String {
        for _ in 0..200 {
            let out = body(overlay);
            if out.contains(needle) {
                return out;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("timed out waiting for {needle:?}");
    }

    #[test]
    fn eligibility_needs_credits_and_a_source() {
        // Credits present and a matching source: eligible.
        let (overlay, _) = overlay_with(
            vec![codex_status(Some(2))],
            vec![fake_source(Ok(ResetOutcome::Reset))],
        );
        assert!(overlay.has_eligible_provider());

        // Credits but no matching source: not eligible.
        let (overlay, _) = overlay_with(vec![codex_status(Some(2))], vec![]);
        assert!(!overlay.has_eligible_provider());

        // A source but zero credits: not eligible.
        let (overlay, _) = overlay_with(
            vec![codex_status(Some(0))],
            vec![fake_source(Ok(ResetOutcome::Reset))],
        );
        assert!(!overlay.has_eligible_provider());

        // A source but no credit field at all: not eligible.
        let (overlay, _) = overlay_with(
            vec![codex_status(None)],
            vec![fake_source(Ok(ResetOutcome::Reset))],
        );
        assert!(!overlay.has_eligible_provider());
    }

    #[test]
    fn reset_key_noop_without_eligible_provider() {
        // Credits present but no matching source, so not eligible.
        let (mut overlay, _) = overlay_with(vec![codex_status(Some(2))], vec![]);
        assert!(!overlay.has_eligible_provider());
        send(&mut overlay, &key(u32::from('r'), Modifiers::empty()));
        let out = body(&mut overlay);
        assert!(out.contains("Rate-limit resets"), "{out}");
        assert!(!out.contains("Use a reset"), "{out}");
    }

    #[test]
    fn reset_key_opens_confirm_then_esc_returns() {
        let (mut overlay, _) = overlay_with(
            vec![codex_status(Some(2))],
            vec![fake_source(Ok(ResetOutcome::Reset))],
        );
        assert!(overlay.has_eligible_provider());

        send(&mut overlay, &key(u32::from('r'), Modifiers::empty()));
        let out = body(&mut overlay);
        // The confirm names the provider and defaults to the reset.
        assert!(out.contains("Use a reset for openai-codex"), "{out}");
        assert!(out.contains("Cancel"), "{out}");
        assert_eq!(overlay.selected_value().as_deref(), Some("confirm"));

        // Esc backs out to the read-only page without spending.
        send(&mut overlay, &key(Key::ESCAPE, Modifiers::empty()));
        let out = body(&mut overlay);
        assert!(out.contains("5h limit"), "{out}");
        assert!(!out.contains("Use a reset"), "{out}");
    }

    #[test]
    fn apply_consume_result_maps_outcomes() {
        let cases = [
            (Ok(ResetOutcome::Reset), "Usage reset."),
            (Ok(ResetOutcome::AlreadyRedeemed), "Usage reset."),
            (
                Ok(ResetOutcome::NothingToReset),
                "Nothing to reset right now.",
            ),
            (
                Ok(ResetOutcome::NoCredit),
                "No rate-limit resets available.",
            ),
            (Err("boom".to_string()), "Couldn't reset usage: boom"),
        ];
        for (result, expected) in cases {
            let (mut overlay, _) = overlay_with(vec![codex_status(Some(2))], vec![]);
            overlay.phase = Phase::Consuming {
                provider_id: "openai-codex".into(),
                key: "k".into(),
            };
            overlay.apply_consume_result(result);
            let out = body(&mut overlay);
            assert!(out.contains(expected), "expected `{expected}` in:\n{out}");
        }
    }

    #[test]
    fn consume_wires_source_and_shows_outcome() {
        let (mut overlay, _) = overlay_with(
            vec![codex_status(Some(2))],
            vec![fake_source(Ok(ResetOutcome::Reset))],
        );
        overlay.begin_consume("openai-codex".into(), "k".into());
        let out = wait_for(&mut overlay, "Usage reset.");
        assert!(out.contains("Usage reset."), "{out}");
    }

    #[test]
    fn retry_reuses_idempotency_key() {
        // A source that always fails, so the flow lands back on the retry
        // menu and we can drive a second attempt.
        let keys = Arc::new(Mutex::new(Vec::new()));
        let source: Arc<dyn RateLimitResetSource> = Arc::new(FakeResetSource {
            provider_id: "openai-codex".into(),
            outcome: Err("boom".into()),
            keys: Arc::clone(&keys),
        });
        let (mut overlay, _) = overlay_with(vec![codex_status(Some(2))], vec![source]);

        // r -> confirm; "Use a reset" is the default row, so Enter mints a
        // key and spends it.
        send(&mut overlay, &key(u32::from('r'), Modifiers::empty()));
        overlay.select_menu_value("confirm");
        send(&mut overlay, &key(Key::ENTER, Modifiers::empty()));
        wait_for(&mut overlay, "Try again");

        // "Try again" is the default selection, so Enter retries.
        send(&mut overlay, &key(Key::ENTER, Modifiers::empty()));
        wait_for(&mut overlay, "Try again");

        let recorded = keys.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2, "two attempts: {recorded:?}");
        assert_eq!(
            recorded[0], recorded[1],
            "retry must reuse the idempotency key"
        );
    }

    #[test]
    fn multiple_eligible_providers_show_picker_and_change_footer() {
        let statuses = vec![
            usage_status("openai-codex", Some(2)),
            usage_status("other", Some(1)),
        ];
        let sources = vec![
            fake_source_for("openai-codex", Ok(ResetOutcome::Reset)),
            fake_source_for("other", Ok(ResetOutcome::Reset)),
        ];
        let (mut overlay, _) = overlay_with(statuses, sources);

        send(&mut overlay, &key(u32::from('r'), Modifiers::empty()));
        let out = body(&mut overlay);
        assert!(out.contains("openai-codex"), "{out}");
        assert!(out.contains("other"), "{out}");
        assert_eq!(
            overlay.footer_hint(),
            "\u{2191}\u{2193} select \u{00b7} Enter confirm \u{00b7} Esc back"
        );

        // Choosing a provider advances to its confirm step.
        overlay.select_menu_value("other");
        send(&mut overlay, &key(Key::ENTER, Modifiers::empty()));
        assert!(body(&mut overlay).contains("Use a reset"));
        match &overlay.phase {
            Phase::Confirm { provider_id } => assert_eq!(provider_id, "other"),
            _ => panic!("expected Confirm for the chosen provider"),
        }
    }

    #[test]
    fn done_enter_refetches_and_returns_to_display() {
        let (mut overlay, _) = overlay_with(
            vec![codex_status(Some(2))],
            vec![fake_source(Ok(ResetOutcome::Reset))],
        );
        overlay.set_phase(Phase::Done {
            message: "Usage reset.".into(),
        });
        send(&mut overlay, &key(Key::ENTER, Modifiers::empty()));
        // A refetch is now in flight, back on the read-only page. Checked
        // without a draw so the poll can't race the spawned fetch to a
        // conclusion.
        assert!(matches!(overlay.phase, Phase::Display));
        assert!(overlay.statuses_rx.is_some(), "a refetch is in flight");
        let rows = overlay.display_rows();
        let text: String = rows
            .iter()
            .flat_map(|r| r.iter().map(|s| s.text.clone()))
            .collect();
        assert!(text.contains("Loading usage"), "loading row: {text}");
    }

    #[test]
    fn esc_closes_from_display() {
        let (mut overlay, closed) = overlay_with(vec![codex_status(None)], vec![]);
        send(&mut overlay, &key(Key::ESCAPE, Modifiers::empty()));
        assert!(*closed.borrow(), "Esc runs on_close from Display");
    }

    #[test]
    fn footer_hint_resolves_reset_label_from_binding_data() {
        let (overlay, _) = overlay_with(
            vec![codex_status(Some(2))],
            vec![fake_source(Ok(ResetOutcome::Reset))],
        );
        // The assertion value is itself derived from the binding table, so
        // a rebind moves both the rendered label and this expectation
        // together (never a literal `r`).
        let reset =
            default_action_shortcut(ACTION_USAGE_RESET).expect("usage-reset has a default chord");
        let hint = overlay.footer_hint();
        assert_eq!(
            hint,
            format!("{reset} use a reset  \u{2022}  {}", subtitle_close())
        );
        assert!(hint.contains(&reset), "{hint}");
        // The literal chord is not what shows: the default `r` formats to
        // `R`, so the hint must not carry a bare `r use a reset`.
        assert!(!hint.contains("r use a reset"), "{hint}");
    }

    /// The usage Display/Done footers share the exact `subtitle_close`
    /// convention every other overlay uses. Tying the assertions to
    /// `subtitle_close` (rather than a literal) means a future change to the
    /// shared convention flows through to the usage footer automatically.
    #[test]
    fn usage_footer_shares_close_convention() {
        // Display, not eligible: the footer is exactly the shared close
        // convention.
        let (overlay, _) = overlay_with(vec![codex_status(None)], vec![]);
        assert_eq!(overlay.footer_hint(), subtitle_close());

        // Done: the footer ends with the shared close convention.
        let (mut overlay, _) = overlay_with(
            vec![codex_status(Some(2))],
            vec![fake_source(Ok(ResetOutcome::Reset))],
        );
        overlay.set_phase(Phase::Done {
            message: "Usage reset.".into(),
        });
        let hint = overlay.footer_hint();
        assert!(hint.ends_with(&subtitle_close()), "{hint}");
    }

    /// A `draw` publishes the phase hint into `footer_source` (the window's
    /// live subtitle cell) rather than rendering it as a body row: the
    /// inline footer is gone and the list uses the full height.
    #[test]
    fn draw_publishes_footer_hint_to_chrome_not_body() {
        // Not eligible: the display footer is the shared close convention.
        let (mut overlay, _) = overlay_with(vec![codex_status(None)], vec![]);
        let out = body(&mut overlay);
        assert_eq!(overlay.footer_hint(), subtitle_close());
        assert_eq!(*overlay.footer_source.borrow(), subtitle_close());
        // The inline footer is gone: the child draw no longer carries the
        // hint as a row.
        assert!(
            !out.contains(&subtitle_close()),
            "footer leaked into body:\n{out}"
        );

        // Eligible: the footer carries the resolved reset chord, and the
        // shared cell matches `footer_hint()` exactly.
        let (mut overlay, _) = overlay_with(
            vec![codex_status(Some(2))],
            vec![fake_source(Ok(ResetOutcome::Reset))],
        );
        let out = body(&mut overlay);
        let hint = overlay.footer_hint();
        let reset =
            default_action_shortcut(ACTION_USAGE_RESET).expect("usage-reset has a default chord");
        assert!(hint.contains(&reset), "{hint}");
        assert_eq!(*overlay.footer_source.borrow(), hint);
        assert!(!out.contains(&hint), "footer leaked into body:\n{out}");
    }

    /// The Display rows keep the P4a tinting: the provider-id column in the
    /// dim tint, the status detail in the muted tint. Fails if a column is
    /// left at the default fg.
    #[test]
    fn display_rows_preserve_column_tints() {
        let (overlay, _) = overlay_with(vec![codex_status(Some(2))], vec![]);
        let rows = overlay.display_rows();
        let first = &rows[0];
        assert_eq!(first.len(), 3, "id, label, and detail spans: {first:?}");
        assert!(first[0].text.contains("openai-codex"), "{first:?}");
        assert_eq!(first[0].style, test_styles().dim);
        assert!(first[1].text.contains("5h limit"), "{first:?}");
        assert_eq!(first[1].style, Style::default());
        assert_eq!(first[2].style, test_styles().muted);
    }
}
