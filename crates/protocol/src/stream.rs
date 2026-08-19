//! What a subscriber reads.
//!
//! A subscriber connects, reads a [`Snapshot`], and then reads lines forever. The
//! snapshot exists because a subscriber attaching mid-session has to learn
//! *current state*, not merely future events. After it come live [`Event`] lines,
//! a [`Heartbeat`] every ten seconds so that a dead stream is distinguishable
//! from a quiet one, and [`ForegroundChange`] lines as observations change.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::event::{Agent, Event, Kind, OriginHop, Source};
use crate::status::SessionStatus;
use crate::timestamp::Timestamp;

/// One line of the subscribe stream.
///
/// A line whose `kind` this build does not recognize deserializes to
/// [`StreamLine::Unknown`] and is meant to be ignored, so that a subscriber built
/// today keeps working against a daemon that has learned to say something new.
/// That variant carries nothing and the enum is therefore read-only: a writer
/// serializes the specific line type it means, because there is no honest way to
/// write back a line that was never understood.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamLine {
    /// The current state of everything, sent first.
    Snapshot(Snapshot),
    /// Proof the stream is alive.
    Heartbeat(Heartbeat),
    /// A correlation's foreground observation changed.
    ForegroundChange(ForegroundChange),
    /// A normalized lifecycle event.
    Event(Event),
    /// A line of a kind this build does not know.
    Unknown,
}

impl<'de> Deserialize<'de> for StreamLine {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        let value = Value::deserialize(deserializer)?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| D::Error::missing_field("kind"))?;

        let line = match kind {
            SNAPSHOT => Self::Snapshot(from_value(value)?),
            HEARTBEAT => Self::Heartbeat(from_value(value)?),
            FOREGROUND_CHANGE => Self::ForegroundChange(from_value(value)?),
            other if Kind::from(other).is_known() => Self::Event(from_value(value)?),
            _ => Self::Unknown,
        };
        Ok(line)
    }
}

fn from_value<T: serde::de::DeserializeOwned, E: serde::de::Error>(value: Value) -> Result<T, E> {
    serde_json::from_value(value).map_err(E::custom)
}

/// The `kind` of a [`Snapshot`] line.
pub const SNAPSHOT: &str = "snapshot";
/// The `kind` of a [`Heartbeat`] line.
pub const HEARTBEAT: &str = "heartbeat";
/// The `kind` of a [`ForegroundChange`] line.
pub const FOREGROUND_CHANGE: &str = "foreground_change";

/// The first line of every subscription: everything the daemon currently knows.
///
/// The two arrays answer different questions and are deliberately independent.
/// `sessions` is "what are my agents doing"; `foreground` is "what is running in
/// each correlated slot". Something running an editor appears in the second and
/// not the first, and filling `sessions` with pseudo-entries for it would
/// pollute the array whose entire job is the first question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename = "snapshot")]
pub struct Snapshot {
    /// Envelope version.
    pub v: u32,
    /// The sequence number this snapshot is current as of. Events that follow
    /// carry higher numbers.
    pub seq: u64,
    /// Which daemon produced the snapshot. Optional, and the `id` is opaque to
    /// readers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<DaemonIdentity>,
    /// Every session the daemon knows about.
    pub sessions: Vec<SessionEntry>,
    /// Every foreground observation the daemon holds, or absent entirely when no
    /// daemon in the chain can observe processes at all. Absent and empty mean
    /// different things: "nobody is looking" and "nobody is running anything".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<Vec<ForegroundEntry>>,
}

impl Snapshot {
    /// Builds a snapshot with no foreground observations reported at all.
    pub fn new(seq: u64, sessions: Vec<SessionEntry>) -> Self {
        Self {
            v: crate::VERSION,
            seq,
            daemon: None,
            sessions,
            foreground: None,
        }
    }

    /// Attaches the identity of the producing daemon.
    #[must_use]
    pub fn with_daemon(mut self, daemon: DaemonIdentity) -> Self {
        self.daemon = Some(daemon);
        self
    }

    /// Reports foreground observations, including the fact that there are none.
    #[must_use]
    pub fn with_foreground(mut self, foreground: Vec<ForegroundEntry>) -> Self {
        self.foreground = Some(foreground);
        self
    }
}

/// Which daemon produced a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DaemonIdentity {
    /// A stable identity for the daemon. Opaque: readers compare it and nothing
    /// else.
    pub id: String,
}

