//! The event envelope: one normalized lifecycle event from one agent session.

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};

use crate::VERSION;
use crate::timestamp::Timestamp;

/// The `detail` field an emitter puts the connection it was reached over in.
///
/// Set only where the emitter had no correlation of its own to report — a
/// process that inherited neither is still reachable from whatever opened the
/// connection, and that is worth saying. The value is copied out of the
/// environment verbatim and is never interpreted here.
pub const SSH_CONNECTION_DETAIL: &str = "ssh_connection";

/// Which agent produced an event.
///
/// An opaque identifier, chosen by whoever emits and compared only for
/// equality. This bus carries events for any agent at all, so there is no list
/// of names here to be taught: a harness nobody has heard of emits under its own
/// id and every consumer works unchanged. Validation exists only to keep the
/// value usable as an identifier — see [`Agent::new`] — and never to decide
/// which harnesses are real.
///
/// [`Deserialize`] applies the same validation, so a line naming an unusable
/// agent fails to parse and is skipped like any other malformed line. That is
/// stricter than carrying the value through: an id that cannot be compared or
/// displayed is worse than no line at all, and it costs exactly one line rather
/// than a connection.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Agent(String);

/// The largest agent id this protocol accepts, in bytes.
const AGENT_MAX_BYTES: usize = 64;

/// How much of an oversized value an error message repeats.
const AGENT_ERROR_PREFIX_BYTES: usize = 16;

impl Agent {
    /// The conventional value for an emitter that cannot tell which agent, if
    /// any, is running. It is an ordinary agent id; nothing in this crate
    /// treats it specially.
    pub const UNKNOWN: &'static str = "unknown";

    /// Validates `value` and takes ownership of it.
    ///
    /// An agent id must be non-empty, at most 64 bytes, and free of whitespace
    /// and ASCII control characters — enough to be logged, compared and put in a
    /// key without surprises. Everything else is accepted verbatim, deliberately:
    /// a stricter charset would eventually reject somebody's real tool name.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidAgent> {
        let value = value.into();
        if value.is_empty() {
            return Err(InvalidAgent::Empty);
        }
        if value.len() > AGENT_MAX_BYTES {
            return Err(InvalidAgent::TooLong {
                len: value.len(),
                prefix: prefix_of(&value),
            });
        }
        if let Some(character) = value.chars().find(|c| c.is_whitespace()) {
            return Err(InvalidAgent::Whitespace { character });
        }
        if let Some(character) = value.chars().find(char::is_ascii_control) {
            return Err(InvalidAgent::Control { character });
        }
        Ok(Self(value))
    }

    /// The owned form of [`Agent::UNKNOWN`].
    pub fn unknown() -> Self {
        Self(Self::UNKNOWN.to_owned())
    }

    /// The value as it appears on the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The leading bytes of `value`, on a character boundary, for an error message.
