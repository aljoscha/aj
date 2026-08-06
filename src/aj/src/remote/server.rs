//! The HTTP transport over a [`SessionHost`] (spec 6.1, 6.6, 6.7).
//!
//! Commands are JSON POSTs, reads are GETs, and effects arrive on one SSE
//! stream per connection. The server owns no protocol state of its own: it
//! parses a request into a host call, serializes what the host answers, and
//! writes the host's frames out unchanged. Everything the protocol calls
//! correctness (seqs, epochs, attach atomicity, flow control) already lives
//! in the host, which is why this layer stays this thin.
//!
//! The one rule that is this layer's alone: a request arriving over the
//! network is not the local user. A settings change therefore always carries
//! [`PersistAction::None`], so no peer can rewrite the host's config files,
//! and a model change travels as the (api, url, name) triple the host
//! resolves against its own catalog rather than as a catalog object.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aj_agent::events::AgentId;
use aj_agent::tool::TaskId;
use aj_app::host::{
    AttachRequest, Attachment, Command, CommandOutcome, HostError, QueueOp, SessionHost,
    SettingsAxis, SettingsChange,
};
use aj_app::session_setup::thinking_display_from_name;
use aj_app::settings::PersistAction;
use aj_conf::ConfigVerbosity;
use aj_models::{speed_from_name, thinking_config_from_name};
use aj_wire::{
    CancelRequest, CompactRequest, CreateSessionRequest, Cursor, ErrorResponse, Frame, HeadRequest,
    PromptRequest, QueueOperation, QueueOutcome, QueueRequest, SessionCreated, SessionSettings,
    SettingsRequest, SteerRequest,
};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequest, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use futures::Stream;
use serde::de::DeserializeOwned;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::remote::identity::{IdentityError, IdentityGate};

/// How long a stream may be idle before a heartbeat frame goes out.
///
/// A real `heartbeat` frame rather than an SSE comment, because a client
/// reading decoded frames must be able to tell "the stream is alive" from
/// "the transport buffered something", and only a frame reaches its decoder
/// (spec 6.1).
const HEARTBEAT: Duration = Duration::from_secs(30);

/// How long [`RemoteServer::shutdown`] waits for in-flight streams.
///
/// An attached stream ends when the host closes it, so the ordinary teardown
/// is host first, server second. This bound only exists so a client that is
/// still attached cannot wedge the process.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Why a server could not be started.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ServerError {
    /// The identity gate refuses to serve this address at all (spec 6.11).
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("could not bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

struct ServerState {
    host: SessionHost,
    gate: IdentityGate,
    /// How long a stream may be idle before a heartbeat frame. Carried here
    /// rather than read from [`HEARTBEAT`] at the write site so a test can
    /// watch the behavior without a thirty-second wait.
    heartbeat: Duration,
}

/// One bound control port serving the protocol over HTTP.
pub(crate) struct RemoteServer {
    addr: SocketAddr,
    shutdown: CancellationToken,
    serving: JoinHandle<()>,
}

impl RemoteServer {
    /// Bind `addr` and start serving `host` behind `gate`.
    ///
    /// The gate validates the address before the socket exists, so a `local`
    /// gate on a public address refuses to start rather than serving
    /// unauthenticated. Returning means the listener is accepting, so a
    /// caller may dial [`Self::url`] immediately.
    pub(crate) async fn bind(
        host: SessionHost,
        addr: SocketAddr,
        gate: IdentityGate,
    ) -> Result<Self, ServerError> {
        Self::bind_with(host, addr, gate, HEARTBEAT).await
    }

    /// [`Self::bind`] with the heartbeat interval named, so a test can watch
    /// an idle stream without a thirty-second wait.
    pub(super) async fn bind_with(
        host: SessionHost,
        addr: SocketAddr,
        gate: IdentityGate,
        heartbeat: Duration,
    ) -> Result<Self, ServerError> {
        gate.validate_bind(addr)?;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| ServerError::Bind { addr, source })?;
        let local = listener
            .local_addr()
            .map_err(|source| ServerError::Bind { addr, source })?;
        let state = Arc::new(ServerState {
            host,
            gate,
            heartbeat,
        });
        let app = router(Arc::clone(&state));
        let shutdown = CancellationToken::new();
        let serving = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                let served = axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async move { shutdown.cancelled().await });
                if let Err(err) = served.await {
                    tracing::warn!("the control port stopped serving: {err}");
                }
            }
        });
        Ok(Self {
            addr: local,
            shutdown,
            serving,
        })
    }

    /// The address actually bound, which resolves a port-zero request.
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The base URL a client dials.
    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop accepting and let in-flight streams finish.
    ///
    /// Shut the host down first: an attached stream only ends when the host
    /// closes it, so a stream still attached here is waited on until
    /// [`SHUTDOWN_GRACE`] expires and then dropped.
    pub(crate) async fn shutdown(self) {
        self.shutdown.cancel();
        let abort = self.serving.abort_handle();
        if tokio::time::timeout(SHUTDOWN_GRACE, self.serving)
            .await
            .is_err()
        {
            tracing::warn!("giving up on streams still open after {SHUTDOWN_GRACE:?}");
            abort.abort();
        }
    }
}

