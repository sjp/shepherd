//! The state every part of the daemon shares: the sequence counter, the session
//! table, the foreground observations, the recent-event buffer and the
//! publisher.
//!
//! Ingest is where an emitter's line stops being a string and becomes an event
//! this daemon vouches for. Three of the envelope's fields are the daemon's to
//! decide and are never taken from an emitter: `seq`, because a counter that
//! anyone can write is not a counter; `origin`, because an emitter has no idea
//! what it is inside, and a line arriving on this daemon's own socket has by
//! definition crossed no boundary to get here; and `ts` when the emitter did not
//! supply a usable one.
//!
//! The lock is held for stamping, the fold, the buffer write and the publish,
//! and for nothing else. Those four are one step: an event's sequence number is
//! the order subscribers are promised, so the moment it is numbered is the
//! moment it has to be handed to them, or two ingests racing could publish 41
//! after 42. Reading from a socket happens outside the lock, so a client that
//! connects and stalls holds up nothing but itself, and publishing under it
//! costs nothing that could block: a full subscriber is dropped rather than
//! waited for.
//!
//! What the process table says goes through the same door for the same reason.
//! An observation is numbered from the same counter as an event and published
//! from under the same lock, so the stream a subscriber reads has one order in
//! it rather than two that have to be reconciled.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use agentbus_protocol::{
    Event, ForegroundChange, ForegroundEntry, SessionEntry, SessionTable, Snapshot, Timestamp,
    UnstampedEvent,
};
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::clock;
use crate::foreground::Transition;
use crate::procfs::Pid;

/// How many recent events the daemon keeps.
///
/// Enough to hold the burst a busy session produces, so that a snapshot and the
/// live stream can be built from one consistent moment; not a replay log, and
/// deliberately not persisted anywhere.
pub const RECENT_EVENTS: usize = 1024;

/// One line the bus publishes to everything watching it.
///
/// Both kinds travel on one channel because they are numbered from one counter
/// and a subscriber is promised that counter's order. Two channels would be free
/// to deliver 42 after 43.
#[derive(Debug, Clone, PartialEq)]
pub enum Published {
    /// An event that was ingested.
    Event(Event),
    /// A change in what is running in front of a correlated shell.
    Foreground(ForegroundChange),
}

impl Published {
    /// The sequence number this line was stamped with.
    pub fn seq(&self) -> u64 {
        match self {
            Self::Event(event) => event.seq,
            Self::Foreground(change) => change.seq,
        }
    }
}

/// Everything the daemon knows, and the one place events enter it.
#[derive(Debug)]
pub struct Bus {
    state: Mutex<State>,
    events: broadcast::Sender<Published>,
}

/// The parts of the bus that only ever move together.
#[derive(Debug)]
struct State {
    table: SessionTable,
    seq: u64,
    recent: VecDeque<Event>,
    /// What is in the foreground of each correlated shell, keyed by the
    /// correlation and the shell it was seen through, or `None` where nothing
    /// is watching the process table at all.
    ///
    /// Two shells may carry one correlation, and they are two answers to one
    /// question rather than one answer twice, so the shell is part of the key.
    foreground: Option<BTreeMap<(String, Pid), ForegroundEntry>>,
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus {
    /// An empty bus with no sessions and no events.
    pub fn new() -> Self {
        Self::with_table(SessionTable::new())
    }

    /// An empty bus whose fold and retention are configured by the caller.
    pub fn with_table(table: SessionTable) -> Self {
        let (events, _) = broadcast::channel(RECENT_EVENTS);
        Self {
            state: Mutex::new(State {
                table,
                seq: 0,
                recent: VecDeque::with_capacity(RECENT_EVENTS),
                foreground: None,
            }),
            events,
        }
    }

    /// The same bus, reporting foreground observations.
    ///
    /// Whether anything is watching the process table is settled before the bus
    /// serves anything, because "nobody is looking" and "nobody is running
    /// anything" are different facts and a snapshot has to be able to say which
    /// one it means. A bus that is not watching says nothing whatever about the
    /// foreground; one that is says so from its first snapshot, when the answer
    /// is still an empty list.
    #[must_use]
    pub fn observing(mut self) -> Self {
        self.state
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .foreground = Some(BTreeMap::new());
        self
    }

    /// Turns one received line into a stamped event, folds it into its session,
    /// records it and publishes it. Returns the event, or nothing if the line
    /// was not an event at all.
    ///
    /// A line that does not parse is dropped: the emitter is a hook inside
    /// somebody's coding agent and cannot be told about it, and a daemon that
    /// stopped ingesting because one client sent nonsense would be trading every
    /// other session's status for a message nobody reads.
    pub fn ingest(&self, line: &[u8]) -> Option<Event> {
        let (mut event, reported_ts) = match parse(line) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug!(%error, "dropped a line that is not an event");
                return None;
            }
        };
        // Nothing received here came from anywhere else.
        event.origin.clear();
        let ts = reported_ts.unwrap_or_else(clock::now);

