//! Splicing a client's session streams onto the hosts that own them (spec 7.1).
//!
//! One client stream is one [`Splice`]: the merged directory, one upstream
//! stream per host whose sessions that client attached, and one bounded queue
//! ([`crate::gateway::outbound`]) merging what the upstreams say. Every frame
//! travels downstream with its session id namespaced and nothing else touched,
//! kinds this build does not know included (spec 6.10).
//!
//! A stream that attaches nothing is a splice of nothing, which is what keeps
//! one writer for both cases: the directory and heartbeats reach a sidebar the
//! same way they reach a client watching ten sessions.
//!
//! What this deliberately does not do is redial an upstream that dropped.
//! Resuming one needs a *current* cursor, and the client's cursor advances as it
//! applies the frames this gateway forwarded, so tracking one here would give
//! the gateway per-session cursor state that spec 7.1 forbids and put a second,
//! subtly different cursor authority in the system. A drop therefore emits
//! `reset` downstream, which means "continuity broke, re-attach with your
//! cursor" (spec 6.3), and the client's own re-attach is the only thing that
//! opens an upstream. Resume is then incremental when the host's epoch survived
//! and full when it did not, inherited from the host protocol with no gateway
//! involvement.
//!
//! The one end that is not a drop is a withdrawal: the host stops being this
//! gateway's, so its upstream ends and its sessions are *not* reset, because the
//! re-attach a `reset` asks for would be refused and would take the client's
//! sessions on every other host down with it (spec 6.5, 7.1). The client learns
//! it from the directory, where that host's rows and its group are gone.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

use aj_wire::{DecodedFrame, Frame, MergedDirectory};
use tokio::sync::watch;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::gateway::config::HostAddress;
use crate::gateway::directory::AttachGroup;
use crate::gateway::naming::SessionAddress;
use crate::gateway::outbound::{self, Offered, Sender};
use crate::gateway::{GatewayError, Tuning};
use crate::remote::{RemoteClient, RemoteError, RemoteEvents};

/// What one client stream writes next.
pub(crate) enum Outgoing {
    /// The merged directory, which this gateway composes from its hosts' rows
    /// and writes as a `list` frame (spec 7.1).
    Directory(Arc<MergedDirectory>),
    /// A frame this gateway composed itself: a heartbeat.
    Own(Frame),
    /// A frame spliced from the host that owns the session it names, forwarded
    /// as it arrived apart from that id.
    Spliced(DecodedFrame),
}

/// Everything one client stream reads from.
pub(crate) struct Splice {
    /// The spliced frames of the sessions this client attached.
    frames: outbound::Receiver,
    /// The merged directory, which every client stream carries.
    directory: watch::Receiver<Arc<MergedDirectory>>,
    /// Whether the opening directory has been written.
    opened: bool,
    /// Cancelled when this is dropped, which is what ends the upstream streams
    /// and the tasks pumping them: a client that goes away stops costing this
    /// gateway, and its hosts, anything.
    _cancel: DropGuard,
}

impl Splice {
    /// Open the upstream stream of every host in `groups`.
    ///
    /// Returning means every upstream that could be opened is open and its
    /// attach block is already on its way, so a refusal is an HTTP status rather
    /// than a failure a client would have to look for among the frames (spec
    /// 6.5). A host this gateway holds no link to contributes no upstream at all
    /// (see [`AttachGroup::dial`]), and so does one withdrawn while this was
    /// dialing it.
    ///
    /// `shutdown` is the serving gateway's own token, and this splice's is a
    /// child of it: a client that stopped reading never observes a shutdown,
    /// because its stream is only polled when there is room to write to it, and
    /// its upstreams have to end anyway.
    pub(crate) async fn open(
        groups: Vec<AttachGroup>,
        reachable: watch::Receiver<Arc<BTreeSet<String>>>,
        directory: watch::Receiver<Arc<MergedDirectory>>,
        tuning: Tuning,
        shutdown: &CancellationToken,
    ) -> Result<Self, GatewayError> {
        let cancel = shutdown.child_token();
        // Taken before anything is spawned, so an upstream that will not open
        // takes the ones that already did down with it on the way out.
        let guard = cancel.clone().drop_guard();
        let (sender, frames) = outbound::channel(tuning.outbound_queue, cancel.clone());
        let mut watched = Vec::new();
        let mut opened = Vec::new();
        for group in groups {
            watched.push(HostReturn {
                sessions: group.namespaced(),
                // What the watcher compares against: a host that was there when
                // its upstream opened has broken nothing, and one that was not
                // is a client waiting for it to come back.
                up: group.dial.is_some(),
                host_id: group.host_id.clone(),
            });
            let Some(address) = group.dial.clone() else {
                continue;
            };
            // Raced against the withdrawal, because a dial is bounded by
            // `upstream_timeout` and the dials are sequential: waiting one out on
            // a host that is no longer this gateway's would hold up every other
            // host on this stream, answer the client after the withdrawal already
            // has, and end in a timeout, which is a 503 for the whole stream
            // rather than the "contributes no upstream" a withdrawn host owes it
            // (spec 7.1).
            let events = tokio::select! {
                _ = group.serving.cancelled() => continue,
                events = dial(&address, &group, tuning.upstream_timeout) => events?,
            };
            opened.push((
                Upstream {
                    host_id: group.host_id,
                    address,
                    sessions: group
                        .attach
                        .iter()
                        .map(|request| request.session.clone())
                        .collect(),
                    serving: group.serving,
                },
                events,
            ));
        }
        // Pumped only once every dial is done. The dials are sequential and each
        // one is bounded by `upstream_timeout`, so a pump started inside that
        // loop would forward one host's frames into a queue for a client that
        // has not been handed its response head yet, and a busy session would
        // evict a client that never saw a frame. Until then the frames wait in
        // the upstream connection, which is where backpressure belongs.
        for (upstream, events) in opened {
            tokio::spawn(pump(upstream, events, sender.clone(), cancel.clone()));
        }
        if !watched.is_empty() {
            tokio::spawn(returns(watched, reachable, sender, cancel.clone()));
        }
        Ok(Self {
            frames,
            directory,
            opened: false,
            _cancel: guard,
        })
    }

