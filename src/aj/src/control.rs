//! The transport boundary the interactive shell sits on.
//!
//! The frontend is a client of a session host either way.
//! [`Control`] is the seam where "in process" and "over HTTP" stop mattering:
//! one async surface covering exactly what the drive loop needs, with a
//! [`Stream`] of frames behind it. Everything above this module is written
//! once and runs in both modes.
//!
//! The vocabulary is the host's own: [`aj_app::host::Command`] is the one
//! command language, and the remote arm translates it into the wire requests
//! rather than the shell knowing two dialects. What the arms do
//! *not* share is how a loss reads: a remote stream reports the transport
//! failure behind it, an in-process one just ends, which is why
//! [`ControlFrame`] separates the two and [`ControlError`] keeps "the peer
//! refused this" apart from "the transport failed". The recovery is the same
//! either way, a re-attach with a cursor.

use std::collections::BTreeMap;
use std::time::Duration;

use aj_agent::events::AgentId;
use aj_agent::tool::TaskId;
use aj_app::host::{
    AttachRequest, Attachment, Command, CommandOutcome, CreateError, HeadTarget, HostError,
    QueueOp, SessionHost, SettingsAxis, SettingsChange,
};
use aj_app::session_setup::thinking_display_name;
use aj_models::types::UserContent;
use aj_models::{speed_name, thinking_config_name, verbosity_name};
use aj_wire::{
    AccountList, AccountRequest, ArchiveRequest, CancelRequest, CompactRequest,
    CreateSessionRequest, EnvRequest, Frame, HeadRequest, ModelSelection, PromptInput,
    PromptRequest, QueueOperation, QueueRequest, QueueState, SessionList, SessionSettings,
    SessionTree, SettingsRequest, SteerRequest, TagRequest, TaskDetails, TaskTable,
};
use futures::{FutureExt, StreamExt};
use reqwest::StatusCode;

use crate::remote::{RemoteClient, RemoteCommand, RemoteError, RemoteEvents, SILENCE};