        let event = {
            let mut state = self.lock();
            state.seq += 1;
            let event = event.stamp(state.seq, ts);
            if let Some(conflict) = state.table.apply_event(&event) {
                warn!(
                    agent = %conflict.key.agent,
                    session = %conflict.key.session,
                    "an event's origin disagrees with the chain this session was first seen with"
                );
            }
            if state.recent.len() == RECENT_EVENTS {
                state.recent.pop_front();
            }
            state.recent.push_back(event.clone());
            // Published while the number is still being handed out, because
            // subscribers are promised the stream in `seq` order and two
            // connections ingesting at once would otherwise be free to reach the
            // publisher in the opposite order to the one they were numbered in.
            // Nobody may be listening, and that is not a failure: the bus exists
            // whether or not anything is watching it.
            let _ = self.events.send(Published::Event(event.clone()));
            event
        };
        Some(event)
    }

    /// Moves every session's clock forward, so that a session that has gone
    /// quiet becomes stale and one that is over is eventually forgotten.
    pub fn tick(&self, now: &Timestamp) {
        self.lock().table.tick(now);
    }

    /// Records what a foreground monitor saw, publishes a line for each change,
    /// and hands back the pids it reported as having ended.
    ///
    /// A transition is by construction news — the monitor produces one only when
    /// something differs from what it last reported — so every one of them is
    /// numbered and published. That is also why the numbering happens here,
    /// under the lock ingest stamps events under: a subscriber reads one stream
    /// and is owed one order for it.
    ///
    /// The table a snapshot is built from is maintained *from the transitions*
    /// rather than copied from whatever produced them, which is what makes the
    /// two halves of a subscription agree: a subscriber that applies the lines
    /// it reads to the snapshot it started with arrives at exactly this table.
    ///
    /// The pids that ended are returned rather than acted on. What the end of a
    /// process means for a session that was running under it is the fold's
    /// business, and this is the only place that hears about it.
    pub fn observed(&self, transitions: &[Transition], now: &Timestamp) -> Vec<Pid> {
        let gone: Vec<Pid> = transitions
            .iter()
            .filter_map(|change| change.gone)
            .collect();
        let mut state = self.lock();
        let State {
            seq, foreground, ..
        } = &mut *state;
        // Nothing is watching, so there is nothing to report and nothing that
        // could have produced a transition in the first place.
        let Some(table) = foreground.as_mut() else {
            return gone;
        };

        for transition in transitions {
            *seq += 1;
            let key = (transition.correlation.clone(), transition.shell);
            let line = match &transition.foreground {
                Some(entry) => {
                    table.insert(key, entry.clone());
                    ForegroundChange::observed(*seq, now.clone(), entry.clone())
                }
                None => {
                    table.remove(&key);
                    ForegroundChange::withdrawn(*seq, now.clone(), &transition.correlation)
                }
            };
            let _ = self.events.send(Published::Foreground(line));
        }
        gone
    }

    /// A receiver of every line published from now on.
    pub fn events(&self) -> broadcast::Receiver<Published> {
        self.events.subscribe()
    }

    /// Everything the bus knows now, and a receiver of everything it learns
    /// afterwards.
    ///
    /// The two are taken together under the lock that ingest also stamps and
    /// publishes under, so an event is on exactly one side of the join: either
    /// it is in the snapshot, or it arrives on the receiver. A reader that
    /// ignores what the snapshot already covers is therefore reading a
    /// continuation of it, and stays right whatever the bus does with its own
    /// ordering later.
    pub fn subscribe(&self) -> (Snapshot, broadcast::Receiver<Published>) {
        let state = self.lock();
        let snapshot = state.table.snapshot(state.seq);
        let snapshot = match &state.foreground {
            Some(table) => snapshot.with_foreground(table.values().cloned().collect()),
            None => snapshot,
        };
        (snapshot, self.events.subscribe())
    }

    /// The sequence number the most recently published line was stamped with;
    /// zero before there has been one.
    pub fn last_seq(&self) -> u64 {
        self.lock().seq
    }

    /// The sessions worth reporting, in the order a snapshot lists them.
    pub fn sessions(&self) -> Vec<SessionEntry> {
        self.lock().table.snapshot_sessions()
    }

    /// The foreground observations a snapshot would carry, or nothing where the
    /// bus is not watching the process table.
    pub fn foreground(&self) -> Option<Vec<ForegroundEntry>> {
        self.lock()
            .foreground
            .as_ref()
            .map(|table| table.values().cloned().collect())
    }

    /// The events still in the recent buffer, oldest first.
    pub fn recent(&self) -> Vec<Event> {
        self.lock().recent.iter().cloned().collect()
    }

    /// The shared state.
    ///
    /// A panic somewhere under this lock poisons it. The daemon carries on with
    /// the state as it was left: the alternative is that one malformed session
    /// takes down the bus for every other session on the machine.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Reads one line as an event, along with the timestamp it reported if that
