//! The transport boundary the interactive shell sits on.
//!
//! The frontend is a client of a session host either way (spec section 5).
//! [`Control`] is the seam where "in process" and "over HTTP" stop mattering:
//! one async surface covering exactly what the drive loop needs, with a
//! [`Stream`] of frames behind it. Everything above this module is written
//! once and runs in both modes.
//!
//! The vocabulary is the host's own: [`aj_app::host::Command`] is the one
//! command language, and the remote arm translates it into the wire requests
//! of spec 6.6 rather than the shell knowing two dialects. What the arms do
//! *not* share is how a loss reads: a remote stream reports the transport
//! failure behind it, an in-process one just ends, which is why
//! [`ControlFrame`] separates the two and [`ControlError`] keeps "the peer
//! refused this" apart from "the transport failed". The recovery is the same
//! either way, a re-attach with a cursor (spec 6.5).

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
    ArchiveRequest, CancelRequest, CompactRequest, CreateSessionRequest, Frame, HeadRequest,
    ModelSelection, PromptInput, PromptRequest, QueueOperation, QueueRequest, QueueState,
    SessionList, SessionSettings, SessionTree, SettingsRequest, SteerRequest, TagRequest,
    TaskDetails, TaskTable,
};
use futures::FutureExt;
use reqwest::StatusCode;

use crate::remote::{RemoteClient, RemoteCommand, RemoteError, RemoteEvents};

/// Why a control operation did not do what was asked.
///
/// The `Display` of either arm is user-facing: it is what gets folded into
/// the transcript as a notice, which is what makes the same refusal read the
/// same locally and over the wire (the host words its own refusals, spec
/// 6.6).
#[derive(Debug, thiserror::Error)]
pub(crate) enum ControlError {
    // Transparent, so the wrapper adds no layer of its own: what the peer
    // said is what a notice shows and what an error chain reports.
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
    /// would strand it (spec 6.1's 409).
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
            Self::Host(_) | Self::PartialCreate { .. } => false,
            Self::Remote(err) => err.status() == Some(StatusCode::CONFLICT),
        }
    }

    /// Whether the peer does not know an entry the request named (spec 6.1's
    /// 404 `unknown_entry`).
    pub(crate) fn unknown_entry(&self) -> bool {
        match self {
            Self::Host(HostError::UnknownEntry(_)) => true,
            Self::Host(_) | Self::PartialCreate { .. } => false,
            Self::Remote(err) => err.code() == Some("unknown_entry"),
        }
    }

    /// Whether the peer does not know the endpoint the request named (spec
    /// 6.1's 404 `unknown_endpoint`).
    ///
    /// Told apart from an unknown entry because it says nothing about the
    /// session: the peer is older than the feature being asked for. Spec 6.10
    /// makes this the sanctioned check, since capabilities are declared-only
    /// and a gateway's hello cannot speak for the hosts behind it, so a caller
    /// attempts the request and reads this off the refusal.
    pub(crate) fn unknown_endpoint(&self) -> bool {
        match self {
            // A host in this process has every endpoint this process knows.
            Self::Host(_) | Self::PartialCreate { .. } => false,
            Self::Remote(err) => err.code() == Some("unknown_endpoint"),
        }
    }

    /// Whether the peer refused the request as malformed (spec 6.1's 400).
    ///
    /// Told apart from the other refusals because the host's message quotes
    /// the entry id it was given, which a user has never seen. A caller that
    /// knows what it asked for can say it in its own words.
    pub(crate) fn invalid(&self) -> bool {
        match self {
            Self::Host(HostError::Invalid(_)) => true,
            Self::Host(_) | Self::PartialCreate { .. } => false,
            Self::Remote(err) => err.status() == Some(StatusCode::BAD_REQUEST),
        }
    }

    /// Whether a create was refused for naming no host on a peer that serves
    /// several (spec 6.6's `ambiguous_host`).
    ///
    /// Only a gateway answers this, and only to a client that did not say which
    /// host it meant, so a caller that cannot ask a user says how to name one
    /// instead of relaying a refusal with no remedy in it.
    pub(crate) fn ambiguous_host(&self) -> bool {
        match self {
            Self::Host(_) | Self::PartialCreate { .. } => false,
            Self::Remote(err) => err.code() == Some("ambiguous_host"),
        }
    }
}