impl DaemonIdentity {
    /// Builds an identity.
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

/// One agent session in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    /// The agent's own session id; unique only together with `agent`.
    pub session: String,
    /// The agent running the session.
    pub agent: Agent,
    /// What it is doing.
    pub status: SessionStatus,
    /// Whether the status is agent-reported or inferred. Always written, unlike
    /// on an event: a receiver has to be able to render the difference, and a
    /// snapshot is not on the hot path where the saved bytes would matter.
    pub source: Source,
    /// The agent's working directory, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The opaque correlation string, if the session carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<String>,
    /// The chain of hops to the daemon that folded this session; empty if local.
    #[serde(default)]
    pub origin: Vec<OriginHop>,
    /// When the current status began.
    pub since: Timestamp,
}

/// What is running in one correlated slot.
///
/// A foreground observation gives identity, never state: it says a process by
/// this name is in the foreground, not what that process is doing. Absence of an
/// observation is not evidence of absence — an agent that was backgrounded, or
/// started under another program, is never in the foreground.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForegroundEntry {
    /// The opaque correlation string this observation is about, where the shell
    /// it was made through carried one. Absent for a shell that carried none —
    /// an observation is still worth making about a terminal nobody labelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<String>,
    /// The observed process id, in the observing daemon's process namespace.
    pub pid: u32,
    /// The process name.
    pub process: String,
    /// The process command line.
    pub cmdline: String,
    /// The chain of hops to the daemon that made the observation; empty if
    /// local. Two daemons may report the same correlation, in which case the
    /// longer chain is the inner, and better, observation.
    #[serde(default)]
    pub origin: Vec<OriginHop>,
    /// When this observation began.
    pub since: Timestamp,
    /// Whether the process is in the foreground or has been backgrounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ForegroundState>,
    /// The single established outbound TCP source port of the observed process,
    /// where it has exactly one such connection open.
    ///
    /// A monitor reports it so that an aggregator can match this observation
    /// against one made on the other side of that connection. It is a number
    /// the kernel chose, and a consumer compares it as an opaque value: nothing
    /// is inferred from its magnitude, and a process with no connection or with
    /// several is simply one this cannot speak for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_client_port: Option<u32>,
    /// The `SSH_CONNECTION` the shell this observation was made through carried,
    /// verbatim.
    ///
    /// Serves the same matching purpose from the other end, and under the same
    /// rule: it is copied from an environment variable and compared, never
    /// interpreted. The one thing anybody may do with its shape is documented
    /// where the matching is done, and it is `sshd`'s documented format rather
    /// than anything a receiver invented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_connection: Option<String>,
}

impl ForegroundEntry {
    /// Builds an observation from the parts every monitor can supply.
    pub fn new(
        pid: u32,
        process: impl Into<String>,
        cmdline: impl Into<String>,
        since: Timestamp,
    ) -> Self {
        Self {
            correlation: None,
            pid,
            process: process.into(),
            cmdline: cmdline.into(),
            origin: Vec::new(),
            since,
            state: None,
            ssh_client_port: None,
            ssh_connection: None,
        }
    }

    /// The same observation, about `correlation`.
    #[must_use]
    pub fn with_correlation(mut self, correlation: impl Into<String>) -> Self {
        self.correlation = Some(correlation.into());
        self
    }

    /// The opaque value this observation is filed under.
    ///
    /// A slot is the `correlation` where there is one and the `ssh_connection`
    /// where there is not, because those are the two things that identify the
    /// shell an observation was made through: the label whatever started it
    /// exported, or failing that the connection it arrived over. Both are
    /// strings this crate never looks inside, so a slot is compared with `==`
    /// and nothing else.
    ///
    /// It is the identity a whole shell's observations are withdrawn by; see
    /// [`ForegroundChange`].
    pub fn slot(&self) -> Option<&str> {
        self.correlation
            .as_deref()
            .or(self.ssh_connection.as_deref())
    }
}

/// Whether an observed process still holds its terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForegroundState {
    /// Running in the foreground of its controlling terminal.
    Foreground,
    /// Alive, but no longer in the foreground — suspended or backgrounded.
    Suspended,
}

/// Proof that the stream is alive rather than merely quiet.
///
/// One arrives every ten seconds whatever else is happening, so silence is
/// measurable: a subscriber that has heard nothing for about thirty seconds
/// should reconnect rather than keep waiting. Reconnecting is cheap and always
/// correct — the next stream begins with a [`Snapshot`] of the current state —
/// which is also what makes it the right response to being disconnected for any
/// other reason, including having been too slow to keep up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename = "heartbeat")]
pub struct Heartbeat {
    /// Envelope version.
    pub v: u32,
    /// When the heartbeat was written.
    pub ts: Timestamp,
    /// The daemon's sequence counter as of now.
    pub seq: u64,
}