/// timestamp is one this protocol can carry.
///
/// An emitter's `ts` is kept because it is closer to when the thing actually
/// happened than the moment the daemon got round to reading the socket. It is
/// not trusted any further than that: anything that is not a well-formed
/// timestamp is replaced rather than rejected, since the event itself is still
/// news.
fn parse(line: &[u8]) -> Result<(UnstampedEvent, Option<Timestamp>), serde_json::Error> {
    let value: Value = serde_json::from_slice(line)?;
    let ts = value
        .get("ts")
        .and_then(Value::as_str)
        .and_then(|ts| Timestamp::parse(ts).ok());
    Ok((serde_json::from_value(value)?, ts))
}

#[cfg(test)]
mod tests {
    use super::*;

    use agentbus_protocol::{Agent, ForegroundState, Kind, OriginHop, SessionStatus, Source};
    use serde_json::json;

    /// A bus that is watching a process table, as one with a monitor behind it
    /// is.
    fn watching() -> Bus {
        Bus::new().observing()
    }

    fn at(text: &str) -> Timestamp {
        Timestamp::parse(text).expect("not a timestamp")
    }

    fn now() -> Timestamp {
        at("2026-08-17T10:32:01.412Z")
    }

    /// What a monitor produces when it first sees something in front of a shell.
    fn appeared(correlation: &str, shell: Pid, pid: u32, process: &str) -> Transition {
        let mut entry = ForegroundEntry::new(correlation, pid, process, process, now());
        entry.state = Some(ForegroundState::Foreground);
        Transition {
            correlation: correlation.to_owned(),
            shell,
            foreground: Some(entry),
            gone: None,
        }
    }

    /// What it produces when there is no longer anything to report there.
    fn withdrawn(correlation: &str, shell: Pid, gone: Option<Pid>) -> Transition {
        Transition {
            correlation: correlation.to_owned(),
            shell,
            foreground: None,
            gone,
        }
    }

    fn line(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    fn tool_start() -> Value {
        json!({"v": 1, "agent": "claude", "session": "abc123", "kind": "tool_start"})
    }

    #[test]
    fn an_event_is_stamped_folded_and_recorded() {
        let bus = Bus::new();

        let event = bus.ingest(&line(&tool_start())).unwrap();

        assert_eq!(event.seq, 1);
        assert_eq!(event.agent, Agent::Claude);
        assert_eq!(event.kind, Kind::ToolStart);
        assert_eq!(bus.last_seq(), 1);
        assert_eq!(bus.recent(), vec![event.clone()]);

        let sessions = bus.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session, "abc123");
        assert_eq!(sessions[0].status, SessionStatus::Working);
        assert_eq!(sessions[0].since, event.ts);
    }

    #[test]
    fn sequence_numbers_start_at_one_and_never_repeat() {
        let bus = Bus::new();

        let seqs: Vec<u64> = (0..3)
            .map(|_| bus.ingest(&line(&tool_start())).unwrap().seq)
            .collect();

        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(bus.last_seq(), 3);
    }

