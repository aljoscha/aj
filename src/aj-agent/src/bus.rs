//! Internal event bus for the [`crate::Agent`] runtime.
//!
//! The agent owns a single bus and emits every state transition
//! through it as an [`crate::events::AgentEvent`]. Subscribers
//! register an async listener via [`EventBus::subscribe`]; the bus
//! awaits each listener inline in registration order, so any
//! durability guarantee a listener requires (e.g. "the persisted log
//! is never more than one event behind reality") falls out for free
//! — when a listener is awaited inline, the agent cannot move on
//! until the listener has handled the event. A listener that returns
//! `Err` propagates the error back to the caller of [`EventBus::emit`] or
//! [`EventBus::emit_sequence`], which the agent surfaces as
//! [`crate::TurnError::Fatal`] so disk failures abort the run instead of
//! silently continuing. A sequence snapshots one listener cohort and completes
//! all its events for one listener before advancing to the next.
//!
//! Channel-style subscribers (where the listener forwards events into
//! a `tokio::sync::mpsc` queue) compose on top of [`EventBus::subscribe`]
//! without any special-case API: the listener just calls `tx.send(...)`.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::error::BoxError;
use crate::events::AgentEvent;

/// Async listener invoked for every event on the bus.
///
/// The listener is held behind an `Arc` so the bus can snapshot its
/// current registration list under a short-lived lock and then await
/// each listener without blocking subsequent [`EventBus::subscribe`]
/// calls. Listeners must be `Send + Sync` because the agent loop runs
/// on a `tokio` task and the bus is cloned into both the agent and
/// any future helpers (e.g. sub-agent spawn paths).
pub type Listener = Arc<
    dyn for<'a> Fn(
            &'a AgentEvent,
        ) -> Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send + 'a>>
        + Send
        + Sync,
>;

/// One registered subscription.
struct Slot {
    /// Stable identifier issued by [`EventBus::subscribe`] and used by
    /// [`SubscriptionHandle::drop`] to find the slot to remove.
    id: u64,
    listener: Listener,
}

/// Shared state between an [`EventBus`] and any outstanding
/// [`SubscriptionHandle`]s.
struct BusInner {
    listeners: Mutex<Vec<Slot>>,
    next_id: AtomicU64,
}

/// Event bus owned by an [`crate::Agent`].
///
/// Cloning is cheap — clones share the underlying state via `Arc` —
/// so the bus can be handed to sub-systems (e.g. the
/// sub-agent spawn path that shares the parent's bus) without
/// ceremony.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<BusInner>,
}

/// Subscription-only capability for an [`EventBus`].
///
/// A component that only observes an Agent receives this handle rather than an
/// [`EventBus`] clone, so it can add and remove listeners without acquiring an
/// event-emission path around the Agent's typed state transitions.
#[derive(Clone)]
pub struct EventSubscriptions {
    inner: Arc<BusInner>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    /// Construct a fresh bus with no subscribers.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(BusInner {
                listeners: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// Register a listener and return a handle whose drop removes it.
    ///
    /// Listeners are invoked in registration order; if an earlier
    /// listener returns `Err`, later listeners do not run for that
    /// event and the error is returned from [`EventBus::emit`].
    pub fn subscribe(&self, listener: Listener) -> SubscriptionHandle {
        subscribe(&self.inner, listener)
    }

