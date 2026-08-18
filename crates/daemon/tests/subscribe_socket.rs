//! What a subscriber reads, and what happens to one that stops reading.
//!
//! These drive the socket the way a subscriber does — connect, read lines, and
//! sometimes deliberately stop — because everything being asserted is about the
//! connection rather than about the state behind it: that the snapshot really is
//! first, that the stream really is a continuation of it, and above all that one
//! subscriber's slowness costs nobody else anything.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentbus_daemon::bus::Bus;
use agentbus_daemon::subscribe::{DEFAULT_HEARTBEAT, SUBSCRIBER_QUEUE};
use agentbus_daemon::{Daemon, Settings, SocketPaths};
use agentbus_protocol::{Kind, SessionStatus, StreamLine};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// How long a test will wait for something that should happen immediately.
const PATIENCE: Duration = Duration::from_secs(10);

/// A daemon listening in a directory of its own, and the state behind it.
struct Running {
    bus: Arc<Bus>,
    paths: SocketPaths,
    _dir: tempfile::TempDir,
}

/// Starts a daemon on a temporary directory, with the timings given.
fn start(settings: Settings) -> Running {
    let dir = tempfile::tempdir().expect("cannot make a temporary directory");
    let daemon = Daemon::bind(SocketPaths::in_dir(dir.path().join("agentbus")), settings)
        .expect("cannot start the daemon");
    let bus = Arc::clone(daemon.bus());
    let paths = daemon.paths().clone();
    tokio::spawn(daemon.run());
    Running {
        bus,
        paths,
        _dir: dir,
    }
}

/// Starts a daemon whose heartbeat is far too long to arrive during a test, so
/// that what a test reads is only what it asked for.
fn quiet() -> Running {
    start(Settings {
        heartbeat: Duration::from_secs(3600),
        ..Settings::default()
    })
}

impl Running {
    /// Connects a subscriber.
    async fn subscribe(&self) -> Subscriber {
        Subscriber {
            lines: BufReader::new(
                UnixStream::connect(self.paths.sub())
                    .await
                    .expect("cannot connect"),
            ),
        }
    }