/// Why a control operation did not do what was asked.
///
/// Its transparent arms preserve the source diagnostic. Presentation callers
/// choose whether a remote status wrapper is useful transport context or
/// should be stripped so a refusal reads the same locally and over the wire.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ControlError {
    // Transparent, so the wrapper adds no layer of its own to the diagnostic
    // or its error chain.
    #[error(transparent)]
    Host(#[from] HostError),
    #[error(transparent)]
    Remote(#[from] RemoteError),
    /// The peer created the session and could not apply everything the
    /// create asked for afterwards (see [`aj_app::host::PartialCreate`]).
    ///
    /// Apart from the refusals because the session exists: a caller opens it
    /// and says what did not stick rather than reporting a create that
    /// failed. Both arms report it the same way, the local one off the
    /// host's error and the remote one off the field the create response
    /// carries, so the wording is the host's either way.
    #[error("{message}")]
    PartialCreate { session: String, message: String },
    #[error("{0}")]
    Preview(String),
}

impl From<CreateError> for ControlError {
    fn from(err: CreateError) -> Self {
        match err {
            // A refused create is an ordinary host refusal and reads as one.
            CreateError::Refused(err) => Self::Host(err),
            CreateError::Incomplete(partial) => Self::PartialCreate {
                session: partial.session.clone(),
                message: partial.to_string(),
            },
        }
    }
}

impl ControlError {
    /// Whether the peer refused because its current state conflicts with the
    /// request: a turn in flight, background work live, a head switch that
    /// would strand it (the wire's 409).
    ///
    /// This is the one distinction a caller acts on rather than just displays,
    /// because a busy refusal has a local remedy to name (the chord that
    /// cancels the turn). Everything else, a transport failure included, is
    /// reported in the peer's own words.
    pub(crate) fn conflict(&self) -> bool {
        match self {
            Self::Host(HostError::Conflict { .. }) => true,
            // A create that minted its session is not a refusal at all, so
            // none of these predicates hold for it.
            Self::Host(_) | Self::PartialCreate { .. } | Self::Preview(_) => false,
            Self::Remote(err) => err.status() == Some(StatusCode::CONFLICT),
        }
    }

    /// Whether the peer does not know an entry the request named (the wire's
    /// 404 `unknown_entry`).
    pub(crate) fn unknown_entry(&self) -> bool {
        match self {
            Self::Host(HostError::UnknownEntry(_)) => true,
            Self::Host(_) | Self::PartialCreate { .. } | Self::Preview(_) => false,
            Self::Remote(err) => err.code() == Some("unknown_entry"),
        }
    }

    /// Whether the peer does not know the endpoint the request named (the
    /// wire's 404 `unknown_endpoint`).
    ///
    /// Told apart from an unknown entry because it says nothing about the
    /// session: the peer is older than the feature being asked for. A
    /// capability is self-description, never a gate, so probing an endpoint
    /// is a valid fallback check, which is what a caller reading this is
    /// doing. It is the fallback and not the first choice
    /// because the endpoint's capability string reaches a client only in the
    /// peer's own hello, and a gateway's hello cannot speak for the hosts
    /// behind it.
    pub(crate) fn unknown_endpoint(&self) -> bool {
        match self {
            // A host in this process has every endpoint this process knows.
            Self::Host(_) | Self::PartialCreate { .. } | Self::Preview(_) => false,
            Self::Remote(err) => err.code() == Some("unknown_endpoint"),
        }
    }

    /// Whether the peer refused the request as malformed (the wire's 400).
    ///
    /// Told apart from the other refusals because the host's message quotes
    /// the entry id it was given, which a user has never seen. A caller that
    /// knows what it asked for can say it in its own words.
    pub(crate) fn invalid(&self) -> bool {
        match self {
            Self::Host(HostError::Invalid(_)) => true,
            Self::Host(_) | Self::PartialCreate { .. } | Self::Preview(_) => false,
            Self::Remote(err) => err.status() == Some(StatusCode::BAD_REQUEST),
        }
    }

    /// Whether a create was refused for naming no host on a peer that serves
    /// several (the `ambiguous_host` code).
    ///
    /// Only a gateway answers this, and only to a client that did not say which
    /// host it meant, so a caller that cannot ask a user says how to name one
    /// instead of relaying a refusal with no remedy in it.
    pub(crate) fn ambiguous_host(&self) -> bool {
        match self {
            Self::Host(_) | Self::PartialCreate { .. } | Self::Preview(_) => false,
            Self::Remote(err) => err.code() == Some("ambiguous_host"),
        }
    }
}

/// Sessions per preview request. Small enough that the first rows of the
/// browser fill within one round trip, large enough that a store of hundreds
/// of sessions is a few dozen requests rather than one per row.
const PREVIEW_BATCH: usize = 16;

/// The host this frontend drives.
#[derive(Clone)]
pub(crate) enum Control {
    Local(LocalControl),
    Remote(RemoteControl),
}

/// The in-process host: this process owns the sessions it renders.
#[derive(Clone)]
pub(crate) struct LocalControl {
    host: SessionHost,
}

/// A host reached over the control port.
#[derive(Clone)]
pub(crate) struct RemoteControl {
    client: RemoteClient,
}

impl Control {
    pub(crate) fn local(host: SessionHost) -> Self {
        Self::Local(LocalControl { host })
    }

    pub(crate) fn remote(client: RemoteClient) -> Self {
        Self::Remote(RemoteControl { client })
    }

    /// The in-process host, `None` in connect mode.
    ///
    /// For the two things only a local run can do: serving a control port
    /// over the very host it renders, and the usage read behind the exit
    /// banner. Everything else goes through this type's own surface, so that
    /// both modes share one path.
    pub(crate) fn host(&self) -> Option<&SessionHost> {
        match self {
            Self::Local(local) => Some(&local.host),
            Self::Remote(_) => None,
        }
    }

    /// Whether this frontend is a remote client, which is what decides
    /// whether a gesture with no wire equivalent is refused and
    /// whether a re-attach that fails can be waited out (only this process's
    /// own host cannot be).
    pub(crate) fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    /// The url this host was dialed at, `None` for the in-process one.
    pub(crate) fn base_url(&self) -> Option<&str> {
        match self {
            Self::Local(_) => None,
            Self::Remote(remote) => Some(remote.client.base()),
        }
    }

    /// Apply one mutation to `session`.
    pub(crate) async fn command(
        &self,
        session: &str,
        command: Command,
    ) -> Result<CommandOutcome, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.command(session, command).await?),
            Self::Remote(remote) => Ok(remote
                .client
                .command(session, &wire_command(command))
                .await?),
        }
    }

    /// Export the host's complete log, not the client's attached transcript.
    pub(crate) async fn export_html(
        &self,
        session: &str,
    ) -> Result<aj_wire::SessionExport, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.export_html(session).await?),
            Self::Remote(remote) => Ok(remote.client.export_html(session).await?),
        }
    }

    /// Aggregate facts from the host's log, loading the session if needed.
    pub(crate) async fn session_info(
        &self,
        session: &str,
    ) -> Result<aj_session::SessionStats, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.session_info(session).await?),
            Self::Remote(remote) => Ok(aj_app::session_info::from_wire(
                remote.client.session_info(session).await?,
            )),
        }
    }

    /// User-paced prompt history. Local snapshots are coalesced while the UI is
    /// busy, so scanning cannot queue an unbounded number of full lists.
    pub(crate) async fn prompt_history(
        &self,
        session: Option<&str>,
        updates: Option<tokio::sync::watch::Sender<aj_wire::PromptHistory>>,
    ) -> Result<aj_wire::PromptHistory, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.prompt_history(session, updates).await?),
            Self::Remote(remote) => Ok(remote.client.prompt_history(session).await?),
        }
    }

    /// The session's branch tree, with its current head.
    pub(crate) async fn tree(&self, session: &str) -> Result<SessionTree, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.tree(session).await?),
            Self::Remote(remote) => Ok(remote.client.tree(session).await?),
        }
    }

    /// The session overlay at the current head or before a named message.
    /// Reads only when requested and never moves the head.
    pub(crate) async fn environment(
        &self,
        session: &str,
        before: Option<&str>,
    ) -> Result<BTreeMap<String, String>, ControlError> {
        match self {
            Self::Local(local) => Ok(match before {
                Some(message) => local.host.environment_before(session, message).await?,
                None => local.host.environment(session).await?,
            }),
            Self::Remote(remote) => Ok(remote.client.environment(session, before).await?),
        }
    }

    pub(crate) async fn tasks(&self, session: &str) -> Result<TaskTable, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.tasks(session).await?),
            Self::Remote(remote) => Ok(remote.client.tasks(session).await?),
        }
    }

    /// The host's non-secret account choices for a provider and this session's pin.
    pub(crate) async fn accounts(
        &self,
        session: &str,
        provider: Option<&str>,
    ) -> Result<AccountList, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.accounts(session, provider).await?),
            Self::Remote(remote) => Ok(remote.client.accounts(session, provider).await?),
        }
    }

    pub(crate) async fn queue(&self, session: &str) -> Result<QueueState, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.queue(session).await?),
            Self::Remote(remote) => Ok(remote.client.queue(session).await?),
        }
    }

    /// One task's detailed output, which is what backs the task-output
    /// overlay: the spill file on the host's disk is not reachable remotely.
    pub(crate) async fn task_details(
        &self,
        session: &str,
        task: TaskId,
    ) -> Result<TaskDetails, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.task(session, task).await?),
            Self::Remote(remote) => Ok(remote.client.task(session, task).await?),
        }
    }

    pub(crate) async fn sessions(&self) -> Result<SessionList, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.sessions().await?),
            Self::Remote(remote) => Ok(remote.client.sessions().await?),
        }
    }

    /// Read every available preview on request. Batches arrive independently,
    /// errors do not discard healthy rows, and receiver closure cancels work.
    pub(crate) async fn session_previews(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<
            Result<Vec<aj_session::SessionPreview>, ControlError>,
        >,
    ) {
        let directory = tokio::select! {
            biased;
            _ = tx.closed() => return,
            result = self.sessions() => match result {
                Ok(directory) => directory,
                Err(err) => { let _ = tx.send(Err(err)); return; }
            },
        };
        // Start in directory order so the first rows fill first. A few batches
        // in flight let healthy hosts make progress beside a slow one.
        let ids: Vec<String> = directory.sessions.into_iter().map(|row| row.id).collect();
        let batches: Vec<Vec<String>> = ids.chunks(PREVIEW_BATCH).map(<[String]>::to_vec).collect();
        let mut reads = futures::stream::iter(batches)
            .map(|batch| async move {
                match self {
                    Self::Local(local) => local
                        .host
                        .session_previews_for(&batch)
                        .await
                        .map(|previews| (previews, Vec::new()))
                        .map_err(ControlError::from),
                    Self::Remote(remote) => remote
                        .client
                        .session_previews(&batch)
                        .await
                        .map(|answer| {
                            (
                                answer
                                    .previews
                                    .into_iter()
                                    .map(aj_app::session_preview::from_wire)
                                    .collect(),
                                answer.incomplete,
                            )
                        })
                        .map_err(ControlError::from),
                }
            })
            .buffer_unordered(4);
        let mut reported = std::collections::HashSet::new();
        loop {
            let next = tokio::select! {
                biased;
                _ = tx.closed() => return,
                next = reads.next() => next,
            };
            let Some(result) = next else {
                break;
            };
            let sent = match result {
                Ok((previews, incomplete)) => {
                    let mut sent = tx.send(Ok(previews));
                    for failure in incomplete {
                        if sent.is_ok() && reported.insert(failure.host.clone()) {
                            sent = tx.send(Err(ControlError::Preview(format!(
                                "previews from {}: {}",
                                failure.host, failure.message
                            ))));
                        }
                    }
                    sent
                }
                Err(err) => {
                    // A refusal applies to the remaining batches too. Dropping
                    // the stream cancels every in-flight request future.
                    let message = if err.unknown_endpoint() {
                        "previews: host does not support session previews (upgrade the host)"
                            .to_string()
                    } else {
                        format!("previews: {err}")
                    };
                    let _ = tx.send(Err(ControlError::Preview(message)));
                    return;
                }
            };
            if sent.is_err() {
                return;
            }
        }
    }

    /// Create a session with the creator's settings, an optional first prompt
    /// and an optional tag, answering its id. Per-session settings follow
    /// whoever creates the session.
    ///
    /// `host` names which of the peer's hosts the session is for. `None` leaves
    /// that to the peer, which is what an absent host field means on the wire:
    /// the one working directory a plain host serves, or the sole host of a
    /// gateway that has one. A gateway with a choice to make refuses rather
    /// than guessing, so a caller with a user to ask asks first.
    ///
    /// `tag` is expected to have been normalized already, which is what lets
    /// the local and the remote arm hand it on unchanged.
    ///
    /// A create whose session exists but whose tag or first prompt did not
    /// land answers [`ControlError::PartialCreate`], which carries the id: it
    /// is not a create that failed, and a caller that treats it as one
    /// strands the session it just made.
    pub(crate) async fn create(
        &self,
        host: Option<String>,
        settings: Option<SessionSettings>,
        prompt: Option<Vec<UserContent>>,
        tag: Option<String>,
        session_env: Option<BTreeMap<String, String>>,
    ) -> Result<String, ControlError> {
        match self {
            Self::Local(local) => {
                local.host.creates_here(host.as_deref())?;
                Ok(local
                    .host
                    .create_with(settings, prompt, tag, session_env)
                    .await?)
            }
            Self::Remote(remote) => {
                let created = remote
                    .client
                    .create_session(CreateSessionRequest {
                        host,
                        settings,
                        prompt: prompt.map(|content| PromptInput::Content { content }),
                        tag,
                        env: session_env,
                    })
                    .await?;
                match created.incomplete {
                    None => Ok(created.id),
                    Some(message) => Err(ControlError::PartialCreate {
                        session: created.id,
                        message,
                    }),
                }
            }
        }
    }

    /// Open one frame stream covering every session in `requests`, each
    /// offering its own cursor.
    ///
    /// One stream per client, not one per session: the ordering guarantees are
    /// per stream, and changing the attach set means reopening it.
    /// The attach is all-or-nothing, so a refusal for any session leaves the
    /// caller with the stream it already had.
    ///
    /// Every session on the stream has to be armed before reading, each against
    /// what the peer reports it attached (see [`Stream::attached`]).
    pub(crate) async fn attach_all(
        &self,
        requests: &[AttachRequest],
    ) -> Result<Stream, ControlError> {
        match self {
            Self::Local(local) => Ok(Stream::Local(local.host.attach(requests).await?)),
            Self::Remote(remote) => Ok(Stream::Remote {
                events: remote.client.events(requests).await?,
                lost: None,
                attached: requests
                    .iter()
                    .map(|request| request.session.clone())
                    .collect(),
            }),
        }
    }
}

