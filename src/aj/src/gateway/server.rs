//! The HTTP surface of a gateway (spec 6.1, 6.7, 7.1).
//!
//! The session-facing routes are the ones a host serves, which is what makes a
//! gateway and a host indistinguishable to a client (spec section 4). Two things
//! are this layer's alone: the enrollment endpoints, and the proxy.
//!
//! The proxy is **one wildcard route, not a handler per command**. Everything
//! under `/v1/sessions/{id}/` travels to the owning host unread: the method, the
//! query, the body, and back the status and the response body. What is touched is
//! the id, to strip the namespace, and the `session` an error body names, to put
//! it back (spec 6.6). That is what lets an older gateway sit between a newer host
//! and a newer client (spec 6.10's forward-don't-filter) and it means a
//! request/response route a host gains needs no change here. What gets validated
//! is the namespace, never the route.
//!
//! Request and response, and not a stream: a proxied request is bounded end to
//! end by `UPSTREAM_TIMEOUT` and its answer is read whole before any of it is
//! written back. That is right for the routes this carries, which answer promptly
//! by contract (a command is accepted, not awaited), and it is why the one route
//! that stays open is spliced rather than proxied
//! ([`crate::gateway::splice`]).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aj_app::host::AttachRequest;
use aj_wire::{Cursor, EnrollHostRequest, ErrorResponse};
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
use serde_json::value::RawValue;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::gateway::config::HostAddress;
use crate::gateway::directory::{DirectoryError, HostTarget, Route};
use crate::gateway::naming::SessionAddress;
use crate::gateway::splice::{Outgoing, Splice};
use crate::gateway::{Gateway, GatewayError};
use crate::remote::{IdentityError, IdentityGate};

/// How long [`GatewayServer::shutdown`] waits for in-flight connections.
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
    ///
    /// A client that stopped reading is not told at all: its stream is polled
    /// only when hyper can write to it, so it observes nothing and its
    /// connection outlives the grace. What the token still reaches is the splice
    /// behind it, whose own token is a child of this one, so the upstream streams
    /// and the subscribers they hold on hosts end either way.
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
    ///
    /// The grace is a bound on a connection that does not go away even once
    /// nothing is being written to it, which is the shape of a client that
    /// stopped reading: it cannot be told, and the abort ends the accept loop
    /// rather than the connections it spawned, so it lasts as long as the
    /// process. Its upstreams are gone before the grace begins (see
    /// [`ServerState::shutdown`]), which is what the wait was protecting.
    pub(crate) async fn shutdown(self) {
        self.shutdown.cancel();
        let abort = self.serving.abort_handle();
        if tokio::time::timeout(SHUTDOWN_GRACE, self.serving)
            .await
            .is_err()
        {
            tracing::warn!(
                "giving up on gateway connections still open after {SHUTDOWN_GRACE:?}: \
                 the upstreams behind them are already released"
            );
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

/// Create a session on the host it is for (spec 6.6).
///
/// The target is resolved here and nowhere else: the host the body names, or
/// the sole enrolled one when it names none. What travels upstream is the
/// client's own body with `host` set to that host's own id, because a create
/// names its target in the vocabulary of the server that answers it, and what
/// comes back has its session id namespaced for the same reason. Everything
/// else in both directions is carried unread, a field this build does not know
/// included (spec 6.10).
async fn create_session(
    State(state): State<Arc<ServerState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let content_type = forwarded_content_type(&request);
    let mut body = create_body(read_body(request).await?)?;
    let target = state.gateway.create_target(named_host(&body)?.as_deref())?;
    set_string_field(&mut body, HOST_FIELD, &target.host_id);
    let url = sessions_url(&target.address).ok_or_else(|| not_a_base_url(&target.address))?;
    let body = serde_json::to_vec(&body).map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "internal",
        message: format!(
            "could not re-encode the create for {}: {err}",
            target.address
        ),
    })?;
    let answer = send(
        state.gateway.http(),
        Upstream {
            method: Method::POST,
            url,
            content_type,
            body: Bytes::from(body),
        },
        &target.address,
    )
    .await?;
    namespace_created(answer, &target)
}

