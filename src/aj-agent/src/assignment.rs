//! A foreground delegation outlives an interrupted child turn. Its caller
//! waits for one terminal report while the host can drive ordinary turns on
//! the retained child. Dropping the caller ends that ownership as well.

use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{BoxError, SubAgentOutcome};

type Report = Result<SubAgentOutcome, BoxError>;

/// Live ownership of a foreground child by its parent's tool call.
pub struct ForegroundAssignment {
    lifetime: CancellationToken,
    initial_turn: CancellationToken,
    reply: Mutex<Option<oneshot::Sender<Report>>>,
}

impl ForegroundAssignment {
    pub(crate) fn new(parent: &CancellationToken) -> (Arc<Self>, oneshot::Receiver<Report>) {
        let lifetime = parent.child_token();
        let initial_turn = lifetime.child_token();
        let (tx, rx) = oneshot::channel();
        (
            Arc::new(Self {
                lifetime,
                initial_turn,
                reply: Mutex::new(Some(tx)),
            }),
            rx,
        )
    }

    /// Whether the caller is still waiting for this assignment.
    pub fn is_pending(&self) -> bool {
        !self.lifetime.is_cancelled()
            && self
                .reply
                .lock()
                .expect("assignment mutex poisoned")
                .is_some()
    }

    /// Interrupt only the initial turn. Host-driven continuations have their
    /// own turn tokens and are interrupted by their driver instead.
    pub fn interrupt(&self) {
        self.initial_turn.cancel();
    }

    /// End the assignment, including any host-driven continuation.
    pub fn cancel(&self) {
        self.lifetime.cancel();
    }

    /// A new turn belongs to the assignment, but interrupting it cannot cancel
    /// the assignment or its parent. Detached work never uses this token.
    pub fn turn_token(&self) -> CancellationToken {
        self.lifetime.child_token()
    }

    pub(crate) fn initial_token(&self) -> CancellationToken {
        self.initial_turn.clone()
    }

    // Reserve terminal delivery before publishing AgentEnd. The host can
    // wake independent queued work as soon as it observes that event.
    pub(crate) fn take_reply(&self) -> Option<oneshot::Sender<Report>> {
        self.reply.lock().expect("assignment mutex poisoned").take()
    }

    pub(crate) async fn wait(&self, reply: oneshot::Receiver<Report>) -> Report {
        tokio::select! {
            // An explicit kill wins if the report and kill are both ready.
            biased;
            _ = self.lifetime.cancelled() => Err("sub-agent assignment cancelled by user".into()),
            report = reply => report.map_err(|_| -> BoxError { "sub-agent assignment ended without a report".into() })?,
        }
    }
}

/// The parent tool future owns this guard. Its drop also reaches a child
/// continuation that the host is driving outside that future.
pub(crate) struct AssignmentGuard(pub Arc<ForegroundAssignment>);

impl Drop for AssignmentGuard {
    fn drop(&mut self) {
        self.0.cancel();
        self.0
            .reply
            .lock()
            .expect("assignment mutex poisoned")
            .take();
    }
}

/// A child turn may unwind or be dropped rather than returning through its
/// normal terminal path. Its parent must still get one result.
pub(crate) struct AssignmentTurnGuard(pub Option<Arc<ForegroundAssignment>>);

impl Drop for AssignmentTurnGuard {
    fn drop(&mut self) {
        if let Some(assignment) = &self.0
            && let Some(reply) = assignment.take_reply()
        {
            let _ = reply.send(Err("sub-agent continuation ended unexpectedly".into()));
        }
    }
}