/// Translate a host command into the wire request that carries it (spec
/// 6.6).
///
/// A model change travels as the `(api, url, name)` triple rather than as a
/// catalog object: the host resolves it against its own catalog and
/// credentials, so a client never hands a peer a model row it made up.
fn wire_command(command: Command) -> RemoteCommand {
    match command {
        Command::Prompt { agent, content } => RemoteCommand::Prompt(PromptRequest {
            agent: agent_target(agent),
            input: PromptInput::Content { content },
        }),
        Command::Steer { agent, text } => RemoteCommand::Steer(SteerRequest {
            text,
            agent: agent_target(agent),
        }),
        Command::Cancel { agent } => RemoteCommand::Cancel(CancelRequest {
            agent: agent_target(agent),
        }),
        Command::Queue(QueueOp::Remove { agent }) => RemoteCommand::Queue(QueueRequest {
            op: QueueOperation::Remove,
            agent: agent_target(agent),
        }),
        Command::Queue(QueueOp::Clear) => RemoteCommand::Queue(QueueRequest {
            op: QueueOperation::Clear,
            agent: None,
        }),
        Command::Compact { instructions } => {
            RemoteCommand::Compact(CompactRequest { instructions })
        }
        Command::Env { key, value } => RemoteCommand::Env(EnvRequest { key, value }),
        Command::Account { provider, account } => {
            RemoteCommand::Account(AccountRequest { provider, account })
        }
        Command::Settings(change) => RemoteCommand::Settings(settings_request(change)),
        // A cleared tag travels as the empty string, which is what the route
        // reads as "clear".
        Command::Tag { tag } => RemoteCommand::Tag(TagRequest {
            tag: tag.unwrap_or_default(),
        }),
        Command::Archive { archived } => RemoteCommand::Archive(ArchiveRequest { archived }),
        Command::Head { target, changes } => {
            let mut request = head_request(target);
            request.changes = changes;
            RemoteCommand::Head(request)
        }
        Command::KillTask { task } => RemoteCommand::KillTask(task),
    }
}