///
/// An oversized value is quoted back only in part: the point of the message is
/// to say which id was rejected, not to reproduce whatever arrived.
fn prefix_of(value: &str) -> String {
    let mut end = AGENT_ERROR_PREFIX_BYTES.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

impl TryFrom<String> for Agent {
    type Error = InvalidAgent;

    fn try_from(value: String) -> Result<Self, InvalidAgent> {
        Self::new(value)
    }
}

impl TryFrom<&str> for Agent {
    type Error = InvalidAgent;

    fn try_from(value: &str) -> Result<Self, InvalidAgent> {
        Self::new(value)
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Agent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Agent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Why a string is not usable as an agent id. See [`Agent::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidAgent {
    /// The value was empty.
    Empty,
    /// The value was longer than the protocol allows.
    TooLong {
        /// The length that was rejected, in bytes.
        len: usize,
        /// The leading characters of the value, for the message.
        prefix: String,
    },
    /// The value contained whitespace.
    Whitespace {
        /// The offending character.
        character: char,
    },
    /// The value contained an ASCII control character.
    Control {
        /// The offending character.
        character: char,
    },
}

impl fmt::Display for InvalidAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("an agent id must not be empty"),
            Self::TooLong { len, prefix } => write!(
                f,
                "agent id {prefix:?}... is {len} bytes, over the {AGENT_MAX_BYTES} the protocol allows"
            ),
            Self::Whitespace { character } => {
                write!(f, "agent id contains whitespace ({character:?})")
            }
            Self::Control { character } => {
                write!(f, "agent id contains a control character ({character:?})")
            }
        }
    }
}

impl Error for InvalidAgent {}

/// A normalized event kind.
///
/// The named variants are the whole closed set, and they are the only thing the
/// status fold acts on. [`Kind::Unknown`] exists so that a subscriber built today
/// can read a stream from a daemon that has learned a new kind: such a line
/// deserializes and is ignored rather than aborting the stream. Nothing in this
/// crate ever constructs an `Unknown` from a name that is in the set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A session began.
    SessionStart,
    /// A session ended.
    SessionEnd,
    /// The user submitted a prompt; a turn began.
    TurnStart,
    /// The agent finished its turn and is waiting for the user.
    TurnEnd,
    /// A tool call began.
    ToolStart,
    /// A tool call finished.
    ToolEnd,
    /// The agent is waiting on a human — the state this bus exists to report.
    Blocked,
    /// The agent is no longer waiting on a human.
    Unblocked,
    /// A subagent began.
    SubagentStart,
    /// A subagent finished.
    SubagentEnd,
    /// The session's context was compacted.
    Compact,
    /// The agent reported an error.
    Error,
    /// A kind this build does not know, verbatim. Only ever produced by reading.
    Unknown(String),
}

impl Kind {
    /// Every kind in the closed set, in the order they are documented.
    pub const ALL: [Self; 12] = [
        Self::SessionStart,
        Self::SessionEnd,
        Self::TurnStart,
        Self::TurnEnd,
        Self::ToolStart,
        Self::ToolEnd,
        Self::Blocked,
        Self::Unblocked,
        Self::SubagentStart,
        Self::SubagentEnd,
        Self::Compact,
        Self::Error,
    ];

    /// The kind as it appears on the wire.
    pub fn as_str(&self) -> &str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::ToolStart => "tool_start",
            Self::ToolEnd => "tool_end",
            Self::Blocked => "blocked",
            Self::Unblocked => "unblocked",
            Self::SubagentStart => "subagent_start",
            Self::SubagentEnd => "subagent_end",
            Self::Compact => "compact",
            Self::Error => "error",
            Self::Unknown(other) => other,
        }
    }

    /// Whether this kind is in the closed set.
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

impl From<String> for Kind {
    fn from(value: String) -> Self {
        match value.as_str() {
            "session_start" => Self::SessionStart,
            "session_end" => Self::SessionEnd,
            "turn_start" => Self::TurnStart,
            "turn_end" => Self::TurnEnd,
            "tool_start" => Self::ToolStart,
            "tool_end" => Self::ToolEnd,
            "blocked" => Self::Blocked,
            "unblocked" => Self::Unblocked,
            "subagent_start" => Self::SubagentStart,
            "subagent_end" => Self::SubagentEnd,
            "compact" => Self::Compact,
            "error" => Self::Error,
            _ => Self::Unknown(value),
        }
    }
}

impl From<&str> for Kind {
    fn from(value: &str) -> Self {
        Self::from(value.to_owned())
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Kind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from(String::deserialize(deserializer)?))
    }
}

/// Where an event's claim comes from.
///
/// A receiver must be able to tell the two apart: an inferred status presented as
/// an authoritative one is how trust in the whole stream dies.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Reported by the agent itself, through a hook. Authoritative.
    #[default]
    Hook,
    /// Inferred by watching from the outside. A floor, never an authority.
    Observed,
}

impl Source {
    /// The value as it appears on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::Observed => "observed",
        }
    }

    /// Whether this is the default, which is omitted from the wire.
    pub fn is_hook(&self) -> bool {
        matches!(self, Self::Hook)
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One hop on the path from the daemon a subscriber is talking to, down to the
/// daemon that folded the event.
///
/// A chain, rather than a single field, because nesting is ordinary: a remote
/// host that runs containers reaches two hops immediately. It also gives merging
/// aggregators one general rule — the longest chain wins — instead of a table of
/// special cases.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OriginHop {
    /// The kind of boundary this hop crosses. Open: whatever transport produced
    /// it. [`OriginHop::CONTAINER`] and [`OriginHop::SSH`] are the ones that
    /// exist today.
    pub kind: String,
    /// Stable identity of the far end, used for deduplication and for deciding
    /// which of two observations of one thing is the inner one.
    pub id: String,
    /// A human-readable name. Display only, and deliberately not authoritative:
    /// several names may share one `id`, and a receiver should prefer whatever
    /// name it has local reason to show.
    pub name: String,
}

impl OriginHop {
    /// The `kind` of a hop into a container.
    pub const CONTAINER: &'static str = "container";
    /// The `kind` of a hop across an ssh connection.
    pub const SSH: &'static str = "ssh";

    /// Builds a hop.
    pub fn new(kind: impl Into<String>, id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
            name: name.into(),
        }
    }
}

/// A normalized lifecycle event, as it appears on the wire.
///
/// This is the envelope every other part of the system is written against. Two
/// rules keep it stable, and both are load-bearing:
///
/// - The core is small and closed, and [`Event::raw`] is the escape hatch. An
///   agent payload that does not fit the normalized fields is carried verbatim
///   rather than driving a new field, so a schema gap never blocks a consumer.
/// - Readers ignore what they do not recognize: unknown top-level fields are
///   dropped, and an unknown [`Kind`] is preserved but not acted on.
///
/// `(agent, session)` is the identity of a session. `session` alone is the
/// agent's own id and is not unique across agents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Envelope version.
    pub v: u32,
    /// Per-daemon monotonic counter, stamped by the daemon that folded the
    /// event. A reattaching subscriber uses it to detect a gap, which a
    /// timestamp cannot do reliably.
    pub seq: u64,
    /// When the daemon stamped the event.
    pub ts: Timestamp,
    /// The agent that produced the event.
    pub agent: Agent,
    /// The agent's own session id.
    pub session: String,
    /// The normalized kind.
    pub kind: Kind,
    /// Where the claim comes from. Absent on the wire means [`Source::Hook`].
    #[serde(default, skip_serializing_if = "Source::is_hook")]
    pub source: Source,
    /// The agent's working directory, if it reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The chain of hops from the reader to the emitter, outermost first. Empty
    /// for a local session. Stamped by aggregators as they merge a stream, never
    /// by the emitter: an emitter has no idea it is inside anything.
    #[serde(default)]
    pub origin: Vec<OriginHop>,
    /// An opaque string copied from the emitting environment, used by receivers
    /// to tie an event back to whatever they think produced it. This crate never
    /// parses, validates or interprets it, and nothing may start: two of these
    /// are equal or they are not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<String>,
    /// Small normalized extras, such as `{"tool": "Bash"}`. Deliberately not a
    /// place to unify tool-call semantics: agents drift, and chasing that is
    /// unbounded work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Map<String, Value>>,
    /// The agent's payload, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

/// An event before a daemon has stamped it.
///
/// Emitters do not have a sequence counter and are not trusted with one, so the
/// two fields a daemon owns — `seq` and `ts` — are simply absent from the type an
/// emitter builds, rather than being optional fields that could be filled in by
/// the wrong side. [`UnstampedEvent::stamp`] is the one way to get an [`Event`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnstampedEvent {
    /// Envelope version.
    pub v: u32,
    /// The agent that produced the event.
    pub agent: Agent,
    /// The agent's own session id.
    pub session: String,
    /// The normalized kind.
    pub kind: Kind,
    /// Where the claim comes from. Absent on the wire means [`Source::Hook`].
    #[serde(default, skip_serializing_if = "Source::is_hook")]
    pub source: Source,
    /// The agent's working directory, if it reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The chain of hops, outermost first. An emitter leaves this empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origin: Vec<OriginHop>,
    /// An opaque string, never parsed. See [`Event::correlation`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<String>,
    /// Small normalized extras.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Map<String, Value>>,
    /// The agent's payload, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

impl UnstampedEvent {
    /// Builds the smallest legal event: an agent, its session, and what
    /// happened. Everything else is added with the `with_*` methods.
    pub fn new(agent: impl Into<Agent>, session: impl Into<String>, kind: Kind) -> Self {
        Self {
            v: VERSION,
            agent: agent.into(),
            session: session.into(),
            kind,
            source: Source::Hook,
            cwd: None,
            origin: Vec::new(),
            correlation: None,
            detail: None,
            raw: None,
        }
    }

    /// Marks where the claim comes from.
    #[must_use]
    pub fn with_source(mut self, source: Source) -> Self {
        self.source = source;
        self
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets the opaque correlation string.
    #[must_use]
    pub fn with_correlation(mut self, correlation: impl Into<String>) -> Self {
        self.correlation = Some(correlation.into());
        self
    }

    /// Sets the normalized extras.
    #[must_use]
    pub fn with_detail(mut self, detail: Map<String, Value>) -> Self {
        self.detail = Some(detail);
        self
    }

    /// Sets one normalized extra, creating the map if needed.
    #[must_use]
    pub fn with_detail_field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.detail
            .get_or_insert_with(Map::new)
            .insert(key.into(), value.into());
        self
    }

    /// Carries the agent's payload verbatim.
    #[must_use]
    pub fn with_raw(mut self, raw: impl Into<Value>) -> Self {
        self.raw = Some(raw.into());
        self
    }

    /// Sets the origin chain. Only an aggregator merging someone else's stream
    /// has any business calling this.
    #[must_use]
    pub fn with_origin(mut self, origin: Vec<OriginHop>) -> Self {
        self.origin = origin;
        self
    }

    /// Stamps the event with the daemon's sequence number and clock.
    pub fn stamp(self, seq: u64, ts: Timestamp) -> Event {
        Event {
            v: self.v,
            seq,
            ts,
            agent: self.agent,
            session: self.session,
            kind: self.kind,
            source: self.source,
            cwd: self.cwd,
            origin: self.origin,
            correlation: self.correlation,
            detail: self.detail,
            raw: self.raw,
        }
    }
}

impl Event {
    /// Drops the daemon-owned fields, for a relay that is about to re-stamp the
    /// event with its own counter and clock.
    pub fn unstamp(self) -> UnstampedEvent {
        UnstampedEvent {
            v: self.v,
            agent: self.agent,
            session: self.session,
            kind: self.kind,
            source: self.source,
            cwd: self.cwd,
            origin: self.origin,
            correlation: self.correlation,
            detail: self.detail,
            raw: self.raw,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Builds an agent id from a literal, which is what every one of these is.
    fn agent(name: &str) -> Agent {
        Agent::new(name).expect("a test's own agent id is a valid one")
    }

    const ENVELOPE: &str = r#"{
      "v": 1,
      "seq": 1041,
      "ts": "2026-08-17T10:32:01.412Z",
      "agent": "claude",
      "session": "abc123",
      "kind": "tool_start",
      "source": "hook",
      "cwd": "/srv/project",
      "origin": [{"kind":"ssh","id":"9f3c:1000","name":"fileserver"},
                 {"kind":"container","id":"a1b2c3","name":"eager_mclean"}],
      "correlation": "w9:p3",
      "detail": { "tool": "Bash" },
      "raw": { "hook_event_name": "PreToolUse" }
    }"#;

    fn ts() -> Timestamp {
        Timestamp::parse("2026-08-17T10:32:01.412Z").unwrap()
    }

    #[test]
    fn documented_envelope_round_trips() {
        let event: Event = serde_json::from_str(ENVELOPE).unwrap();
        assert_eq!(event.agent, agent("claude"));
        assert_eq!(event.kind, Kind::ToolStart);
        assert_eq!(event.source, Source::Hook);
        assert_eq!(event.origin.len(), 2);
        assert_eq!(event.origin[0].kind, OriginHop::SSH);
        assert_eq!(event.correlation.as_deref(), Some("w9:p3"));

        // `source` is the one field that differs: it is the default, so it is
        // omitted when written back.
        let mut expected: Value = serde_json::from_str(ENVELOPE).unwrap();
        expected.as_object_mut().unwrap().remove("source");
        assert_eq!(serde_json::to_value(&event).unwrap(), expected);
    }

    #[test]
    fn hook_events_omit_source_and_observed_events_carry_it() {
        let hook = UnstampedEvent::new(agent("claude"), "abc123", Kind::ToolStart).stamp(1, ts());
        let written = serde_json::to_value(&hook).unwrap();
        assert!(written.get("source").is_none());
        assert_eq!(
            serde_json::from_value::<Event>(written).unwrap().source,
            Source::Hook
        );

        let observed = UnstampedEvent::new(Agent::unknown(), "observed:w9:p3", Kind::Blocked)
            .with_source(Source::Observed)
            .stamp(2, ts());
        assert_eq!(
            serde_json::to_value(&observed).unwrap()["source"],
            json!("observed")
        );
    }

    #[test]
    fn absent_optional_fields_are_omitted_and_default_on_read() {
        let event = UnstampedEvent::new(agent("codex"), "s", Kind::TurnEnd).stamp(7, ts());
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({
                "v": 1, "seq": 7, "ts": "2026-08-17T10:32:01.412Z",
                "agent": "codex", "session": "s", "kind": "turn_end",
                "origin": []
            })
        );

        // `origin` is written even when empty, but tolerated when absent.
        let minimal: Event = serde_json::from_str(
            r#"{"v":1,"seq":7,"ts":"2026-08-17T10:32:01.412Z",
                "agent":"codex","session":"s","kind":"turn_end"}"#,
        )
        .unwrap();
        assert_eq!(minimal, event);
    }

    #[test]
    fn unknown_top_level_fields_are_ignored() {
        let event: Event = serde_json::from_str(
            r#"{"v":1,"seq":7,"ts":"2026-08-17T10:32:01.412Z","agent":"claude",
                "session":"s","kind":"turn_end","invented_later":{"a":1}}"#,
        )
        .unwrap();
        assert_eq!(event.kind, Kind::TurnEnd);
    }

    #[test]
    fn unknown_kinds_and_agents_survive_a_round_trip() {
        let event: Event = serde_json::from_str(
            r#"{"v":1,"seq":7,"ts":"2026-08-17T10:32:01.412Z","agent":"newagent",
                "session":"s","kind":"some_future_kind"}"#,
        )
        .unwrap();
        assert_eq!(event.kind, Kind::Unknown("some_future_kind".to_owned()));
        assert!(!event.kind.is_known());
        assert_eq!(event.agent, agent("newagent"));
        assert_eq!(
            serde_json::to_value(&event).unwrap()["kind"],
            json!("some_future_kind")
        );
    }

    #[test]
    fn every_kind_has_its_documented_wire_string() {
        let expected = [
            "session_start",
            "session_end",
            "turn_start",
            "turn_end",
            "tool_start",
            "tool_end",
            "blocked",
            "unblocked",
            "subagent_start",
            "subagent_end",
            "compact",
            "error",
        ];
        assert_eq!(Kind::ALL.len(), expected.len());
        for (kind, wire) in Kind::ALL.iter().zip(expected) {
            assert_eq!(kind.as_str(), wire);
            assert!(kind.is_known());
            assert_eq!(&Kind::from(wire), kind);
            assert_eq!(
                serde_json::from_value::<Kind>(json!(wire)).unwrap(),
                kind.clone()
            );
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(wire));
        }
    }

    #[test]
    fn an_agent_id_is_carried_verbatim_whoever_it_names() {
        // The names this workspace happens to know, the one an emitter uses when
        // it cannot tell, and one nobody has heard of: the type treats all of
        // them identically, which is the whole point of it being a string.
        for wire in [
            "claude",
            "codex",
            "opencode",
            Agent::UNKNOWN,
            "an-agent-from-the-future",
        ] {
            let parsed = Agent::new(wire).expect("that is a usable agent id");
            assert_eq!(parsed.as_str(), wire);
            assert_eq!(parsed.to_string(), wire);
            assert_eq!(Agent::try_from(wire).unwrap(), parsed);
            assert_eq!(Agent::try_from(wire.to_owned()).unwrap(), parsed);
            assert_eq!(serde_json::to_value(&parsed).unwrap(), json!(wire));
            assert_eq!(
                serde_json::from_value::<Agent>(json!(wire)).unwrap(),
                parsed
            );
        }
        assert_eq!(Agent::unknown().as_str(), Agent::UNKNOWN);
    }

    #[test]
    fn an_unusable_agent_id_is_refused_by_the_constructor_and_by_reading() {
        let too_long = "a".repeat(AGENT_MAX_BYTES + 1);
        for (wire, expected) in [
            ("", InvalidAgent::Empty),
            (
                too_long.as_str(),
                InvalidAgent::TooLong {
                    len: AGENT_MAX_BYTES + 1,
                    prefix: "a".repeat(AGENT_ERROR_PREFIX_BYTES),
                },
            ),
            ("a b", InvalidAgent::Whitespace { character: ' ' }),
            ("tab\there", InvalidAgent::Whitespace { character: '\t' }),
            ("bell\u{7}", InvalidAgent::Control { character: '\u{7}' }),
        ] {
            assert_eq!(Agent::new(wire).unwrap_err(), expected, "{wire:?}");
            assert!(
                serde_json::from_value::<Agent>(json!(wire)).is_err(),
                "{wire:?}"
            );
        }

        // The whole envelope fails with it, so a reader skips the line rather
        // than carrying an id it could not use.
        assert!(
            serde_json::from_str::<Event>(
                r#"{"v":1,"seq":7,"ts":"2026-08-17T10:32:01.412Z","agent":"a b",
                    "session":"s","kind":"turn_end"}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn an_oversized_agent_id_is_only_partly_quoted_back() {
        let shouted = "x".repeat(4096);
        let message = Agent::new(shouted).unwrap_err().to_string();
        assert!(message.contains(&"x".repeat(AGENT_ERROR_PREFIX_BYTES)));
        assert!(!message.contains(&"x".repeat(AGENT_ERROR_PREFIX_BYTES + 1)));

        // A cut through the middle of a character is a cut before it.
        let wide = "\u{1f411}".repeat(64);
        let InvalidAgent::TooLong { prefix, .. } = Agent::new(wide).unwrap_err() else {
            panic!("that should have been too long");
        };
        assert_eq!(prefix, "\u{1f411}".repeat(AGENT_ERROR_PREFIX_BYTES / 4));
    }

    #[test]
    fn source_wire_strings() {
        assert_eq!(serde_json::to_value(Source::Hook).unwrap(), json!("hook"));
        assert_eq!(
            serde_json::to_value(Source::Observed).unwrap(),
            json!("observed")
        );
        assert_eq!(Source::default(), Source::Hook);
    }

    #[test]
    fn stamping_is_the_only_way_to_get_an_event() {
        let unstamped = UnstampedEvent::new(agent("opencode"), "s", Kind::ToolStart)
            .with_cwd("/srv/project")
            .with_correlation("w9:p3")
            .with_detail_field("tool", "Bash")
            .with_raw(json!({"hook_event_name": "PreToolUse"}));
        assert!(
            serde_json::to_value(&unstamped)
                .unwrap()
                .get("seq")
                .is_none()
        );

        let event = unstamped.clone().stamp(1041, ts());
        assert_eq!(event.seq, 1041);
        assert_eq!(event.ts, ts());
        assert_eq!(event.detail.as_ref().unwrap()["tool"], json!("Bash"));
        assert_eq!(event.unstamp(), unstamped);
    }
}
