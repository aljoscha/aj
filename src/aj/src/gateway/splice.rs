//! Splicing a client's session streams onto the hosts that own them.
//!
//! One client stream is one [`Splice`]: the merged directory, one upstream
//! stream per host whose sessions that client attached, and one bounded queue
//! ([`aj_app::outbound`]) merging what the upstreams say. Every frame
//! travels downstream with its session id namespaced and nothing else touched,
//! kinds this build does not know included.
//!
//! A stream that attaches nothing is a splice of nothing, which is what keeps
//! one writer for both cases: the directory and heartbeats reach a sidebar the
//! same way they reach a client watching ten sessions.
//!
//! What this deliberately does not do is redial an upstream that dropped.
//! Resuming one needs a *current* cursor, and the client's cursor advances as it
//! applies the frames this gateway forwarded, so tracking one here would give
//! the gateway per-session cursor state it must not hold and put a second,
//! subtly different cursor authority in the system. A drop therefore emits
//! `reset` downstream, which means "continuity broke, re-attach with your
//! cursor", and the client's own re-attach is the only thing that
//! opens an upstream. Resume is then incremental when the host's epoch survived
//! and full when it did not, inherited from the host protocol with no gateway
//! involvement.
//!
//! The one end that is not a break in continuity is a withdrawal, and it ends
//! the same way all the same, `reset` included: the host has stopped being this
//! gateway's, so the client is asked to re-attach, and what its re-attach finds
//! is the difference between the two. A withdrawn host's ids resolve to nothing
//! here, so each is refused with its own `error` frame, which costs
//! that client the one attachment and nothing else. That is what makes the
//! `reset` safe to send: a client attached across several hosts keeps its stream
//! and every other host's sessions through it, and the directory, where the
//! withdrawn host's rows and its group are gone, confirms what the refusal said.
//!
//! Every session a client names is answered: its host's block, or an `error`.
//! A session whose host this gateway cannot reach right now, because it holds
//! no link to it or the dial failed, is answered `host_unreachable`, which a
//! client keeps its cursor through, and the host's return is announced with
//! `reset`. Each host is served by a task of its own, started before the client
//! has its response head, so a host that is slow, hung or gone delays and fails
//! nothing but its own sessions.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use aj_app::client::HOST_UNREACHABLE_CODE;
use aj_app::host::{AttachRequest, ListFrames};
use aj_app::outbound::{self, Offered, Sender};
use aj_wire::{DecodedFrame, Frame, MergedDirectory};
use tokio::sync::watch;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::gateway::Tuning;
use crate::gateway::config::HostAddress;
use crate::gateway::directory::{AttachGroup, AttachPlan, Unresolvable};
use crate::gateway::naming::SessionAddress;
use crate::remote::{RemoteClient, RemoteError, RemoteEvents};

/// What one client stream writes next.
pub(crate) enum Outgoing {
    /// The merged directory, which this gateway composes from its hosts' rows
    /// and writes as a `list` frame.
    Directory(Arc<MergedDirectory>),
    /// A frame this gateway composed itself: a heartbeat, or the refusal of a
    /// session it could not resolve.
    Own(Frame),
    /// A frame spliced from the host that owns the session it names, forwarded
    /// as it arrived apart from that id.
    Spliced(DecodedFrame),
}

/// Everything one client stream reads from.
pub(crate) struct Splice {
    /// The spliced frames of the sessions this client attached.
    frames: outbound::Receiver<DecodedFrame>,
    /// The merged directory, which every client stream carries.
    directory: watch::Receiver<Arc<MergedDirectory>>,
    /// Whether the opening directory has been written.
    opened: bool,
    /// The refusal owed to each session this gateway could not resolve, in the
    /// order the client named them.
    ///
    /// Written straight onto the stream rather than through the client's
    /// bounded queue, because they are part of what the attach answers: a
    /// client naming more dead ids than the bound would otherwise be evicted
    /// by its own attach, which is the mistake pacing an attach block exists
    /// to avoid.
    refused: VecDeque<Frame>,
    /// Cancelled when this is dropped, which is what ends the upstream streams
    /// and the tasks pumping them: a client that goes away stops costing this
    /// gateway, and its hosts, anything.
    _cancel: DropGuard,
}