/// The protocol's routes (spec 6.1, 6.6, 6.7).
fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/v1/hello", get(hello))
        .route("/v1/events", get(events))
        .route("/v1/sessions", get(sessions).post(create_session))
        .route("/v1/sessions/{id}/tasks", get(tasks))
        .route("/v1/sessions/{id}/tasks/{task_id}", get(task))
        .route("/v1/sessions/{id}/tasks/{task_id}/kill", post(kill_task))
        // The one path that is both a read and a mutation (spec 6.6, 6.7).
        .route("/v1/sessions/{id}/queue", get(queue).post(queue_command))
        .route("/v1/sessions/{id}/tree", get(tree))
        .route("/v1/sessions/{id}/prompt", post(prompt))
        .route("/v1/sessions/{id}/steer", post(steer))
        .route("/v1/sessions/{id}/cancel", post(cancel))
        .route("/v1/sessions/{id}/compact", post(compact))
        .route("/v1/sessions/{id}/settings", post(settings))
        .route("/v1/sessions/{id}/head", post(head))
        .fallback(unknown_endpoint)
        // Outside the routes rather than per handler, so an unauthorized
        // peer cannot even probe which endpoints exist.
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            authorize,
        ))
        .with_state(state)
}

/// Reject a peer the gate does not accept, before the request is routed.
async fn authorize(
    State(state): State<Arc<ServerState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    match state.gate.authorize(peer).await {
        Ok(_) => next.run(request).await,
        Err(err) => {
            tracing::warn!("refused {peer}: {err}");
            ApiError::from(err).into_response()
        }
    }
}

async fn hello(State(state): State<Arc<ServerState>>) -> Response {
    Json(state.host.hello()).into_response()
}

async fn sessions(State(state): State<Arc<ServerState>>) -> Result<Response, ApiError> {
    Ok(Json(state.host.sessions().await?).into_response())
}

/// Create a session, answering 200 with its id (spec 6.6).
async fn create_session(
    State(state): State<Arc<ServerState>>,
    Body(request): Body<CreateSessionRequest>,
) -> Result<Response, ApiError> {
    let CreateSessionRequest { settings, prompt } = request;
    let id = state
        .host
        .create_with(settings, prompt.map(|prompt| prompt.into_content()))
        .await?;
    Ok(Json(SessionCreated { id }).into_response())
}

async fn tasks(
    State(state): State<Arc<ServerState>>,
    Path(session): Path<String>,
) -> Result<Response, ApiError> {
    Ok(Json(state.host.tasks(&session).await?).into_response())
}

