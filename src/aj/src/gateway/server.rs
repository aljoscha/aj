//! The HTTP surface of a gateway (spec 6.1, 6.7, 7.1).
//!
//! The session-facing routes are the ones a host serves, which is what makes a
//! gateway and a host indistinguishable to a client (spec section 4). Two things
//! are this layer's alone: the enrollment endpoints, and the proxy.
//!
//! The proxy is **one wildcard route, not a handler per command**. Everything
//! under `/v1/sessions/{id}/` travels to the owning host unread: the method, the
//! query, the body, and back the status and the response body. Only the id is
//! touched, and only to strip the namespace. That is what lets an older gateway
//! sit between a newer host and a newer client (spec 6.10's forward-don't-filter)
//! and it means a route a host gains needs no change here. What gets validated is
//! the namespace, never the route.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aj_wire::{ErrorResponse, Frame, SessionSummary};
use axum::body::{Body as AxumBody, Bytes};
use axum::extract::{ConnectInfo, FromRequest, Path, Query, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get};
use axum::{Json, Router, middleware};
use futures::Stream;
use serde::de::DeserializeOwned;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::gateway::directory::{DirectoryError, Route};
use crate::gateway::enrollment::EnrollHostRequest;
use crate::gateway::{Gateway, GatewayError};
use crate::remote::{IdentityError, IdentityGate};

/// How long [`GatewayServer::shutdown`] waits for in-flight streams.
///
/// Shutdown ends the streams itself, so this is the bound on a connection that
/// does not go away even once nothing is being written to it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The largest request body the proxy will carry.
///
/// Axum's own default request-body limit, which is what the host applies to the
/// very same body: the gateway must not be the stricter of the two, and it has
/// no reason to be the more generous one either.
const PROXY_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Why a gateway could not be served.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ServerError {
    /// The identity gate refuses to serve this address at all (spec 6.11). A
    /// gateway is remote code execution exactly as a host is.
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
    gateway: Gateway,
    gate: IdentityGate,
    /// How long a stream may be idle before a heartbeat frame. Carried here
    /// rather than read at the write site so a test can watch the behavior
    /// without a thirty-second wait.
    heartbeat: Duration,
    /// Cancelled by [`GatewayServer::shutdown`].
    ///
    /// A client's stream ends on it, which is what makes teardown prompt: unlike
    /// a host, a gateway has nothing else that would close an attached stream, so
    /// without this every shutdown would wait out [`SHUTDOWN_GRACE`] for a
    /// perfectly healthy client.
    shutdown: CancellationToken,
}

/// One bound gateway port.
pub(crate) struct GatewayServer {
    addr: SocketAddr,
    shutdown: CancellationToken,
    serving: JoinHandle<()>,
}

impl GatewayServer {
    /// Bind `addr` and start serving `gateway` behind `gate`.
    ///
    /// The gate validates the address before the socket exists, so a `local`
    /// gate on a public address refuses to start rather than serving
    /// unauthenticated. Returning means the listener is accepting, so a caller
    /// may dial [`Self::url`] immediately.
    pub(crate) async fn bind(
        gateway: Gateway,
        addr: SocketAddr,
        gate: IdentityGate,
    ) -> Result<Self, ServerError> {
        gate.validate_bind(addr)?;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| ServerError::Bind { addr, source })?;
        let local = listener
            .local_addr()
            .map_err(|source| ServerError::Bind { addr, source })?;
        let shutdown = CancellationToken::new();
        let state = Arc::new(ServerState {
            heartbeat: gateway.tuning().heartbeat,
            gateway,
            gate,
            shutdown: shutdown.clone(),
        });
        let app = router(Arc::clone(&state));
        let serving = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                let served = axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async move { shutdown.cancelled().await });
                if let Err(err) = served.await {
                    tracing::warn!("the gateway stopped serving: {err}");
                }
            }
        });
        Ok(Self {
            addr: local,
            shutdown,
            serving,
        })
    }

    /// The base URL a client dials.
    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop accepting and let in-flight streams finish.
    pub(crate) async fn shutdown(self) {
        self.shutdown.cancel();
        let abort = self.serving.abort_handle();
        if tokio::time::timeout(SHUTDOWN_GRACE, self.serving)
            .await
            .is_err()
        {
            tracing::warn!("giving up on gateway streams still open after {SHUTDOWN_GRACE:?}");
            abort.abort();
        }
    }
}