/// Namespace the session id a host just minted (spec 6.2, 6.6).
///
/// Only a create that happened names a session. Anything else is the host's own
/// answer travelling back untouched, code and all: a refused create minted
/// nothing, and there is no id to rewrite.
fn namespace_created(answer: Answer, target: &HostTarget) -> Result<Response, ApiError> {
    if !answer.status.is_success() {
        return Ok(answer.into_response());
    }
    let unreadable = |reason: String| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "internal",
        message: format!(
            "the host at {} answered a create this gateway cannot namespace, so the session \
             it created is not addressable here: {reason}",
            target.address
        ),
    };
    let mut created: JsonObject =
        serde_json::from_slice(&answer.body).map_err(|err| unreadable(err.to_string()))?;
    let id = string_field(&created, CREATED_ID_FIELD)
        .map_err(|err| unreadable(err.to_string()))?
        .ok_or_else(|| unreadable(format!("it names no {CREATED_ID_FIELD}")))?;
    set_string_field(
        &mut created,
        CREATED_ID_FIELD,
        &SessionAddress::new(&target.host_id, &id).to_string(),
    );
    let body = serde_json::to_vec(&created).map_err(|err| unreadable(err.to_string()))?;
    Ok(Answer {
        body: Bytes::from(body),
        ..answer
    }
    .into_response())
}

/// Open the gateway's event stream (spec 6.5, 7.1).
///
/// Every session the request names is attached on the host that owns it, with
/// the client's own cursor, and its frames travel back with their ids namespaced
/// ([`crate::gateway::splice`]). A client that attaches nothing gets the merged
/// directory and heartbeats, which is what a session list and a sidebar need.
async fn events(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<Vec<(String, String)>>,
) -> Result<Sse<impl Stream<Item = Result<Event, aj_agent::BoxError>>>, Response> {
    let attach = attach_requests(&params).map_err(IntoResponse::into_response)?;
    let splice = state
        .gateway
        .splice(&attach, &state.shutdown)
        .await
        .map_err(refused)?;
    Ok(Sse::new(client_stream(
        splice,
        state.heartbeat,
        state.shutdown.clone(),
    )))
}

/// Why a client's stream could not be opened.
///
/// A refusal the owning host wrote travels back as that host wrote it, with the
/// session ids in it namespaced and nothing else touched, which is the path a
/// proxied refusal takes: the client asked this question and the host answered
/// it, so its own fields are what a capable client composes its wording from
/// (spec 6.6). A body this gateway cannot read that way, and everything that is
/// the gateway's own answer, goes through [`ApiError`].
fn refused(err: GatewayError) -> Response {
    if let GatewayError::AttachRefused {
        status,
        host_id,
        body,
        ..
    } = &err
        && let Some(body) = namespaced_error(body.as_bytes(), host_id)
    {
        return Answer {
            status: *status,
            content_type: Some(header::HeaderValue::from_static("application/json")),
            body,
        }
        .into_response();
    }
    ApiError::from(err).into_response()
}

/// Parse the stream's repeatable `session=<id>[@<epoch>:<seq>]` parameters
/// (spec 6.5).
///
/// The cursor is split off at the **first** `@`, exactly as a host does, so a
/// namespaced id and the cursor behind it read the same on both sides of a
/// gateway. Unknown parameters are ignored (spec 6.10), and attaching nothing is
/// legal.
///
/// The id itself is judged where it is resolved
/// ([`crate::gateway::directory::Directory::group`]): it is one namespace plus
/// one opaque half, and only the enrolled hosts say whether the namespace names
/// anything.
fn attach_requests(params: &[(String, String)]) -> Result<Vec<AttachRequest>, ApiError> {
    let mut requests = Vec::new();
    for (key, value) in params {
        if key != "session" {
            continue;
        }
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

async fn unknown_endpoint() -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        code: "unknown_endpoint",
        message: "no such endpoint on this gateway".to_string(),
    }
}

