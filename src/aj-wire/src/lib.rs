//! Remote-control protocol data types.
//!
//! This crate contains only serializable models and the rules a field's value
//! has to satisfy ([`normalize_host_name`]). Transport and session behavior
//! live in their respective frontend and application crates.

use std::fmt;
use std::path::PathBuf;

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_agent::message::AgentMessage;
use aj_agent::tool::{TaskId, TaskKind, TaskStatus};
use aj_models::types::UserContent;
use chrono::{DateTime, Utc};
use serde::de::{DeserializeOwned, Error as _, IgnoredAny, MapAccess, Visitor};
use serde::ser::{Error as _, SerializeMap};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

/// The current remote-control protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// The capability a host declares when it serves `POST
/// /v1/sessions/{id}/archive` (spec 6.10).
///
/// Honest self-description, not a gate: a client attempts the route and reads
/// a 404 as "this host does not archive", because a gateway's own hello cannot
/// speak for the hosts behind it.
pub const ARCHIVE_CAPABILITY: &str = "archive";

/// A creator-selected model, resolved against the receiving host's catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    pub api: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub name: String,
}

/// Optional inference-setting overrides for a session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_display: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<String>,
}

/// A prompt represented either as plain text or typed content blocks.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PromptInput {
    Text { text: String },
    Content { content: Vec<UserContent> },
}

impl PromptInput {
    /// Converts the wire input into the content accepted by the session host.
    pub fn into_content(self) -> Vec<UserContent> {
        match self {
            Self::Text { text } => vec![UserContent::text(text)],
            Self::Content { content } => content,
        }
    }
}

/// Creates a session with optional creator settings and first prompt.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    /// Which host the session is created on, named in the same vocabulary
    /// [`SessionSummary::host`] and [`HostSummary::id`] use (spec 6.6).
    ///
    /// A gateway needs it unless exactly one host is enrolled, and refuses an
    /// ambiguous create rather than guessing. A plain host accepts its own id
    /// and nothing else, because it serves one working directory. Absent
    /// leaves the choice to the server that answers, which is what keeps a
    /// client that never names a host working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<SessionSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptInput>,
    /// The tag the session is created with, absent for an untagged one. Same
    /// rules as [`TagRequest`], so a blank one leaves the session untagged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// Identifies a newly created session, and says what the create could not
/// apply to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreated {
    pub id: String,
    /// What the create asked for after minting the session and could not do,
    /// in the host's own words, absent when it applied everything.
    ///
    /// The session exists whenever this is present, which is why a create
    /// that could not label its session still answers 200 with an id: the
    /// client shows this and retags rather than creating a second session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<String>,
}

/// Submits a prompt to an optional viewed agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    #[serde(flatten)]
    pub input: PromptInput,
}

/// Queues steering text for an optional viewed agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerRequest {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
}

/// Cancels an optional viewed agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
}

/// A pending-message queue mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueOperation {
    Remove,
    Clear,
}

/// Withdraws one agent's pending message or clears all session queues.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueRequest {
    pub op: QueueOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
}

/// The text withdrawn by a queue mutation, when one was pending.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueOutcome {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Starts a manual compaction with optional instructions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Applies one or more session setting changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    #[serde(flatten)]
    pub change: SessionSettings,
}

/// Switches a session's active branch head.
///
/// Exactly one target: [`Self::entry`] names the head directly, and
/// [`Self::before`] names an entry whose *parent* becomes the head, which is
/// the branch-from-a-message gesture (a branch replaces the message rather
/// than continuing after it).
///
/// The host resolves the parent, so the gesture is one command rather than a
/// parent read plus a switch. A read would be an endpoint with a single
/// consumer, and every client would have to repeat the same resolution.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
}

impl HeadRequest {
    /// Switch the head to `entry`.
    pub fn entry(entry: impl Into<String>) -> Self {
        Self {
            entry: Some(entry.into()),
            before: None,
        }
    }

    /// Switch the head to the parent of `entry`.
    pub fn before(entry: impl Into<String>) -> Self {
        Self {
            entry: None,
            before: Some(entry.into()),
        }
    }
}

/// Sets or clears a session's tag (spec 6.6).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagRequest {
    /// The label to set. Empty or whitespace-only clears the session's tag,
    /// so setting and clearing are one route, and an absent field reads the
    /// same as an empty one.
    #[serde(default)]
    pub tag: String,
}

/// Sets or clears a session's archived bit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRequest {
    /// The bit to leave the session with. `false` unarchives, so setting and
    /// clearing are one route, and an absent field reads as `false` the same
    /// way a blank [`TagRequest`] clears a label.
    #[serde(default)]
    pub archived: bool,
}

/// Server identity and supported protocol features.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub capabilities: Vec<String>,
    pub app_version: String,
    pub host_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
    /// What this host calls itself, for a reader rather than for addressing.
    ///
    /// Display metadata and never an id: sessions are namespaced by
    /// [`Self::host_id`] and a client that showed a name still addresses the
    /// host by the id. Names may collide, two clones of one repo being the
    /// obvious case, and that is accepted the way a session tag's collisions
    /// are.
    ///
    /// The host is the authority: it states the name at startup and there is
    /// no rename over the wire. Absent means the peer offers none, from an
    /// older host or a working directory that made no legal name, and a
    /// reader falls back to the id. A present one satisfies
    /// [`normalize_host_name`], though a reader that paints it should apply
    /// that itself rather than trust the sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Longest host name we carry, in bytes.
///
/// A name labels a group header in a 24-column strip, so anything near this
/// is elided on screen long before it gets here. The cap bounds the payload,
/// not the display.
pub const MAX_HOST_NAME_BYTES: usize = 80;