fn head_request(target: HeadTarget) -> HeadRequest {
    match target {
        HeadTarget::Entry(entry) => HeadRequest::entry(entry),
        HeadTarget::Before(entry) => HeadRequest::before(entry),
    }
}

/// The wire form of a settings change.
///
/// Persistence is deliberately dropped: the wire has no persist axis, since
/// the config files a host would write are the host's own. The caller says
/// so in its notice rather than silently pretending the default moved.
pub(crate) fn settings_request(change: SettingsChange) -> SettingsRequest {
    let SettingsChange { agent, axis, .. } = change;
    let mut wire = SessionSettings::default();
    match axis {
        SettingsAxis::Model(info) => {
            // The triple, never the catalog row: the host resolves
            // `(api, name)` against its own catalog and credentials. The url
            // slot stays empty because it is an *override*, and echoing this
            // client's catalog default back would silently repoint the host at
            // whatever endpoint our own `models.json` happens to name. An
            // explicit endpoint override belongs to the session's creator and
            // rides the create request instead.
            wire.model = Some(ModelSelection {
                api: info.provider.clone(),
                url: None,
                name: info.id.clone(),
            });
        }
        SettingsAxis::Thinking(level) => {
            // The wire vocabulary is the log's and the `state` frame's, so the
            // level travels by its canonical name and the host validates it
            // against the model it actually holds.
            wire.thinking = Some(thinking_config_name(level.as_ref()).to_string());
        }
        SettingsAxis::ThinkingDisplay(display) => {
            wire.thinking_display = Some(thinking_display_name(display).to_string());
        }
        SettingsAxis::Speed(speed) => wire.speed = Some(speed_name(speed).to_string()),
        SettingsAxis::Verbosity(verbosity) => {
            let unified = verbosity.map(aj_app::model::config_verbosity_to_unified);
            wire.verbosity = Some(verbosity_name(unified).to_string());
        }
    }
    SettingsRequest {
        agent: agent_target(agent),
        change: wire,
    }
}