impl Heartbeat {
    /// Builds a heartbeat.
    pub fn new(seq: u64, ts: Timestamp) -> Self {
        Self {
            v: crate::VERSION,
            ts,
            seq,
        }
    }
}

/// A change to what is running in one correlated slot.
///
/// This line is *correlation*-scoped, where every [`Kind`] is *session*-scoped,
/// which is exactly why it is a line kind of its own rather than a thirteenth
/// event kind: the status fold is per session and never sees one of these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename = "foreground_change")]
pub struct ForegroundChange {
    /// Envelope version.
    pub v: u32,
    /// The daemon's sequence number for this line.
    pub seq: u64,
    /// When the change was noticed.
    pub ts: Timestamp,
    /// The new observation, or `null` when an observation is withdrawn.
    pub foreground: Option<ForegroundEntry>,
    /// Which correlation lost its observations. Carried only on a withdrawal,
    /// where there is no entry to read it from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<String>,
    /// Which connection lost its observations, for a shell that carried no
    /// correlation to withdraw them by. Carried only on a withdrawal, and only
    /// where [`ForegroundChange::correlation`] is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_connection: Option<String>,
}

impl ForegroundChange {
    /// Reports a new or changed observation.
    pub fn observed(seq: u64, ts: Timestamp, foreground: ForegroundEntry) -> Self {
        Self {
            v: crate::VERSION,
            seq,
            ts,
            foreground: Some(foreground),
            correlation: None,
            ssh_connection: None,
        }
    }

    /// Withdraws every observation filed under a correlation.
    pub fn withdrawn(seq: u64, ts: Timestamp, correlation: impl Into<String>) -> Self {
        Self {
            v: crate::VERSION,
            seq,
            ts,
            foreground: None,
            correlation: Some(correlation.into()),
            ssh_connection: None,
        }
    }

    /// Withdraws every observation filed under a connection, for a shell that
    /// carried no correlation.
    pub fn withdrawn_connection(
        seq: u64,
        ts: Timestamp,
        ssh_connection: impl Into<String>,
    ) -> Self {
        Self {
            v: crate::VERSION,
            seq,
            ts,
            foreground: None,
            correlation: None,
            ssh_connection: Some(ssh_connection.into()),
        }
    }

    /// Withdraws every observation filed where `entry` was filed.
    ///
    /// Takes the identity off the observation being withdrawn rather than
    /// asking a caller to say which of the two fields it was, so that a
    /// withdrawal cannot name a slot the entry was never under.
    pub fn withdrawing(seq: u64, ts: Timestamp, entry: &ForegroundEntry) -> Self {
        match &entry.correlation {
            Some(correlation) => Self::withdrawn(seq, ts, correlation),
            None => Self {
                v: crate::VERSION,
                seq,
                ts,
                foreground: None,
                correlation: None,
                ssh_connection: entry.ssh_connection.clone(),
            },
        }
    }

    /// The correlation this line is about, wherever it is carried.
    pub fn correlation(&self) -> Option<&str> {
        self.foreground
            .as_ref()
            .and_then(|entry| entry.correlation.as_deref())
            .or(self.correlation.as_deref())
    }