impl Splice {
    /// Serve every host in `plan` on a task of its own ([`attend`]), and hold
    /// the refusal owed to every session in it this gateway could not resolve.
    ///
    /// Returns at once, without waiting on any host, so the client has its
    /// response head while the hosts are still being dialed. What becomes of
    /// each named session travels as that session's own frames.
    ///
    /// `shutdown` is the serving gateway's own token, and this splice's is a
    /// child of it: a client that stopped reading never observes a shutdown,
    /// because its stream is only polled when there is room to write to it, and
    /// its upstreams have to end anyway.
    pub(crate) fn open(
        plan: AttachPlan,
        reachable: watch::Receiver<Arc<BTreeSet<String>>>,
        directory: watch::Receiver<Arc<MergedDirectory>>,
        tuning: Tuning,
        shutdown: &CancellationToken,
    ) -> Self {
        let AttachPlan { groups, refused } = plan;
        let cancel = shutdown.child_token();
        let guard = cancel.clone().drop_guard();
        let (sender, frames) = outbound::channel(tuning.outbound_queue, cancel.clone());
        for group in groups {
            tokio::spawn(attend(
                group,
                reachable.clone(),
                sender.clone(),
                cancel.clone(),
                tuning.upstream_timeout,
            ));
        }
        Self {
            frames,
            directory,
            opened: false,
            refused: refused.into_iter().map(refusal).collect(),
            _cancel: guard,
        }
    }

    /// The next frame for this client, `None` once the stream is over.
    ///
    /// The latest merged directory opens the stream. Then come the refusals this
    /// gateway composed for the attach. Afterwards, directory changes, spliced
    /// upstream frames, and heartbeats are written in arrival order. The
    /// directory is a watch rather than a queued frame because `list` is a
    /// cumulative snapshot the newest supersedes.
    ///
    /// A `list` or a heartbeat can land in the middle of an attach block, which a
    /// host's own stream never does (it drains a block before its live queue).
    /// That is harmless: the ordering an attach block promises is within one
    /// session's frames, and neither of these belongs to a session.
    pub(crate) async fn next_frame(
        &mut self,
        idle: Duration,
        shutdown: &CancellationToken,
    ) -> Option<Outgoing> {
        // The directory as it stands opens the stream: a client that has just
        // attached has been sent nothing and would otherwise wait for a change
        // to learn what is there.
        if !self.opened {
            self.opened = true;
            return Some(self.list());
        }
        // Then what this attach could not serve, before anything it could: a
        // session named here is one no upstream will ever say anything about,
        // so there is nothing to interleave it with.
        if let Some(refusal) = self.refused.pop_front() {
            return Some(Outgoing::Own(refusal));
        }
        let woken = tokio::select! {
            _ = shutdown.cancelled() => Woken::Over,
            // `None` only from an eviction, which is what ends the stream of a
            // client this gateway could not keep up with.
            frame = self.frames.recv() => match frame {
                Some(frame) => Woken::Spliced(frame),
                None => Woken::Over,
            },
            changed = self.directory.changed() => match changed {
                Ok(()) => Woken::Directory,
                // The gateway is gone, so there is nothing left to say.
                Err(_) => Woken::Over,
            },
            _ = tokio::time::sleep(idle) => Woken::Idle,
        };
        match woken {
            Woken::Spliced(frame) => Some(Outgoing::Spliced(frame)),
            Woken::Directory => Some(self.list()),
            Woken::Idle => Some(Outgoing::Own(Frame::Heartbeat)),
            Woken::Over => None,
        }
    }

    /// The merged directory as this client's next frame, marked as seen so the
    /// next change is one this client has not been sent.
    fn list(&mut self) -> Outgoing {
        Outgoing::Directory(Arc::clone(&self.directory.borrow_and_update()))
    }
}

/// Which of a client stream's sources woke it.
enum Woken {
    Spliced(DecodedFrame),
    Directory,
    Idle,
    Over,
}