/// Why a host name was refused.
///
/// The sentences state the rule without naming their subject: the surface
/// that renders one already says which field it is refusing (`--name:` on the
/// flag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostNameError {
    /// Longer than [`MAX_HOST_NAME_BYTES`] after trimming.
    TooLong { bytes: usize },
    /// Carries a control character, which includes the escape a name would
    /// otherwise smuggle into a terminal that paints it.
    Control,
}

impl fmt::Display for HostNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostNameError::TooLong { bytes } => write!(
                f,
                "at most {MAX_HOST_NAME_BYTES} bytes, and this one is {bytes}"
            ),
            HostNameError::Control => write!(f, "a single line, with no control characters"),
        }
    }
}

impl std::error::Error for HostNameError {}

/// Validate and normalize a name a host reports for itself.
///
/// The one rule behind [`Hello::name`] and [`DirectoryHost::name`]: a host
/// states a name that satisfies it, a gateway republishes what it was told,
/// and a reader applies this before painting, so the three cannot drift
/// apart.
///
/// `Ok(None)` covers everything that names nothing, so a caller treats "no
/// name" and "a name that is blank" as one case. Surrounding whitespace is
/// trimmed, because a name that differs from another only by padding reads as
/// the same label.
///
/// Control characters are refused rather than stripped: a name reaches a
/// terminal, and the newline and the escape in one are a rendering hazard
/// rather than a label. Refusing also keeps a rewritten name from claiming to
/// be something the operator did not type.
///
/// Deliberately the same rule as a session tag's (`aj_session::normalize_tag`)
/// over a field with a different owner. The two crates are siblings with no
/// edge between them, so the rule is stated twice on purpose. They are free to
/// diverge, and a change to what a label may contain is worth making in both.
pub fn normalize_host_name(name: &str) -> Result<Option<String>, HostNameError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().any(char::is_control) {
        return Err(HostNameError::Control);
    }
    if trimmed.len() > MAX_HOST_NAME_BYTES {
        return Err(HostNameError::TooLong {
            bytes: trimmed.len(),
        });
    }
    Ok(Some(trimmed.to_string()))
}

/// Counts of pending messages by delivery class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueCounts {
    pub steering: usize,
    pub follow_up: usize,
}

/// One session in the host or gateway directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub live: bool,
    pub working: bool,
    pub queued: QueueCounts,
    pub tasks: usize,
    /// The session's durable high-water mark. A host sets it exactly when
    /// [`Self::live`] is set (spec 6.8), and a reader may rely on that only as
    /// far as it trusts the host: nothing on this type enforces it, because
    /// nothing reads the field without also reading `live`.
    ///
    /// A cold row has none. Nothing in a log records its entry count, so an
    /// exact position costs a read of the whole file, and the protocol
    /// forbids using a list-observed position as a cursor anyway (spec 6.5).
    /// [`Self::last_activity`] is the signal a cold row carries instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<u64>,
    /// When the session last did something, on the host's clock: the last
    /// durable event for a live row, the log file's modification time for a
    /// cold one.
    ///
    /// A client records this at view time and compares it against later rows
    /// to derive the unseen-output glyph (spec 6.8). Both sides of that
    /// comparison are host clock, so the client never consults its own.
    pub last_activity: DateTime<Utc>,
    /// The label the user gave the session, when it has one.
    ///
    /// Display metadata and never an id (spec 6.8): a client shows it in a row
    /// instead of the id, and addresses the session by [`Self::id`] all the
    /// same. Session-scoped rather than branch-scoped, so a head switch does
    /// not move it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Which enrolled host the row belongs to.
    ///
    /// A gateway fills this in as it merges its hosts' directories, and a
    /// plain host's rows carry nothing: they are all its own. Clients group by
    /// it and must not derive it from [`Self::id`], which is opaque (spec
    /// 6.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default)]
    pub unreachable: bool,
    /// Whether the user has put the session away.
    ///
    /// Display metadata with no lifecycle meaning: an archived session keeps
    /// its log, its lock and any turn it is running. It changes only by the
    /// archive command, so nothing a session does clears it, and what a client
    /// makes of it is the client's own business.
    ///
    /// Absent reads as unarchived, which is what an older host's rows say and
    /// what the great majority of rows say, so the key is written only when it
    /// is set.
    #[serde(default, skip_serializing_if = "not_archived")]
    pub archived: bool,
}

/// Unarchived reads as absent, so a row carries the key only when the session
/// is put away.
fn not_archived(archived: &bool) -> bool {
    !archived
}

/// The complete session directory returned by the sessions read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionList {
    pub sessions: Vec<SessionSummary>,
    /// The hosts a gateway has enrolled, empty from a plain host (spec 7.1).
    ///
    /// The same field a gateway's `list` frames carry, because the read and the
    /// frames are one payload: a client that reads the directory and a client
    /// that watches it must not disagree about which hosts there are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<DirectoryHost>,
}