    /// Sends one event through the emit socket, the way an emitter does.
    async fn emit(&self, session: &str, kind: &str) {
        let line = format!(r#"{{"v":1,"agent":"claude","session":"{session}","kind":"{kind}"}}"#);
        let mut stream = UnixStream::connect(self.paths.emit())
            .await
            .expect("cannot connect");
        stream
            .write_all(line.as_bytes())
            .await
            .expect("cannot write");
        stream.shutdown().await.expect("cannot close");
    }

    /// Waits for the bus to have ingested `seq` events.
    async fn wait_for_seq(&self, seq: u64) {
        let deadline = Instant::now() + PATIENCE;
        while self.bus.last_seq() < seq {
            assert!(
                Instant::now() < deadline,
                "only {} of {seq} events were ingested",
                self.bus.last_seq()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// A connected subscriber.
struct Subscriber {
    lines: BufReader<UnixStream>,
}

impl Subscriber {
    /// The next line, failing the test if none arrives.
    async fn line(&mut self) -> StreamLine {
        let line = self
            .raw()
            .await
            .expect("the daemon closed the connection early");
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error} in {line:?}"))
    }

    /// The next line as it arrived, or nothing once the daemon has closed the
    /// connection.
    async fn raw(&mut self) -> Option<String> {
        let mut line = String::new();
        let read = tokio::time::timeout(PATIENCE, self.lines.read_line(&mut line))
            .await
            .expect("nothing arrived")
            .expect("cannot read");
        (read > 0).then_some(line)
    }

    /// The snapshot every stream begins with.
    async fn snapshot(&mut self) -> agentbus_protocol::Snapshot {
        match self.line().await {
            StreamLine::Snapshot(snapshot) => snapshot,
            other => panic!("the stream began with {other:?}"),
        }
    }

    /// The next event, skipping anything else on the stream.
    async fn event(&mut self) -> agentbus_protocol::Event {
        loop {
            if let StreamLine::Event(event) = self.line().await {
                return event;
            }
        }
    }
}

#[tokio::test]
async fn a_subscriber_is_told_the_whole_state_before_it_is_told_anything_else() {
    let running = quiet();
    running.emit("abc123", "tool_start").await;
    running.emit("def456", "blocked").await;
    running.wait_for_seq(2).await;

    let mut subscriber = running.subscribe().await;
    let snapshot = subscriber.snapshot().await;

    assert_eq!(snapshot.v, 1);
    assert_eq!(snapshot.seq, 2);
    assert_eq!(snapshot.sessions.len(), 2);
    let statuses: Vec<(&str, SessionStatus)> = snapshot
        .sessions
        .iter()
        .map(|session| (session.session.as_str(), session.status))
        .collect();
    assert!(
        statuses.contains(&("abc123", SessionStatus::Working)),
        "{statuses:?}"
    );
    assert!(
        statuses.contains(&("def456", SessionStatus::Blocked)),
        "{statuses:?}"
    );
    // Nothing this daemon cannot observe is claimed to be observed, and nothing
    // says which daemon this is: both are absent rather than empty.
    assert!(snapshot.foreground.is_none());
    assert!(snapshot.daemon.is_none());
}

#[tokio::test]
async fn what_follows_the_snapshot_is_every_event_since_it_in_order() {
    let running = quiet();
    running.emit("abc123", "session_start").await;
    running.wait_for_seq(1).await;

    let mut subscriber = running.subscribe().await;
    let snapshot = subscriber.snapshot().await;

    for kind in ["turn_start", "tool_start", "tool_end", "turn_end"] {
        running.emit("abc123", kind).await;
    }

    let mut seqs = Vec::new();
    let mut kinds = Vec::new();
    for _ in 0..4 {
        let event = subscriber.event().await;
        seqs.push(event.seq);
        kinds.push(event.kind);
    }
    assert_eq!(seqs, vec![2, 3, 4, 5]);
    assert!(seqs.iter().all(|seq| *seq > snapshot.seq));
    assert_eq!(
        kinds,
        vec![
            Kind::TurnStart,
            Kind::ToolStart,
            Kind::ToolEnd,
            Kind::TurnEnd
        ]
    );
}

#[tokio::test]
async fn an_event_in_the_snapshot_is_not_sent_again() {
    let running = quiet();

    // Connecting while events are arriving is the case worth pinning: an event
    // that lands between the snapshot being built and the stream starting must
    // appear exactly once, in one of them.
    for index in 0..50 {
        running
            .emit(&format!("session-{index}"), "tool_start")
            .await;
    }
    running.wait_for_seq(50).await;
    let mut subscriber = running.subscribe().await;
    let snapshot = subscriber.snapshot().await;
    running.emit("later", "tool_start").await;

    let event = subscriber.event().await;

    assert!(
        event.seq > snapshot.seq,
        "seq {} was already in a snapshot at {}",
        event.seq,
        snapshot.seq
    );
    assert_eq!(event.session, "later");
}

#[tokio::test]
async fn the_stream_says_it_is_alive_when_nothing_is_happening() {
    let beat = Duration::from_millis(200);
    let running = start(Settings {
        heartbeat: beat,
        ..Settings::default()
    });
    running.emit("abc123", "tool_start").await;
    running.wait_for_seq(1).await;

    let mut subscriber = running.subscribe().await;
    let snapshot = subscriber.snapshot().await;
    let start = Instant::now();

    let mut beats = Vec::new();
    while beats.len() < 3 {
        if let StreamLine::Heartbeat(heartbeat) = subscriber.line().await {
            assert_eq!(heartbeat.v, 1);
            assert_eq!(heartbeat.seq, snapshot.seq);
            beats.push(start.elapsed());
        }
    }

    // On schedule rather than merely eventually: the whole point of a heartbeat
    // is that a subscriber can time it and conclude something from silence.
    let tolerance = beat / 2;
    for (index, at) in beats.iter().enumerate() {
        let expected = beat * (index as u32 + 1);
        assert!(
            at.abs_diff(expected) < tolerance,
            "heartbeat {index} arrived at {at:?}, not near {expected:?}"
        );
    }
}

#[tokio::test]
async fn the_default_heartbeat_is_the_one_subscribers_are_told_to_expect() {
    assert_eq!(DEFAULT_HEARTBEAT, Duration::from_secs(10));
    assert_eq!(Settings::default().heartbeat, DEFAULT_HEARTBEAT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_that_stops_reading_is_dropped_and_costs_nobody_else_anything() {
    let running = quiet();
    let stalled = running.subscribe().await;
    let mut healthy = running.subscribe().await;
    healthy.snapshot().await;

    // The healthy subscriber reads throughout, which is the only thing being
    // healthy means here.
    let storm = 10_000;
    let reading = tokio::spawn(async move {
        let mut seq = 0;
        while seq < storm {
            if let StreamLine::Event(event) = healthy.line().await {
                seq += 1;
                assert_eq!(event.seq, seq, "the stream skipped or repeated a line");
            }
        }
    });

    // The stalled subscriber never reads a byte, not even its snapshot.
    for index in 0..storm {
        running
            .emit(&format!("session-{index}"), "tool_start")
            .await;
    }

    // Every line reached the healthy subscriber, in order and with none missing,
    // and the daemon ingested all of it while the other one was stuck.
    reading
        .await
        .expect("the healthy subscriber lost the stream");
    running.wait_for_seq(storm).await;

    // The stalled one was disconnected rather than waited for. It cannot have
    // been sent more than its queue holds, and what it does read is whatever had
    // already been queued when the daemon gave up on it.
    let mut stalled = stalled;
    let mut read = 0;
    while stalled.raw().await.is_some() {
        read += 1;
        assert!(
            read <= SUBSCRIBER_QUEUE + 1,
            "a subscriber that never read was sent {read} lines"
        );
    }
}

#[tokio::test]
async fn a_dropped_subscriber_gets_the_current_state_when_it_comes_back() {
    let running = quiet();
    running.emit("abc123", "blocked").await;
    running.wait_for_seq(1).await;

    let mut first = running.subscribe().await;
    assert_eq!(
        first.snapshot().await.sessions[0].status,
        SessionStatus::Blocked
    );
    drop(first);

    // Whatever happened while nothing was subscribed is in the next snapshot,
    // which is the whole of a subscriber's recovery story.
    running.emit("def456", "tool_start").await;
    running.wait_for_seq(2).await;

    let mut second = running.subscribe().await;
    let snapshot = second.snapshot().await;

    assert_eq!(snapshot.seq, 2);
    assert_eq!(snapshot.sessions.len(), 2);
    let blocked = snapshot
        .sessions
        .iter()
        .find(|session| session.session == "abc123")
        .expect("the blocked session was forgotten");
    assert_eq!(blocked.status, SessionStatus::Blocked);
}

#[tokio::test]
async fn a_subscriber_that_talks_is_ignored_rather_than_answered() {
    let running = quiet();
    let mut subscriber = running.subscribe().await;

    subscriber
        .lines
        .get_mut()
        .write_all(b"{\"please\":\"send me everything twice\"}\n")
        .await
        .expect("cannot write");

    // It is still an ordinary subscriber: what it said changed nothing, and it
    // is still owed the stream.
    subscriber.snapshot().await;
    running.emit("abc123", "tool_start").await;
    assert_eq!(subscriber.event().await.session, "abc123");
}

#[tokio::test]
async fn a_subscriber_that_closes_its_writing_half_is_still_a_subscriber() {
    let running = quiet();
    let mut subscriber = running.subscribe().await;

    // Having nothing to say on a socket with nothing to say on it, and saying so
    // — which is what anything piping `/dev/null` into this socket does.
    subscriber
        .lines
        .get_mut()
        .shutdown()
        .await
        .expect("cannot close the writing half");

    subscriber.snapshot().await;
    running.emit("abc123", "tool_start").await;
    assert_eq!(subscriber.event().await.session, "abc123");
}

#[tokio::test]
async fn the_subscribe_socket_is_the_owner_s_alone() {
    let running = quiet();

    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(running.paths.sub()), 0o600);
}
