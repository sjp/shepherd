use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use agentbus_protocol::{
    Agent, AssertedState, ForegroundChange, ForegroundEntry, Heartbeat, Kind, SessionEntry,
    SessionStatus, Snapshot, Source, StateAssertion, UnstampedEvent,
};
use tempfile::TempDir;

use super::*;

/// How long a test waits for something that is already on its way.
///
/// Long enough that a loaded machine does not fail a test about correctness;
/// nothing here waits for it when things are working.
const PATIENCE: Duration = Duration::from_secs(5);

/// How long these subscribers wait before reconnecting. Short, because every
/// test that reconnects is waiting for it.
const RETRY: Duration = Duration::from_millis(100);

/// How long a test's stream may be silent before its subscriber gives up on it.
///
/// Well past the length of a test that is not about silence, so that only the
/// test that asks for a shorter one ever sees a reconnection it did not script.
const QUIET: Duration = Duration::from_secs(30);

/// A bus that is not one: a socket in a directory of its own, answering each
/// connection from a script.
///
/// Every connection is answered on a thread of its own, so a script that holds
/// one open does not stop the next from being accepted — which is the whole
/// point, since what is being tested is what happens when a subscriber gives up
/// on a connection and opens another.
struct FakeBus {
    /// Held so the directory outlives the socket in it.
    _dir: TempDir,
    paths: SocketPaths,
    accepted: Arc<Mutex<Vec<Instant>>>,
}

impl FakeBus {
    /// Answers connection number `index`, counting from zero, by calling
    /// `answer` with it.
    fn new<F>(answer: F) -> Self
    where
        F: Fn(usize, &mut UnixStream) + Send + Sync + 'static,
    {
        let dir = tempfile::tempdir().expect("a directory to put a socket in");
        let paths = SocketPaths::in_dir(dir.path());
        let listener = UnixListener::bind(paths.sub()).expect("cannot bind the socket");
        let accepted = Arc::new(Mutex::new(Vec::new()));

        let answer = Arc::new(answer);
        let seen = Arc::clone(&accepted);
        thread::spawn(move || {
            for (index, connection) in listener.incoming().enumerate() {
                let Ok(mut connection) = connection else {
                    return;
                };
                seen.lock()
                    .expect("the record of connections")
                    .push(Instant::now());
                let answer = Arc::clone(&answer);
                thread::spawn(move || answer(index, &mut connection));
            }
        });

        Self {
            _dir: dir,
            paths,
            accepted,
        }
    }

    /// A subscriber pointed at this socket, with the waits a test can afford.
    fn subscriber(&self) -> Subscriber {
        Subscriber::at(self.paths.clone())
            .with_silence(QUIET)
            .with_backoff(RETRY, RETRY)
    }

    /// When each connection so far was accepted.
    fn connections(&self) -> Vec<Instant> {
        self.accepted
            .lock()
            .expect("the record of connections")
            .clone()
    }