/// One enrolled host in a gateway's directory (spec 7.1).
///
/// Carried alongside the rows rather than derived from them, because a gateway
/// holds a host's rows only for as long as that host has sent them: across a
/// restart it holds none for a host it cannot reach, and it stores none
/// deliberately. A client renders such a host as an empty group rather than as
/// nothing, which is what keeps "unreachable, contents unknown" tellable from
/// "no such host".
///
/// A gateway names each host by exactly one of [`Self::id`] and
/// [`Self::address`], and carries [`Self::name`] on top of that when the host
/// reported one: a client labels a group by the name, else the id, else the
/// address, and addresses that host's sessions by the id either way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryHost {
    /// The id this host's sessions are namespaced under, in the vocabulary
    /// [`SessionSummary::host`] and [`HostSummary::id`] use.
    ///
    /// Absent for a host the gateway has never spoken to: an id is learned by
    /// asking the host, and a gateway does not invent one, because ids namespace
    /// sessions and a made-up one would poison every client's state the moment
    /// the real id arrived (spec 7.1). Such a host carries
    /// [`Self::address`] instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Where this host is enrolled, carried only while it has no id.
    ///
    /// A label and never an id: nothing addresses a session or a host by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// What the host called itself when the gateway last heard from it, to be
    /// republished as [`Hello::name`] stated it.
    ///
    /// A third label rather than a replacement for the two above: a client
    /// labels a group by the name, and still addresses that host's sessions by
    /// [`Self::id`]. Absent for a host that reported none and for one the
    /// gateway has never spoken to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The gateway's control connection to this host is down, which is what
    /// [`SessionSummary::unreachable`] says about each of its rows.
    #[serde(default)]
    pub unreachable: bool,
}

/// The directory a gateway composes from its hosts (spec 7.1): their rows as
/// they wrote them, and the hosts themselves.
///
/// The writer's view of what [`SessionList`] reads. The rows stay unparsed
/// because a gateway owns three of their fields and passes the rest through, so
/// a typed re-encode here would drop a newer host's (spec 6.10). One value
/// serves both places the directory appears, the sessions read and the `list`
/// frames, so the two cannot drift.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergedDirectory {
    pub sessions: Vec<RawObject>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<DirectoryHost>,
}

impl MergedDirectory {
    /// This directory as the `list` frame a client of a gateway receives.
    pub fn as_frame(&self) -> DirectoryFrame<'_> {
        DirectoryFrame {
            sessions: &self.sessions,
            hosts: &self.hosts,
        }
    }
}

/// A [`MergedDirectory`] written as a `list` frame (spec 6.3).
#[derive(Serialize)]
#[serde(tag = "kind", rename = "list")]
pub struct DirectoryFrame<'a> {
    sessions: &'a [RawObject],
    #[serde(skip_serializing_if = "no_hosts")]
    hosts: &'a [DirectoryHost],
}

/// Empty reads as absent, so a gateway with nothing enrolled writes the same
/// `list` frame a plain host does.
fn no_hosts(hosts: &&[DirectoryHost]) -> bool {
    hosts.is_empty()
}

/// Wall-clock summary of one background task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: TaskId,
    pub owner: AgentId,
    /// Originating tool call used to route task updates to its launch cell.
    pub call_id: String,
    pub kind: TaskKind,
    pub label: String,
    pub status: TaskStatus,
    pub started_at: DateTime<Utc>,
}

/// The complete background task table for a session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTable {
    pub tasks: Vec<TaskSummary>,
}

/// Detailed status and remotely reachable output for one background task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDetails {
    pub id: TaskId,
    pub status: TaskStatus,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub stdout_total_bytes: u64,
    pub stderr_total_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
}

/// Pending messages for one agent in a session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentQueue {
    pub agent_id: AgentId,
    pub steering: Vec<AgentMessage>,
    pub follow_up: Vec<AgentMessage>,
}

/// The complete pending-message state for a session.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueueState {
    pub queues: Vec<AgentQueue>,
}

/// The segment-collapsed branch tree for a session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTree {
    pub segments: Vec<TreeSegment>,
    /// The session's current head entry, absent only for a log with no head
    /// yet.
    ///
    /// Not derivable from the segments: a head can sit mid-segment, and both
    /// the active-row pre-selection and the "switching to the current tip is
    /// a no-op" rule need the exact entry (spec 6.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

/// One maximal linear segment in a session branch tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeSegment {
    pub head: String,
    pub label: String,
    pub message_count: usize,
    pub last_timestamp: Option<DateTime<Utc>>,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub on_active_path: bool,
    pub is_leaf: bool,
}

/// Where an enrollment came from, which decides whether it can be withdrawn
/// over the wire (spec 7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostSource {
    /// Named by the gateway's configuration file, and so the file's to remove.
    Config,
    /// Enrolled over the wire, and so the gateway's to remember.
    Dynamic,
}

/// The address of a host to enroll on a gateway (spec 7.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollHostRequest {
    /// `<host>:<port>` or a full `http(s)://` URL.
    ///
    /// A plain string rather than a parsed address, because it is what a peer
    /// of any age can produce. The gateway normalizes and refuses it.
    pub address: String,
}

/// One enrolled host in a gateway's host table (spec 7.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSummary {
    /// The id the host reports for itself, which is the namespace its sessions
    /// appear under and the vocabulary a directory row's `host` field and a
    /// create's `host` field use (spec 6.6, 6.8).
    ///
    /// Absent only for a configured host that has never answered: a gateway
    /// cannot invent an id for a store it has not spoken to, and a dynamic
    /// enrollment always has one, because reaching the host is what enrolling
    /// it means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub address: String,
    pub source: HostSource,
    /// Whether the gateway's control connection to this host is up.
    pub connected: bool,
    /// How many of this host's sessions are in the merged directory.
    pub sessions: usize,
    /// Why the last connection attempt did not succeed, when one did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The complete enrolled-host table returned by a gateway.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostList {
    pub hosts: Vec<HostSummary>,
}

/// Current provisioning state of a VM.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum VmStatus {
    Provisioning,
    Ready { address: String, host_id: String },
    Failed { message: String },
    Destroyed,
}

/// One VM managed by a gateway provisioner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmSummary {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub status: VmStatus,
}

/// The complete VM table returned by a gateway.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmList {
    pub vms: Vec<VmSummary>,
}

/// Structured body returned for an unsuccessful request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
}

/// Log identity carried by a durable event frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableEvent {
    pub seq: u64,
    pub entry_id: String,
}