async fn task(
    State(state): State<Arc<ServerState>>,
    Path((session, task)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let task = task_id(&task)?;
    Ok(Json(state.host.task(&session, task).await?).into_response())
}

async fn kill_task(
    State(state): State<Arc<ServerState>>,
    Path((session, task)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let task = task_id(&task)?;
    accepted(
        state
            .host
            .command(&session, Command::KillTask { task })
            .await?,
    )
}

async fn queue(
    State(state): State<Arc<ServerState>>,
    Path(session): Path<String>,
) -> Result<Response, ApiError> {
    Ok(Json(state.host.queue(&session).await?).into_response())
}

/// Withdraw one agent's pending message, or clear the session's queues.
///
/// A withdrawal answers 200 with the text it took, which is what makes a
/// client's dequeue-into-the-editor gesture work (spec 6.6). A clear is an
/// ordinary mutation.
async fn queue_command(
    State(state): State<Arc<ServerState>>,
    Path(session): Path<String>,
    Body(request): Body<QueueRequest>,
) -> Result<Response, ApiError> {
    let op = match request.op {
        QueueOperation::Remove => QueueOp::Remove {
            agent: request.agent.unwrap_or(AgentId::Main),
        },
        QueueOperation::Clear => QueueOp::Clear,
    };
    let outcome = state.host.command(&session, Command::Queue(op)).await?;
    match outcome {
        CommandOutcome::Withdrawn(text) => Ok(Json(QueueOutcome { text }).into_response()),
        CommandOutcome::Accepted => Ok(StatusCode::ACCEPTED.into_response()),
    }
}

async fn tree(
    State(state): State<Arc<ServerState>>,
    Path(session): Path<String>,
) -> Result<Response, ApiError> {
    Ok(Json(state.host.tree(&session).await?).into_response())
}

async fn prompt(
    State(state): State<Arc<ServerState>>,
    Path(session): Path<String>,
    Body(request): Body<PromptRequest>,
) -> Result<Response, ApiError> {
    let PromptRequest { agent, input } = request;
    accepted(
        state
            .host
            .command(
                &session,
                Command::Prompt {
                    agent: agent.unwrap_or(AgentId::Main),
                    content: input.into_content(),
                },
            )
            .await?,
    )
}

async fn steer(
    State(state): State<Arc<ServerState>>,
    Path(session): Path<String>,
    Body(request): Body<SteerRequest>,
) -> Result<Response, ApiError> {
    let SteerRequest { text, agent } = request;
    accepted(
        state
            .host
            .command(
                &session,
                Command::Steer {
                    agent: agent.unwrap_or(AgentId::Main),
                    text,
                },
            )
            .await?,
    )
}

async fn cancel(
    State(state): State<Arc<ServerState>>,
    Path(session): Path<String>,
    Body(request): Body<CancelRequest>,
) -> Result<Response, ApiError> {
    accepted(
        state
            .host
            .command(
                &session,
                Command::Cancel {
                    agent: request.agent.unwrap_or(AgentId::Main),
                },
            )
            .await?,
    )
}

async fn compact(
    State(state): State<Arc<ServerState>>,
    Path(session): Path<String>,
    Body(request): Body<CompactRequest>,
) -> Result<Response, ApiError> {
    accepted(
        state
            .host
            .command(
                &session,
                Command::Compact {
                    instructions: request.instructions,
                },
            )
            .await?,
    )
}

async fn settings(
    State(state): State<Arc<ServerState>>,
    Path(session): Path<String>,
    Body(request): Body<SettingsRequest>,
) -> Result<Response, ApiError> {
    let change = settings_change(&state.host, request)?;
    accepted(
        state
            .host
            .command(&session, Command::Settings(change))
            .await?,
    )
}

async fn head(
    State(state): State<Arc<ServerState>>,
    Path(session): Path<String>,
    Body(request): Body<HeadRequest>,
) -> Result<Response, ApiError> {
    accepted(
        state
            .host
            .command(
                &session,
                Command::Head {
                    entry: request.entry,
                },
            )
            .await?,
    )
}

/// Open the unified event stream, attaching every named session.
///
/// The attach happens before the response is returned, so a refusal (an
/// unknown session, a lock conflict) is an HTTP status rather than an error
/// frame on a stream that already opened.
async fn events(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<Vec<(String, String)>>,
) -> Result<Sse<impl Stream<Item = Result<Event, aj_agent::BoxError>>>, ApiError> {
    let requests = attach_requests(&params)?;
    let attachment = state.host.attach(&requests).await?;
    Ok(Sse::new(frame_stream(attachment, state.heartbeat)))
}

async fn unknown_endpoint() -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        code: "unknown_endpoint",
        message: "no such endpoint on this host".to_string(),
    }
}

/// The task id in a path segment, as the protocol's error shape rather than
/// the framework's own rejection for a segment that is not one.
fn task_id(raw: &str) -> Result<TaskId, ApiError> {
    raw.parse()
        .map_err(|_| ApiError::invalid(format!("{raw:?} is not a task id")))
}