    /// Return a cloneable handle that can subscribe but cannot emit.
    pub fn subscriptions(&self) -> EventSubscriptions {
        EventSubscriptions {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Emit an event to every subscriber in registration order.
    ///
    /// Listeners are awaited inline: the future returned by this
    /// method only resolves once every subscribed listener has
    /// observed (or rejected) the event. If a listener returns
    /// `Err`, that error is propagated and remaining listeners are
    /// not invoked for this event.
    pub async fn emit(&self, event: AgentEvent) -> Result<(), BoxError> {
        self.emit_sequence(std::slice::from_ref(&event)).await
    }

    /// Emit an ordered event sequence to one stable listener cohort.
    ///
    /// The listener list is snapshotted once, then each listener receives the
    /// complete sequence before the next listener starts. If listener `N`
    /// rejects an event, every earlier listener has already received the whole
    /// sequence and no later listener runs. Registrations and removals during
    /// delivery affect only subsequent bus operations. Separate `emit` or
    /// `emit_sequence` futures are not globally serialized by the bus; callers
    /// that require one history serialize their own operations.
    pub async fn emit_sequence(&self, events: &[AgentEvent]) -> Result<(), BoxError> {
        let listeners = self.listener_snapshot();
        for listener in listeners {
            for event in events {
                listener(event).await?;
            }
        }
        Ok(())
    }

    /// Clone the current listener cohort without holding the registration lock
    /// across listener code or an await.
    fn listener_snapshot(&self) -> Vec<Listener> {
        self.inner
            .listeners
            .lock()
            .expect("event bus listeners mutex poisoned")
            .iter()
            .map(|slot| Arc::clone(&slot.listener))
            .collect()
    }

    /// Number of currently-registered listeners. Test helper.
    #[cfg(test)]
    pub(crate) fn listener_count(&self) -> usize {
        self.inner
            .listeners
            .lock()
            .expect("event bus listeners mutex poisoned")
            .len()
    }
}

impl EventSubscriptions {
    /// Register a listener and return a handle whose drop removes it.
    pub fn subscribe(&self, listener: Listener) -> SubscriptionHandle {
        subscribe(&self.inner, listener)
    }
}

fn subscribe(inner: &Arc<BusInner>, listener: Listener) -> SubscriptionHandle {
    let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
    inner
        .listeners
        .lock()
        .expect("event bus listeners mutex poisoned")
        .push(Slot { id, listener });
    SubscriptionHandle {
        inner: Arc::downgrade(inner),
        id,
    }
}

/// Handle returned from [`EventBus::subscribe`].
///
/// Dropping the handle removes the listener from the bus. The handle
/// holds a `Weak` reference to the bus so a long-outstanding handle
/// does not keep the bus alive after the agent that owned it is
/// dropped.
pub struct SubscriptionHandle {
    inner: Weak<BusInner>,
    id: u64,
}

impl SubscriptionHandle {
    /// Detach the listener immediately rather than waiting for drop.
    /// Equivalent to dropping the handle but reads more naturally at
    /// some call sites.
    pub fn detach(self) {
        // Drop runs the removal logic.
        drop(self);
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            let mut listeners = inner
                .listeners
                .lock()
                .expect("event bus listeners mutex poisoned");
            listeners.retain(|slot| slot.id != self.id);
        }
    }
}

/// Wrap a synchronous closure into a [`Listener`].
///
/// Convenience for subscribers that don't need async work — most
/// listeners (and every test listener) just push the event into a
/// `Mutex<Vec<...>>` or a channel. Without this helper every call
/// site spells out the same `Box::pin(async move { ... Ok(()) })`
/// boilerplate.
pub fn listener_from_sync<F>(mut f: F) -> Listener
where
    F: FnMut(&AgentEvent) + Send + Sync + 'static,
{
    let f = Arc::new(Mutex::new(move |event: &AgentEvent| f(event)));
    Arc::new(move |event: &AgentEvent| {
        let f = Arc::clone(&f);
        // Run the synchronous body before yielding so subscribers
        // that observe events purely for their side effects do not
        // need to schedule themselves on the runtime.
        f.lock().expect("listener_from_sync mutex poisoned")(event);
        Box::pin(async { Ok(()) })
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::events::AgentId;

    fn record() -> (Listener, Arc<Mutex<Vec<String>>>) {
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let log_clone = Arc::clone(&log);
        let listener = listener_from_sync(move |event| match event {
            AgentEvent::Notice { text, .. } => {
                log_clone
                    .lock()
                    .expect("test record mutex poisoned")
                    .push(text.clone());
            }
            _ => {}
        });
        (listener, log)
    }

    #[tokio::test]
    async fn emit_dispatches_to_subscribers_in_registration_order() {
        let bus = EventBus::new();

        let order: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));

        let order_a = Arc::clone(&order);
        let _h1 = bus.subscribe(listener_from_sync(move |_| {
            order_a.lock().unwrap().push(1);
        }));
        let order_b = Arc::clone(&order);
        let _h2 = bus.subscribe(listener_from_sync(move |_| {
            order_b.lock().unwrap().push(2);
        }));

        bus.emit(AgentEvent::Notice {
            agent_id: AgentId::Main,
            text: "hi".into(),
        })
        .await
        .expect("emit should succeed");

        assert_eq!(order.lock().unwrap().clone(), vec![1, 2]);
    }

    #[tokio::test]
    async fn emit_sequence_completes_earlier_listeners_before_a_later_failure() {
        let bus = EventBus::new();
        let first = Arc::new(Mutex::new(Vec::new()));
        let first_record = Arc::clone(&first);
        let _first_handle = bus.subscribe(listener_from_sync(move |event| {
            first_record.lock().unwrap().push(event_kind(event));
        }));
        let second = Arc::new(Mutex::new(Vec::new()));
        let second_record = Arc::clone(&second);
        let _second_handle = bus.subscribe(Arc::new(move |event| {
            second_record.lock().unwrap().push(event_kind(event));
            let reject = matches!(event, AgentEvent::Notice { .. });
            Box::pin(async move {
                if reject {
                    Err(BoxError::from("injected sequence failure"))
                } else {
                    Ok(())
                }
            })
        }));
        let events = [
            AgentEvent::Notice {
                agent_id: AgentId::Main,
                text: "first".into(),
            },
            AgentEvent::Warning {
                agent_id: AgentId::Main,
                text: "second".into(),
            },
        ];

        let error = bus
            .emit_sequence(&events)
            .await
            .expect_err("later listener rejects the sequence");

        assert_eq!(error.to_string(), "injected sequence failure");
        assert_eq!(first.lock().unwrap().as_slice(), ["notice", "warning"]);
        assert_eq!(second.lock().unwrap().as_slice(), ["notice"]);
    }

    #[tokio::test]
    async fn emit_sequence_uses_one_listener_snapshot() {
        let bus = EventBus::new();
        let late_events = Arc::new(Mutex::new(Vec::new()));
        let late_handles = Arc::new(Mutex::new(Vec::new()));
        let registering_bus = bus.clone();
        let registered_events = Arc::clone(&late_events);
        let registered_handles = Arc::clone(&late_handles);
        let _registering_handle = bus.subscribe(listener_from_sync(move |event| {
            if matches!(event, AgentEvent::Notice { .. }) {
                let events = Arc::clone(&registered_events);
                let handle = registering_bus.subscribe(listener_from_sync(move |event| {
                    events.lock().unwrap().push(event_kind(event));
                }));
                registered_handles.lock().unwrap().push(handle);
            }
        }));
        let sequence = [
            AgentEvent::Notice {
                agent_id: AgentId::Main,
                text: "first".into(),
            },
            AgentEvent::Warning {
                agent_id: AgentId::Main,
                text: "second".into(),
            },
        ];

        bus.emit_sequence(&sequence).await.expect("emit sequence");
        assert!(
            late_events.lock().unwrap().is_empty(),
            "listener added during the sequence joined its cohort"
        );
        bus.emit(AgentEvent::Error {
            agent_id: AgentId::Main,
            text: "later".into(),
        })
        .await
        .expect("emit subsequent event");
        assert_eq!(late_events.lock().unwrap().as_slice(), ["error"]);
    }

    fn event_kind(event: &AgentEvent) -> &'static str {
        match event {
            AgentEvent::Notice { .. } => "notice",
            AgentEvent::Warning { .. } => "warning",
            AgentEvent::Error { .. } => "error",
            _ => "other",
        }
    }