/// Where a client's view of one session stands: the epoch it applied
/// under, and the last durable seq it is willing to claim.
///
/// A client offers this on re-attach and a server decides whether it can
/// serve a suffix from it (spec 6.5). It travels in a stream request as
/// `<epoch>:<seq>`, which is what [`fmt::Display`] and [`str::parse`]
/// implement here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.epoch, self.seq)
    }
}

impl std::str::FromStr for Cursor {
    type Err = CursorParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Split at the last colon: an epoch is opaque to the client that
        // echoes it back, so it may hold one even though the tokens this
        // host mints do not.
        let (epoch, seq) = s
            .rsplit_once(':')
            .ok_or(CursorParseError::MissingSeparator)?;
        if epoch.is_empty() {
            return Err(CursorParseError::EmptyEpoch);
        }
        Ok(Self {
            epoch: epoch.to_string(),
            seq: seq.parse().map_err(|_| CursorParseError::InvalidSeq)?,
        })
    }
}

/// A cursor string does not have the `<epoch>:<seq>` shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorParseError {
    MissingSeparator,
    EmptyEpoch,
    InvalidSeq,
}

impl fmt::Display for CursorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSeparator => write!(f, "a cursor is <epoch>:<seq>"),
            Self::EmptyEpoch => write!(f, "a cursor's epoch must not be empty"),
            Self::InvalidSeq => write!(f, "a cursor's seq must be a non-negative integer"),
        }
    }
}

impl std::error::Error for CursorParseError {}

/// A locally constructed frame violates a wire invariant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameValidationError {
    EmptyEntryId,
    MissingMessageEndDurability,
    MessageIdMismatch {
        message_id: String,
        entry_id: String,
    },
}

impl fmt::Display for FrameValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyEntryId => write!(f, "durable event frame entry_id must not be empty"),
            Self::MissingMessageEndDurability => {
                write!(f, "message_end event frame must carry seq and entry_id")
            }
            Self::MessageIdMismatch {
                message_id,
                entry_id,
            } => write!(
                f,
                "message_end id {message_id:?} does not match frame entry_id {entry_id:?}"
            ),
        }
    }
}

impl std::error::Error for FrameValidationError {}

/// A known domain event or an unknown event retained for forwarding.
#[derive(Clone, Debug)]
pub enum DecodedAgentEvent {
    Known(DecodedKnown<AgentEvent>),
    Unknown {
        event_type: String,
        raw: Box<RawValue>,
    },
}

/// A typed value paired with its original JSON when it came from the wire.
///
/// The retained JSON remains the serialization source so additive fields and
/// number spellings survive forwarding. Locally constructed values have no
/// retained JSON and serialize from the typed value instead.
#[derive(Clone, Debug)]
pub struct DecodedKnown<T> {
    value: T,
    raw: Option<Box<RawValue>>,
}

impl<T> DecodedKnown<T> {
    fn from_wire(value: T, raw: Box<RawValue>) -> Self {
        Self {
            value,
            raw: Some(raw),
        }
    }

    fn local(value: T) -> Self {
        Self { value, raw: None }
    }

    /// Returns the decoded typed value.
    pub fn value(&self) -> &T {
        &self.value
    }

    /// Consumes the wrapper and returns the decoded typed value.
    pub fn into_value(self) -> T {
        self.value
    }

    /// Returns the original JSON for a value decoded from the wire.
    pub fn raw_json(&self) -> Option<&RawValue> {
        self.raw.as_deref()
    }

    fn value_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl From<AgentEvent> for DecodedAgentEvent {
    fn from(event: AgentEvent) -> Self {
        Self::Known(DecodedKnown::local(event))
    }
}

impl DecodedAgentEvent {
    /// The decoded event, or `None` for an event type this build does not
    /// know. An endpoint client skips the unknown case before its reducer
    /// (spec 6.10); only a gateway forwards it.
    pub fn known(&self) -> Option<&AgentEvent> {
        match self {
            Self::Known(event) => Some(event.value()),
            Self::Unknown { .. } => None,
        }
    }
}

impl Serialize for DecodedAgentEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Known(event) => match &event.raw {
                Some(raw) => raw.serialize(serializer),
                None => event.value.serialize(serializer),
            },
            Self::Unknown { raw, .. } => raw.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for DecodedAgentEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let EventTag { event_type } = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        if is_known_event_type(&event_type) {
            let event = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
            Ok(Self::Known(DecodedKnown::from_wire(event, raw)))
        } else {
            Ok(Self::Unknown { event_type, raw })
        }
    }
}

#[derive(Deserialize)]
struct EventTag {
    #[serde(rename = "type")]
    event_type: String,
}

/// One item on the unified event stream.
#[derive(Clone, Debug)]
pub enum Frame {
    Event {
        session: String,
        epoch: String,
        durability: Option<DurableEvent>,
        event: DecodedAgentEvent,
    },
    State {
        session: String,
        epoch: String,
        working: bool,
        settings: AgentSettings,
        last_seq: u64,
    },
    CaughtUp {
        session: String,
        epoch: String,
        last_seq: u64,
    },
    List {
        sessions: Vec<SessionSummary>,
        /// The hosts a gateway has enrolled, empty on a plain host's own frames
        /// (spec 7.1).
        hosts: Vec<DirectoryHost>,
    },
    /// The error envelope of spec 6.6 as a stream frame, scoped to one session
    /// (spec 6.3).
    ///
    /// Its first use is the per-session attach refusal: a named session the
    /// server cannot resolve produces this instead of an attach block, and the
    /// stream serves every other session it named. Reliable-transient, so it
    /// is neither dropped as lossy nor treated as durable (spec 6.4).
    Error {
        session: String,
        /// The epoch the error is about, for a code that refers to one.
        ///
        /// Absent where the session has no epoch to speak of, which is what an
        /// attach refusal carries: the session was never resolved, so nothing
        /// minted one.
        epoch: Option<String>,
        /// A stable snake_case token a client may branch on. A code this build
        /// does not know renders as its `message` (spec 6.10).
        code: String,
        /// The human sentence, produced where the facts are and always
        /// sufficient on its own.
        message: String,
    },
    Reset {
        session: String,
    },
    Heartbeat,
    Vms {
        vms: Vec<VmSummary>,
    },
}