/// A session mutation's answer: 202, per spec 6.6.
fn accepted(outcome: CommandOutcome) -> Result<Response, ApiError> {
    match outcome {
        CommandOutcome::Accepted => Ok(StatusCode::ACCEPTED.into_response()),
        // Only the queue withdrawal returns one, and it has its own handler.
        CommandOutcome::Withdrawn(_) => Err(ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: "the host withdrew a message for a command that takes none".to_string(),
        }),
    }
}

/// Parse the stream's repeatable `session=<id>[@<epoch>:<seq>]` parameters.
///
/// Unknown parameters are ignored (spec 6.10). Attaching nothing is legal:
/// that is the control connection a gateway opens for `list` frames alone.
///
/// The id itself is not judged here. It is opaque at this layer, and the
/// host's own gate answers 404 for anything its store could not hold (spec
/// 6.2), so an empty or malformed id reads the same on this route as on
/// `/v1/sessions/{id}/...`.
fn attach_requests(params: &[(String, String)]) -> Result<Vec<AttachRequest>, ApiError> {
    let mut requests = Vec::new();
    for (key, value) in params {
        if key != "session" {
            continue;
        }
        // The session id comes first, so an opaque epoch carrying an `@`
        // cannot swallow it. Session ids therefore must not contain one,
        // which the store's grammar guarantees.
        let (session, cursor) = match value.split_once('@') {
            Some((session, cursor)) => {
                let cursor: Cursor = cursor.parse().map_err(|err| {
                    ApiError::invalid(format!("invalid cursor in session={value:?}: {err}"))
                })?;
                (session, Some(cursor))
            }
            None => (value.as_str(), None),
        };
        requests.push(AttachRequest {
            session: session.to_string(),
            cursor,
        });
    }
    Ok(requests)
}

/// One SSE `data:` line per frame, with a heartbeat frame whenever the
/// writer has been idle for `idle`.
///
/// Each poll starts a fresh timeout, so any frame written restarts the idle
/// clock. Dropping the response drops this stream and with it the
/// [`Attachment`], which deregisters the subscriber.
fn frame_stream(
    attachment: Attachment,
    idle: Duration,
) -> impl Stream<Item = Result<Event, aj_agent::BoxError>> {
    futures::stream::unfold(Some(attachment), move |state| async move {
        let mut attachment = state?;
        let frame = match tokio::time::timeout(idle, attachment.recv()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => return None,
            Err(_) => Frame::Heartbeat,
        };
        match serde_json::to_string(&frame) {
            Ok(json) => Some((Ok(Event::default().data(json)), Some(attachment))),
            // A frame this host built that will not serialize is a host bug.
            // Ending the stream makes the client reconnect and re-sync,
            // which is the only honest answer: skipping it would silently
            // drop a reliable frame.
            Err(err) => Some((Err(err.into()), None)),
        }
    })
}