/// Everything one client stream is owed about one host's sessions.
///
/// Each session is answered with its upstream block, or with `host_unreachable`
/// when there is no upstream to be had: this gateway holds no link to the host,
/// or the dial failed or did not answer within `answer_within`. Then the first
/// of three things ends the task with a `reset` for every session: the
/// upstream ending, the host's return as the control link reports it, or the
/// host's withdrawal.
///
/// One task writes every frame about this host's sessions on this stream, into
/// one FIFO queue, which is what orders them: a `host_unreachable` is queued
/// before the task starts watching for the return whose `reset` follows it.
async fn attend(
    group: AttachGroup,
    reachable: watch::Receiver<Arc<BTreeSet<String>>>,
    queue: Sender<DecodedFrame>,
    cancel: CancellationToken,
    answer_within: Duration,
) {
    let sessions = group.namespaced();
    let ended = tokio::select! {
        _ = cancel.cancelled() => return,
        _ = group.serving.cancelled() => "its host's enrollment was withdrawn".to_string(),
        ended = follow(&group, reachable, &queue, answer_within) => match ended {
            Some(ended) => ended,
            // The client this was for went away, and a client that is gone is
            // owed nothing.
            None => return,
        },
    };
    tracing::info!(
        "host {}'s sessions on a client stream end: {ended}",
        group.host_id
    );
    // Continuity is broken for exactly this host's sessions, and this gateway
    // does not resume them itself (see the module docs).
    for session in &sessions {
        if queue.offer(reset(session)) == Offered::Evicted {
            return;
        }
    }
}

/// Answer `group`'s sessions and follow them until something ends that,
/// answering what did, or `None` once the client is gone.
async fn follow(
    group: &AttachGroup,
    mut reachable: watch::Receiver<Arc<BTreeSet<String>>>,
    queue: &Sender<DecodedFrame>,
    answer_within: Duration,
) -> Option<String> {
    let not_reachable = format!("host {} is not reachable from this gateway", group.host_id);
    let reason = match &group.dial {
        Some(address) => match dial(address, &group.attach, answer_within).await {
            Ok(events) => {
                let sessions: Vec<String> = group
                    .attach
                    .iter()
                    .map(|request| request.session.clone())
                    .collect();
                // A link that dropped and came back while this upstream stayed
                // open is a return all the same: the host may have restarted
                // behind it.
                return tokio::select! {
                    ended = carry(&group.host_id, &sessions, events, queue) => ended,
                    () = returned(&mut reachable, &group.host_id, true) => {
                        Some("its host's link came back".to_string())
                    }
                };
            }
            Err(err) => {
                tracing::info!("could not open the spliced stream to {address}: {err}");
                format!("{not_reachable}: {err}")
            }
        },
        None => not_reachable,
    };
    for session in group.namespaced() {
        let unreachable = DecodedFrame::try_from(Frame::Error {
            session,
            code: HOST_UNREACHABLE_CODE.to_string(),
            message: reason.clone(),
        })
        .expect("an error frame carries nothing that could fail validation");
        // Paced, as the block it stands in for would be.
        if !queue.send_paced(unreachable).await {
            return None;
        }
    }
    // A dial that failed on a link that reads up says the link is not the whole
    // story, so any later reading of the host as up is taken as its return.
    returned(&mut reachable, &group.host_id, false).await;
    Some("its host came back".to_string())
}

/// Wait for `host_id`'s control link to report it up after it was not, `up`
/// being what the caller last knew. Pending for good once the gateway is gone.
///
/// Only an edge this observes: `watch` coalesces, so a flap that lands entirely
/// between two wakes presents as no change at all. That is a flap during which
/// an open upstream is the authority, and one that broke with it is reported
/// by its own end.
async fn returned(
    reachable: &mut watch::Receiver<Arc<BTreeSet<String>>>,
    host_id: &str,
    mut up: bool,
) {
    loop {
        if reachable.changed().await.is_err() {
            return std::future::pending().await;
        }
        let now = reachable.borrow_and_update().contains(host_id);
        if now && !up {
            return;
        }
        up = now;
    }
}