impl Frame {
    /// The session a session-scoped frame belongs to, `None` for the
    /// host-level kinds (`list`, `heartbeat`, `vms`).
    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Event { session, .. }
            | Self::State { session, .. }
            | Self::CaughtUp { session, .. }
            | Self::Error { session, .. }
            | Self::Reset { session } => Some(session),
            Self::List { .. } | Self::Heartbeat | Self::Vms { .. } => None,
        }
    }

    /// The log position a durable event frame carries (spec 6.4), `None`
    /// for every other frame.
    pub fn durable_seq(&self) -> Option<u64> {
        match self {
            Self::Event {
                durability: Some(durability),
                ..
            } => Some(durability.seq),
            _ => None,
        }
    }

    /// Whether the frame is lossy, i.e. a cumulative snapshot that a newer
    /// one supersedes (spec 6.4). Only these may be coalesced or dropped.
    ///
    /// An event type this build does not know classifies as **reliable**,
    /// which is the safe side of the decision: an attach holds and flushes it
    /// rather than dropping it, and a client that cannot keep up is evicted
    /// rather than left missing it. A newer peer's lossy event costs a
    /// needless delivery that way, whereas the opposite default would drop a
    /// one-shot frame whose loss wedges the client.
    pub fn is_lossy(&self) -> bool {
        match self {
            Self::Event { event, .. } => matches!(
                event.known(),
                Some(
                    AgentEvent::MessageUpdate { .. }
                        | AgentEvent::ToolExecutionUpdate { .. }
                        | AgentEvent::TaskOutput { .. }
                )
            ),
            Self::State { .. } | Self::List { .. } | Self::Vms { .. } => true,
            Self::CaughtUp { .. } | Self::Error { .. } | Self::Reset { .. } | Self::Heartbeat => {
                false
            }
        }
    }

    fn prepare(&mut self) -> Result<(), FrameValidationError> {
        let Self::Event {
            durability, event, ..
        } = self
        else {
            return Ok(());
        };
        if durability
            .as_ref()
            .is_some_and(|durability| durability.entry_id.is_empty())
        {
            return Err(FrameValidationError::EmptyEntryId);
        }
        if let DecodedAgentEvent::Known(event) = event
            && let AgentEvent::MessageEnd { message, .. } = event.value_mut()
        {
            let durability = durability
                .as_ref()
                .ok_or(FrameValidationError::MissingMessageEndDurability)?;
            message.set_id(durability.entry_id.clone());
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        let Self::Event {
            durability, event, ..
        } = self
        else {
            return Ok(());
        };
        if durability
            .as_ref()
            .is_some_and(|durability| durability.entry_id.is_empty())
        {
            return Err(FrameValidationError::EmptyEntryId);
        }
        if let DecodedAgentEvent::Known(event) = event
            && let AgentEvent::MessageEnd { message, .. } = event.value()
        {
            let durability = durability
                .as_ref()
                .ok_or(FrameValidationError::MissingMessageEndDurability)?;
            if message.id() != durability.entry_id {
                return Err(FrameValidationError::MessageIdMismatch {
                    message_id: message.id().to_string(),
                    entry_id: durability.entry_id.clone(),
                });
            }
        }
        Ok(())
    }

    fn rewrite_session(&mut self, replacement: &str) -> bool {
        let session = match self {
            Self::Event { session, .. }
            | Self::State { session, .. }
            | Self::CaughtUp { session, .. }
            | Self::Error { session, .. }
            | Self::Reset { session } => session,
            Self::List { .. } | Self::Heartbeat | Self::Vms { .. } => return false,
        };
        replacement.clone_into(session);
        true
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FrameRef<'a> {
    Event {
        session: &'a str,
        epoch: &'a str,
        #[serde(flatten)]
        durability: Option<&'a DurableEvent>,
        event: &'a DecodedAgentEvent,
    },
    State {
        session: &'a str,
        epoch: &'a str,
        working: bool,
        settings: &'a AgentSettings,
        last_seq: u64,
    },
    CaughtUp {
        session: &'a str,
        epoch: &'a str,
        last_seq: u64,
    },
    List {
        sessions: &'a [SessionSummary],
        #[serde(skip_serializing_if = "no_hosts")]
        hosts: &'a [DirectoryHost],
    },
    Error {
        session: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        epoch: Option<&'a str>,
        code: &'a str,
        message: &'a str,
    },
    Reset {
        session: &'a str,
    },
    Heartbeat,
    Vms {
        vms: &'a [VmSummary],
    },
}

impl Serialize for Frame {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        let frame = match self {
            Self::Event {
                session,
                epoch,
                durability,
                event,
            } => FrameRef::Event {
                session,
                epoch,
                durability: durability.as_ref(),
                event,
            },
            Self::State {
                session,
                epoch,
                working,
                settings,
                last_seq,
            } => FrameRef::State {
                session,
                epoch,
                working: *working,
                settings,
                last_seq: *last_seq,
            },
            Self::CaughtUp {
                session,
                epoch,
                last_seq,
            } => FrameRef::CaughtUp {
                session,
                epoch,
                last_seq: *last_seq,
            },
            Self::List { sessions, hosts } => FrameRef::List { sessions, hosts },
            Self::Error {
                session,
                epoch,
                code,
                message,
            } => FrameRef::Error {
                session,
                epoch: epoch.as_deref(),
                code,
                message,
            },
            Self::Reset { session } => FrameRef::Reset { session },
            Self::Heartbeat => FrameRef::Heartbeat,
            Self::Vms { vms } => FrameRef::Vms { vms },
        };
        frame.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Frame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let FrameTag { kind } = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        match kind.as_str() {
            "event" => {
                let EventFrameFields {
                    session,
                    epoch,
                    seq,
                    entry_id,
                    mut event,
                } = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                let durability = match (seq, entry_id) {
                    (MetadataField::Missing, MetadataField::Missing) => None,
                    (MetadataField::Value(seq), MetadataField::Value(entry_id)) => {
                        if entry_id.is_empty() {
                            return Err(D::Error::custom(
                                "durable event frame entry_id must not be empty",
                            ));
                        }
                        Some(DurableEvent { seq, entry_id })
                    }
                    (MetadataField::Null, _) | (_, MetadataField::Null) => {
                        return Err(D::Error::custom(
                            "event frame seq and entry_id must be omitted rather than null",
                        ));
                    }
                    _ => {
                        return Err(D::Error::custom(
                            "event frame must carry both seq and entry_id, or neither",
                        ));
                    }
                };
                if let DecodedAgentEvent::Known(event) = &mut event
                    && let AgentEvent::MessageEnd { message, .. } = event.value_mut()
                {
                    let Some(durability) = durability.as_ref() else {
                        return Err(D::Error::custom(
                            "message_end event frame must carry seq and entry_id",
                        ));
                    };
                    message.set_id(durability.entry_id.clone());
                }
                Ok(Self::Event {
                    session,
                    epoch,
                    durability,
                    event,
                })
            }
            "state" => {
                let StateFrameFields {
                    session,
                    epoch,
                    working,
                    settings,
                    last_seq,
                } = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                Ok(Self::State {
                    session,
                    epoch,
                    working,
                    settings,
                    last_seq,
                })
            }
            "caught_up" => {
                let CaughtUpFrameFields {
                    session,
                    epoch,
                    last_seq,
                } = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                Ok(Self::CaughtUp {
                    session,
                    epoch,
                    last_seq,
                })
            }
            "list" => {
                let ListFrameFields { sessions, hosts } =
                    serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                Ok(Self::List { sessions, hosts })
            }
            "error" => {
                let ErrorFrameFields {
                    session,
                    epoch,
                    code,
                    message,
                } = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                Ok(Self::Error {
                    session,
                    epoch,
                    code,
                    message,
                })
            }
            "reset" => {
                let ResetFrameFields { session } =
                    serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                Ok(Self::Reset { session })
            }
            "heartbeat" => Ok(Self::Heartbeat),
            "vms" => {
                let VmsFrameFields { vms } =
                    serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                Ok(Self::Vms { vms })
            }
            _ => Err(D::Error::custom(format!("unknown frame kind {kind:?}"))),
        }
    }
}

