//! What a daemon does with a claim an observer sends it.
//!
//! The emit socket takes two line shapes, and everything below drives the real
//! one — connect, write, close — because what is being asserted is that the
//! second shape travels the whole way: in through the socket an emitter uses,
//! into the table a snapshot is built from, and out to whoever is subscribed.
//! The arbitration itself is settled elsewhere, in the crate that owns the
//! table; these are about the wiring around it.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentbus_daemon::bus::Bus;
use agentbus_daemon::{Daemon, Settings, SocketPaths};
use agentbus_protocol::{SessionEntry, SessionStatus, Source, StreamLine};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// How long a test will wait for something that should happen promptly.
const PATIENCE: Duration = Duration::from_secs(10);

/// The slot every claim here is about.
const SLOT: &str = "w9:p3";

/// Long enough that two lines stamped either side of it are stamped at
/// different milliseconds, which is what makes a claim later than the event it
/// is being shown over.
const A_MOMENT: Duration = Duration::from_millis(5);

/// A daemon listening in a directory of its own, and the state behind it.
struct Running {
    bus: Arc<Bus>,
    paths: SocketPaths,
    _dir: tempfile::TempDir,
}

/// Starts a daemon with the given timings, on a temporary directory and in
/// front of a process table that does not exist: what is being counted here is
/// what a test sent, and a daemon reading this machine's own process table
/// would number its observations of whatever else is running in among them.
fn start(settings: Settings) -> Running {
    let dir = tempfile::tempdir().expect("cannot make a temporary directory");
    let settings = Settings {
        proc_root: dir.path().join("no-process-table"),
        heartbeat: Duration::from_secs(3600),
        ..settings
    };
    let daemon = Daemon::bind(SocketPaths::in_dir(dir.path().join("agentbus")), settings)
        .expect("cannot start the daemon")
        // Nothing on the machine this is running on is to be reached into.
        .discovering(Vec::new());
    let bus = Arc::clone(daemon.bus());
    let paths = daemon.paths().clone();
    tokio::spawn(daemon.run());
    Running {
        bus,
        paths,
        _dir: dir,
    }
}

impl Running {
    /// Sends one line to the emit socket the way an emitter does.
    async fn emit(&self, line: &str) {
        send(self.paths.emit(), line.as_bytes()).await;
    }

    /// Sends one hook event for `session`.
    async fn hook(&self, session: &str, kind: &str) {
        self.emit(&format!(
            r#"{{"v":1,"agent":"claude","session":"{session}","kind":"{kind}","correlation":"{SLOT}"}}"#
        ))
        .await;
    }

    /// Sends one claim about [`SLOT`].
    async fn claim(&self, assert: &str, visible: bool) {
        self.emit(&format!(
            r#"{{"v":1,"agent":"claude","assert":"{assert}","visible":{visible},"correlation":"{SLOT}"}}"#
        ))
        .await;
    }

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

    /// Waits for the bus to have numbered `seq` lines.
    async fn wait_for_seq(&self, seq: u64) {
        until(&format!("only {seq} lines were numbered"), || {
            self.bus.last_seq() >= seq
        })
        .await;
    }

    /// The one session the bus reports, waiting for there to be one.
    async fn only_session(&self) -> SessionEntry {
        until("no session was reported", || self.bus.sessions().len() == 1).await;
        self.bus.sessions().pop().expect("there was one")
    }
}

/// A connected subscriber.
struct Subscriber {
    lines: BufReader<UnixStream>,
}

impl Subscriber {
    /// The next line, failing the test if none arrives.
    async fn line(&mut self) -> StreamLine {
        let mut line = String::new();
        let read = tokio::time::timeout(PATIENCE, self.lines.read_line(&mut line))
            .await
            .expect("the daemon sent nothing in time")
            .expect("cannot read from the daemon");
        assert_ne!(read, 0, "the daemon closed the connection early");
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error} in {line:?}"))
    }

    /// The snapshot a subscription opens with.
    async fn snapshot(&mut self) -> agentbus_protocol::Snapshot {
        match self.line().await {
            StreamLine::Snapshot(snapshot) => snapshot,
            other => panic!("a subscription did not open with a snapshot: {other:?}"),
        }
    }
}

/// Connects, writes `bytes`, and closes without waiting for anything.
async fn send(socket: &Path, bytes: &[u8]) {
    let mut stream = UnixStream::connect(socket).await.expect("cannot connect");
    stream.write_all(bytes).await.expect("cannot write");
    stream.shutdown().await.expect("cannot close");
}