    #[test]
    fn a_reported_timestamp_is_kept_and_a_missing_one_is_stamped() {
        let bus = Bus::new();
        let mut reported = tool_start();
        reported["ts"] = json!("2026-08-17T10:32:01.412Z");

        let kept = bus.ingest(&line(&reported)).unwrap();
        assert_eq!(kept.ts.as_str(), "2026-08-17T10:32:01.412Z");

        let stamped = bus.ingest(&line(&tool_start())).unwrap();
        assert!(stamped.ts.as_str() > "2020-01-01T00:00:00.000Z");
    }

    #[test]
    fn a_timestamp_this_protocol_cannot_carry_is_replaced() {
        let bus = Bus::new();
        let mut event = tool_start();
        event["ts"] = json!("yesterday afternoon");

        let ingested = bus.ingest(&line(&event)).unwrap();

        assert!(ingested.ts.as_str() > "2020-01-01T00:00:00.000Z");
    }

    #[test]
    fn an_origin_chain_claimed_by_an_emitter_is_discarded() {
        let bus = Bus::new();
        let mut event = tool_start();
        event["origin"] = json!([{"kind": "ssh", "id": "9f3c:1000", "name": "fileserver"}]);

        let ingested = bus.ingest(&line(&event)).unwrap();

        assert_eq!(ingested.origin, Vec::<OriginHop>::new());
        assert_eq!(bus.sessions()[0].origin, Vec::<OriginHop>::new());
    }

    #[test]
    fn the_rest_of_the_envelope_survives_verbatim() {
        let bus = Bus::new();
        let mut event = tool_start();
        event["source"] = json!("observed");
        event["cwd"] = json!("/srv/project");
        event["correlation"] = json!("w9:p3");
        event["detail"] = json!({"tool": "Bash"});
        event["raw"] = json!({"hook_event_name": "PreToolUse"});

        let ingested = bus.ingest(&line(&event)).unwrap();

        assert_eq!(ingested.source, Source::Observed);
        assert_eq!(ingested.cwd.as_deref(), Some("/srv/project"));
        assert_eq!(ingested.correlation.as_deref(), Some("w9:p3"));
        assert_eq!(ingested.detail.unwrap()["tool"], json!("Bash"));
        assert_eq!(
            ingested.raw.unwrap(),
            json!({"hook_event_name": "PreToolUse"})
        );
    }

    #[test]
    fn a_line_that_is_not_an_event_is_dropped_without_consuming_a_sequence_number() {
        let bus = Bus::new();

        assert!(bus.ingest(b"not json at all").is_none());
        assert!(bus.ingest(b"{}").is_none());
        assert!(
            bus.ingest(&line(&json!({"v": 1, "agent": "claude"})))
                .is_none()
        );
        assert!(bus.ingest(b"").is_none());
        assert_eq!(bus.last_seq(), 0);
        assert!(bus.sessions().is_empty());

        assert_eq!(bus.ingest(&line(&tool_start())).unwrap().seq, 1);
    }

    #[test]
    fn an_unknown_kind_is_carried_rather_than_rejected() {
        let bus = Bus::new();
        let mut event = tool_start();
        event["kind"] = json!("sang_a_song");

        let ingested = bus.ingest(&line(&event)).unwrap();

        assert_eq!(ingested.kind, Kind::Unknown("sang_a_song".to_owned()));
    }

    #[test]
    fn every_ingested_event_reaches_a_subscriber() {
        let bus = Bus::new();
        let mut events = bus.events();

        let ingested = bus.ingest(&line(&tool_start())).unwrap();

        assert_eq!(events.try_recv().unwrap(), Published::Event(ingested));
    }

    #[test]
    fn ingest_does_not_depend_on_anything_listening() {
        let bus = Bus::new();
        drop(bus.events());

        assert!(bus.ingest(&line(&tool_start())).is_some());
    }

    #[test]
    fn the_recent_buffer_keeps_the_newest_events_and_no_more() {
        let bus = Bus::new();

        for _ in 0..RECENT_EVENTS + 10 {
            bus.ingest(&line(&tool_start()));
        }

        let recent = bus.recent();
        assert_eq!(recent.len(), RECENT_EVENTS);
        assert_eq!(recent.first().unwrap().seq, 11);
        assert_eq!(recent.last().unwrap().seq, (RECENT_EVENTS + 10) as u64);
    }

