//! The state every part of the daemon shares: the sequence counter, the session
//! table, the recent-event buffer and the publisher.
//!
//! Ingest is where an emitter's line stops being a string and becomes an event
//! this daemon vouches for. Three of the envelope's fields are the daemon's to
//! decide and are never taken from an emitter: `seq`, because a counter that
//! anyone can write is not a counter; `origin`, because an emitter has no idea
//! what it is inside, and a line arriving on this daemon's own socket has by
//! definition crossed no boundary to get here; and `ts` when the emitter did not
//! supply a usable one.
//!
//! The lock is held for the fold and the buffer write and nothing else. Reading
//! from a socket happens outside it, so a client that connects and stalls holds
//! up nothing but itself.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use agentbus_protocol::{Event, SessionEntry, SessionTable, Timestamp, UnstampedEvent};
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::clock;

/// How many recent events the daemon keeps.
///
/// Enough to hold the burst a busy session produces, so that a snapshot and the
/// live stream can be built from one consistent moment; not a replay log, and
/// deliberately not persisted anywhere.
pub const RECENT_EVENTS: usize = 1024;

/// Everything the daemon knows, and the one place events enter it.
#[derive(Debug)]
pub struct Bus {
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
}

/// The parts of the bus that only ever move together.
#[derive(Debug)]
struct State {
    table: SessionTable,
    seq: u64,
    recent: VecDeque<Event>,
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
            }),
            events,
        }
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
            event
        };

        // Nobody may be listening, and that is not a failure: the bus exists
        // whether or not anything is watching it.
        let _ = self.events.send(event.clone());
        Some(event)
    }

    /// Moves every session's clock forward, so that a session that has gone
    /// quiet becomes stale and one that is over is eventually forgotten.
    pub fn tick(&self, now: &Timestamp) {
        self.lock().table.tick(now);
    }

    /// A receiver of every event ingested from now on.
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// The sequence number of the most recently ingested event; zero before the
    /// first one.
    pub fn last_seq(&self) -> u64 {
        self.lock().seq
    }

    /// The sessions worth reporting, in the order a snapshot lists them.
    pub fn sessions(&self) -> Vec<SessionEntry> {
        self.lock().table.snapshot_sessions()
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

    use agentbus_protocol::{Agent, Kind, OriginHop, SessionStatus, Source};
    use serde_json::json;

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

        assert_eq!(events.try_recv().unwrap(), ingested);
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