/// The wire's agent target: absent for the main agent, which is the default
/// every request omits.
fn agent_target(agent: AgentId) -> Option<AgentId> {
    match agent {
        AgentId::Main => None,
        AgentId::Sub(_) => Some(agent),
    }
}

/// One session's frame stream, whichever transport carries it.
pub(crate) enum Stream {
    Local(Attachment),
    Remote {
        events: RemoteEvents,
        /// A failure `try_recv` saw. The next [`Stream::recv`] reports it, so
        /// a drain never swallows a lost stream.
        lost: Option<RemoteError>,
        /// The sessions this stream was opened for, which is what
        /// [`Stream::attached`] answers from.
        ///
        /// The request, because the protocol gives a client no per-session
        /// answer at attach time. A stream request never fails wholesale over
        /// one bad session, and what becomes of each named session
        /// arrives afterwards, in one of three shapes: its attach block, a
        /// session-scoped `error` frame, or, for a session on a host a gateway
        /// cannot currently reach, nothing at all beyond the `unreachable` mark
        /// on its `list` row.
        ///
        /// So this says which sessions the peer was asked about and nothing
        /// about which it will serve. Whoever folds a block owes it a deadline.
        attached: Vec<String>,
    },
}

/// What a receive step yielded.
pub(crate) enum ControlFrame {
    Frame(Frame),
    /// The stream failed and the client owes a re-attach.
    Lost(ControlError),
    /// The stream ended with no failure behind it.
    ///
    /// What an in-process stream reports for every loss, since the host does
    /// not word them: the host going away, and reliable-frame overflow
    /// evicting a shell that stopped draining. The re-attach tells
    /// those apart, because a host that is gone refuses it.
    Closed,
}

impl Stream {
    /// Whether the peer reports it will serve `session`'s attach block, which
    /// is what a client arms its fold from.
    ///
    /// False for a session this stream does not carry, so a caller holding
    /// several streams can ask any of them about any session.
    pub(crate) fn attached(&self, session: &str) -> bool {
        let names = match self {
            Self::Local(attachment) => attachment.attached(),
            Self::Remote { attached, .. } => attached.as_slice(),
        };
        names.iter().any(|name| name == session)
    }

    /// How long this stream may be silent before it counts as dead.
    ///
    /// A connection answers the tolerance it was built with, which is what lets
    /// a caller tune it ([`RemoteClient::with_silence`]). An in-process stream
    /// has no transport to fall silent and answers the same span, so that a
    /// caller has one number to reach for in either mode.
    ///
    /// The protocol scopes this to the stream: two missed heartbeats, and
    /// heartbeats are host-level. So a caller bounding a wait for one *session* is
    /// borrowing a scale rather than reading a budget the protocol defines for
    /// it, and inherits a minute of patience by doing so. Whether that is the
    /// right patience for a session-scoped wait is a live question, see the
    /// beads behind [`crate::interactive`]'s catch-up.
    pub(crate) fn silence(&self) -> Duration {
        match self {
            Self::Local(_) => SILENCE,
            Self::Remote { events, .. } => events.silence(),
        }
    }

    /// The next frame, awaiting one.
    pub(crate) async fn recv(&mut self) -> ControlFrame {
        match self {
            Self::Local(attachment) => match attachment.recv().await {
                Some(frame) => ControlFrame::Frame(frame),
                None => ControlFrame::Closed,
            },
            Self::Remote { events, lost, .. } => {
                if let Some(err) = lost.take() {
                    return ControlFrame::Lost(err.into());
                }
                match events.recv().await {
                    Some(Ok(frame)) => ControlFrame::Frame(frame),
                    Some(Err(err)) => ControlFrame::Lost(err.into()),
                    // A remote stream that ends cleanly is still a connection
                    // this client no longer has, and the recovery is the same
                    // re-attach, so it is reported as a loss rather than as a
                    // shutdown.
                    None => ControlFrame::Lost(
                        RemoteError::Stream("the host closed the event stream".to_string()).into(),
                    ),
                }
            }
        }
    }