/// Waits for something that should be true shortly, failing the test if it is
/// not true within [`PATIENCE`].
async fn until(complaint: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < deadline, "{complaint}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn a_claim_about_a_slot_no_agent_is_speaking_for_becomes_a_session_and_a_line() {
    let running = start(Settings::default());
    let mut subscriber = running.subscribe().await;
    subscriber.snapshot().await;

    running.claim("blocked", true).await;

    let session = running.only_session().await;
    assert_eq!(session.session, "observed:w9:p3");
    assert_eq!(session.status, SessionStatus::Blocked);
    assert_eq!(session.source, Source::Observed);
    // Nothing arriving on this daemon's own socket crossed a boundary to get
    // here, so there is no chain on it for a deeper view to be measured against.
    assert!(session.origin.is_empty(), "{session:?}");

    let StreamLine::Assertion(published) = subscriber.line().await else {
        panic!("the claim did not reach a subscriber as a claim");
    };
    assert_eq!(published.correlation, SLOT);
    assert!(published.visible);
    assert_eq!(published.seq, running.bus.last_seq());
}

#[tokio::test]
async fn a_live_claim_is_shown_over_a_quieter_record_and_the_agent_takes_it_straight_back() {
    let running = start(Settings::default());
    running.hook("abc123", "tool_start").await;
    running.wait_for_seq(1).await;
    tokio::time::sleep(A_MOMENT).await;

    running.claim("blocked", true).await;
    running.wait_for_seq(2).await;

    let shown = running.only_session().await;
    assert_eq!(
        shown.session, "abc123",
        "the hook's session was not the row"
    );
    assert_eq!(shown.status, SessionStatus::Blocked);
    assert_eq!(shown.source, Source::Hook);
    assert_eq!(shown.status_source, Some(Source::Observed));

    // An agent calling a tool is not an agent sitting on a prompt.
    running.hook("abc123", "tool_start").await;
    until("the agent's own word did not win back its row", || {
        running
            .bus
            .sessions()
            .first()
            .is_some_and(|session| session.status_source.is_none())
    })
    .await;
    assert_eq!(running.only_session().await.status, SessionStatus::Working);
}

#[tokio::test]
async fn a_claim_nobody_repeats_stops_being_shown_once_the_hold_runs_out() {
    let running = start(Settings {
        assert_hold: Duration::from_millis(50),
        ..Settings::default()
    });
    running.hook("abc123", "tool_start").await;
    running.wait_for_seq(1).await;
    tokio::time::sleep(A_MOMENT).await;

    running.claim("blocked", true).await;
    until("the claim was never shown", || {
        running
            .bus
            .sessions()
            .first()
            .is_some_and(|session| session.status_source == Some(Source::Observed))
    })
    .await;

    // Nothing is sent again. The daemon's own tick is what takes it away.
    until("the claim was still being shown", || {
        running
            .bus
            .sessions()
            .first()
            .is_some_and(|session| session.status_source.is_none())
    })
    .await;
    assert_eq!(running.only_session().await.status, SessionStatus::Working);
}

#[tokio::test]
async fn a_subscriber_that_arrives_mid_override_learns_it_from_the_snapshot_alone() {
    let running = start(Settings::default());
    running.hook("abc123", "tool_start").await;
    running.wait_for_seq(1).await;
    tokio::time::sleep(A_MOMENT).await;
    running.claim("blocked", true).await;
    running.wait_for_seq(2).await;

    let mut arriving = running.subscribe().await;
    let snapshot = arriving.snapshot().await;

    let session = snapshot.sessions.first().expect("no session was reported");
    assert_eq!(session.session, "abc123");
    assert_eq!(session.status, SessionStatus::Blocked);
    assert_eq!(session.status_source, Some(Source::Observed));
}

#[tokio::test]
async fn a_line_whose_meaning_is_unclear_is_dropped_and_the_next_one_is_not() {
    let running = start(Settings::default());

    // Not a line at all.
    running.emit("neither one thing nor the other").await;
    // Both discriminating fields: there is no reading of it that is not a guess.
    running
        .emit(
            r#"{"v":1,"agent":"claude","session":"abc123","kind":"tool_start",
                "assert":"blocked","correlation":"w9:p3"}"#,
        )
        .await;
    // A claim with nothing to attribute it to.
    running
        .emit(r#"{"v":1,"agent":"claude","assert":"blocked"}"#)
        .await;

    running.claim("blocked", true).await;
    running.wait_for_seq(1).await;

    // One line was numbered, and it is the only one that meant anything.
    assert_eq!(running.bus.last_seq(), 1);
    assert_eq!(running.only_session().await.session, "observed:w9:p3");
}