/// The gateway's routes: a host's session-facing surface, plus enrollment.
fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/v1/hello", get(hello))
        .route("/v1/events", get(events))
        .route("/v1/hosts", get(hosts).post(enroll))
        .route("/v1/hosts/{id}", delete(withdraw))
        .route("/v1/sessions", get(sessions).post(create_session))
        // Everything about one session goes to the host that owns it, whether or
        // not this build knows the route. `{id}` on its own is here for the same
        // reason: a route a newer host serves there is not this gateway's to
        // refuse.
        .route("/v1/sessions/{id}", any(proxy_session))
        .route("/v1/sessions/{id}/{*rest}", any(proxy_session_route))
        .fallback(unknown_endpoint)
        // Outside the routes rather than per handler, so an unauthorized peer
        // cannot even probe which endpoints exist.
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
    Json(state.gateway.hello()).into_response()
}

/// The merged directory (spec 7.1), which is the payload the `list` frames carry.
async fn sessions(State(state): State<Arc<ServerState>>) -> Response {
    Json(state.gateway.sessions()).into_response()
}

async fn hosts(State(state): State<Arc<ServerState>>) -> Response {
    Json(state.gateway.hosts()).into_response()
}

/// Enroll a host, answering the row it now has in `GET /v1/hosts`.
async fn enroll(
    State(state): State<Arc<ServerState>>,
    Body(request): Body<EnrollHostRequest>,
) -> Result<Response, ApiError> {
    let summary = state.gateway.enroll(&request.address).await?;
    Ok(Json(summary).into_response())
}

async fn withdraw(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    state.gateway.withdraw(&id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Creating a session through a gateway, which this build does not do.
///
/// TODO(aljoscha): a create has to name the host to create on, because a host
/// serves one working directory (spec 6.6 says so in passing). That parameter is
/// in neither section 7's gateway surface nor `CreateSessionRequest`, and
/// choosing a host here would be inventing a selection rule. Until the ruling
/// lands, the route refuses, deliberately and in one place.
async fn create_session() -> ApiError {
    ApiError::unsupported(
        "a gateway cannot create a session yet: a create has to name the host to create it \
         on, since a host serves one working directory. Create it on that host instead.",
    )
}

/// Open the gateway's event stream.
///
/// A client that attaches nothing gets the merged directory and heartbeats,
/// which is what a session list and a sidebar need. A client that attaches a
/// session is refused: forwarding a session's frames means splicing the owning
/// host's stream, and this build does not, so the alternative to refusing is a
/// stream that silently carries nothing for the session it was asked for.
async fn events(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<Vec<(String, String)>>,
) -> Result<Sse<impl Stream<Item = Result<Event, aj_agent::BoxError>>>, ApiError> {
    let attached = params.iter().filter(|(key, _)| key == "session").count();
    if attached > 0 {
        return Err(ApiError::unsupported(format!(
            "this gateway cannot attach a session yet ({attached} named): attach it on the \
             host that owns it, which every row of `GET /v1/sessions` names"
        )));
    }
    Ok(Sse::new(list_stream(
        state.gateway.subscribe(),
        state.heartbeat,
        state.shutdown.clone(),
    )))
}

async fn unknown_endpoint() -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        code: "unknown_endpoint",
        message: "no such endpoint on this gateway".to_string(),
    }
}

/// One `list` frame per change of the merged directory, plus a heartbeat
/// whenever the writer has been idle for `idle`.
struct ListWriter {
    directory: watch::Receiver<Arc<Vec<SessionSummary>>>,
    /// Whether the opening frame has been written.
    opened: bool,
}