/// The host this frontend drives.
pub(crate) enum Control {
    Local(LocalControl),
    Remote(RemoteControl),
}

/// The in-process host: this process owns the sessions it renders.
pub(crate) struct LocalControl {
    host: SessionHost,
}

/// A host reached over the control port.
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
    /// whether a gesture with no wire equivalent is refused (spec 9.1) and
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

    /// The session's branch tree, with its current head (spec 6.7).
    pub(crate) async fn tree(&self, session: &str) -> Result<SessionTree, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.tree(session).await?),
            Self::Remote(remote) => Ok(remote.client.tree(session).await?),
        }
    }

    pub(crate) async fn tasks(&self, session: &str) -> Result<TaskTable, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.tasks(session).await?),
            Self::Remote(remote) => Ok(remote.client.tasks(session).await?),
        }
    }

    pub(crate) async fn queue(&self, session: &str) -> Result<QueueState, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.queue(session).await?),
            Self::Remote(remote) => Ok(remote.client.queue(session).await?),
        }
    }

    /// One task's detailed output, which is what backs the task-output
    /// overlay (spec 6.7).
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

    /// Create a session with the creator's settings, an optional first prompt
    /// and an optional tag, answering its id (spec section 8: per-session
    /// settings follow whoever creates the session).
    ///
    /// `host` names which of the peer's hosts the session is for. `None` leaves
    /// that to the peer, which is what an absent host field means on the wire:
    /// the one working directory a plain host serves, or the sole host of a
    /// gateway that has one. A gateway with a choice to make refuses rather
    /// than guessing (spec 6.6), so a caller with a user to ask asks first.
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
    ) -> Result<String, ControlError> {
        match self {
            Self::Local(local) => {
                local.host.creates_here(host.as_deref())?;
                Ok(local.host.create_with(settings, prompt, tag).await?)
            }
            Self::Remote(remote) => {
                let created = remote
                    .client
                    .create_session(CreateSessionRequest {
                        host,
                        settings,
                        prompt: prompt.map(|content| PromptInput::Content { content }),
                        tag,
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
    /// per stream, and changing the attach set means reopening it (spec 6.5).
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
        Command::Settings(change) => RemoteCommand::Settings(settings_request(change)),
        // A cleared tag travels as the empty string, which is what the route
        // reads as "clear" (spec 6.6).
        Command::Tag { tag } => RemoteCommand::Tag(TagRequest {
            tag: tag.unwrap_or_default(),
        }),
        Command::Archive { archived } => RemoteCommand::Archive(ArchiveRequest { archived }),
        Command::Head { target } => RemoteCommand::Head(match target {
            HeadTarget::Entry(entry) => HeadRequest::entry(entry),
            HeadTarget::Before(entry) => HeadRequest::before(entry),
        }),
        Command::KillTask { task } => RemoteCommand::KillTask(task),
    }
}

/// The wire form of a settings change.
///
/// Persistence is deliberately dropped: the wire has no persist axis, since
/// the config files a host would write are the host's own (spec 6.6, section
/// 8). The caller says so in its notice rather than silently pretending the
/// default moved.
fn settings_request(change: SettingsChange) -> SettingsRequest {
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
/// every request omits (spec 6.6).
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
        /// [`Stream::attached`] answers from. The host's attach is
        /// all-or-nothing, so a stream that exists carries a block for every
        /// session named in its request and for no other.
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
    /// evicting a shell that stopped draining (spec 6.9). The re-attach tells
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