    /// The next frame if one is already buffered, for the drive loop's
    /// per-iteration drain.
    ///
    /// A failure is not returned here: it is held for the next [`Self::recv`],
    /// which is the one place the loop reacts to a lost stream.
    pub(crate) fn try_recv(&mut self) -> Option<Frame> {
        match self {
            Self::Local(attachment) => attachment.try_recv(),
            Self::Remote { events, lost, .. } => {
                if lost.is_some() {
                    return None;
                }
                // `now_or_never` is the try-form of the stream: polling the
                // event source once either yields a decoded frame or leaves it
                // exactly where it was. Nothing is buffered inside the dropped
                // future, so no frame can be lost this way.
                match events.recv().now_or_never() {
                    Some(Some(Ok(frame))) => Some(frame),
                    Some(Some(Err(err))) => {
                        *lost = Some(err);
                        None
                    }
                    Some(None) => {
                        *lost = Some(RemoteError::Stream(
                            "the host closed the event stream".to_string(),
                        ));
                        None
                    }
                    None => None,
                }
            }
        }
    }

    /// Mark a remote stream as failed, the state a transport error leaves it
    /// in, so a test can model a dropped connection without a real network
    /// fault. A local stream has no such state and is left alone.
    #[cfg(test)]
    pub(crate) fn cut(&mut self) {
        if let Self::Remote { lost, .. } = self {
            *lost = Some(RemoteError::Stream("the connection was cut".to_string()));
        }
    }
}

#[cfg(test)]
pub(crate) mod history_tests {
    use super::*;
    use crate::remote::tests::{HostHandles, addr, bounded, scripted, scripted_host};
    use crate::remote::{IdentityGate, RemoteServer};

    pub(crate) fn write_prompts(dir: &std::path::Path, name: &str, prompts: &[(&str, i64)]) {
        use std::io::Write;
        std::fs::create_dir_all(dir).unwrap();
        let mut file = std::fs::File::create(dir.join(format!("{name}.jsonl"))).unwrap();
        for (i, (text, time)) in prompts.iter().enumerate() {
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "id": i.to_string(), "thread": "user", "type": "message",
                    "timestamp": chrono::DateTime::from_timestamp_millis(*time).unwrap(),
                    "message": { "role": "user", "timestamp": time,
                        "content": [{"type": "text", "text": text}] }
                })
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn prompt_history_adapters_scope_rank_dedup_and_cap_without_materializing() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("sessions");
        // The newest prompt lives in the oldest-named file. Traversal-order
        // truncation or selecting a duplicate's first occurrence loses it.
        let text = format!("{}\nsearch-tail", "full prompt ".repeat(200));
        write_prompts(&store, "a", &[(&text, 9000), (" shared ", 8000)]);
        write_prompts(&store, "z", &[("shared", 1), ("workspace-only", 2)]);
        let other = dir.path().join("other-workspace");
        let bulk = (0..2100).map(|i| format!("other-{i}")).collect::<Vec<_>>();
        let mut prompts = bulk.iter().map(|s| (s.as_str(), 3)).collect::<Vec<_>>();
        prompts.push(("shared", 10000));
        write_prompts(&other, "z", &prompts);
        let host = scripted_host(
            &dir,
            scripted(vec![], 0, Duration::ZERO),
            HostHandles::new(&dir),
            None,
        );
        let server = RemoteServer::bind(host.clone(), addr("127.0.0.1:0"), IdentityGate::local())
            .await
            .unwrap();
        let client = RemoteClient::new(&server.url()).unwrap();
        assert!(
            client
                .hello()
                .await
                .unwrap()
                .capabilities
                .contains(&aj_wire::PROMPT_HISTORY_CAPABILITY.to_string())
        );
        let local = Control::local(host.clone());
        let remote = Control::remote(client);
        for session in [Some("a"), None] {
            let left = bounded("local history", local.prompt_history(session, None))
                .await
                .unwrap();
            let right = bounded("HTTP history", remote.prompt_history(session, None))
                .await
                .unwrap();
            assert_eq!(left, right);
            assert!(right.incomplete.is_empty());
            if session.is_some() {
                assert_eq!(
                    right
                        .prompts
                        .iter()
                        .map(|p| p.text.as_str())
                        .collect::<Vec<_>>(),
                    [&text, "shared", "workspace-only"]
                );
                assert!(right.prompts.iter().all(|p| p.project.is_none()));
            } else {
                assert_eq!(right.prompts.len(), aj_wire::PROMPT_HISTORY_LIMIT);
                assert_eq!(right.prompts[0].text, "shared");
                assert_eq!(right.prompts[0].project.as_deref(), Some("other-workspace"));
                assert_eq!(right.prompts[1].text, text);
                assert_eq!(
                    right.prompts.iter().filter(|p| p.text == "shared").count(),
                    1
                );
            }
        }
        assert!(
            host.sessions()
                .await
                .unwrap()
                .sessions
                .iter()
                .all(|row| !row.live)
        );
        assert!(local.prompt_history(Some("missing"), None).await.is_err());
        assert!(remote.prompt_history(Some("missing"), None).await.is_err());
        host.shutdown().await;
        server.shutdown().await;
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use crate::remote::tests::{HostHandles, addr, bounded, scripted, scripted_host};
    use crate::remote::{IdentityGate, RemoteServer};