/// Open one host's upstream stream with the client's own attach set.
///
/// The ids that travel are the host's own and the cursors are the client's,
/// untouched: the gateway holds no cursors, so what it offers upstream is what
/// the client offered it.
///
/// `answer_within` bounds the response head only. The body stays open for as
/// long as the client is attached, and silence on an open stream is the
/// upstream's own failure (two missed heartbeats).
async fn dial(
    address: &HostAddress,
    attach: &[AttachRequest],
    answer_within: Duration,
) -> Result<RemoteEvents, RemoteError> {
    RemoteClient::new(address.url())?
        .with_open_timeout(answer_within)
        // The merged directory comes from the control link, so this stream
        // has no use for the host's own.
        .events_with(attach, ListFrames::Omitted)
        .await
}

/// Forward frames until the upstream ends, answering why it did, or `None` once
/// the client this was for is gone.
async fn carry(
    host_id: &str,
    sessions: &[String],
    mut events: RemoteEvents,
    queue: &Sender<DecodedFrame>,
) -> Option<String> {
    // The sessions whose attach block is still being written. Their frames are
    // paced rather than measured against the client's bound, see
    // [`Sender::send_paced`].
    let mut attaching: HashSet<String> = sessions.iter().cloned().collect();
    loop {
        let frame = match events.recv_decoded().await {
            None => return Some("the host closed the stream".to_string()),
            Some(Err(err)) => return Some(err.to_string()),
            Some(Ok(frame)) => frame,
        };
        if !forward(host_id, frame, &mut attaching, queue).await {
            return None;
        }
    }
}

/// Forward one frame downstream with its session id namespaced.
///
/// Answers whether the client is still there.
async fn forward(
    host_id: &str,
    mut frame: DecodedFrame,
    attaching: &mut HashSet<String>,
    queue: &Sender<DecodedFrame>,
) -> bool {
    let session = match frame.session() {
        Ok(session) => session,
        // A top-level `session` that is not an id, which only a kind this build
        // does not know can carry this far: a known kind with one fails to
        // decode. It cannot be namespaced, and forwarding it under the host's
        // own id would put an id no client of this gateway can address on the
        // wire. An endpoint client discards unknown kinds anyway.
        Err(err) => {
            tracing::debug!("dropping a frame whose session id cannot be read: {err}");
            return true;
        }
    };
    let Some(session) = session else {
        // Host-scoped. The merged `list` is this gateway's own composition from
        // its control links, so a host's own would put ids no client
        // here can address on the stream. [`dial`] asks for none, and one
        // that arrives anyway is still not forwarded. A heartbeat belongs to
        // the connection it was written on rather than to what rides it.
        // Everything else travels, an unknown kind that names no session
        // included.
        if matches!(
            &frame,
            DecodedFrame::Known(known)
                if matches!(known.value(), Frame::List { .. } | Frame::Heartbeat),
        ) {
            return true;
        }
        return queue.offer(frame) != Offered::Evicted;
    };
    let namespaced = SessionAddress::new(host_id, &session).to_string();
    // `false` would say the frame has no top-level `session`, which the read
    // above already answered for: the two decide on the same field, so neither
    // that nor an error can happen here. Dropping the frame is the honest
    // fallback, because forwarding it would carry the host's own id downstream.
    match frame.rewrite_session(&namespaced) {
        Ok(true) => {}
        outcome => {
            tracing::warn!("dropping a frame that will not take a namespaced id: {outcome:?}");
            return true;
        }
    }
    let paced = attaching.contains(&session);
    if paced && ends_a_block(&frame) {
        attaching.remove(&session);
    }
    if paced {
        queue.send_paced(frame).await
    } else {
        queue.offer(frame) != Offered::Evicted
    }
}

/// Whether `frame` ends a session's attach block: the `caught_up` that closes
/// one, or the `error` frame the server sent instead of one.
fn ends_a_block(frame: &DecodedFrame) -> bool {
    matches!(
        frame,
        DecodedFrame::Known(known)
            if matches!(known.value(), Frame::CaughtUp { .. } | Frame::Error { .. }),
    )
}