#[derive(Deserialize)]
struct EventFrameFields {
    session: String,
    epoch: String,
    #[serde(default)]
    seq: MetadataField<u64>,
    #[serde(default)]
    entry_id: MetadataField<String>,
    event: DecodedAgentEvent,
}

#[derive(Deserialize)]
struct StateFrameFields {
    session: String,
    epoch: String,
    working: bool,
    settings: AgentSettings,
    last_seq: u64,
}

#[derive(Deserialize)]
struct CaughtUpFrameFields {
    session: String,
    epoch: String,
    last_seq: u64,
}

#[derive(Deserialize)]
struct ListFrameFields {
    sessions: Vec<SessionSummary>,
    #[serde(default)]
    hosts: Vec<DirectoryHost>,
}

#[derive(Deserialize)]
struct ErrorFrameFields {
    session: String,
    #[serde(default)]
    epoch: Option<String>,
    code: String,
    message: String,
}

#[derive(Deserialize)]
struct ResetFrameFields {
    session: String,
}

#[derive(Deserialize)]
struct VmsFrameFields {
    vms: Vec<VmSummary>,
}

#[derive(Default)]
enum MetadataField<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<'de, T> Deserialize<'de> for MetadataField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::deserialize(deserializer).map(|value| match value {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

/// A known protocol frame or an unknown frame retained for forwarding.
#[derive(Clone, Debug)]
pub enum DecodedFrame {
    Known(DecodedKnown<Frame>),
    Unknown { kind: String, raw: Box<RawValue> },
}

impl DecodedFrame {
    /// The frame's top-level session id, `None` for a host-scoped frame.
    ///
    /// The read side of [`Self::rewrite_session`], answering for the same
    /// field on the same terms, which is what lets a gateway namespace every
    /// frame the rewrite would touch, kinds this build does not know included
    /// (spec 6.10). A frame decoded from the wire answers from the JSON it will
    /// forward, a locally built one from its variant. A `session` nested in a
    /// payload is not the frame's session and is not read. `None` comes back
    /// exactly where the rewrite returns `false`.
    ///
    /// A top-level `session` no id can be read from is an error rather than
    /// `None`: a value that is not a string, `null`, or a string token whose
    /// escapes do not decode. Spec 6.3 mints ids as strings, so such a frame is
    /// malformed, and only an unknown kind can carry one this far, a known kind
    /// is refused at decode. Both other answers would be worse. `None` would
    /// call the frame host-scoped while the rewrite still replaces the field, so
    /// a gateway would forward a session-scoped frame carrying the host's own
    /// id. Handing back the field's literal text would invent an id no host ever
    /// minted.
    ///
    /// The id comes back owned. A caller builds the replacement from it and
    /// hands that straight back to the rewrite, which borrows the frame
    /// mutably.
    pub fn session(&self) -> Result<Option<String>, serde_json::Error> {
        let raw = match self {
            // NOTE: For a known kind the typed value would answer too, and
            // identically, because its strict decode refuses a `session` that is
            // anything but one string. Reading the JSON anyway keeps the answer
            // tied to the bytes the rewrite writes into, so the two cannot drift
            // if that strictness ever loosens.
            Self::Known(frame) => match frame.raw_json() {
                Some(raw) => raw,
                // A locally built frame retains no JSON, so its variant is the
                // only source there is. The rewrite falls back the same way.
                None => return Ok(frame.value().session().map(str::to_string)),
            },
            Self::Unknown { raw, .. } => raw,
        };
        let TopLevelSession(session) = serde_json::from_str(raw.get())?;
        Ok(session)
    }

    /// Rewrites the frame's top-level session id, reporting whether it had
    /// one.
    ///
    /// A gateway calls this on every frame it forwards, kinds this build does
    /// not know included (spec 6.10). What comes back out is the frame the
    /// host wrote, structurally unchanged apart from the id: top-level key
    /// order is not significant and byte identity is not promised, but nothing
    /// below the top level is parsed, so payloads and their number literals
    /// travel verbatim. A `session` nested in a payload is not the frame's
    /// session and is left alone.
    ///
    /// `false` says the frame has no top-level `session`, which makes it
    /// host-scoped (`list`, `heartbeat`, `vms`, and any unknown kind that
    /// carries no id). Such a frame is handed back untouched rather than
    /// re-serialized. A frame decoded from the wire decides on the JSON it
    /// will forward, a locally built one on its variant.
    pub fn rewrite_session(&mut self, replacement: &str) -> Result<bool, serde_json::Error> {
        let raw = match &*self {
            Self::Known(frame) => frame.raw_json(),
            Self::Unknown { raw, .. } => Some(raw.as_ref()),
        }
        .map(|raw| raw.get().to_string());
        let Some(raw) = raw else {
            let Self::Known(frame) = self else {
                unreachable!("unknown frames always retain their raw JSON")
            };
            return Ok(frame.value_mut().rewrite_session(replacement));
        };

        let mut object: RawObject = serde_json::from_str(&raw)?;
        if !object.replace(
            SESSION_FIELD,
            &serde_json::value::to_raw_value(replacement)?,
        ) {
            return Ok(false);
        }
        let rewritten = serde_json::to_string(&object)?;
        *self = serde_json::from_str(&rewritten)?;
        Ok(true)
    }

    /// The rows of a `list` frame as their host wrote them, `None` for every
    /// other kind.
    ///
    /// The read a gateway needs to re-emit a directory under its own name: it
    /// owns `id`, `host` and `unreachable` on a row and passes everything else
    /// through, so it takes the rows unparsed rather than through
    /// [`SessionSummary`], which a re-encode would drop a newer host's fields
    /// from (spec 6.10). A row's own nested values stay text for the same
    /// reason the rewrite leaves a payload alone.
    ///
    /// A frame decoded from the wire answers from the JSON it arrived as, a
    /// locally built one by encoding its typed rows, which is the same fallback
    /// [`Self::session`] makes.
    pub fn rows(&self) -> Result<Option<Vec<RawObject>>, serde_json::Error> {
        // An unknown kind is not `list`: a gateway forwards it whole rather than
        // merging it.
        let Self::Known(frame) = self else {
            return Ok(None);
        };
        let Frame::List { sessions, .. } = frame.value() else {
            return Ok(None);
        };
        let Some(raw) = frame.raw_json() else {
            return sessions
                .iter()
                .map(RawObject::encode)
                .collect::<Result<Vec<_>, _>>()
                .map(Some);
        };
        let ListRows { sessions } = serde_json::from_str(raw.get())?;
        Ok(Some(sessions))
    }
}

/// The rows of a `list` frame, unparsed, with every other field skipped.
#[derive(Deserialize)]
struct ListRows {
    sessions: Vec<RawObject>,
}

impl TryFrom<Frame> for DecodedFrame {
    type Error = FrameValidationError;

    fn try_from(mut frame: Frame) -> Result<Self, Self::Error> {
        frame.prepare()?;
        frame.validate()?;
        Ok(Self::Known(DecodedKnown::local(frame)))
    }
}

impl Serialize for DecodedFrame {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Known(frame) => match &frame.raw {
                Some(raw) => raw.serialize(serializer),
                None => frame.value.serialize(serializer),
            },
            Self::Unknown { raw, .. } => raw.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for DecodedFrame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let FrameTag { kind } = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        if is_known_frame_kind(&kind) {
            let frame = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
            Ok(Self::Known(DecodedKnown::from_wire(frame, raw)))
        } else {
            Ok(Self::Unknown { kind, raw })
        }
    }
}

#[derive(Deserialize)]
struct FrameTag {
    kind: String,
}

/// A JSON object held as its top-level fields, every value left unparsed.
///
/// Flat by design. What edits an object here owns a named field or two of it and
/// must disturb nothing else, and keeping nested values as text is what puts a
/// payload's own `session`, or a row's own nested `id`, out of reach. It is also
/// what carries a peer's number literals through unrounded (spec 6.10).
///
/// Key order is the order the object arrived in, and a duplicate key is kept as
/// two fields: an object edited here is re-emitted, not normalized.
#[derive(Clone, Debug)]
pub struct RawObject(Vec<(String, Box<RawValue>)>);

impl RawObject {
    /// The top-level fields of `value` as it serializes, with every nested value
    /// left as the JSON it produced.
    ///
    /// For a value built in process, which has no wire JSON of its own to keep.
    /// Fails when `value` does not serialize as a JSON object.
    pub fn encode<T>(value: &T) -> Result<Self, serde_json::Error>
    where
        T: ?Sized + Serialize,
    {
        serde_json::from_str(&serde_json::to_string(value)?)
    }

    /// The value of `key` decoded as `T`, `None` when the object has no such
    /// key.
    ///
    /// The last occurrence wins for a duplicate key, which is the one a reader
    /// that parses the object into a map takes. A value that is not a `T`,
    /// `null` included, is an error rather than a missing field: the field is
    /// there and says something this cannot read.
    pub fn get<T>(&self, key: &str) -> Result<Option<T>, serde_json::Error>
    where
        T: DeserializeOwned,
    {
        self.0
            .iter()
            .rev()
            .find(|(field, _)| field == key)
            .map(|(_, value)| serde_json::from_str(value.get()))
            .transpose()
    }

    /// Names `value` for `key`, so the object carries it afterwards either way:
    /// every occurrence of an existing key is replaced, and a key that is not
    /// there is appended.
    pub fn set<T>(&mut self, key: &str, value: &T) -> Result<(), serde_json::Error>
    where
        T: ?Sized + Serialize,
    {
        let value = serde_json::value::to_raw_value(value)?;
        if !self.replace(key, &value) {
            self.0.push((key.to_string(), value));
        }
        Ok(())
    }

    /// Replaces every occurrence of `key`, reporting whether it found one. A key
    /// that is not there is not added: this edits an object, it does not extend
    /// it.
    fn replace(&mut self, key: &str, replacement: &RawValue) -> bool {
        let mut replaced = false;
        for (field, value) in &mut self.0 {
            if field == key {
                // No early exit: a duplicate key is malformed for a known kind
                // and refused at decode, but an unknown frame is forwarded as
                // it arrived, and a reader downstream may take either
                // occurrence.
                replacement.clone_into(value);
                replaced = true;
            }
        }
        replaced
    }
}

/// Text equality field by field, in order.
///
/// What a gateway compares is what it would send, and a peer serializes an
/// unchanged row the same way twice, so this answers "has anything moved" without
/// parsing values back. Two structurally equal objects spelled differently do
/// compare unequal, which costs a republished snapshot and never a wrong one.
impl PartialEq for RawObject {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && std::iter::zip(&self.0, &other.0).all(|((key, value), (other_key, other_value))| {
                key == other_key && value.get() == other_value.get()
            })
    }
}