    /// The slot this line is about: what an observation it reports is filed
    /// under, or what a withdrawal withdraws.
    ///
    /// See [`ForegroundEntry::slot`] for what a slot is and why there are two
    /// fields it can come from.
    pub fn slot(&self) -> Option<&str> {
        match &self.foreground {
            Some(entry) => entry.slot(),
            None => self
                .correlation
                .as_deref()
                .or(self.ssh_connection.as_deref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SNAPSHOT_LINE: &str = r#"
      {"v":1,"kind":"snapshot","seq":1041,
       "daemon":{"id":"9f3c1000:1000"},
       "sessions":[
         {"session":"abc123","agent":"claude","status":"working","source":"hook",
          "cwd":"/srv/project","correlation":"w9:p3","origin":[],
          "since":"2026-08-17T10:31:02.006Z"}
       ],
       "foreground":[
         {"correlation":"w9:p3","pid":4471,"process":"claude","cmdline":"claude --resume",
          "origin":[],"since":"2026-08-17T10:31:02.006Z"}
       ]}"#;

    const HEARTBEAT_LINE: &str =
        r#"{"v":1,"kind":"heartbeat","ts":"2026-08-17T10:32:11.000Z","seq":1041}"#;

    const EVENT_LINE: &str = r#"{"v":1,"seq":1041,"ts":"2026-08-17T10:32:01.412Z",
        "agent":"claude","session":"abc123","kind":"tool_start","origin":[]}"#;

    fn ts() -> Timestamp {
        Timestamp::parse("2026-08-17T10:31:02.006Z").unwrap()
    }

    /// Reads a line, writes the parsed value back, and asserts the two documents
    /// are the same field for field.
    fn assert_round_trips(line: &str) -> StreamLine {
        let parsed: StreamLine = serde_json::from_str(line).unwrap();
        let written = match &parsed {
            StreamLine::Snapshot(s) => serde_json::to_value(s).unwrap(),
            StreamLine::Heartbeat(h) => serde_json::to_value(h).unwrap(),
            StreamLine::ForegroundChange(f) => serde_json::to_value(f).unwrap(),
            StreamLine::Event(e) => serde_json::to_value(e).unwrap(),
            StreamLine::Unknown => panic!("{line} was not understood"),
        };
        let mut expected: Value = serde_json::from_str(line).unwrap();
        if matches!(parsed, StreamLine::Event(_)) {
            // An event line has no `kind` of its own on the stream, and a hook
            // event omits `source`; both are covered by the event's own tests.
            expected.as_object_mut().unwrap().remove("source");
        }
        assert_eq!(written, expected);
        parsed
    }

    #[test]
    fn documented_snapshot_round_trips() {
        let StreamLine::Snapshot(snapshot) = assert_round_trips(SNAPSHOT_LINE) else {
            panic!("expected a snapshot");
        };
        assert_eq!(snapshot.seq, 1041);
        assert_eq!(snapshot.daemon.unwrap().id, "9f3c1000:1000");
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].status, SessionStatus::Working);
        assert_eq!(snapshot.sessions[0].agent, Agent::Claude);
        assert_eq!(snapshot.sessions[0].source, Source::Hook);
        let foreground = snapshot.foreground.unwrap();
        assert_eq!(foreground[0].pid, 4471);
        assert_eq!(foreground[0].process, "claude");
        assert!(foreground[0].state.is_none());
    }

    #[test]
    fn documented_heartbeat_and_event_lines_round_trip() {
        let StreamLine::Heartbeat(heartbeat) = assert_round_trips(HEARTBEAT_LINE) else {
            panic!("expected a heartbeat");
        };
        assert_eq!(heartbeat.seq, 1041);

        let StreamLine::Event(event) = assert_round_trips(EVENT_LINE) else {
            panic!("expected an event");
        };
        assert_eq!(event.kind, Kind::ToolStart);
    }

    #[test]
    fn foreground_change_carries_an_entry_or_withdraws_one() {
        let entry =
            ForegroundEntry::new(4471, "claude", "claude --resume", ts()).with_correlation("w9:p3");
        let observed = ForegroundChange::observed(1042, ts(), entry);
        assert_eq!(
            serde_json::to_value(&observed).unwrap(),
            json!({
                "kind": "foreground_change", "v": 1, "seq": 1042,
                "ts": "2026-08-17T10:31:02.006Z",
                "foreground": {
                    "correlation": "w9:p3", "pid": 4471, "process": "claude",
                    "cmdline": "claude --resume", "origin": [],
                    "since": "2026-08-17T10:31:02.006Z"
                }
            })
        );
        assert_eq!(observed.correlation(), Some("w9:p3"));

        let withdrawn = ForegroundChange::withdrawn(1043, ts(), "w9:p3");
        let written = serde_json::to_value(&withdrawn).unwrap();
        assert_eq!(written["foreground"], Value::Null);
        assert_eq!(written["correlation"], json!("w9:p3"));
        assert_eq!(withdrawn.correlation(), Some("w9:p3"));

        for change in [observed, withdrawn] {
            let line = serde_json::to_string(&change).unwrap();
            assert_eq!(
                serde_json::from_str::<StreamLine>(&line).unwrap(),
                StreamLine::ForegroundChange(change)
            );
        }
    }

    #[test]
    fn a_foreground_entry_carries_its_optional_observations() {
        let entry = ForegroundEntry {
            state: Some(ForegroundState::Suspended),
            ssh_client_port: Some(51234),
            ssh_connection: Some("10.0.0.2 51234 10.0.0.1 22".to_owned()),
            origin: vec![OriginHop::new(OriginHop::SSH, "9f3c:1000", "fileserver")],
            ..ForegroundEntry::new(4471, "claude", "claude", ts()).with_correlation("w9:p3")
        };
        let written = serde_json::to_value(&entry).unwrap();
        assert_eq!(written["state"], json!("suspended"));
        assert_eq!(written["ssh_client_port"], json!(51234));
        assert_eq!(
            serde_json::from_value::<ForegroundEntry>(written).unwrap(),
            entry
        );
    }

