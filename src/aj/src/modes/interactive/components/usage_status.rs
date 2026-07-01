//! Usage overlay (`/usage`).
//!
//! One page for every provider's plan-usage report: rate-limit windows
//! as rows (provider id shown as a dim prefix on the provider's first
//! row only, so consecutive rows read as a group), plus reason rows for
//! providers that can't report usage. The reports arrive from a
//! background fetch through a oneshot channel drained in `render`. Until
//! then a loading row is shown.
//!
//! The page is read-only except for one action: spending an earned
//! rate-limit reset credit. A provider is *eligible* when its report
//! carries [`aj_models::usage::ProviderUsage::reset_credits`] with a
//! non-zero count and a matching [`RateLimitResetSource`] is configured.
//! When eligible, the footer offers the `tui.usage.reset` key, which
//! drives a small in-overlay state machine ([`Phase`]): confirm, spend
//! (a `POST` behind the source trait), show the outcome, then refetch so
//! the reset windows and the new count show. Everything else in the
//! overlay stays read-only, and closing at any point just drops the
//! in-flight receivers.

use std::sync::Arc;

use aj_models::auth::AuthStorage;
use aj_models::usage::{RateLimitResetSource, ResetOutcome};
use aj_tui::component::Component;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;
use aj_tui::tui::RenderHandle;
use tokio::sync::oneshot;

use crate::modes::interactive::components::outcome::OutcomeSlot;
use crate::usage::{ProviderUsageStatus, UsageOutcome, format_window_status, now_unix_ms};

/// Outcome of a single usage-overlay session. The overlay is read-only
/// apart from the reset action, and even that returns to `Display`
/// internally, so the only terminal state is `Closed`.
#[derive(Clone, Debug)]
pub enum UsageStatusOutcome {
    Closed,
}

/// Cheap-to-clone handle pointing at the overlay's outcome slot.
pub type UsageStatusOutcomeHandle = OutcomeSlot<UsageStatusOutcome>;

/// Dependencies the overlay needs to spend a reset credit and to refetch
/// the report afterwards. The initial fetch stays host-spawned (its
/// receiver is passed to [`UsageStatusComponent::new`]). These let the
/// component drive the consume and the follow-up refetch itself.
pub struct UsageActionDeps {
    pub auth: AuthStorage,
    pub reset_sources: Vec<Arc<dyn RateLimitResetSource>>,
    pub runtime: tokio::runtime::Handle,
    pub render: RenderHandle,
}

/// Where the overlay is in the reset-credit interaction. `Display` is
/// the read-only usage page. The rest are the steps of spending one
/// credit.
enum Phase {
    Display,
    /// More than one provider is eligible: pick which one to reset.
    SelectProvider,
    /// Confirm spending a credit for `provider_id`.
    Confirm {
        provider_id: String,
    },
    /// Consume request in flight. `key` is the idempotency key, retained
    /// so a retry after a transient failure reuses it and can't
    /// double-spend.
    Consuming {
        provider_id: String,
        key: String,
    },
    /// The consume failed transiently. Offers a retry that reuses `key`.
    Failed {
        provider_id: String,
        key: String,
        message: String,
    },
    /// The consume completed (reset, or a benign no-op like "nothing to
    /// reset"). `Enter` refetches and returns to `Display`.
    Done {
        message: String,
    },
}

/// Usage list overlay with an async-fill loading state and the
/// in-overlay reset-credit action.
pub struct UsageStatusComponent {
    phase: Phase,
    /// Fetched per-provider statuses, once the first fetch has landed.
    statuses: Option<Vec<ProviderUsageStatus>>,
    /// Pending fetch (initial or refetch). `None` once received.
    statuses_rx: Option<oneshot::Receiver<Vec<ProviderUsageStatus>>>,
    /// Set when a fetch's sender vanished, so `Display` shows an error
    /// row instead of wedging in its loading state.
    fetch_failed: bool,
    /// Pending consume result. `Some` only while in `Consuming`.
    consume_rx: Option<oneshot::Receiver<Result<ResetOutcome, String>>>,
    list: SelectList,
    theme: SelectListTheme,
    outcome: UsageStatusOutcomeHandle,
    focused: bool,
    deps: UsageActionDeps,
}