impl Eq for RawObject {}

impl<'de> Deserialize<'de> for RawObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RawObjectVisitor;

        impl<'de> Visitor<'de> for RawObjectVisitor {
            type Value = RawObject;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut fields = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some(field) = map.next_entry()? {
                    fields.push(field);
                }
                Ok(RawObject(fields))
            }
        }

        deserializer.deserialize_map(RawObjectVisitor)
    }
}

impl Serialize for RawObject {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

/// The field a session-scoped frame names its session in (spec 6.3), and the one
/// field of a frame a gateway rewrites.
const SESSION_FIELD: &str = "session";

/// The top-level `session` of a frame's JSON, with every other field skipped.
///
/// A gateway reads this for every frame it forwards, so nothing else is
/// materialized: no other key is allocated and no value is copied out. The
/// document is still walked once, which is what finding a top-level key costs
/// in JSON, and walking it rather than searching its text is what keeps a
/// payload's own `session` out of reach.
struct TopLevelSession(Option<String>);

impl<'de> Deserialize<'de> for TopLevelSession {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TopLevelSessionVisitor;

        impl<'de> Visitor<'de> for TopLevelSessionVisitor {
            type Value = TopLevelSession;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut session = None;
                while let Some(key) = map.next_key::<TopLevelKey>()? {
                    match key {
                        // Read as a string, so a `session` that is not one is
                        // an error rather than an id this invented. The last
                        // occurrence wins if a duplicate key gets this far,
                        // which is the one a reader that parses the frame into
                        // a map takes.
                        TopLevelKey::Session => session = Some(map.next_value::<String>()?),
                        TopLevelKey::Other => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(TopLevelSession(session))
            }
        }