    #[test]
    fn an_observation_is_recorded_numbered_and_published() {
        let bus = watching();
        let mut published = bus.events();

        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());

        let observations = bus.foreground().expect("this bus is watching");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].correlation, "w9:p3");
        assert_eq!(observations[0].pid, 4471);
        assert_eq!(bus.last_seq(), 1);
        let Published::Foreground(line) = published.try_recv().unwrap() else {
            panic!("an observation was published as something else");
        };
        assert_eq!(line.seq, 1);
        assert_eq!(line.ts, now());
        assert_eq!(line.foreground.unwrap().process, "claude");
    }

    #[test]
    fn observations_and_events_are_numbered_from_the_one_counter() {
        let bus = watching();

        bus.ingest(&line(&tool_start()));
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());
        let last = bus.ingest(&line(&tool_start())).unwrap();

        assert_eq!(last.seq, 3);
        assert_eq!(bus.last_seq(), 3);
    }

    #[test]
    fn a_withdrawal_takes_the_observation_away_and_says_which_correlation() {
        let bus = watching();
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());
        let mut published = bus.events();

        bus.observed(&[withdrawn("w9:p3", 100, Some(4471))], &now());

        assert_eq!(bus.foreground(), Some(Vec::new()));
        let Published::Foreground(line) = published.try_recv().unwrap() else {
            panic!("a withdrawal was published as something else");
        };
        assert_eq!(line.foreground, None);
        assert_eq!(line.correlation.as_deref(), Some("w9:p3"));
    }

    #[test]
    fn a_process_that_ended_is_handed_back_to_the_caller() {
        let bus = watching();

        let gone = bus.observed(
            &[
                withdrawn("w9:p3", 100, Some(4471)),
                withdrawn("w9:p4", 101, None),
            ],
            &now(),
        );

        assert_eq!(gone, vec![4471]);
    }

    #[test]
    fn two_shells_carrying_one_correlation_are_two_observations() {
        let bus = watching();

        bus.observed(
            &[
                appeared("w9:p3", 100, 4471, "claude"),
                appeared("w9:p3", 200, 5512, "vim"),
            ],
            &now(),
        );

        let observations = bus.foreground().expect("this bus is watching");
        assert_eq!(observations.len(), 2);
        // One of them going away leaves the other where it was.
        bus.observed(&[withdrawn("w9:p3", 100, None)], &now());
        let observations = bus.foreground().expect("this bus is watching");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].pid, 5512);
    }

    #[test]
    fn a_bus_that_is_not_watching_says_nothing_about_the_foreground() {
        let bus = Bus::new();
        let mut published = bus.events();

        let gone = bus.observed(&[withdrawn("w9:p3", 100, Some(4471))], &now());

        assert_eq!(
            gone,
            vec![4471],
            "what ended is news to the caller either way"
        );
        assert_eq!(bus.foreground(), None);
        assert_eq!(bus.last_seq(), 0, "a line nobody was sent was numbered");
        assert!(published.try_recv().is_err());
    }

    #[test]
    fn a_snapshot_reports_the_foreground_only_from_a_bus_that_is_watching() {
        let watching = watching();
        watching.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());

        let (snapshot, _) = watching.subscribe();
        assert_eq!(snapshot.foreground.as_deref().map(<[_]>::len), Some(1));
        assert_eq!(snapshot.seq, 1);

        let (snapshot, _) = Bus::new().subscribe();
        assert_eq!(snapshot.foreground, None);

        let (snapshot, _) = Bus::new().observing().subscribe();
        assert_eq!(snapshot.foreground, Some(Vec::new()));
    }

    #[test]
    fn ticking_advances_the_sessions_clock() {
        let bus = Bus::new();
        let mut event = tool_start();
        event["ts"] = json!("2026-08-17T10:32:01.412Z");
        bus.ingest(&line(&event));
        assert_eq!(bus.sessions()[0].status, SessionStatus::Working);

        bus.tick(&Timestamp::parse("2026-08-17T11:32:01.412Z").unwrap());

        assert_eq!(bus.sessions()[0].status, SessionStatus::Stale);
    }
}