/// Resolve a settings request into the single axis the host applies.
///
/// Exactly one axis per request: the host applies, logs and publishes one
/// change at a time, and a body naming two would leave a client guessing
/// which one a refusal referred to.
fn settings_change(
    host: &SessionHost,
    request: SettingsRequest,
) -> Result<SettingsChange, ApiError> {
    let SettingsRequest { agent, change } = request;
    let SessionSettings {
        model,
        thinking,
        thinking_display,
        speed,
        verbosity,
    } = change;

    let mut axes = Vec::new();
    // Counted before anything is resolved, so a body naming two axes reads as
    // malformed rather than as whatever the first of them happened to fail on.
    let named = [
        model.is_some(),
        thinking.is_some(),
        thinking_display.is_some(),
        speed.is_some(),
        verbosity.is_some(),
    ]
    .into_iter()
    .filter(|named| *named)
    .count();
    if named != 1 {
        return Err(ApiError::invalid(format!(
            "a settings change names {named} axes: send exactly one of model, thinking, \
             thinking_display, speed, or verbosity"
        )));
    }
    if let Some(selection) = model {
        // Resolved against the host's own catalog and credentials: the wire
        // carries the (api, url, name) triple, never a catalog object.
        axes.push(SettingsAxis::Model(
            host.resolve_model_selection(&selection)?,
        ));
    }
    if let Some(name) = thinking {
        axes.push(SettingsAxis::Thinking(
            thinking_config_from_name(&name).ok_or_else(|| {
                ApiError::invalid(format!(
                    "unknown thinking level {name:?}. Expected off, minimal, low, medium, high, xhigh, or max"
                ))
            })?,
        ));
    }
    if let Some(name) = thinking_display {
        axes.push(SettingsAxis::ThinkingDisplay(
            thinking_display_from_name(&name).ok_or_else(|| {
                ApiError::invalid(format!(
                    "unknown thinking display {name:?}. Expected default, summarized, detailed, or omitted"
                ))
            })?,
        ));
    }
    if let Some(name) = speed {
        axes.push(SettingsAxis::Speed(speed_from_name(&name).ok_or_else(
            || ApiError::invalid(format!("unknown speed {name:?}. Expected standard or fast")),
        )?));
    }
    if let Some(name) = verbosity {
        // `"default"` is the vocabulary's unset value, as in the log's
        // settings entries and the local settings window.
        let verbosity = match name.as_str() {
            "default" => None,
            name => Some(name.parse::<ConfigVerbosity>().map_err(ApiError::invalid)?),
        };
        axes.push(SettingsAxis::Verbosity(verbosity));
    }

    let axis = axes.pop().expect("exactly one axis was named");

    Ok(SettingsChange {
        agent: agent.unwrap_or(AgentId::Main),
        // Forced: a network peer must not be able to rewrite this host's
        // config files, so a remote change is session-only however it was
        // asked for.
        persist: PersistAction::None,
        axis,
    })
}

/// A JSON request body with the protocol's error shape for a malformed one.
///
/// An absent or blank body reads as `{}`, so a command whose fields are all
/// optional (cancel, compact) can be sent without one. A body missing a
/// required field still fails as a malformed request.
struct Body<T>(T);

impl<S, T> FromRequest<S> for Body<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(|err| ApiError::invalid(err.body_text()))?;
        let bytes = if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            Bytes::from_static(b"{}")
        } else {
            bytes
        };
        serde_json::from_slice(&bytes)
            .map(Self)
            .map_err(|err| ApiError::invalid(format!("malformed request body: {err}")))
    }
}

/// An unsuccessful response: the status plus the stable `{code, message}`
/// body of spec 6.1.
struct ApiError {
    status: StatusCode,
    /// A stable snake_case token, so a client can branch on the reason
    /// without parsing prose.
    code: &'static str,
    message: String,
}

impl ApiError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                code: self.code.to_string(),
                message: self.message,
            }),
        )
            .into_response()
    }
}

impl From<HostError> for ApiError {
    fn from(err: HostError) -> Self {
        let (status, code) = match &err {
            HostError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
            HostError::UnknownSession(_) => (StatusCode::NOT_FOUND, "unknown_session"),
            HostError::UnknownTask(_) => (StatusCode::NOT_FOUND, "unknown_task"),
            HostError::UnknownEntry(_) => (StatusCode::NOT_FOUND, "unknown_entry"),
            HostError::Conflict { .. } => (StatusCode::CONFLICT, "conflict"),
            HostError::Locked { .. } => (StatusCode::CONFLICT, "locked"),
            HostError::Unsupported(_) => (StatusCode::CONFLICT, "unsupported"),
            HostError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::warn!("the host failed internally: {err}");
        }
        Self {
            status,
            code,
            message: err.to_string(),
        }
    }
}

impl From<IdentityError> for ApiError {
    fn from(err: IdentityError) -> Self {
        let (status, code) = match &err {
            IdentityError::Forbidden(_) | IdentityError::Lookup(_) => {
                (StatusCode::FORBIDDEN, "forbidden")
            }
            // A bind refusal happens before the listener exists, so it
            // cannot reach a request. Answering 500 keeps the mapping total
            // without inventing a status for it.
            IdentityError::UnsafeBind(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        Self {
            status,
            code,
            // A rejected peer learns that it was rejected, not why: the
            // reason names the allowlist and the tailnet identity, which is
            // information for this host's log rather than for the caller.
            message: match status {
                StatusCode::FORBIDDEN => "this peer is not authorized".to_string(),
                _ => err.to_string(),
            },
        }
    }
}