    #[tokio::test]
    async fn dropping_handle_unsubscribes() {
        let bus = EventBus::new();
        let (listener, log) = record();

        let handle = bus.subscribe(listener);
        bus.emit(AgentEvent::Notice {
            agent_id: AgentId::Main,
            text: "first".into(),
        })
        .await
        .expect("emit");

        drop(handle);
        assert_eq!(bus.listener_count(), 0);

        bus.emit(AgentEvent::Notice {
            agent_id: AgentId::Main,
            text: "second".into(),
        })
        .await
        .expect("emit");

        assert_eq!(log.lock().unwrap().clone(), vec!["first".to_string()]);
    }

    #[tokio::test]
    async fn listener_error_propagates_and_short_circuits() {
        let bus = EventBus::new();

        let later_called: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

        let _h1 = bus.subscribe(Arc::new(|_event| Box::pin(async { Err("boom".into()) })));
        let later = Arc::clone(&later_called);
        let _h2 = bus.subscribe(listener_from_sync(move |_| {
            *later.lock().unwrap() = true;
        }));

        let err = bus
            .emit(AgentEvent::Notice {
                agent_id: AgentId::Main,
                text: "x".into(),
            })
            .await
            .expect_err("listener error should bubble");
        assert!(err.to_string().contains("boom"));
        assert!(
            !*later_called.lock().unwrap(),
            "subsequent listener should not run after an earlier listener errored"
        );
    }

    #[tokio::test]
    async fn handle_outliving_bus_is_inert_on_drop() {
        // Holding a SubscriptionHandle past the bus' lifetime should
        // not panic — the weak reference upgrade fails and the drop
        // becomes a no-op. This matters for sub-agents that may hold
        // their own subscription while the parent agent shuts down.
        let handle = {
            let bus = EventBus::new();
            bus.subscribe(listener_from_sync(|_| {}))
        };
        drop(handle);
    }
}