/// A `reset` for one namespaced session.
fn reset(session: &str) -> DecodedFrame {
    DecodedFrame::try_from(Frame::Reset {
        session: session.to_string(),
    })
    .expect("a reset frame carries nothing that could fail validation")
}

/// The refusal one unresolvable session is owed: a session-scoped `error`
/// frame in place of its attach block.
///
/// Named as the client named it: an id this gateway cannot resolve is one it
/// could not have minted either, so there is nothing to namespace and the id
/// the client asked about is the only one it can match the refusal to.
fn refusal(unresolvable: Unresolvable) -> Frame {
    Frame::Error {
        session: unresolvable.session,
        code: UNKNOWN_SESSION.to_string(),
        message: unresolvable.message,
    }
}

/// The code an attach refusal carries, the same one a proxied request to an
/// unresolvable id answers with.
const UNKNOWN_SESSION: &str = "unknown_session";

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;

    /// The merged directory is the literal first frame even when a spliced
    /// refusal is already queued.
    ///
    /// Queued upstream traffic must not overtake the gateway's opening list.
    #[tokio::test]
    async fn the_opening_directory_precedes_an_already_queued_spliced_refusal() {
        let (_directory_tx, directory) = watch::channel(Arc::new(MergedDirectory::default()));
        let cancel = CancellationToken::new();
        let (sender, frames) =
            outbound::channel(NonZeroUsize::new(4).expect("non-zero"), cancel.clone());
        let refusal = DecodedFrame::try_from(Frame::Error {
            session: "left:s-1".to_string(),
            code: "locked".to_string(),
            message: "held".to_string(),
        })
        .expect("a locked refusal");
        assert!(
            sender.offer(refusal) == Offered::Queued,
            "the refusal never reached the splice queue",
        );
        let mut splice = Splice {
            frames,
            directory,
            opened: false,
            refused: VecDeque::new(),
            _cancel: cancel.clone().drop_guard(),
        };
        let shutdown = CancellationToken::new();

        assert!(
            matches!(
                splice.next_frame(Duration::from_secs(1), &shutdown).await,
                Some(Outgoing::Directory(_))
            ),
            "an already queued refusal overtook the opening directory",
        );
        assert!(
            matches!(
                splice.next_frame(Duration::from_secs(1), &shutdown).await,
                Some(Outgoing::Spliced(DecodedFrame::Known(known)))
                    if matches!(known.value(), Frame::Error { session, code, .. }
                        if session == "left:s-1" && code == "locked")
            ),
            "the queued refusal did not follow the opening directory",
        );
    }

    /// Namespacing a host's locked refusal rewrites only its session id. The
    /// fields this gateway does not know travel from the raw frame unchanged.
    #[tokio::test]
    async fn a_refusals_unknown_metadata_survives_the_raw_gateway_rewrite() {
        let frame: DecodedFrame = serde_json::from_str(
            r#"{"kind":"error","session":"s-1","code":"locked","message":"held","added_later":{"kept":true}}"#,
        )
        .expect("a locked refusal");
        let cancel = CancellationToken::new();
        let (sender, mut receiver) =
            outbound::channel(NonZeroUsize::new(4).expect("non-zero"), cancel);
        let mut attaching = HashSet::from(["s-1".to_string()]);

        assert!(forward("left", frame, &mut attaching, &sender).await);
        let forwarded = receiver.recv().await.expect("the forwarded refusal");
        let DecodedFrame::Known(known) = forwarded else {
            panic!("the known refusal changed kind");
        };
        assert!(
            matches!(
                known.value(),
                Frame::Error {
                    session,
                    code,
                    message,
                    ..
                } if session == "left:s-1" && code == "locked" && message == "held"
            ),
            "the typed refusal lost its namespace or envelope: {:?}",
            known.value(),
        );
        let raw = known
            .raw_json()
            .expect("a rewritten wire frame retains JSON");
        assert!(
            raw.get().contains(r#""added_later":{"kept":true}"#),
            "the raw rewrite re-encoded away an additive field: {}",
            raw.get(),
        );
    }
}
