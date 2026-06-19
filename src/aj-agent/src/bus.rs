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
//! `Err` propagates the error back to the caller of [`EventBus::emit`],
//! which the agent surfaces as [`crate::TurnError::Fatal`] so disk
//! failures abort the run instead of silently continuing.
//!
//! Channel-style subscribers (where the listener forwards events into
//! a `tokio::sync::mpsc` queue) compose on top of [`EventBus::subscribe`]
//! without any special-case API: the listener just calls `tx.send(...)`.
//!
//! See `docs/aj-next-plan.md` §1.4 — "Hook vs subscriber pattern".

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
/// so the bus can be handed to sub-systems (e.g. the future
/// sub-agent spawn path that shares the parent's bus, per
/// `docs/aj-next-plan.md` §1.6) without ceremony.
#[derive(Clone)]
pub struct EventBus {
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
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .listeners
            .lock()
            .expect("event bus listeners mutex poisoned")
            .push(Slot { id, listener });
        SubscriptionHandle {
            inner: Arc::downgrade(&self.inner),
            id,
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
        // Snapshot the listener list under the lock so that an
        // in-flight emit cannot race a `subscribe` or a `drop`.
        // Each slot holds an `Arc<...>`, so cloning the snapshot is
        // a refcount bump per subscriber.
        let listeners: Vec<Listener> = self
            .inner
            .listeners
            .lock()
            .expect("event bus listeners mutex poisoned")
            .iter()
            .map(|slot| Arc::clone(&slot.listener))
            .collect();
        for listener in listeners {
            listener(&event).await?;
        }
        Ok(())
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
