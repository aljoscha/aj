//! The HTTP client for the remote-control protocol (spec 6.1, 6.5-6.7).
//!
//! One [`RemoteClient`] is one connection's worth of surface: the reads, the
//! commands, and [`RemoteClient::events`] for the stream. It decodes the
//! host's `{code, message}` bodies into a typed [`RemoteError`], so a caller
//! can tell a refusal (a 409 the user should see) from a transport failure (a
//! reconnect).
//!
//! The stream side is deliberately thin: [`RemoteEvents`] yields frames and
//! nothing else, either decoded to [`Frame`] for an endpoint client or as
//! [`aj_wire::DecodedFrame`] for a gateway, which has to forward the kinds it
//! does not know. Cursors, epochs and reconciliation are the fold's business
//! ([`aj_app::client::SessionClient`]), which is what keeps the local and the
//! remote client one implementation.

use std::pin::Pin;
use std::time::Duration;

use aj_agent::tool::TaskId;
use aj_app::host::{AttachRequest, CommandOutcome};
use aj_wire::{
    ArchiveRequest, CancelRequest, CompactRequest, CreateSessionRequest, DecodedFrame, Frame,
    HeadRequest, Hello, PROTOCOL_VERSION, PromptRequest, QueueOperation, QueueOutcome,
    QueueRequest, QueueState, SessionCreated, SessionList, SessionTree, SettingsRequest,
    SteerRequest, TagRequest, TaskDetails, TaskTable,
};
use eventsource_stream::{EventStreamError, Eventsource};
use futures::{Stream, StreamExt};
use reqwest::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// How long a read, a command, or the opening of the event stream may take
/// before it is abandoned.
///
/// For the stream this bounds the response *head* only. The body stays open
/// for as long as the client is attached, and silence is what a dead stream
/// looks like once it is open (see [`RemoteEvents`]).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long establishing a connection may take, for every request this client
/// makes.
///
/// A control port sits on loopback or on a tailnet, so a connect that takes
/// longer than this is a peer that is not there. Bounding it separately is
/// what keeps a black-holed address from burning a caller's whole request
/// budget before the request is even sent.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a stream may be silent before it counts as dead.
///
/// The host heartbeats every 30 seconds, so two missed heartbeats is the
/// signal (spec 6.1). The caller reconnects with backoff.
pub(crate) const SILENCE: Duration = Duration::from_secs(60);

/// Why a remote call did not answer what was asked.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RemoteError {
    #[error("invalid base url {url:?}: {reason}")]
    InvalidUrl { url: String, reason: String },
    #[error("could not reach the host: {0}")]
    Transport(#[from] reqwest::Error),
    /// The host refused. `code` is the protocol's stable token when the body
    /// carried one, which is what a caller branches on.
    ///
    /// `body` is that body as it arrived, because a gateway re-emits a refusal it
    /// did not author and every field of it is the host's to keep (spec 6.6,
    /// 6.10). `code` and `message` are the two fields this protocol names inside
    /// it, read out for callers that branch or display.
    #[error("the host answered {status}: {message}")]
    Status {
        status: StatusCode,
        code: Option<String>,
        /// The host's own sentence: its envelope's `message`, or the whole body
        /// when the body was no envelope at all.
        message: String,
        body: String,
    },
    #[error("the host sent something this build cannot read: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("could not encode the request: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("the event stream failed: {0}")]
    Stream(String),
    #[error("the event stream was silent for {0:?}")]
    Silent(Duration),
    #[error(
        "the host speaks protocol {found}, this build speaks {expected}: upgrade the older side"
    )]
    Protocol { found: u32, expected: u32 },
}

impl RemoteError {
    /// The HTTP status behind a refusal, `None` for a transport or decode
    /// failure.
    pub(crate) fn status(&self) -> Option<StatusCode> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// The protocol's error code behind a refusal, when the host sent one.
    pub(crate) fn code(&self) -> Option<&str> {
        match self {
            Self::Status { code, .. } => code.as_deref(),
            _ => None,
        }
    }
}