/// One SSE `data:` line per frame this client is owed (spec 6.1).
///
/// Which frames those are, and where they come from, is
/// [`Splice::next_frame`]'s: this only writes them. Dropping the response drops
/// the splice, which closes the upstream streams it opened.
fn client_stream(
    splice: Splice,
    idle: Duration,
    shutdown: CancellationToken,
) -> impl Stream<Item = Result<Event, aj_agent::BoxError>> {
    futures::stream::unfold(Some(splice), move |state| {
        let shutdown = shutdown.clone();
        async move {
            let mut splice = state?;
            let frame = splice.next_frame(idle, &shutdown).await?;
            let json = match &frame {
                // A frame from a host is re-serialized from the JSON it arrived
                // as, so a payload this build does not understand travels
                // verbatim (spec 6.10).
                Outgoing::Spliced(frame) => serde_json::to_string(frame),
                Outgoing::Own(frame) => serde_json::to_string(frame),
            };
            match json {
                Ok(json) => Some((Ok(Event::default().data(json)), Some(splice))),
                // A frame that will not serialize is a bug in this gateway, or a
                // host that sent JSON this one cannot write back. Ending the
                // stream makes the client reconnect, which is the only honest
                // answer: skipping the frame would silently drop a reliable one.
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
    let mut url = upstream_url(&route, rest).ok_or_else(|| not_a_base_url(&route.address))?;
    // Forwarded as it arrived: a parameter this build does not know is not this
    // gateway's to drop (spec 6.10).
    url.set_query(request.uri().query());
    let method = request.method().clone();
    let content_type = forwarded_content_type(&request);
    let body = read_body(request).await?;
    let mut answer = send(
        state.gateway.http(),
        Upstream {
            method,
            url,
            content_type,
            body,
        },
        &route.address,
    )
    .await?;
    // A refusal the host wrote names its session in the host's own vocabulary,
    // which no client of this gateway can address (spec 6.6).
    if !answer.status.is_success()
        && let Some(body) = namespaced_error(&answer.body, &route.host_id)
    {
        answer.body = body;
    }
    Ok(answer.into_response())
}

/// A host's error body under this gateway's own vocabulary, `None` when the body
/// is no JSON object at all (spec 6.6).
///
/// An error that references a session names it in a top-level `session` field,
/// and that is the one field of an error body a gateway owes anything to: the id
/// the host used is one no client here can address. Everything else travels as it
/// arrived, a field this build does not know included, which is the discipline
/// the create route follows in the other direction (spec 6.10).
///
/// The `None` is what separates a refusal this gateway can carry from one it can
/// only summarize: a proxy's HTML page, an empty body.
fn namespaced_error(body: &[u8], host_id: &str) -> Option<Bytes> {
    let mut object: JsonObject = serde_json::from_slice(body).ok()?;
    let Some(session) = string_field(&object, SESSION_FIELD).ok().flatten() else {
        // An envelope naming no session is already in every vocabulary.
        return Some(Bytes::copy_from_slice(body));
    };
    set_string_field(
        &mut object,
        SESSION_FIELD,
        &SessionAddress::new(host_id, &session).to_string(),
    );
    serde_json::to_vec(&object).ok().map(Bytes::from)
}

/// One request as it goes upstream.
struct Upstream {
    method: Method,
    url: reqwest::Url,
    content_type: Option<String>,
    body: Bytes,
}

/// The one request header with meaning in this protocol.
///
/// Blanket forwarding would drag hop-by-hop headers (`connection`,
/// `transfer-encoding`) along, which belong to the connection the gateway
/// terminated rather than to the request.
fn forwarded_content_type(request: &Request) -> Option<String> {
    request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// The request body, bounded by [`PROXY_BODY_LIMIT`].
async fn read_body(request: Request) -> Result<Bytes, ApiError> {
    axum::body::to_bytes(request.into_body(), PROXY_BODY_LIMIT)
        .await
        .map_err(|err| ApiError::invalid(format!("could not read the request body: {err}")))
}

/// An enrolled address that will not parse back into a URL, which only a
/// corrupt state file can produce: every address is parsed on the way in.
fn not_a_base_url(address: &HostAddress) -> ApiError {
    ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "internal",
        message: format!("{address} is not a base URL"),
    }
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
    let mut url = sessions_url(&route.address)?;
    {
        let mut path = url.path_segments_mut().ok()?;
        path.push(&route.session);
        for segment in rest.unwrap_or_default().split('/') {
            if !segment.is_empty() {
                path.push(segment);
            }
        }
    }
    Some(url)
}

/// The `/v1/sessions` route on a host: the create, and the root every session
/// URL extends.
fn sessions_url(address: &HostAddress) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(address.url()).ok()?;
    {
        let mut path = url.path_segments_mut().ok()?;
        path.pop_if_empty();
        path.push("v1").push("sessions");
    }
    Some(url)
}

/// What a host answered, before it becomes this gateway's response.
///
/// A step rather than a response, because the create route reads the body it
/// gets back and every other route does not.
struct Answer {
    status: StatusCode,
    content_type: Option<header::HeaderValue>,
    body: Bytes,
}

impl IntoResponse for Answer {
    /// The host's status and body, unchanged: a client of a gateway has to be
    /// able to read a host's own refusal, code and all.
    fn into_response(self) -> Response {
        let mut response = Response::new(AxumBody::from(self.body));
        *response.status_mut() = self.status;
        if let Some(content_type) = self.content_type {
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type);
        }
        response
    }
}

/// Send `upstream` to the host at `address` and read its whole answer.
///
/// A transport failure becomes the gateway's own 503, because from the client's
/// side the host is simply not there.
async fn send(
    http: &reqwest::Client,
    upstream: Upstream,
    address: &HostAddress,
) -> Result<Answer, ApiError> {
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
        Err(err) => return Err(ApiError::unreachable(&address.to_string(), err)),
    };
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let body = match response.bytes().await {
        Ok(body) => body,
        // The head arrived and the body did not, so the answer is as lost as if
        // nothing had arrived at all.
        Err(err) => return Err(ApiError::unreachable(&address.to_string(), err)),
    };
    Ok(Answer {
        status,
        content_type,
        body,
    })
}

/// The create body's target-host field, [`aj_wire::CreateSessionRequest::host`].
const HOST_FIELD: &str = "host";

/// The created answer's session id, [`aj_wire::SessionCreated::id`].
const CREATED_ID_FIELD: &str = "id";

/// The field an error body names a session in (spec 6.6), the same convention
/// frames use (spec 6.3).
const SESSION_FIELD: &str = "session";

/// A JSON object held as its top-level fields, every value left unparsed.
///
/// The create is the one route where a gateway edits a body rather than
/// carrying it: it sets the target `host` on the way up and namespaces the
/// minted `id` on the way back. Keeping every other value as text is what makes
/// those two the *only* changes, so a field this build does not know travels
/// with its number literals intact (spec 6.10's forward-don't-filter).
type JsonObject = BTreeMap<String, Box<RawValue>>;

/// A create body as its top-level fields, with a blank one reading as `{}`.
///
/// The same tolerance the host's own extractor applies, so a create sent with
/// no body at all reads the same through a gateway as it does against a host. A
/// body that is not a JSON object is malformed, and saying so here rather than
/// forwarding it keeps the refusal in one place.
fn create_body(bytes: Bytes) -> Result<JsonObject, ApiError> {
    if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(JsonObject::new());
    }
    serde_json::from_slice(&bytes)
        .map_err(|err| ApiError::invalid(format!("malformed request body: {err}")))
}