        deserializer.deserialize_map(TopLevelSessionVisitor)
    }
}

/// Whether a top-level key is `session`, decided after JSON unescaping and
/// without allocating the key.
enum TopLevelKey {
    Session,
    Other,
}

impl<'de> Deserialize<'de> for TopLevelKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TopLevelKeyVisitor;

        impl<'de> Visitor<'de> for TopLevelKeyVisitor {
            type Value = TopLevelKey;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object key")
            }

            fn visit_str<E>(self, key: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(match key {
                    SESSION_FIELD => TopLevelKey::Session,
                    _ => TopLevelKey::Other,
                })
            }
        }

        deserializer.deserialize_str(TopLevelKeyVisitor)
    }
}

fn is_known_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "agent_start"
            | "agent_end"
            | "turn_start"
            | "turn_end"
            | "message_start"
            | "message_update"
            | "message_end"
            | "tool_execution_start"
            | "tool_execution_update"
            | "tool_execution_end"
            | "sub_agent_start"
            | "sub_agent_end"
            | "task_start"
            | "task_output"
            | "task_end"
            | "notice"
            | "warning"
            | "error"
            | "stream_retry"
            | "usage_update"
            | "compaction_start"
            | "compaction_progress"
            | "compaction_end"
            | "queue_update"
    )
}

fn is_known_frame_kind(kind: &str) -> bool {
    matches!(
        kind,
        "event" | "state" | "caught_up" | "list" | "error" | "reset" | "heartbeat" | "vms"
    )
}