impl ListWriter {
    /// The current directory, marked as seen so the next
    /// [`watch::Receiver::changed`] waits for one this stream has not been sent.
    fn snapshot(&mut self) -> Vec<SessionSummary> {
        self.directory.borrow_and_update().as_ref().clone()
    }
}

/// The stream of frames one attached client receives.
///
/// The current directory opens it, because a client that has just attached has
/// been sent nothing and would otherwise wait for a change to learn what is
/// there. After that only changes are written: `list` is cumulative, so a
/// snapshot identical to the last one carries no information (spec 6.8).
fn list_stream(
    directory: watch::Receiver<Arc<Vec<SessionSummary>>>,
    idle: Duration,
    shutdown: CancellationToken,
) -> impl Stream<Item = Result<Event, aj_agent::BoxError>> {
    let writer = ListWriter {
        directory,
        opened: false,
    };
    futures::stream::unfold(Some(writer), move |state| {
        let shutdown = shutdown.clone();
        async move {
            let mut writer = state?;
            let frame = if writer.opened {
                let change = tokio::select! {
                    _ = shutdown.cancelled() => return None,
                    change = tokio::time::timeout(idle, writer.directory.changed()) => change,
                };
                match change {
                    Ok(Ok(())) => Frame::List {
                        sessions: writer.snapshot(),
                    },
                    // The gateway is gone, so there is nothing left to say.
                    Ok(Err(_)) => return None,
                    Err(_) => Frame::Heartbeat,
                }
            } else {
                writer.opened = true;
                Frame::List {
                    sessions: writer.snapshot(),
                }
            };
            match serde_json::to_string(&frame) {
                Ok(json) => Some((Ok(Event::default().data(json)), Some(writer))),
                // A frame this gateway built that will not serialize is a bug in it.
                // Ending the stream makes the client reconnect, which is the only
                // honest answer.
                Err(err) => Some((Err(err.into()), None)),
            }
        }
    })
}

/// Forward everything about `/v1/sessions/{id}` to the host that owns it.
async fn proxy_session(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    forward(&state, &id, None, request).await
}

/// Forward everything under `/v1/sessions/{id}/` to the host that owns it.
async fn proxy_session_route(
    State(state): State<Arc<ServerState>>,
    Path((id, rest)): Path<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    forward(&state, &id, Some(&rest), request).await
}

async fn forward(
    state: &ServerState,
    id: &str,
    rest: Option<&str>,
    request: Request,
) -> Result<Response, ApiError> {
    let route = state.gateway.route(id)?;
    let mut url = upstream_url(&route, rest).ok_or_else(|| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "internal",
        message: format!("{} is not a base URL", route.address),
    })?;
    // Forwarded as it arrived: a parameter this build does not know is not this
    // gateway's to drop (spec 6.10).
    url.set_query(request.uri().query());
    let method = request.method().clone();
    // The only header with meaning in this protocol. Blanket forwarding would
    // drag hop-by-hop headers (`connection`, `transfer-encoding`) along, which
    // belong to the connection the gateway terminated rather than to the request.
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = axum::body::to_bytes(request.into_body(), PROXY_BODY_LIMIT)
        .await
        .map_err(|err| ApiError::invalid(format!("could not read the request body: {err}")))?;
    relay(
        state.gateway.http(),
        Upstream {
            method,
            url,
            content_type,
            body,
        },
        &route,
    )
    .await
}

/// One request as it goes upstream.
struct Upstream {
    method: Method,
    url: reqwest::Url,
    content_type: Option<String>,
    body: Bytes,
}