impl UsageStatusComponent {
    /// Build the overlay in its loading state. `statuses_rx` delivers the
    /// host-spawned initial fetch. The host is expected to request a
    /// render when it completes so the page repaints without a keypress.
    pub fn new(
        list_theme: SelectListTheme,
        statuses_rx: oneshot::Receiver<Vec<ProviderUsageStatus>>,
        deps: UsageActionDeps,
    ) -> Self {
        let list = build_list(vec![loading_item()], list_theme.clone());
        Self {
            phase: Phase::Display,
            statuses: None,
            statuses_rx: Some(statuses_rx),
            fetch_failed: false,
            consume_rx: None,
            list,
            theme: list_theme,
            outcome: UsageStatusOutcomeHandle::new(),
            focused: true,
            deps,
        }
    }

    /// Hand the host a clone of the outcome slot.
    pub fn outcome_handle(&self) -> UsageStatusOutcomeHandle {
        self.outcome.clone()
    }

    /// The bottom-border hint for the current phase. Resolved every frame
    /// by the overlay frame, so it tracks the state machine.
    pub fn footer_hint(&self) -> String {
        match &self.phase {
            Phase::Display => {
                if self.has_eligible_provider() {
                    "r use a reset · Esc close".to_string()
                } else {
                    "Esc close".to_string()
                }
            }
            Phase::SelectProvider | Phase::Confirm { .. } | Phase::Failed { .. } => {
                "↑↓ select · Enter confirm · Esc back".to_string()
            }
            Phase::Consuming { .. } => "resetting…".to_string(),
            Phase::Done { .. } => "Enter refresh · Esc close".to_string(),
        }
    }

    /// Providers whose report shows reset credits available and that have
    /// a matching reset source, i.e. the ones we can act on.
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
        self.deps
            .reset_sources
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

    /// `tui.usage.reset` from `Display`: enter the confirm flow, picking
    /// a provider first when more than one is eligible.
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

