//! One enrolled host's control connection (spec 7.1).
//!
//! Per host: its `/v1/events` stream with no session attachments, which is what
//! the gateway learns that host's directory from. A client that attaches
//! sessions gets a stream of its own onto the same host
//! ([`crate::gateway::splice`]), so this connection carries `list` frames and
//! heartbeats and nothing else.
//!
//! A drop is ordinary, not exceptional: the link marks its host unreachable,
//! waits out a backoff and dials again. A client sees the flap in the list's
//! `unreachable` flag, and the sessions of the other hosts are untouched. This
//! link is also the reachability oracle a splice waits on: a host it reaches
//! again is what earns that client's sessions a `reset`.

use std::sync::Arc;
use std::time::Instant;

use aj_wire::{DecodedFrame, Frame};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::gateway::Tuning;
use crate::gateway::config::HostAddress;
use crate::gateway::directory::Directory;
use crate::remote::RemoteClient;

/// The task keeping one host's control connection up.
pub(crate) struct Link {
    address: HostAddress,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl Link {
    /// Start dialing `address` and feeding what it says into `directory`.
    pub(crate) fn spawn(address: HostAddress, directory: Arc<Directory>, tuning: Tuning) -> Self {
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(address.clone(), directory, tuning, cancel.clone()));
        Self {
            address,
            cancel,
            task,
        }
    }

    /// Close the connection and wait for the task to be gone.
    ///
    /// Awaited rather than fired and forgotten, so a withdrawal that has
    /// answered cannot still be writing rows into a directory it was removed
    /// from.
    pub(crate) async fn stop(self) {
        self.cancel.cancel();
        if let Err(err) = self.task.await
            && !err.is_cancelled()
        {
            tracing::warn!("the link to {} ended badly: {err}", self.address);
        }
    }
}

/// Dial until told to stop.
async fn run(
    address: HostAddress,
    directory: Arc<Directory>,
    tuning: Tuning,
    cancel: CancellationToken,
) {
    let mut delay = tuning.reconnect_delay;
    loop {
        let started = Instant::now();
        let outcome = tokio::select! {
            _ = cancel.cancelled() => return,
            outcome = attempt(&address, &directory) => outcome,
        };
        match outcome {
            Attempt::Ended(reason) => {
                tracing::info!("the control connection to {address} ended: {reason}");
                directory.disconnected(&address, reason);
                // A connection that stood for a while was a real one, so the
                // next attempt starts patient again rather than inheriting the
                // backoff of an earlier outage. One that ended as soon as it
                // opened keeps backing off: a host that hangs up immediately
                // would otherwise be redialed at the floor rate forever.
                if started.elapsed() >= tuning.max_reconnect_delay {
                    delay = tuning.reconnect_delay;
                }
            }
            Attempt::Failed(reason) => {
                tracing::debug!("could not reach {address}: {reason}");
                directory.disconnected(&address, reason);
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(tuning.max_reconnect_delay);
    }
}

/// How one connection attempt turned out.
enum Attempt {
    /// The stream was open and then ended, with the reason it did.
    Ended(String),
    /// The connection was never established.
    Failed(String),
}

/// One attempt: dial, settle the host's id, then pump its directory until the
/// stream ends.
async fn attempt(address: &HostAddress, directory: &Directory) -> Attempt {
    let client = match RemoteClient::new(address.url()) {
        Ok(client) => client,
        Err(err) => return Attempt::Failed(err.to_string()),
    };
    // The handshake first, because it is what names the host: the gateway
    // cannot namespace a session before it knows whose it is, and a protocol
    // this build does not speak fails here rather than on a frame.
    let hello = match client.hello().await {
        Ok(hello) => hello,
        Err(err) => return Attempt::Failed(err.to_string()),
    };
    if let Err(err) = directory.adopt(address, &hello.host_id) {
        return Attempt::Failed(err.to_string());
    }
    // No session is named: this is the control connection of spec 7.1, so the
    // host sends it `list` frames and heartbeats and nothing else.
    let mut events = match client.events(&[]).await {
        Ok(events) => events,
        Err(err) => return Attempt::Failed(err.to_string()),
    };
    directory.connected(address);
    loop {
        let frame = match events.recv_decoded().await {
            None => return Attempt::Ended("the host closed the stream".to_string()),
            Some(Err(err)) => return Attempt::Ended(err.to_string()),
            Some(Ok(frame)) => frame,
        };
        match &frame {
            DecodedFrame::Known(known) => match known.value() {
                // The rows travel as their host wrote them: this gateway owns
                // three of their fields and passes the rest through, so a typed
                // re-encode here would strip a newer host's (spec 6.10).
                Frame::List { .. } => match frame.rows() {
                    // `Ok(None)` cannot come back for a `list` frame: the read
                    // decides on the same kind this arm matched on.
                    Ok(Some(rows)) => directory.set_rows(address, rows),
                    outcome => {
                        tracing::warn!(
                            "{address} sent a directory this gateway cannot read: {outcome:?}"
                        );
                    }
                },
                // A control connection names no session, so a host publishes it
                // none of these (spec 6.5). The ones a client asked for reach it
                // on that client's own spliced stream instead.
                Frame::Event { .. }
                | Frame::State { .. }
                | Frame::CaughtUp { .. }
                | Frame::Reset { .. } => {}
                Frame::Heartbeat => {}
                // A gateway learns about VMs from its own provisioner, not from
                // a host.
                Frame::Vms { .. } => {}
            },
            // A kind from a newer host. Retained rather than read: forwarding it
            // to a client is what keeps an older gateway usable between newer
            // peers (spec 6.10), and dropping the connection over it would make
            // this gateway the one thing that cannot tolerate the future.
            DecodedFrame::Unknown { kind, .. } => {
                tracing::debug!("{address} sent a frame of unknown kind {kind:?}");
            }
        }
    }
}