/// The upstream URL for a proxied request.
///
/// Built segment by segment rather than by concatenating strings, so every
/// segment is percent-encoded on the way out. That is what keeps a session id
/// carrying a path separator from turning into a different route on the host: the
/// host decodes it back into the id it was given and answers 404 for it, which is
/// its own grammar's business (spec 6.2).
///
/// Encoding does not save a dot segment, which a URL path drops rather than
/// escapes. `SessionAddress::parse` refuses those, so the id here cannot be one.
/// A dot segment in `rest` is dropped and can only shorten the path within this
/// session's own subtree, which names a route the client could have named
/// outright.
fn upstream_url(route: &Route, rest: Option<&str>) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(route.address.url()).ok()?;
    {
        let mut path = url.path_segments_mut().ok()?;
        path.pop_if_empty();
        path.push("v1").push("sessions").push(&route.session);
        for segment in rest.unwrap_or_default().split('/') {
            if !segment.is_empty() {
                path.push(segment);
            }
        }
    }
    Some(url)
}

/// Send `upstream` and answer with what came back.
///
/// The status and the body travel unchanged, which is the whole contract: a
/// client of a gateway has to be able to read a host's own refusal, code and all.
/// A transport failure becomes the gateway's own 503, because from the client's
/// side the host is simply not there.
async fn relay(
    http: &reqwest::Client,
    upstream: Upstream,
    route: &Route,
) -> Result<Response, ApiError> {
    let Upstream {
        method,
        url,
        content_type,
        body,
    } = upstream;
    let mut request = http.request(method, url);
    if let Some(content_type) = content_type {
        request = request.header(header::CONTENT_TYPE, content_type);
    }
    let response = match request.body(body).send().await {
        Ok(response) => response,
        Err(err) => return Err(ApiError::unreachable(&route.address.to_string(), err)),
    };
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let body = match response.bytes().await {
        Ok(body) => body,
        // The head arrived and the body did not, so the answer is as lost as if
        // nothing had arrived at all.
        Err(err) => return Err(ApiError::unreachable(&route.address.to_string(), err)),
    };
    let mut response = Response::new(AxumBody::from(body));
    *response.status_mut() = status;
    if let Some(content_type) = content_type {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
    }
    Ok(response)
}

/// A JSON request body with the protocol's error shape for a malformed one.
///
/// An absent or blank body reads as `{}`. The twin of the host's own extractor:
/// the two servers share no state, and spec 6.1's `{code, message}` is what both
/// of them owe a client.
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

/// An unsuccessful response: the status plus the stable `{code, message}` body
/// of spec 6.1.
struct ApiError {
    status: StatusCode,
    /// A stable snake_case token, so a client can branch on the reason without
    /// parsing prose.
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