/// The host a create names, `None` when it names none (spec 6.6).
///
/// A `null` reads as naming none, which is how the typed body decodes it. A
/// value that is not a string is a malformed request rather than a host nobody
/// has: inventing an id from it would refuse the create for the wrong reason.
fn named_host(body: &JsonObject) -> Result<Option<String>, ApiError> {
    string_field(body, HOST_FIELD).map_err(|err| {
        ApiError::invalid(format!(
            "the create's {HOST_FIELD} field names an enrolled host: {err}"
        ))
    })
}

/// The value of `key` as a string, `None` when the key is absent or `null`.
fn string_field(object: &JsonObject, key: &str) -> Result<Option<String>, serde_json::Error> {
    match object.get(key) {
        None => Ok(None),
        Some(raw) => serde_json::from_str(raw.get()),
    }
}

/// Set `key` to a string, replacing whatever was there.
fn set_string_field(object: &mut JsonObject, key: &str, value: &str) {
    // A string always encodes, so the only way this fails is a serializer bug.
    let value = serde_json::value::to_raw_value(value).expect("a string encodes as JSON");
    object.insert(key.to_string(), value);
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
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    /// A stable snake_case token, so a client can branch on the reason without
    /// parsing prose. This gateway's own vocabulary, always: a token an owning
    /// host coined travels inside that host's own body (see [`refused`]) rather
    /// than through here.
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
            // A refusal whose body is not an envelope at all: an HTML page from
            // something in front of the host, nothing. The gateway names it
            // itself, because the host named nothing, and carries the host's own
            // words. A body that *is* an envelope never reaches here (see
            // [`refused`]), because every field of one is the host's to keep
            // (spec 6.6).
            GatewayError::AttachRefused {
                status, message, ..
            } => {
                return Self {
                    status: *status,
                    code: "host_refused",
                    message: message.clone(),
                };
            }
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
        // Both are well-formed creates this gateway cannot serve as it stands,
        // which is 409 rather than 400: the client's body is fine, and naming
        // an enrolled host (or enrolling one) makes the same request work.
        DirectoryError::AmbiguousHost { .. } => (StatusCode::CONFLICT, "ambiguous_host"),
        DirectoryError::NoHostEnrolled => (StatusCode::CONFLICT, "no_host_enrolled"),
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
            host_id: "left".to_string(),
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

    /// A create body is read for its `host` and for nothing else, and what goes
    /// upstream is the client's own body with that one field set (spec 6.6).
    #[test]
    fn a_create_body_is_read_for_its_host_alone() {
        for (raw, named) in [
            // A blank body is `{}`, as it is on a host.
            ("", None),
            ("  ", None),
            ("{}", None),
            (r#"{"host":null}"#, None),
            (r#"{"tag":"fix-auth"}"#, None),
            (r#"{"host":"left"}"#, Some("left")),
        ] {
            let body = create_body(Bytes::from_static(raw.as_bytes()))
                .unwrap_or_else(|err| panic!("{raw:?} is a create body: {}", err.message));
            assert_eq!(
                named_host(&body).expect("a host or none").as_deref(),
                named,
                "{raw:?}",
            );
        }

        // A host that is not a string is a malformed request rather than a host
        // nobody has: refusing it as unknown would name the wrong problem.
        let body = create_body(Bytes::from_static(br#"{"host":5}"#)).expect("an object");
        assert_eq!(
            named_host(&body).expect_err("5 names no host").status,
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(
            create_body(Bytes::from_static(b"[1]"))
                .expect_err("a create body is an object")
                .status,
            StatusCode::BAD_REQUEST,
        );

        let mut body = create_body(Bytes::from_static(
            br#"{"tag":"fix-auth","added_later":{"n":18446744073709551616}}"#,
        ))
        .expect("an object");
        set_string_field(&mut body, HOST_FIELD, "left");
        let forwarded = serde_json::to_string(&body).expect("it re-encodes");
        assert!(forwarded.contains(r#""host":"left""#), "{forwarded}");
        assert!(
            forwarded.contains("18446744073709551616"),
            "a field this gateway does not know keeps its own number literal: {forwarded}",
        );
    }

    /// The id a host minted is namespaced on the way out, and anything that is
    /// not a created session travels back as the host wrote it (spec 6.6).
    #[tokio::test]
    async fn a_created_answer_is_namespaced_and_a_refusal_is_forwarded() {
        let target = HostTarget {
            address: HostAddress::parse("127.0.0.1:6161").expect("an address"),
            host_id: "left".to_string(),
        };
        let answer = |status: StatusCode, body: &'static str| Answer {
            status,
            content_type: None,
            body: Bytes::from_static(body.as_bytes()),
        };

        let created = namespace_created(
            answer(
                StatusCode::OK,
                r#"{"id":"s-1","incomplete":"tag not applied"}"#,
            ),
            &target,
        )
        .expect("a created session");
        assert_eq!(created.status(), StatusCode::OK);
        let body = body_json(created).await;
        assert_eq!(body["id"], serde_json::json!("left:s-1"));
        assert_eq!(
            body["incomplete"],
            serde_json::json!("tag not applied"),
            "and the rest of the host's answer is untouched",
        );

        let refused = namespace_created(
            answer(
                StatusCode::CONFLICT,
                r#"{"code":"unsupported","message":"no"}"#,
            ),
            &target,
        )
        .expect("a refusal is an answer too");
        assert_eq!(refused.status(), StatusCode::CONFLICT);
        assert_eq!(
            body_json(refused).await["code"],
            serde_json::json!("unsupported"),
            "a create that minted nothing names no session to rewrite",
        );

        // A success this gateway cannot namespace is reported, never guessed
        // at: the session may exist on the host and is not addressable here.
        let err = namespace_created(answer(StatusCode::OK, r#"{"session":"s-1"}"#), &target)
            .expect_err("a created answer with no id");
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), PROXY_BODY_LIMIT)
            .await
            .expect("a response body");
        serde_json::from_slice(&body).expect("a JSON body")
    }
}