/// One mutation, as a client names it.
///
/// The variants are the wire request types, so this enum only decides which
/// route a body goes to. That keeps the client honest about the protocol:
/// there is no client-side vocabulary that the wire does not have.
#[derive(Clone, Debug)]
pub(crate) enum RemoteCommand {
    Prompt(PromptRequest),
    Steer(SteerRequest),
    Cancel(CancelRequest),
    Queue(QueueRequest),
    Compact(CompactRequest),
    Settings(SettingsRequest),
    Tag(TagRequest),
    Archive(ArchiveRequest),
    Head(HeadRequest),
    KillTask(TaskId),
}

impl RemoteCommand {
    /// The route under `/v1/sessions/{id}/`.
    pub(super) fn route(&self) -> String {
        match self {
            Self::Prompt(_) => "prompt".to_string(),
            Self::Steer(_) => "steer".to_string(),
            Self::Cancel(_) => "cancel".to_string(),
            Self::Queue(_) => "queue".to_string(),
            Self::Compact(_) => "compact".to_string(),
            Self::Settings(_) => "settings".to_string(),
            Self::Tag(_) => "tag".to_string(),
            Self::Archive(_) => "archive".to_string(),
            Self::Head(_) => "head".to_string(),
            Self::KillTask(task) => format!("tasks/{task}/kill"),
        }
    }

    pub(super) fn body(&self) -> Result<Vec<u8>, RemoteError> {
        match self {
            Self::Prompt(request) => encode(request),
            Self::Steer(request) => encode(request),
            Self::Cancel(request) => encode(request),
            Self::Queue(request) => encode(request),
            Self::Compact(request) => encode(request),
            Self::Settings(request) => encode(request),
            Self::Tag(request) => encode(request),
            Self::Archive(request) => encode(request),
            Self::Head(request) => encode(request),
            Self::KillTask(_) => Ok(b"{}".to_vec()),
        }
    }