    /// The next frame for this client, `None` once the stream is over.
    ///
    /// Three sources, written in arrival order: the merged directory, the
    /// spliced upstreams, and a heartbeat whenever both have been quiet for
    /// `idle` (spec 6.1). The directory is a watch rather than a queued frame
    /// because `list` is a cumulative snapshot the newest supersedes, so a
    /// client that fell behind wants only the latest (spec 6.4).
    ///
    /// A `list` or a heartbeat can land in the middle of an attach block, which a
    /// host's own stream never does (it drains a block before its live queue).
    /// That is harmless: the ordering spec 6.5 asks for is within one session's
    /// frames, and neither of these belongs to a session.
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
        let woken = tokio::select! {
            _ = shutdown.cancelled() => Woken::Over,
            // `None` only from an eviction, which is what ends the stream of a
            // client this gateway could not keep up with (spec 6.9).
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

/// One host's spliced stream, as the task pumping it knows it.
struct Upstream {
    /// The namespace this host's session ids appear under downstream.
    host_id: String,
    /// Where the stream was opened, for the log line when it ends.
    address: HostAddress,
    /// The sessions this stream attached, in the host's own vocabulary.
    sessions: Vec<String>,
    /// Cancelled when this host's enrollment is withdrawn (spec 7.1).
    serving: CancellationToken,
}

/// Open one host's upstream stream with the client's own attach set.
///
/// The ids that travel are the host's own and the cursors are the client's,
/// untouched: the gateway holds no cursors, so what it offers upstream is what
/// the client offered it (spec 7.1).
///
/// `answer_within` bounds the response head only. The body stays open for as
/// long as the client is attached, and silence on an open stream is what the
/// client notices (two missed heartbeats). Without the bound a host that took
/// the request and said nothing would hold a client of this gateway waiting on
/// its own stream request for as long as it cared to.
async fn dial(
    address: &HostAddress,
    group: &AttachGroup,
    answer_within: Duration,
) -> Result<RemoteEvents, GatewayError> {
    let client = RemoteClient::new(address.url())
        .map(|client| client.with_open_timeout(answer_within))
        .map_err(|err| unreachable(address, err))?;
    client.events(&group.attach).await.map_err(|err| match err {
        // The host's own answer to a client's attach: a session it does not
        // hold, a lock conflict. It travels back with its status and its body,
        // exactly as a proxied refusal does, because the client asked this
        // question and the owning host answered it.
        RemoteError::Status {
            status,
            message,
            body,
            ..
        } => GatewayError::AttachRefused {
            status,
            host_id: group.host_id.clone(),
            message,
            body,
        },
        // Not a refusal: a host this gateway believed was there did not answer.
        // Carrying its sessions silently would leave a client watching frames
        // that never come, and nothing has marked them unreachable, so this is
        // the 503 a gateway answers for a host it cannot reach (spec 6.1).
        err => unreachable(address, err),
    })
}

fn unreachable(address: &HostAddress, source: RemoteError) -> GatewayError {
    GatewayError::Unreachable {
        address: address.clone(),
        source,
    }
}

/// Forward one host's frames to one client until the stream ends.
///
/// A withdrawal ends this without a `reset`, which is the one way this stream
/// stops that is not a break in continuity: the host is not this gateway's any
/// more, its rows are out of the merged directory, and the ids a `reset` would
/// have the client offer back no longer resolve here. A refused attach fails a
/// client's whole stream (spec 6.5), so that `reset` would cost it the sessions
/// it holds on every other host.
///
/// The whole loop races the withdrawal, not just the read: a pump pacing an
/// attach block is parked on the client's queue, and a signal it only saw
/// between frames would never reach it.
async fn pump(
    upstream: Upstream,
    mut events: RemoteEvents,
    queue: Sender,
    cancel: CancellationToken,
) {
    let ended = tokio::select! {
        _ = upstream.serving.cancelled() => return,
        ended = carry(&upstream, &mut events, &queue, &cancel) => ended,
    };
    let Some(ended) = ended else {
        return;
    };
    tracing::info!("the spliced stream to {} ended: {ended}", upstream.address);
    // A withdrawal that landed as this stream ended anyway: both are ready at
    // once and the select above picked this arm, which says nothing about which
    // happened first. The enrollment is gone either way, so there is nobody left
    // to re-attach to.
    if upstream.serving.is_cancelled() {
        return;
    }
    // Continuity is broken for exactly the sessions this stream carried, and
    // this gateway does not resume them itself (see the module docs).
    for session in &upstream.sessions {
        let namespaced = SessionAddress::new(&upstream.host_id, session).to_string();
        if queue.offer(reset(&namespaced)) == Offered::Evicted {
            return;
        }
    }
}

/// Forward frames until the upstream ends, answering why it did, or `None` once
/// the client this was for is gone.
async fn carry(
    upstream: &Upstream,
    events: &mut RemoteEvents,
    queue: &Sender,
    cancel: &CancellationToken,
) -> Option<String> {
    // The sessions whose attach block is still being written. Their frames are
    // paced rather than measured against the client's bound, see
    // [`Sender::send_paced`].
    let mut attaching: HashSet<String> = upstream.sessions.iter().cloned().collect();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => return None,
            next = events.recv_decoded() => next,
        };
        let frame = match next {
            None => return Some("the host closed the stream".to_string()),
            Some(Err(err)) => return Some(err.to_string()),
            Some(Ok(frame)) => frame,
        };
        if !forward(&upstream.host_id, frame, &mut attaching, queue).await {
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
    queue: &Sender,
) -> bool {
    let session = match frame.session() {
        Ok(session) => session,
        // A top-level `session` that is not an id, which only a kind this build
        // does not know can carry this far: a known kind with one fails to
        // decode. It cannot be namespaced, and forwarding it under the host's
        // own id would put an id no client of this gateway can address on the
        // wire. An endpoint client discards unknown kinds anyway (spec 6.10).
        Err(err) => {
            tracing::debug!("dropping a frame whose session id cannot be read: {err}");
            return true;
        }
    };
    let Some(session) = session else {
        // Host-scoped. The merged `list` is this gateway's own composition from
        // its control links (spec 7.1), so a host's own would put ids no client
        // here can address on the stream, and a heartbeat belongs to the
        // connection it was written on rather than to what rides it. Everything
        // else travels, an unknown kind that names no session included.
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

/// Whether `frame` is the `caught_up` that ends a session's attach block
/// (spec 6.5).
fn ends_a_block(frame: &DecodedFrame) -> bool {
    matches!(
        frame,
        DecodedFrame::Known(known) if matches!(known.value(), Frame::CaughtUp { .. }),
    )
}

/// One host a client's stream is waiting on, and what it last knew about it.
struct HostReturn {
    host_id: String,
    /// The namespaced ids of that client's sessions on this host.
    sessions: Vec<String>,
    up: bool,
}

/// Emit `reset` for a host's sessions when this gateway's link to it returns.
///
/// The control link is the reachability oracle: it redials on its own, and its
/// return is what makes an upstream attach succeed again. A client learns of
/// that only by attaching, and `reset` is how it is asked to (spec 6.3, 7.1).
///
/// Only the edge from down to up, and only one this stream observed. Emitting at
/// open would spin a client whose host is down (attach, reset, re-attach, reset),
/// and a host that was up all along has broken nothing for this stream.
///
/// The edge can be one `reset` more than continuity strictly needed: a host whose
/// return follows a drop the pump already reported earns one, and so does a
/// control link that flapped while the spliced stream survived, when the watcher
/// saw both sides of the flap. It can also be none of those, because `watch`
/// coalesces: a flap that lands entirely between two wakes presents as no change
/// at all. Both are deliberate. A `reset` costs a re-attach that resumes
/// incrementally, and a flap this misses is one where the stream was open, so a
/// stream that broke with it is reported by its own pump.
async fn returns(
    mut hosts: Vec<HostReturn>,
    mut reachable: watch::Receiver<Arc<BTreeSet<String>>>,
    queue: Sender,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            changed = reachable.changed() => if changed.is_err() {
                return;
            },
        }
        let now = Arc::clone(&reachable.borrow_and_update());
        for host in &mut hosts {
            let up = now.contains(&host.host_id);
            let returned = up && !host.up;
            host.up = up;
            if !returned {
                continue;
            }
            for session in &host.sessions {
                if queue.offer(reset(session)) == Offered::Evicted {
                    return;
                }
            }
        }
    }
}

/// A `reset` for one namespaced session (spec 6.3).
fn reset(session: &str) -> DecodedFrame {
    DecodedFrame::try_from(Frame::Reset {
        session: session.to_string(),
    })
    .expect("a reset frame carries nothing that could fail validation")
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use futures::FutureExt;

    use super::*;
    use crate::remote::RemoteClient;
    use crate::remote::tests::{addr, bounded};

    /// How many times the tie is played out.
    ///
    /// Both arms of [`pump`]'s `select!` are ready on its first poll here, and
    /// `select!` resolves that at random, so the round in which the upstream arm
    /// wins is the one the guard after the loop exists for. Enough rounds that
    /// missing it is 2^-64.
    const ROUNDS: usize = 64;

    /// An upstream that ended in the very poll its host was withdrawn in sends
    /// no `reset` (spec 7.1).
    ///
    /// Both are true at once here, which is a state the enrollment reaches for
    /// real: the pump is spawned only after every dial, so a withdrawal during
    /// the dials plus a host that has already closed the stream presents `pump`
    /// with both on its first poll. Whichever arm wins, the enrollment is gone,
    /// so a `reset` would ask the client to attach an id this gateway no longer
    /// resolves, and a refused attach would cost it the sessions it holds on
    /// every other host (spec 6.5).
    ///
    /// The fixture leaves `select!` as the only decider: the stream is driven to
    /// its end first, so `carry` completes on its first poll without yielding,
    /// and the token is cancelled before `pump` is polled at all.
    #[tokio::test]
    async fn an_upstream_that_ended_as_it_was_withdrawn_sends_no_reset() {
        let (address, host) = host_with_no_frames().await;
        let client = RemoteClient::new(address.url()).expect("a client");
        bounded("the withdrawal to race the end of the stream", async {
            for round in 0..ROUNDS {
                let mut events = client.events(&[]).await.expect("a stream");
                assert!(
                    events.recv_decoded().await.is_none(),
                    "round {round}: the stream has to be over before the pump runs, \
                     or this measures an ordinary read instead of the tie",
                );
                let serving = CancellationToken::new();
                serving.cancel();
                let cancel = CancellationToken::new();
                let (queue, mut frames) =
                    outbound::channel(NonZeroUsize::new(8).expect("non-zero"), cancel.clone());

                pump(
                    Upstream {
                        host_id: "left".to_string(),
                        address: address.clone(),
                        sessions: vec!["s-1".to_string()],
                        serving,
                    },
                    events,
                    queue,
                    cancel,
                )
                .await;

                let carried = frames.recv().now_or_never().flatten();
                assert!(
                    carried.is_none(),
                    "round {round}: a withdrawn host's splice asked the client to \
                     re-attach an id this gateway no longer resolves: {carried:?}",
                );
            }
        })
        .await;
        host.abort();
    }

    /// A host whose event stream is over as soon as it opens.
    ///
    /// What makes `carry` complete without yielding: [`RemoteEvents`] marks
    /// itself done at the end of the stream, so every read after that answers on
    /// the spot.
    async fn host_with_no_frames() -> (HostAddress, tokio::task::JoinHandle<()>) {
        use axum::response::sse::{Event, Sse};
        use axum::routing::get;

        let app = axum::Router::new().route(
            "/v1/events",
            get(|| async {
                Sse::new(futures::stream::empty::<
                    Result<Event, std::convert::Infallible>,
                >())
            }),
        );
        let listener = tokio::net::TcpListener::bind(addr("127.0.0.1:0"))
            .await
            .expect("bind a loopback port");
        let bound = listener.local_addr().expect("local addr");
        let serving = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let address = HostAddress::parse(&format!("http://{bound}")).expect("an address");
        (address, serving)
    }
}