    /// A well-formed request this gateway cannot serve, which is 409 in the
    /// protocol's vocabulary (spec 6.1) rather than 404: the endpoint is there,
    /// and the same request against a host may well succeed.
    fn unsupported(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "unsupported",
            message: message.into(),
        }
    }

    /// The 503 that only a gateway can answer (spec 6.1).
    fn unreachable(host: &str, cause: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "host_unreachable",
            message: format!("could not reach the host at {host}: {cause}"),
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

impl From<GatewayError> for ApiError {
    fn from(err: GatewayError) -> Self {
        let (status, code) = match &err {
            GatewayError::Address(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
            GatewayError::Unreachable { .. } => {
                (StatusCode::SERVICE_UNAVAILABLE, "host_unreachable")
            }
            GatewayError::Directory(err) => directory_status(err),
            // A gateway that cannot write down what it was told is not a client's
            // fault, and the enrollment did not stick.
            GatewayError::State(_) | GatewayError::Http(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::warn!("the gateway failed internally: {err}");
        }
        Self {
            status,
            code,
            message: err.to_string(),
        }
    }
}

/// The status vocabulary of spec 6.1 for a directory refusal.
fn directory_status(err: &DirectoryError) -> (StatusCode, &'static str) {
    match err {
        DirectoryError::AddressEnrolled { .. } => (StatusCode::CONFLICT, "already_enrolled"),
        DirectoryError::DuplicateHost { .. } => (StatusCode::CONFLICT, "duplicate_host"),
        DirectoryError::UnknownHost { .. } => (StatusCode::NOT_FOUND, "unknown_host"),
        DirectoryError::StaticHost { .. } => (StatusCode::CONFLICT, "static_host"),
        // An id no namespace can hold reads exactly like a session that is not
        // there, because to a client both are opaque ids that name nothing
        // (spec 6.2).
        DirectoryError::UnknownSession { .. } => (StatusCode::NOT_FOUND, "unknown_session"),
        DirectoryError::Unreachable { .. } => (StatusCode::SERVICE_UNAVAILABLE, "host_unreachable"),
        DirectoryError::UnusableHostId { .. } => (StatusCode::CONFLICT, "unusable_host_id"),
        // Neither of these can reach a request: only a link sees a host change
        // its id or an enrollment vanish under it. Mapping them anyway keeps this
        // total without inventing a status for a case a client cannot provoke.
        DirectoryError::IdChanged { .. } | DirectoryError::Withdrawn { .. } => {
            (StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    }
}

impl From<IdentityError> for ApiError {
    fn from(err: IdentityError) -> Self {
        let (status, code) = match &err {
            IdentityError::Forbidden(_) | IdentityError::Lookup(_) => {
                (StatusCode::FORBIDDEN, "forbidden")
            }
            // A bind refusal happens before the listener exists, so it cannot
            // reach a request.
            IdentityError::UnsafeBind(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        Self {
            status,
            code,
            // A rejected peer learns that it was rejected, not why.
            message: match status {
                StatusCode::FORBIDDEN => "this peer is not authorized".to_string(),
                _ => err.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::config::HostAddress;
    use crate::gateway::naming::SessionAddress;

    fn route(address: &str, session: &str) -> Route {
        Route {
            address: HostAddress::parse(address).expect("an address"),
            session: session.to_string(),
        }
    }

    #[test]
    fn an_upstream_url_de_namespaces_and_keeps_the_route() {
        let route = route("127.0.0.1:6161", "2026-08-06-10-15-30-123");
        assert_eq!(
            upstream_url(&route, Some("prompt"))
                .expect("a url")
                .as_str(),
            "http://127.0.0.1:6161/v1/sessions/2026-08-06-10-15-30-123/prompt",
        );
        assert_eq!(
            upstream_url(&route, Some("tasks/7/kill"))
                .expect("a url")
                .as_str(),
            "http://127.0.0.1:6161/v1/sessions/2026-08-06-10-15-30-123/tasks/7/kill",
        );
        assert_eq!(
            upstream_url(&route, None).expect("a url").as_str(),
            "http://127.0.0.1:6161/v1/sessions/2026-08-06-10-15-30-123",
        );
    }

    /// A base URL with a path prefix keeps it, which is what a host behind a
    /// reverse proxy needs.
    #[test]
    fn an_upstream_url_keeps_a_path_prefix() {
        assert_eq!(
            upstream_url(&route("https://host.example/aj", "s-1"), Some("queue"))
                .expect("a url")
                .as_str(),
            "https://host.example/aj/v1/sessions/s-1/queue",
        );
    }

    /// A session id the host's grammar would refuse still travels as one
    /// segment: escaping it is what keeps it from becoming a route of its own.
    #[test]
    fn a_session_id_is_one_escaped_segment() {
        let url = upstream_url(&route("127.0.0.1:6161", "../../etc/passwd"), Some("prompt"))
            .expect("a url");
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:6161/v1/sessions/..%2F..%2Fetc%2Fpasswd/prompt",
            "the host decodes this back into the id it was sent and 404s it",
        );
        assert_eq!(
            url.path_segments().expect("segments").count(),
            4,
            "still /v1/sessions/<id>/prompt",
        );
    }

    /// The one shape encoding does not save, which is why the namespace check
    /// refuses it before a URL is ever built from it.
    #[test]
    fn a_dot_segment_would_address_another_route() {
        assert_eq!(
            upstream_url(&route("127.0.0.1:6161", ".."), None)
                .expect("a url")
                .path(),
            "/v1/sessions",
            "which is the host's create route, hence SessionAddress::parse's refusal",
        );
        assert_eq!(
            SessionAddress::parse("host:.."),
            Err(crate::gateway::naming::AddressError::DotSession),
        );
    }
}