    /// Waits until `count` connections have been made, and says when each was.
    fn wait_for(&self, count: usize) -> Vec<Instant> {
        let until = Instant::now() + PATIENCE;
        loop {
            let seen = self.connections();
            if seen.len() >= count {
                return seen;
            }
            assert!(
                Instant::now() < until,
                "waited for {count} connections and got {}",
                seen.len()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Writes one line of a stream. A connection the subscriber has already given up
/// on is not a failure of the test that scripted it.
fn write(connection: &mut UnixStream, line: &str) {
    let _ = connection.write_all(format!("{line}\n").as_bytes());
}

/// Keeps a connection open and silent, for a script that has finished talking.
///
/// Well beyond the length of any test here: what it is for is that the
/// subscriber sees a stream that is quiet rather than one that has ended.
fn hold(_connection: &mut UnixStream) {
    thread::sleep(Duration::from_secs(60));
}

/// The time these lines are stamped with, which nothing here depends on.
fn ts() -> Timestamp {
    Timestamp::parse("2026-08-17T10:31:02.006Z").expect("a well-formed timestamp")
}

fn agent() -> Agent {
    Agent::new("claude").expect("a valid agent id")
}

/// One session, as a snapshot would carry it.
fn session(id: &str, status: SessionStatus, correlation: &str) -> SessionEntry {
    SessionEntry {
        session: id.to_owned(),
        agent: agent(),
        status,
        source: Source::Hook,
        status_source: None,
        cwd: Some("/workspaces/project".to_owned()),
        correlation: Some(correlation.to_owned()),
        origin: Vec::new(),
        since: ts(),
    }
}

fn snapshot_line(seq: u64, sessions: Vec<SessionEntry>) -> String {
    serde_json::to_string(&Snapshot::new(seq, sessions)).expect("a snapshot serializes")
}

fn heartbeat_line(seq: u64) -> String {
    serde_json::to_string(&Heartbeat::new(seq, ts())).expect("a heartbeat serializes")
}

fn event_line(seq: u64, id: &str, kind: Kind) -> String {
    let event = UnstampedEvent::new(agent(), id, kind)
        .with_correlation("w1:s1")
        .stamp(seq, ts());
    serde_json::to_string(&event).expect("an event serializes")
}

fn observation_line(seq: u64, correlation: &str, process: &str) -> String {
    let entry = ForegroundEntry::new(4471, process, process, ts()).with_correlation(correlation);
    serde_json::to_string(&ForegroundChange::observed(seq, ts(), entry))
        .expect("an observation serializes")
}

/// The next thing the subscriber has to say.
fn next(subscriber: &SubscriberHandle) -> Update {
    subscriber
        .updates()
        .recv_timeout(PATIENCE)
        .expect("the subscriber said nothing at all")
}

/// The snapshot in an update that should be one.
fn reset(update: Update) -> Snapshot {
    match update {
        Update::Reset(snapshot) => snapshot,
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

#[test]
fn a_snapshot_a_heartbeat_and_events_arrive_in_the_order_the_bus_sent_them() {
    let bus = FakeBus::new(|_, connection| {
        write(
            connection,
            &snapshot_line(1, vec![session("abc123", SessionStatus::Working, "w1:s1")]),
        );
        write(connection, &heartbeat_line(1));
        write(connection, &event_line(2, "abc123", Kind::ToolStart));
        write(connection, &observation_line(3, "w1:s1", "claude"));
        write(connection, &event_line(4, "abc123", Kind::ToolEnd));
        hold(connection);
    });
    let subscriber = bus.subscriber().spawn();

    let snapshot = reset(next(&subscriber));
    assert_eq!(snapshot.seq, 1);
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].session, "abc123");

    match next(&subscriber) {
        Update::Heartbeat(heartbeat) => assert_eq!(heartbeat.seq, 1),
        other => panic!("expected a heartbeat, got {other:?}"),
    }
    match next(&subscriber) {
        Update::Event(event) => {
            assert_eq!(event.seq, 2);
            assert_eq!(event.kind, Kind::ToolStart);
        }
        other => panic!("expected an event, got {other:?}"),
    }
    match next(&subscriber) {
        Update::Foreground(change) => {
            assert_eq!(change.seq, 3);
            assert_eq!(change.correlation(), Some("w1:s1"));
        }
        other => panic!("expected an observation, got {other:?}"),
    }
    match next(&subscriber) {
        Update::Event(event) => {
            assert_eq!(event.seq, 4);
            assert_eq!(event.kind, Kind::ToolEnd);
        }
        other => panic!("expected an event, got {other:?}"),
    }

    // A stream that behaves is read on one connection and no more.
    assert_eq!(bus.connections().len(), 1);
}

#[test]
fn a_gap_in_the_sequence_numbers_is_answered_by_reconnecting() {
    let bus = FakeBus::new(|index, connection| {
        write(
            connection,
            &snapshot_line(1, vec![session("abc123", SessionStatus::Working, "w1:s1")]),
        );
        if index == 0 {
            write(connection, &event_line(2, "abc123", Kind::ToolStart));
            // Then seven lines that never arrive.
            write(connection, &event_line(10, "abc123", Kind::ToolEnd));
        }
        hold(connection);
    });
    let subscriber = bus.subscriber().spawn();

    assert_eq!(reset(next(&subscriber)).seq, 1);
    match next(&subscriber) {
        Update::Event(event) => assert_eq!(event.seq, 2),
        other => panic!("expected an event, got {other:?}"),
    }
    // The line after the gap is not passed on: it is the evidence that the
    // stream is no longer complete, not a change to apply on top of one.
    assert_eq!(next(&subscriber), Update::Disconnected);
    assert_eq!(reset(next(&subscriber)).seq, 1);
    assert!(bus.wait_for(2).len() >= 2);
}

#[test]
fn a_stream_that_says_nothing_at_all_is_answered_by_reconnecting() {
    let bus = FakeBus::new(|_, connection| {
        write(connection, &snapshot_line(1, Vec::new()));
        hold(connection);
    });
    let subscriber = bus
        .subscriber()
        .with_silence(Duration::from_millis(300))
        .spawn();

    assert_eq!(reset(next(&subscriber)).seq, 1);
    // No heartbeat, no event, and no close either: the connection is perfectly
    // healthy and perfectly useless, which only the clock can tell.
    assert_eq!(next(&subscriber), Update::Disconnected);
    assert_eq!(reset(next(&subscriber)).seq, 1);
    assert!(bus.wait_for(2).len() >= 2);
}

#[test]
fn a_connection_that_ends_is_reopened_after_a_wait_rather_than_at_once() {
    let bus = FakeBus::new(|_, connection| {
        write(connection, &snapshot_line(1, Vec::new()));
        // And the connection closes as this returns.
    });
    let subscriber = bus.subscriber().with_backoff(RETRY, RETRY).spawn();

    for _ in 0..3 {
        assert_eq!(reset(next(&subscriber)).seq, 1);
        assert_eq!(next(&subscriber), Update::Disconnected);
    }

    let connections = bus.wait_for(3);
    let spread = connections[2].duration_since(connections[0]);
    assert!(
        spread >= 2 * RETRY,
        "three connections in {spread:?} is a busy loop, not a backoff"
    );
}

#[test]
fn a_fresh_snapshot_supersedes_what_the_one_before_it_said() {
    let bus = FakeBus::new(|index, connection| {
        if index == 0 {
            write(
                connection,
                &snapshot_line(
                    1,
                    vec![
                        session("abc123", SessionStatus::Working, "w1:s1"),
                        session("def456", SessionStatus::Idle, "w1:s2"),
                    ],
                ),
            );
            // And the connection ends, as a daemon dropping a slow subscriber
            // does.
        } else {
            write(
                connection,
                &snapshot_line(9, vec![session("abc123", SessionStatus::Working, "w1:s1")]),
            );
            hold(connection);
        }
    });
    let subscriber = bus.subscriber().spawn();
    let mut state = BusState::new();

    state.apply(&next(&subscriber), &ts());
    let named: Vec<String> = state
        .sessions()
        .into_iter()
        .map(|entry| entry.session)
        .collect();
    assert_eq!(named, vec!["abc123".to_owned(), "def456".to_owned()]);
    assert!(state.connected());

    assert_eq!(next(&subscriber), Update::Disconnected);
    state.apply(&Update::Disconnected, &ts());
    assert!(!state.connected(), "a disconnection is worth knowing about");
    assert_eq!(
        state.sessions().len(),
        2,
        "losing the stream is not evidence that anything ended"
    );

    // The second snapshot does not mention the second session, and that is the
    // whole of what is known about it now.
    state.apply(&next(&subscriber), &ts());
    let named: Vec<String> = state
        .sessions()
        .into_iter()
        .map(|entry| entry.session)
        .collect();
    assert_eq!(named, vec!["abc123".to_owned()]);
    assert!(state.connected());
}

#[test]
fn a_line_of_a_kind_this_build_does_not_know_is_ignored_and_still_counted() {
    let bus = FakeBus::new(|_, connection| {
        write(connection, &snapshot_line(1, Vec::new()));
        write(connection, r#"{"v":1,"kind":"some_future_kind","seq":2}"#);
        write(connection, &event_line(3, "abc123", Kind::ToolEnd));
        hold(connection);
    });
    let subscriber = bus.subscriber().spawn();

    assert_eq!(reset(next(&subscriber)).seq, 1);
    // The line after the one nobody understood arrives, which it could not do if
    // the number the unknown line took had been ignored along with the line.
    match next(&subscriber) {
        Update::Event(event) => assert_eq!(event.seq, 3),
        other => panic!("expected an event, got {other:?}"),
    }
    assert_eq!(bus.connections().len(), 1);
}

#[test]
fn a_stream_that_does_not_begin_with_a_snapshot_is_not_read() {
    let bus = FakeBus::new(|index, connection| {
        if index == 0 {
            write(connection, &heartbeat_line(7));
            write(connection, &event_line(8, "abc123", Kind::ToolStart));
            hold(connection);
        } else {
            write(connection, &snapshot_line(1, Vec::new()));
            hold(connection);
        }
    });
    let subscriber = bus.subscriber().spawn();

    // Nothing from the first connection reaches the caller: without a snapshot
    // there is no state for any of it to be a change to.
    assert_eq!(reset(next(&subscriber)).seq, 1);
    assert!(bus.wait_for(2).len() >= 2);
}

#[test]
fn a_machine_with_no_bus_running_is_waited_for_rather_than_failed_on() {
    let dir = tempfile::tempdir().expect("a directory with no socket in it");
    let subscriber = Subscriber::at(SocketPaths::in_dir(dir.path()))
        .with_backoff(RETRY, RETRY)
        .spawn();

    assert!(
        subscriber
            .updates()
            .recv_timeout(Duration::from_millis(300))
            .is_err(),
        "there is nothing to say about a bus that is not running"
    );
    // And it stops when it is asked to, rather than when it next hears
    // something, which is never.
    subscriber.stop();
}

#[test]
fn an_observation_is_filed_under_its_slot_and_withdrawn_by_it() {
    let mut state = BusState::new();
    state.apply(
        &Update::Reset(Snapshot::new(1, Vec::new()).with_foreground(Vec::new())),
        &ts(),
    );
    assert!(state.observing());
    assert_eq!(state.foreground().count(), 0);

    let entry =
        ForegroundEntry::new(4471, "claude", "claude --resume", ts()).with_correlation("w1:s1");
    state.apply(
        &Update::Foreground(ForegroundChange::observed(2, ts(), entry.clone())),
        &ts(),
    );
    assert_eq!(state.foreground_in("w1:s1"), Some(&entry));
    assert_eq!(state.foreground().count(), 1);

    state.apply(
        &Update::Foreground(ForegroundChange::withdrawn(3, ts(), "w1:s1")),
        &ts(),
    );
    assert_eq!(state.foreground_in("w1:s1"), None);
    assert!(state.observing(), "somebody is still looking");
}

#[test]
fn a_bus_that_reports_no_observations_is_not_one_reporting_none() {
    let mut state = BusState::new();
    assert!(!state.observing());

    state.apply(&Update::Reset(Snapshot::new(1, Vec::new())), &ts());
    assert!(
        !state.observing(),
        "a snapshot with no foreground array says nobody is watching"
    );

    state.apply(
        &Update::Reset(Snapshot::new(2, Vec::new()).with_foreground(Vec::new())),
        &ts(),
    );
    assert!(
        state.observing(),
        "an empty foreground array says nothing is running"
    );
}

#[test]
fn events_and_claims_move_the_sessions_they_are_about() {
    let mut state = BusState::new();
    state.apply(
        &Update::Reset(Snapshot::new(
            1,
            vec![session("abc123", SessionStatus::Idle, "w1:s1")],
        )),
        &ts(),
    );
    assert_eq!(state.sessions()[0].status, SessionStatus::Idle);

    // An event the agent reported for itself.
    let event = UnstampedEvent::new(agent(), "abc123", Kind::ToolStart)
        .with_correlation("w1:s1")
        .stamp(2, ts());
    state.apply(&Update::Event(event), &ts());
    assert_eq!(state.sessions()[0].status, SessionStatus::Working);

    // And a claim about a slot nobody's hooks are speaking for, which is a
    // session of its own rather than a rewrite of somebody else's.
    let claim = StateAssertion::new(agent(), "w1:s9", AssertedState::Blocked)
        .with_visible(true)
        .stamp(3, ts());
    state.apply(&Update::Assertion(claim), &ts());
    let sessions = state.sessions();
    assert_eq!(sessions.len(), 2);
    let claimed = sessions
        .iter()
        .find(|entry| entry.correlation.as_deref() == Some("w1:s9"))
        .expect("the claim is a session of its own");
    assert_eq!(claimed.status, SessionStatus::Blocked);
    assert_eq!(claimed.source, Source::Observed);
}

#[test]
fn a_session_nobody_has_spoken_for_goes_quiet_when_the_clock_is_moved_on() {
    let mut state = BusState::new();
    state.apply(
        &Update::Reset(Snapshot::new(
            1,
            vec![session("abc123", SessionStatus::Working, "w1:s1")],
        )),
        &ts(),
    );
    assert_eq!(state.sessions()[0].status, SessionStatus::Working);

    // Nothing has been heard from it since, and the fold's own timeout is what
    // decides how long that may go on for.
    let later = Timestamp::from_unix_millis(
        Timestamp::parse("2026-08-17T10:31:02.006Z")
            .expect("a well-formed timestamp")
            .millis_since(&Timestamp::from_unix_millis(0))
            + 10 * 60 * 1_000,
    );
    state.tick(&later);
    assert_eq!(state.sessions()[0].status, SessionStatus::Stale);
}