    async fn collect(control: &Control) -> Vec<aj_wire::SessionPreview> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bounded("preview scan", control.session_previews(tx)).await;
        let mut rows = Vec::new();
        while let Some(batch) = rx.recv().await {
            rows.extend(
                batch
                    .expect("preview batch")
                    .iter()
                    .map(aj_app::session_preview::to_wire),
            );
        }
        rows.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        rows
    }

    #[tokio::test]
    async fn preview_adapters_read_archived_and_locked_cold_logs_without_materializing() {
        use aj_agent::message::AgentMessage;
        use aj_models::types::{Message, UserMessage};
        use aj_session::log::{ConversationEntryKind, ThreadKind};
        use aj_session::{ConversationLog, ConversationPersistence, SessionLock};

        let dir = tempfile::tempdir().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let text = format!("{} search-tail", "long prompt ".repeat(1000));
        let mut log = ConversationLog::create(&persistence).expect("log");
        let id = log.session_id().to_string();
        log.append(
            None,
            ThreadKind::User,
            None,
            ConversationEntryKind::Message {
                message: AgentMessage::wire(Message::User(UserMessage::text(&text))),
            },
        )
        .expect("message");
        drop(log);
        persistence.write_tag(&id, Some("label")).expect("tag");
        persistence.write_archived(&id, true).expect("archive");
        let _lock = SessionLock::try_acquire(&persistence, &id, "other-writer")
            .expect("lock")
            .expect("not held");
        let empty = ConversationLog::create(&persistence).expect("empty");
        // Empty files can exist in the store even though creation alone is lazy.
        std::fs::write(empty.path(), b"").expect("empty log file");
        drop(empty);
        let host = scripted_host(
            &dir,
            scripted(vec![], 0, Duration::ZERO),
            HostHandles::new(&dir),
            None,
        );
        let server = RemoteServer::bind(host.clone(), addr("127.0.0.1:0"), IdentityGate::local())
            .await
            .expect("server");
        let client = RemoteClient::new(&server.url()).expect("client");
        assert!(
            client
                .hello()
                .await
                .expect("hello")
                .capabilities
                .iter()
                .any(|c| c == aj_wire::SESSION_PREVIEWS_CAPABILITY)
        );
        let before = host.sessions().await.expect("directory");
        assert_eq!(before.sessions.len(), 2);
        assert!(before.sessions.iter().all(|row| !row.live));
        assert!(
            before
                .sessions
                .iter()
                .find(|row| row.id == id)
                .expect("row")
                .locked
        );
        let local = collect(&Control::local(host.clone())).await;
        let remote = collect(&Control::remote(client.clone())).await;
        assert_eq!(local, remote);
        assert_eq!(remote.len(), 2);
        let row = remote
            .iter()
            .find(|row| row.session_id == id)
            .expect("preview");
        assert_eq!(row.first_user_message.as_deref(), Some(text.as_str()));
        assert_eq!(row.message_count, 1);
        assert_eq!(row.tag.as_deref(), Some("label"));
        assert!(row.archived);
        assert!(
            host.sessions()
                .await
                .expect("directory")
                .sessions
                .iter()
                .all(|row| !row.live)
        );
        // Ids the store does not have, or could never hold, are left out of a
        // batch rather than refusing the rows beside them.
        let answer = client
            .session_previews(&["absent".to_string(), "bad/id".to_string(), id.clone()])
            .await
            .expect("batch");
        assert_eq!(answer.previews.len(), 1);
        assert_eq!(answer.previews[0].session_id, id);
        assert!(answer.incomplete.is_empty());
        host.shutdown().await;
        server.shutdown().await;
    }

    fn preview_json(id: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": id, "modified": "2026-01-01T00:00:00Z",
            "created_at": "2026-01-01T00:00:00Z", "last_message_at": "2026-01-01T00:00:00Z",
            "size_bytes": 0, "message_count": 0, "first_user_message": null,
            "tag": null, "archived": false
        })
    }

    fn directory_json(ids: &[&str]) -> serde_json::Value {
        serde_json::json!({"sessions": ids.iter().map(|id| serde_json::json!({
            "id": id, "live": false, "working": false,
            "queued": {"steering": 0, "follow_up": 0}, "tasks": 0,
            "last_activity": "2026-01-01T00:00:00Z"
        })).collect::<Vec<_>>()})
    }

    fn requested(params: &[(String, String)]) -> Vec<String> {
        params
            .iter()
            .filter(|(key, _)| key == "session")
            .map(|(_, id)| id.clone())
            .collect()
    }

    /// A batch that has answered reaches the browser while a later batch is
    /// still being read, and closing the browser stops the remaining reads.
    #[tokio::test]
    async fn preview_remote_emits_ready_batches_and_cancels_abandoned_reads() {
        use axum::{Json, Router, extract::Query, routing::get};
        let ids: Vec<String> = (0..PREVIEW_BATCH * 2).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let directory = directory_json(&refs);
        let (started, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new()
            .route("/v1/sessions", get(move || async move { Json(directory) }))
            .route(
                "/v1/previews",
                get(move |Query(params): Query<Vec<(String, String)>>| {
                    let started = started.clone();
                    async move {
                        let batch = requested(&params);
                        let _ = started.send(batch.clone());
                        // The second batch never answers.
                        if batch.first().map(String::as_str) != Some("s00") {
                            futures::future::pending::<()>().await;
                        }
                        Json(serde_json::json!({
                            "previews": batch.iter().map(|id| preview_json(id)).collect::<Vec<_>>(),
                            "incomplete": [],
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind(addr("127.0.0.1:0"))
            .await
            .expect("bind");
        let control = Control::remote(
            RemoteClient::new(&format!("http://{}", listener.local_addr().expect("addr")))
                .expect("client"),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let scan = tokio::spawn(async move {
            control.session_previews(tx).await;
        });
        let rows = bounded("first batch before the slow one completes", rx.recv())
            .await
            .expect("batch")
            .expect("previews");
        assert_eq!(rows.len(), PREVIEW_BATCH);
        assert_eq!(rows[0].session_id, "s00");
        let mut started = Vec::new();
        for _ in 0..2 {
            started.push(
                bounded("in-flight request", requests.recv())
                    .await
                    .expect("request"),
            );
        }
        assert!(started.iter().all(|batch| batch.len() == PREVIEW_BATCH));
        drop(rx);
        bounded("abandoned scan stops", scan)
            .await
            .expect("scan task");
        server.abort();
        let _ = server.await;
    }

    /// Through a gateway a batch answers for the hosts it could read and names
    /// the ones it could not. The client keeps the rows, reports each failing
    /// host once, and treats a host that lacks the endpoint as a refusal.
    #[tokio::test]
    async fn preview_remote_keeps_partial_batches_and_reports_each_host_once() {
        use axum::{Json, Router, extract::Query, response::IntoResponse, routing::get};
        let ids: Vec<String> = (0..PREVIEW_BATCH + 1)
            .map(|i| {
                let host = if i % 2 == 0 { "good" } else { "old" };
                format!("{host}/{i:02}")
            })
            .collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let directory = directory_json(&refs);
        let app = Router::new()
            .route("/v1/sessions", get(move || async move { Json(directory) }))
            .route(
                "/v1/previews",
                get(|Query(params): Query<Vec<(String, String)>>| async move {
                    let batch = requested(&params);
                    let previews: Vec<_> = batch
                        .iter()
                        .filter(|id| id.starts_with("good"))
                        // One good row is gone from the store.
                        .filter(|id| !id.ends_with("/00"))
                        .map(|id| preview_json(id))
                        .collect();
                    Json(serde_json::json!({
                        "previews": previews,
                        "incomplete": [{"host": "old", "message": "host does not support session previews (upgrade the host)"}],
                    }))
                    .into_response()
                }),
            );
        let listener = tokio::net::TcpListener::bind(addr("127.0.0.1:0"))
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let control = Control::remote(RemoteClient::new(&base).expect("client"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bounded("scan", control.session_previews(tx)).await;
        let mut rows = Vec::new();
        let mut errors = Vec::new();
        while let Some(batch) = rx.recv().await {
            match batch {
                Ok(batch) => rows.extend(batch),
                Err(err) => errors.push(err.to_string()),
            }
        }
        assert_eq!(rows.len(), (PREVIEW_BATCH + 1).div_ceil(2) - 1, "{rows:?}");
        assert!(rows.iter().all(|row| row.session_id.starts_with("good/")));
        assert_eq!(
            errors.len(),
            1,
            "two batches, one host named once: {errors:?}"
        );
        assert!(errors[0].contains("old") && errors[0].contains("upgrade"));

        // A peer without the endpoint at all is one refusal, not one per batch.
        let control =
            Control::remote(RemoteClient::new(&format!("{base}/nowhere")).expect("client"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bounded("scan", control.session_previews(tx)).await;
        let mut errors = Vec::new();
        while let Some(batch) = rx.recv().await {
            errors.push(batch.expect_err("no rows without a directory").to_string());
        }
        assert_eq!(errors.len(), 1, "{errors:?}");
        server.abort();
        let _ = server.await;
    }
}
