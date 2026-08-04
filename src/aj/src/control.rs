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
    AttachRequest, Attachment, Command, CommandOutcome, HostError, QueueOp, SessionHost,
    SettingsAxis, SettingsChange,
};
use aj_app::session_setup::thinking_display_name;
use aj_models::types::UserContent;
use aj_models::{speed_name, thinking_config_name, verbosity_name};
use aj_wire::{
    CancelRequest, CompactRequest, CreateSessionRequest, Cursor, Frame, HeadRequest,
    ModelSelection, PromptInput, PromptRequest, QueueOperation, QueueRequest, QueueState,
    SessionList, SessionSettings, SettingsRequest, SteerRequest, TaskDetails, TaskTable,
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
            Self::Host(_) => false,
            Self::Remote(err) => err.status() == Some(StatusCode::CONFLICT),
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

    /// Create a session with the creator's settings and an optional first
    /// prompt, answering its id (spec section 8: per-session settings follow
    /// whoever creates the session).
    pub(crate) async fn create(
        &self,
        settings: Option<SessionSettings>,
        prompt: Option<Vec<UserContent>>,
    ) -> Result<String, ControlError> {
        match self {
            Self::Local(local) => Ok(local.host.create_with(settings, prompt).await?),
            Self::Remote(remote) => Ok(remote
                .client
                .create_session(CreateSessionRequest {
                    settings,
                    prompt: prompt.map(|content| PromptInput::Content { content }),
                })
                .await?),
        }
    }

    /// Open a frame stream for `session`, offering `cursor`.
    ///
    /// The attach block follows on the stream, so the caller has to arm its
    /// fold for it (see [`Stream::attached`]) before reading.
    pub(crate) async fn attach(
        &self,
        session: &str,
        cursor: Option<Cursor>,
    ) -> Result<Stream, ControlError> {
        let requests = [AttachRequest {
            session: session.to_string(),
            cursor,
        }];
        match self {
            Self::Local(local) => Ok(Stream::Local(local.host.attach(&requests).await?)),
            Self::Remote(remote) => Ok(Stream::Remote {
                events: remote.client.events(&requests).await?,
                lost: None,
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
        Command::Head { entry } => RemoteCommand::Head(HeadRequest { entry }),
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
    /// A remote attach the host refuses fails before the stream opens, so a
    /// stream that exists is a stream with the block on it.
    pub(crate) fn attached(&self, session: &str) -> bool {
        match self {
            Self::Local(attachment) => attachment.attached().iter().any(|name| name == session),
            Self::Remote { .. } => true,
        }
    }

    /// The next frame, awaiting one.
    pub(crate) async fn recv(&mut self) -> ControlFrame {
        match self {
            Self::Local(attachment) => match attachment.recv().await {
                Some(frame) => ControlFrame::Frame(frame),
                None => ControlFrame::Closed,
            },
            Self::Remote { events, lost } => {
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
            Self::Remote { events, lost } => {
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