    #[test]
    fn an_observation_of_a_shell_nobody_labelled_carries_the_connection_instead() {
        let entry = ForegroundEntry {
            ssh_connection: Some("10.0.0.5 51234 10.0.0.9 22".to_owned()),
            ..ForegroundEntry::new(812, "claude", "claude", ts())
        };
        let written = serde_json::to_value(&entry).unwrap();

        assert_eq!(written.get("correlation"), None, "{written}");
        assert_eq!(entry.slot(), Some("10.0.0.5 51234 10.0.0.9 22"));
        assert_eq!(
            serde_json::from_value::<ForegroundEntry>(written).unwrap(),
            entry
        );

        // A correlation is what an observation is filed under wherever there is
        // one, whatever else it carries.
        let labelled = entry.clone().with_correlation("w9:p3");
        assert_eq!(labelled.slot(), Some("w9:p3"));

        // And an observation with neither is filed under nothing at all.
        assert_eq!(
            ForegroundEntry::new(812, "claude", "claude", ts()).slot(),
            None
        );
    }

    #[test]
    fn a_withdrawal_names_the_slot_in_the_field_that_slot_came_from() {
        let labelled =
            ForegroundEntry::new(812, "claude", "claude", ts()).with_correlation("w9:p3");
        let over = ForegroundEntry {
            ssh_connection: Some("10.0.0.5 51234 10.0.0.9 22".to_owned()),
            ..ForegroundEntry::new(812, "claude", "claude", ts())
        };

        let by_correlation = ForegroundChange::withdrawing(1044, ts(), &labelled);
        assert_eq!(by_correlation.correlation(), Some("w9:p3"));
        assert_eq!(by_correlation.ssh_connection, None);
        assert_eq!(by_correlation.slot(), Some("w9:p3"));

        let by_connection = ForegroundChange::withdrawing(1045, ts(), &over);
        assert_eq!(by_connection.correlation(), None);
        assert_eq!(by_connection.slot(), Some("10.0.0.5 51234 10.0.0.9 22"));
        assert_eq!(
            serde_json::to_value(&by_connection).unwrap(),
            json!({
                "kind": "foreground_change", "v": 1, "seq": 1045,
                "ts": "2026-08-17T10:31:02.006Z",
                "foreground": Value::Null,
                "ssh_connection": "10.0.0.5 51234 10.0.0.9 22"
            })
        );

        // A shell carrying nothing at all cannot be withdrawn by name, and the
        // line says so rather than naming something it invented.
        let anonymous = ForegroundEntry::new(812, "claude", "claude", ts());
        assert_eq!(
            ForegroundChange::withdrawing(1046, ts(), &anonymous).slot(),
            None
        );

        for change in [by_correlation, by_connection] {
            let line = serde_json::to_string(&change).unwrap();
            assert_eq!(
                serde_json::from_str::<StreamLine>(&line).unwrap(),
                StreamLine::ForegroundChange(change)
            );
        }
    }

    #[test]
    fn a_snapshot_distinguishes_no_observers_from_no_observations() {
        let none = Snapshot::new(1, Vec::new());
        assert!(
            serde_json::to_value(&none)
                .unwrap()
                .get("foreground")
                .is_none()
        );
        let empty = Snapshot::new(1, Vec::new()).with_foreground(Vec::new());
        assert_eq!(
            serde_json::to_value(&empty).unwrap()["foreground"],
            json!([])
        );
    }

    #[test]
    fn an_unknown_line_kind_is_ignored_rather_than_an_error() {
        let line: StreamLine =
            serde_json::from_str(r#"{"v":1,"kind":"some_future_kind","seq":3}"#).unwrap();
        assert_eq!(line, StreamLine::Unknown);
    }

    #[test]
    fn unknown_fields_on_a_known_line_are_ignored() {
        let line: StreamLine = serde_json::from_str(
            r#"{"v":1,"kind":"heartbeat","ts":"2026-08-17T10:32:11.000Z","seq":1041,
                "invented_later":true}"#,
        )
        .unwrap();
        assert_eq!(
            line,
            StreamLine::Heartbeat(Heartbeat::new(
                1041,
                Timestamp::parse("2026-08-17T10:32:11.000Z").unwrap()
            ))
        );
    }

    #[test]
    fn a_line_without_a_kind_is_an_error() {
        assert!(serde_json::from_str::<StreamLine>(r#"{"v":1,"seq":3}"#).is_err());
    }
}