    /// Whether this command answers with the text it withdrew (spec 6.6).
    fn withdraws(&self) -> bool {
        matches!(
            self,
            Self::Queue(QueueRequest {
                op: QueueOperation::Remove,
                ..
            })
        )
    }
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, RemoteError> {
    serde_json::to_vec(value).map_err(RemoteError::Encode)
}

/// A client against one host or gateway.
pub(crate) struct RemoteClient {
    /// The base URL with no trailing slash, so every route is `{base}/v1/...`.
    base: String,
    http: reqwest::Client,
    silence: Duration,
    /// How long the event stream's response head may take to arrive.
    open_timeout: Duration,
}

impl RemoteClient {
    /// A client against `base`, which must be an absolute http(s) URL.
    pub(crate) fn new(base: &str) -> Result<Self, RemoteError> {
        let url = reqwest::Url::parse(base).map_err(|err| RemoteError::InvalidUrl {
            url: base.to_string(),
            reason: err.to_string(),
        })?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err(RemoteError::InvalidUrl {
                url: base.to_string(),
                reason: "expected an absolute http or https URL".to_string(),
            });
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(RemoteError::Transport)?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            silence: SILENCE,
            open_timeout: REQUEST_TIMEOUT,
        })
    }

    /// How long a stream this client opens may be silent before it counts as
    /// dead. Two missed heartbeats by default.
    pub(crate) fn with_silence(mut self, silence: Duration) -> Self {
        self.silence = silence;
        self
    }

    /// How long opening a stream may take before it is abandoned.
    pub(crate) fn with_open_timeout(mut self, open_timeout: Duration) -> Self {
        self.open_timeout = open_timeout;
        self
    }

    /// The base URL this client dials.
    pub(crate) fn base(&self) -> &str {
        &self.base
    }

    /// The reachability and identity probe, which also settles version skew.
    ///
    /// A protocol mismatch fails here rather than later on a frame nobody can
    /// read: the integer only moves on a breaking change (spec 6.10).
    pub(crate) async fn hello(&self) -> Result<Hello, RemoteError> {
        let hello: Hello = self.get("/v1/hello").await?;
        check_protocol(&hello)?;
        Ok(hello)
    }

    pub(crate) async fn sessions(&self) -> Result<SessionList, RemoteError> {
        self.get("/v1/sessions").await
    }

    /// Create a session, answering its id and whatever the host could not
    /// apply to it (see [`SessionCreated::incomplete`]).
    pub(crate) async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionCreated, RemoteError> {
        let response = self.post("/v1/sessions", encode(&request)?).await?;
        decode(response).await
    }

    pub(crate) async fn tasks(&self, session: &str) -> Result<TaskTable, RemoteError> {
        self.get(&format!("/v1/sessions/{session}/tasks")).await
    }

    pub(crate) async fn task(
        &self,
        session: &str,
        task: TaskId,
    ) -> Result<TaskDetails, RemoteError> {
        self.get(&format!("/v1/sessions/{session}/tasks/{task}"))
            .await
    }

    pub(crate) async fn queue(&self, session: &str) -> Result<QueueState, RemoteError> {
        self.get(&format!("/v1/sessions/{session}/queue")).await
    }

    pub(crate) async fn tree(&self, session: &str) -> Result<SessionTree, RemoteError> {
        self.get(&format!("/v1/sessions/{session}/tree")).await
    }

    /// Apply one mutation. Every command but the queue withdrawal answers
    /// [`CommandOutcome::Accepted`].
    pub(crate) async fn command(
        &self,
        session: &str,
        command: &RemoteCommand,
    ) -> Result<CommandOutcome, RemoteError> {
        let path = format!("/v1/sessions/{session}/{}", command.route());
        let response = self.post(&path, command.body()?).await?;
        if !command.withdraws() {
            return Ok(CommandOutcome::Accepted);
        }
        let outcome: QueueOutcome = decode(response).await?;
        Ok(CommandOutcome::Withdrawn(outcome.text))
    }

    /// Open the event stream, attaching every session in `attach` with the
    /// cursor it offers.
    ///
    /// An attach the host refuses is an error here, before the stream opens,
    /// so a caller never has to look for a failure among the frames.
    pub(crate) async fn events(
        &self,
        attach: &[AttachRequest],
    ) -> Result<RemoteEvents, RemoteError> {
        let query: Vec<(&str, String)> = attach
            .iter()
            .map(|request| {
                let value = match &request.cursor {
                    Some(cursor) => format!("{}@{cursor}", request.session),
                    None => request.session.clone(),
                };
                ("session", value)
            })
            .collect();
        // The request-level timeout would cover the body too, and this body is
        // open for as long as the client is attached, so the head is bounded
        // here instead. An open that never answers is a peer this client has
        // to give up on, silence on an open stream is what `RemoteEvents`
        // notices.
        let send = self
            .http
            .get(format!("{}/v1/events", self.base))
            .query(&query)
            .send();
        let response = match tokio::time::timeout(self.open_timeout, send).await {
            Ok(response) => response?,
            Err(_) => {
                return Err(RemoteError::Stream(format!(
                    "the host did not answer the stream request within {:?}",
                    self.open_timeout
                )));
            }
        };
        let response = refusal(response).await?;
        Ok(RemoteEvents::new(response, self.silence))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, RemoteError> {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?;
        decode(refusal(response).await?).await
    }

    async fn post(&self, path: &str, body: Vec<u8>) -> Result<reqwest::Response, RemoteError> {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .timeout(REQUEST_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        refusal(response).await
    }
}

/// Whether `hello` names a protocol this build speaks.
fn check_protocol(hello: &Hello) -> Result<(), RemoteError> {
    if hello.protocol == PROTOCOL_VERSION {
        return Ok(());
    }
    Err(RemoteError::Protocol {
        found: hello.protocol,
        expected: PROTOCOL_VERSION,
    })
}

/// Turn a non-2xx response into a typed refusal, preserving the status, the
/// protocol's code and the body it all arrived in.
async fn refusal(response: reqwest::Response) -> Result<reqwest::Response, RemoteError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    // Both fields are read on their own, because spec 6.6 calls an envelope
    // carrying only a `message` a complete error and an unknown `code` renders as
    // its message verbatim. A proxy, or a status the framework answers itself
    // (405), sends no envelope at all, and then the raw text stands in: pasting a
    // whole JSON body into `message` would show a user a blob instead of the
    // sentence the peer wrote.
    let envelope = serde_json::from_str::<Envelope>(&body).ok();
    let code = envelope.as_ref().and_then(|envelope| envelope.code.clone());
    let message = envelope
        .and_then(|envelope| envelope.message)
        .unwrap_or_else(|| {
            if body.trim().is_empty() {
                status.to_string()
            } else {
                body.clone()
            }
        });
    Err(RemoteError::Status {
        status,
        code,
        message,
        body,
    })
}