    /// Kick off spending one credit for `provider_id` under idempotency
    /// key `key`. Spawns the request and moves to `Consuming`.
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
        let auth = self.deps.auth.clone();
        let render = self.deps.render.clone();
        let task_key = key.clone();
        self.deps.runtime.spawn(async move {
            let result = source
                .consume_reset_credit(&auth, &task_key)
                .await
                .map_err(|err| err.to_string());
            if tx.send(result).is_ok() {
                render.request_render();
            }
        });
        self.consume_rx = Some(rx);
        self.set_phase(Phase::Consuming { provider_id, key });
    }

    /// Spawn a fresh usage fetch and route it into `statuses_rx`, shared
    /// by the initial load and the post-reset refresh.
    fn start_fetch(&mut self) {
        let (tx, rx) = oneshot::channel();
        let auth = self.deps.auth.clone();
        let render = self.deps.render.clone();
        self.deps.runtime.spawn(async move {
            let statuses = crate::usage::collect_usage(&auth).await;
            if tx.send(statuses).is_ok() {
                render.request_render();
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
        self.rebuild_list();
    }

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
                    self.rebuild_list();
                }
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.statuses_rx = None;
                self.fetch_failed = true;
                if matches!(self.phase, Phase::Display) {
                    self.rebuild_list();
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

    /// Rebuild the inner list to match the current phase.
    fn rebuild_list(&mut self) {
        let items = match &self.phase {
            Phase::Display => self.display_items(),
            Phase::SelectProvider => provider_items(&self.eligible_providers(), self),
            Phase::Confirm { provider_id } => {
                confirm_items(provider_id, self.available_for(provider_id))
            }
            Phase::Consuming { .. } => vec![SelectItem::new("consuming", "Resetting…")],
            Phase::Failed { message, .. } => failed_items(message),
            Phase::Done { message } => vec![SelectItem::new("done", message)],
        };
        let interactive = matches!(
            self.phase,
            Phase::SelectProvider | Phase::Confirm { .. } | Phase::Failed { .. }
        );
        self.list = if interactive {
            build_menu_list(items, self.theme.clone())
        } else {
            build_list(items, self.theme.clone())
        };
    }

    /// Rows for the read-only `Display` phase.
    fn display_items(&self) -> Vec<SelectItem> {
        if self.statuses_rx.is_some() {
            return vec![loading_item()];
        }
        if self.fetch_failed {
            return vec![SelectItem::new("error", "Usage fetch failed.")];
        }
        match self.statuses.as_ref() {
            Some(statuses) => build_items(statuses),
            None => vec![loading_item()],
        }
    }
}

fn loading_item() -> SelectItem {
    SelectItem::new("loading", "Loading usage…")
}

/// Non-interactive list construction (read-only display rows).
fn build_list(items: Vec<SelectItem>, theme: SelectListTheme) -> SelectList {
    let layout = SelectListLayout {
        show_selection_indicator: false,
        ..Default::default()
    };
    let visible = items.len().max(1);
    SelectList::new(items, visible, theme, layout)
}

/// Interactive list construction (menu rows with a selection indicator).
fn build_menu_list(items: Vec<SelectItem>, theme: SelectListTheme) -> SelectList {
    let layout = SelectListLayout {
        show_selection_indicator: true,
        ..Default::default()
    };
    let visible = items.len().max(1);
    SelectList::new(items, visible, theme, layout)
}

/// Confirm menu: spending a credit or backing out. Names the provider
/// and what the reset does so the screen stands on its own, and defaults
/// to the reset since that's the reason the user opened it.
fn confirm_items(provider_id: &str, available: u32) -> Vec<SelectItem> {
    vec![
        SelectItem::new("confirm", &format!("Use a reset for {provider_id}")).with_description(
            &format!("clears the current limits · {available} available"),
        ),
        SelectItem::new("cancel", "Cancel"),
    ]
}

/// Retry menu shown after a transient consume failure. Defaults to
/// "Try again". The retry reuses the idempotency key, so it can't
/// double-spend even if the failed attempt actually reached the server.
fn failed_items(message: &str) -> Vec<SelectItem> {
    vec![
        SelectItem::new("retry", "Try again").with_description(message),
        SelectItem::new("cancel", "Back"),
    ]
}

/// Provider picker rows when several providers are eligible.
fn provider_items(providers: &[String], component: &UsageStatusComponent) -> Vec<SelectItem> {
    providers
        .iter()
        .map(|id| {
            SelectItem::new(id, id)
                .with_description(&format!("{} available", component.available_for(id)))
        })
        .collect()
}

/// Flatten the per-provider reports into read-only list rows. Only a
/// provider's first row carries the provider-id prefix. Continuation
/// rows leave it empty so the prefix column groups the windows visually.
fn build_items(statuses: &[ProviderUsageStatus]) -> Vec<SelectItem> {
    let now_ms = now_unix_ms();
    let mut items = Vec::new();
    for status in statuses {
        let id = &status.provider_id;
        let mut prefix = id.as_str();
        let mut push = |items: &mut Vec<SelectItem>, label: &str, description: Option<&str>| {
            let item = SelectItem::new(id, label).with_prefix(prefix);
            items.push(match description {
                Some(desc) => item.with_description(desc),
                None => item,
            });
            prefix = "";
        };
        match &status.outcome {
            UsageOutcome::Usage(usage) => {
                if usage.windows.is_empty()
                    && usage.notes.is_empty()
                    && usage.reset_credits.is_none()
                {
                    push(&mut items, "no usage data reported", None);
                }
                for window in &usage.windows {
                    let desc = format_window_status(window.used, window.resets_at, now_ms);
                    push(&mut items, &window.label, Some(&desc));
                }
                for note in &usage.notes {
                    push(&mut items, note, None);
                }
                if let Some(available) = usage.reset_credits {
                    let desc = if available > 0 {
                        format!("{available} available")
                    } else {
                        "no resets available".to_string()
                    };
                    push(&mut items, "Rate-limit resets", Some(&desc));
                }
            }
            UsageOutcome::Unsupported { reason } => {
                push(&mut items, &format!("usage not available — {reason}"), None);
            }
            UsageOutcome::NotConfigured => push(&mut items, "not configured", None),
            UsageOutcome::NoSource => push(&mut items, "usage reporting not supported", None),
            UsageOutcome::Error(err) => push(&mut items, &format!("error: {err}"), None),
        }
    }
    items
}

impl Component for UsageStatusComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<aj_tui::Line> {
        self.poll_statuses();
        self.poll_consume();
        self.list.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();
        match &self.phase {
            Phase::Display => {
                if kb.matches(event, "tui.usage.reset") {
                    self.begin_reset_flow();
                } else if kb.matches(event, "tui.select.cancel")
                    || kb.matches(event, "tui.input.submit")
                {
                    self.outcome.set(UsageStatusOutcome::Closed);
                }
                // Every other key is swallowed: the page is read-only.
                true
            }
            Phase::SelectProvider => {
                if kb.matches(event, "tui.select.cancel") {
                    self.set_phase(Phase::Display);
                } else if kb.matches(event, "tui.select.confirm") {
                    if let Some(provider_id) = self.list.selected_item().map(|i| i.value.clone()) {
                        self.set_phase(Phase::Confirm { provider_id });
                    }
                } else {
                    self.list.handle_input(event);
                }
                true
            }
            Phase::Confirm { provider_id } => {
                let provider_id = provider_id.clone();
                if kb.matches(event, "tui.select.cancel") {
                    self.set_phase(Phase::Display);
                } else if kb.matches(event, "tui.select.confirm") {
                    match self.list.selected_item().map(|i| i.value.as_str()) {
                        Some("confirm") => self.begin_consume(provider_id, new_idempotency_key()),
                        _ => self.set_phase(Phase::Display),
                    }
                } else {
                    self.list.handle_input(event);
                }
                true
            }
            // The consume is quick and idempotency-keyed, so there's no
            // cancel.
            Phase::Consuming { .. } => true,
            Phase::Failed {
                provider_id, key, ..
            } => {
                let (provider_id, key) = (provider_id.clone(), key.clone());
                if kb.matches(event, "tui.select.cancel") {
                    self.set_phase(Phase::Display);
                } else if kb.matches(event, "tui.select.confirm") {
                    match self.list.selected_item().map(|i| i.value.as_str()) {
                        Some("retry") => self.begin_consume(provider_id, key),
                        _ => self.set_phase(Phase::Display),
                    }
                } else {
                    self.list.handle_input(event);
                }
                true
            }
            Phase::Done { .. } => {
                if kb.matches(event, "tui.select.cancel") {
                    self.outcome.set(UsageStatusOutcome::Closed);
                } else if kb.matches(event, "tui.select.confirm")
                    || kb.matches(event, "tui.input.submit")
                {
                    // Refetch so the reset windows and updated count show.
                    self.start_fetch();
                    self.set_phase(Phase::Display);
                }
                true
            }
        }
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

/// A random 128-bit idempotency key formatted as a UUID v4 string.
/// Uniqueness is all the endpoint needs. The version/variant bits keep it
/// a well-formed UUID for servers that validate the shape.
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
    use std::sync::OnceLock;

    use aj_models::usage::{ProviderUsage, UsageError, UsageWindow};
    use aj_tui::keys::Key;
    use async_trait::async_trait;

    use std::sync::{Arc, Mutex};

    use super::*;

    fn identity_theme() -> SelectListTheme {
        SelectListTheme {
            selected_prefix: Arc::new(|s| s.to_string()),
            selected_text: Arc::new(|s| s.to_string()),
            description: Arc::new(|s| s.to_string()),
            scroll_info: Arc::new(|s| s.to_string()),
            no_match: Arc::new(|s| s.to_string()),
            prefix: Arc::new(|s| s.to_string()),
            shortcut: Arc::new(|s| s.to_string()),
        }
    }

    /// A leaked test runtime so `UsageActionDeps` can carry a valid
    /// `Handle` from a plain `#[test]`. Tasks spawned onto it actually
    /// run, which the consume-flow tests rely on.
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
        let dir = std::env::temp_dir().join(format!("aj-usage-status-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        AuthStorage::with_providers(dir.join("auth.json"), HashMap::new())
    }

    /// A reset source that returns a scripted outcome and records every
    /// idempotency key it receives, so tests don't hit the network and
    /// can assert on retry key reuse.
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

    fn deps_with_sources(sources: Vec<Arc<dyn RateLimitResetSource>>) -> UsageActionDeps {
        UsageActionDeps {
            auth: scratch_auth(),
            reset_sources: sources,
            runtime: runtime_handle(),
            render: RenderHandle::detached(),
        }
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

    fn body(component: &mut UsageStatusComponent) -> String {
        component
            .render(120)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Render in a bounded loop until `needle` appears, giving a spawned
    /// consume/refetch task time to land. Panics on timeout so a broken
    /// wiring fails loudly instead of hanging.
    fn wait_for(component: &mut UsageStatusComponent, needle: &str) -> String {
        for _ in 0..200 {
            let out = body(component);
            if out.contains(needle) {
                return out;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("timed out waiting for {needle:?}");
    }

    fn component_with(
        statuses: Vec<ProviderUsageStatus>,
        sources: Vec<Arc<dyn RateLimitResetSource>>,
    ) -> UsageStatusComponent {
        let (tx, rx) = oneshot::channel();
        let mut c = UsageStatusComponent::new(identity_theme(), rx, deps_with_sources(sources));
        tx.send(statuses).unwrap();
        // Drain the fetch into `statuses` and lay out the display rows.
        let _ = body(&mut c);
        c
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

    #[test]
    fn shows_loading_until_statuses_arrive() {
        let (tx, rx) = oneshot::channel();
        let mut c = UsageStatusComponent::new(identity_theme(), rx, deps_with_sources(vec![]));
        assert!(body(&mut c).contains("Loading usage…"));

        tx.send(vec![codex_status(Some(2))]).unwrap();
        let out = body(&mut c);
        assert!(!out.contains("Loading"), "{out}");
        assert!(out.contains("openai-codex"), "{out}");
        assert!(out.contains("5h limit"), "{out}");
    }

    #[test]
    fn reset_row_shows_available_and_none() {
        let items = build_items(&[codex_status(Some(3))]);
        let joined: Vec<(&str, Option<&str>)> = items
            .iter()
            .map(|i| (i.label.as_str(), i.description.as_deref()))
            .collect();
        assert!(joined.contains(&("Rate-limit resets", Some("3 available"))));

        let items = build_items(&[codex_status(Some(0))]);
        let joined: Vec<(&str, Option<&str>)> = items
            .iter()
            .map(|i| (i.label.as_str(), i.description.as_deref()))
            .collect();
        assert!(joined.contains(&("Rate-limit resets", Some("no resets available"))));

        // None: no reset row at all.
        let items = build_items(&[codex_status(None)]);
        assert!(items.iter().all(|i| i.label != "Rate-limit resets"));
    }

    #[test]
    fn reset_key_noop_without_eligible_provider() {
        // Credits present but no matching source → not eligible.
        let mut c = component_with(vec![codex_status(Some(2))], vec![]);
        assert!(!c.has_eligible_provider());
        c.handle_input(&Key::char('r'));
        let out = body(&mut c);
        assert!(out.contains("Rate-limit resets"), "{out}");
        assert!(!out.contains("Use a reset"), "{out}");
    }

    #[test]
    fn reset_key_opens_confirm_then_cancel_returns() {
        let mut c = component_with(
            vec![codex_status(Some(2))],
            vec![fake_source(Ok(ResetOutcome::Reset))],
        );
        assert!(c.has_eligible_provider());

        c.handle_input(&Key::char('r'));
        let out = body(&mut c);
        // The confirm names the provider and defaults to the reset.
        assert!(out.contains("Use a reset for openai-codex"), "{out}");
        assert!(out.contains("Cancel"), "{out}");
        assert_eq!(
            c.list.selected_item().map(|i| i.value.as_str()),
            Some("confirm")
        );

        // Esc backs out to the read-only page without spending.
        c.handle_input(&Key::escape());
        let out = body(&mut c);
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
            let mut c = component_with(vec![codex_status(Some(2))], vec![]);
            c.phase = Phase::Consuming {
                provider_id: "openai-codex".into(),
                key: "k".into(),
            };
            c.apply_consume_result(result);
            let out = body(&mut c);
            assert!(out.contains(expected), "expected `{expected}` in:\n{out}");
        }
    }

    #[test]
    fn consume_wires_source_and_shows_outcome() {
        let mut c = component_with(
            vec![codex_status(Some(2))],
            vec![fake_source(Ok(ResetOutcome::Reset))],
        );
        c.begin_consume("openai-codex".into(), "k".into());
        let out = wait_for(&mut c, "Usage reset.");
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
        let mut c = component_with(vec![codex_status(Some(2))], vec![source]);

        // r -> confirm -> "Use a reset" mints a key and spends it.
        c.handle_input(&Key::char('r'));
        c.list.select_by_value("confirm");
        c.handle_input(&Key::enter());
        wait_for(&mut c, "Try again");

        // "Try again" is the default selection, so Enter retries.
        c.handle_input(&Key::enter());
        wait_for(&mut c, "Try again");

        let recorded = keys.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2, "two attempts: {recorded:?}");
        assert_eq!(
            recorded[0], recorded[1],
            "retry must reuse the idempotency key"
        );
    }

    #[test]
    fn multiple_eligible_providers_show_picker() {
        let statuses = vec![
            usage_status("openai-codex", Some(2)),
            usage_status("other", Some(1)),
        ];
        let sources = vec![
            fake_source_for("openai-codex", Ok(ResetOutcome::Reset)),
            fake_source_for("other", Ok(ResetOutcome::Reset)),
        ];
        let mut c = component_with(statuses, sources);

        c.handle_input(&Key::char('r'));
        let out = body(&mut c);
        assert!(out.contains("openai-codex"), "{out}");
        assert!(out.contains("other"), "{out}");
        assert_eq!(c.footer_hint(), "↑↓ select · Enter confirm · Esc back");

        // Choosing a provider advances to its confirm step.
        c.list.select_by_value("other");
        c.handle_input(&Key::enter());
        assert!(body(&mut c).contains("Use a reset"));
        match &c.phase {
            Phase::Confirm { provider_id } => assert_eq!(provider_id, "other"),
            _ => panic!("expected Confirm for the chosen provider"),
        }
    }

    #[test]
    fn done_enter_refetches_and_returns_to_display() {
        let mut c = component_with(
            vec![codex_status(Some(2))],
            vec![fake_source(Ok(ResetOutcome::Reset))],
        );
        c.set_phase(Phase::Done {
            message: "Usage reset.".into(),
        });
        c.handle_input(&Key::enter());
        // A refetch is now in flight, so Display shows the loading row.
        let out = body(&mut c);
        assert!(out.contains("Loading usage…"), "{out}");
    }

    #[test]
    fn esc_closes_from_display() {
        let mut c = component_with(vec![codex_status(None)], vec![]);
        let h = c.outcome_handle();
        c.handle_input(&Key::escape());
        assert!(matches!(h.take(), Some(UsageStatusOutcome::Closed)));
    }

    #[test]
    fn dropped_sender_shows_error_instead_of_wedging() {
        let (tx, rx) = oneshot::channel::<Vec<ProviderUsageStatus>>();
        drop(tx);
        let mut c = UsageStatusComponent::new(identity_theme(), rx, deps_with_sources(vec![]));
        assert!(body(&mut c).contains("Usage fetch failed."));
    }

    #[test]
    fn provider_prefix_only_on_first_row() {
        let statuses = vec![
            codex_status(None),
            ProviderUsageStatus {
                provider_id: "openai".into(),
                outcome: UsageOutcome::NoSource,
            },
        ];
        let items = build_items(&statuses);
        let prefixes: Vec<&str> = items
            .iter()
            .map(|i| i.prefix.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(prefixes, vec!["openai-codex", "openai"]);
    }
}
