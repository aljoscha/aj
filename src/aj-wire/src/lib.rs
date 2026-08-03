//! Remote-control protocol data types.
//!
//! This crate contains only serializable models. Transport and session
//! behavior live in their respective frontend and application crates.

use std::fmt;
use std::path::PathBuf;

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_agent::message::AgentMessage;
use aj_agent::tool::{TaskId, TaskKind, TaskStatus};
use chrono::{DateTime, Utc};
use serde::de::{Error as _, MapAccess, Visitor};
use serde::ser::{Error as _, SerializeMap};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

/// The current remote-control protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Server identity and supported protocol features.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub capabilities: Vec<String>,
    pub app_version: String,
    pub host_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
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
    pub last_seq: u64,
    pub last_activity: DateTime<Utc>,
    #[serde(default)]
    pub unreachable: bool,
}

/// The complete session directory returned by the sessions read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionList {
    pub sessions: Vec<SessionSummary>,
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
            Self::CaughtUp { .. } | Self::Reset { .. } | Self::Heartbeat => false,
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
            Self::List { sessions } => FrameRef::List { sessions },
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
                let ListFrameFields { sessions } =
                    serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                Ok(Self::List { sessions })
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
    /// Rewrites a top-level session id without parsing or dropping other fields.
    ///
    /// Returns `false` for host-level frames that have no top-level `session`.
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
        if !object.rewrite_session(replacement)? {
            return Ok(false);
        }
        let rewritten = serde_json::to_string(&object)?;
        *self = serde_json::from_str(&rewritten)?;
        Ok(true)
    }
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

struct RawObject(Vec<(String, Box<RawValue>)>);

impl RawObject {
    fn rewrite_session(&mut self, replacement: &str) -> Result<bool, serde_json::Error> {
        let replacement = serde_json::value::to_raw_value(replacement)?;
        let mut rewritten = false;
        for (key, value) in &mut self.0 {
            if key == "session" {
                replacement.clone_into(value);
                rewritten = true;
            }
        }
        Ok(rewritten)
    }
}

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
        "event" | "state" | "caught_up" | "list" | "reset" | "heartbeat" | "vms"
    )
}