/// A refusal's body read for the two fields this protocol names, each optional
/// (spec 6.6).
#[derive(serde::Deserialize)]
struct Envelope {
    code: Option<String>,
    message: Option<String>,
}

async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, RemoteError> {
    let body = response.bytes().await?;
    serde_json::from_slice(&body).map_err(RemoteError::Decode)
}

/// The frames of one open stream.
///
/// Dropping this closes the connection, which is what deregisters the
/// subscriber on the host.
pub(crate) struct RemoteEvents {
    events: Pin<Box<dyn Stream<Item = Result<eventsource_stream::Event, StreamError>> + Send>>,
    /// How long silence is tolerated before the stream counts as dead.
    silence: Duration,
    /// When the current silence becomes fatal.
    ///
    /// It moves forward on every frame received and never on a call to
    /// [`Self::recv`], which is what makes the deadline survive that future's
    /// cancellation: a caller polling from a `select!` arm, or dropping a
    /// fresh `recv` future every loop iteration, still declares a wedged host
    /// dead on time.
    deadline: tokio::time::Instant,
    /// Set once the stream failed or ended, so a caller polling on cannot
    /// read past the failure it was already told about.
    done: bool,
}

type StreamError = EventStreamError<reqwest::Error>;

impl RemoteEvents {
    fn new(response: reqwest::Response, silence: Duration) -> Self {
        Self {
            events: Box::pin(response.bytes_stream().eventsource()),
            silence,
            deadline: tokio::time::Instant::now() + silence,
            done: false,
        }
    }

    /// A stream of frames already decoded off the wire, for a test that needs a
    /// drain to see them.
    ///
    /// The frames are handed over as an in-memory stream, so every poll of this
    /// makes progress. Over a connection they arrive through a task that
    /// forwards chunks between a caller's polls, which is why a test cannot
    /// assert that a non-blocking drain sees a frame the peer has written:
    /// whether it does is a scheduling question. Frames a drive loop must fold
    /// are pinned against this, and what the real transport is asserted on is
    /// the outcome either way.
    #[cfg(test)]
    pub(crate) fn scripted(frames: Vec<String>, silence: Duration) -> Self {
        let events = frames.into_iter().map(|data| {
            Ok(eventsource_stream::Event {
                event: "message".to_string(),
                data,
                id: String::new(),
                retry: None,
            })
        });
        Self {
            events: Box::pin(futures::stream::iter(events)),
            silence,
            deadline: tokio::time::Instant::now() + silence,
            done: false,
        }
    }

    /// How long this stream may be silent before it counts as dead.
    pub(crate) fn silence(&self) -> Duration {
        self.silence
    }

    /// The next frame, `None` once the stream ended.
    ///
    /// An unknown frame kind is skipped: an endpoint client discards those
    /// (spec 6.10). A gateway, which forwards them, reads
    /// [`Self::recv_decoded`] instead. A malformed known frame is an error and
    /// ends the stream, because a reliable frame this client cannot apply
    /// leaves its state incomplete, and a reconnect with a cursor is the
    /// recovery.
    pub(crate) async fn recv(&mut self) -> Option<Result<Frame, RemoteError>> {
        loop {
            match self.recv_decoded().await? {
                Ok(DecodedFrame::Known(frame)) => return Some(Ok(frame.into_value())),
                Ok(DecodedFrame::Unknown { kind, .. }) => {
                    tracing::debug!("discarding a frame of unknown kind {kind:?}");
                }
                Err(err) => return Some(Err(err)),
            }
        }
    }

    /// The next frame in decoded form, `None` once the stream ended.
    ///
    /// A kind this build does not know arrives as [`DecodedFrame::Unknown`]
    /// with its JSON retained, which is what a gateway forwards (spec 6.10). A
    /// malformed *known* frame is still an error: a frame whose kind we
    /// recognize and whose payload we cannot read is a peer we have stopped
    /// understanding, not an additive change.
    pub(crate) async fn recv_decoded(&mut self) -> Option<Result<DecodedFrame, RemoteError>> {
        if self.done {
            return None;
        }
        let next = match tokio::time::timeout_at(self.deadline, self.events.next()).await {
            Ok(next) => next,
            Err(_) => {
                let silence = self.silence;
                return Some(self.fail(RemoteError::Silent(silence)));
            }
        };
        // Any byte the host sent is evidence it is alive, a heartbeat and a
        // frame kind this build cannot read included.
        self.deadline = tokio::time::Instant::now() + self.silence;
        match next {
            None => {
                self.done = true;
                None
            }
            Some(Err(err)) => Some(self.fail(RemoteError::Stream(err.to_string()))),
            Some(Ok(event)) => Some(match serde_json::from_str::<DecodedFrame>(&event.data) {
                Ok(frame) => Ok(frame),
                Err(err) => self.fail(RemoteError::Decode(err)),
            }),
        }
    }

    fn fail<T>(&mut self, err: RemoteError) -> Result<T, RemoteError> {
        self.done = true;
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec 6.6: codes arrive error by error, an envelope with only a `message`
    /// is a complete error, and an unknown code renders as its message verbatim.
    /// So both fields are read on their own, and a body that is no envelope at
    /// all is the only case where the raw text is the best there is.
    #[tokio::test]
    async fn a_refusal_is_read_for_each_field_it_has() {
        for (body, expected_code, expected_message) in [
            (
                r#"{"code":"locked","message":"held by another writer"}"#,
                Some("locked"),
                "held by another writer",
            ),
            // The shape that reached a user as a JSON blob: the sentence is
            // there, and the envelope is complete without a code.
            (
                r#"{"message":"held by another writer","session":"s-1"}"#,
                None,
                "held by another writer",
            ),
            (
                r#"{"code":"locked"}"#,
                Some("locked"),
                r#"{"code":"locked"}"#,
            ),
            // Not an envelope: something in front of the host, or nothing at all.
            ("<html>bad gateway</html>", None, "<html>bad gateway</html>"),
            ("", None, "409 Conflict"),
        ] {
            let response = reqwest::Response::from(
                axum::http::Response::builder()
                    .status(StatusCode::CONFLICT)
                    .body(body.to_string())
                    .expect("a response"),
            );

            let err = refusal(response)
                .await
                .err()
                .unwrap_or_else(|| panic!("{body:?} is a refusal"));

            let RemoteError::Status {
                code,
                message,
                body: carried,
                ..
            } = &err
            else {
                panic!("{body:?} became {err:?}");
            };
            assert_eq!(code.as_deref(), expected_code, "{body:?}");
            assert_eq!(message, expected_message, "{body:?}");
            assert_eq!(
                carried, body,
                "the body travels whole, because a gateway re-emits a refusal it \
                 did not author",
            );
        }
    }
}
