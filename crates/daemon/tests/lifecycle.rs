//! Starting a daemon, and what starting one does to the directory it is given.
//!
//! The parts of a daemon's life that need a real process — being signalled,
//! being killed outright — are exercised against the binary; these cover what
//! can be settled in one process: that the timings a daemon is started with are
//! the timings it uses, that a directory left in a mess by an earlier run is
//! usable again, and that the whole of a daemon's footprint goes away with it.

use std::time::Duration;

use agentbus_daemon::{Daemon, Settings, SocketPaths};
use agentbus_protocol::{SessionStatus, Timestamp};

/// A daemon on a directory of its own, and the directory it will be removed
/// with.
struct Started {
    daemon: Daemon,
    _dir: tempfile::TempDir,
}

/// Starts a daemon with the given timings on an empty directory.
fn start(settings: Settings) -> Started {
    let dir = tempfile::tempdir().expect("cannot make a temporary directory");
    let daemon = Daemon::bind(SocketPaths::in_dir(dir.path().join("agentbus")), settings)
        .expect("cannot start the daemon");
    Started { daemon, _dir: dir }
}

/// One event line, timestamped by the emitter so that a test can move the
/// daemon's idea of the time without waiting for it to pass.
fn event(kind: &str, at: &str) -> Vec<u8> {
    format!(r#"{{"v":1,"agent":"claude","session":"abc123","kind":"{kind}","ts":"{at}"}}"#)
        .into_bytes()
}

/// A moment in the same fixed hour the events use.
fn at(second: u8) -> Timestamp {
    Timestamp::parse(format!("2026-01-01T00:00:{second:02}.000Z")).expect("not a timestamp")
}

#[tokio::test]
async fn a_session_goes_stale_after_the_configured_time_and_not_before() {
    let started = start(Settings {
        stale_after: Duration::from_secs(5),
        ..Settings::default()
    });
    let bus = started.daemon.bus();
    bus.ingest(&event("tool_start", at(0).as_str()))
        .expect("the event was not ingested");

    bus.tick(&at(4));
    assert_eq!(bus.sessions()[0].status, SessionStatus::Working);

    bus.tick(&at(6));
    assert_eq!(bus.sessions()[0].status, SessionStatus::Stale);
}

#[tokio::test]
async fn the_default_stale_timeout_is_far_longer_than_a_configured_short_one() {
    let started = start(Settings::default());
    let bus = started.daemon.bus();
    bus.ingest(&event("tool_start", at(0).as_str()))
        .expect("the event was not ingested");

    bus.tick(&at(6));

    assert_eq!(bus.sessions()[0].status, SessionStatus::Working);
}

#[tokio::test]
async fn a_finished_session_is_forgotten_after_the_configured_retention() {
    let started = start(Settings {
        done_retention: Duration::from_secs(5),
        ..Settings::default()
    });
    let bus = started.daemon.bus();
    bus.ingest(&event("session_end", at(0).as_str()))
        .expect("the event was not ingested");
    assert_eq!(bus.sessions()[0].status, SessionStatus::Done);

    bus.tick(&at(4));
    assert_eq!(bus.sessions().len(), 1);

    bus.tick(&at(6));
    assert!(bus.sessions().is_empty());
}

#[tokio::test]
async fn the_settings_a_daemon_was_started_with_are_readable_from_it() {
    let settings = Settings {
        stale_after: Duration::from_secs(7),
        done_retention: Duration::from_secs(11),
        heartbeat: Duration::from_secs(13),
    };

    let started = start(settings);

    assert_eq!(started.daemon.settings(), settings);
}

#[tokio::test]
async fn sockets_left_behind_by_an_earlier_run_are_cleared_away() {
    let dir = tempfile::tempdir().expect("cannot make a temporary directory");
    let paths = SocketPaths::in_dir(dir.path());
    // Not sockets, just files in the way: a daemon that was killed outright
    // leaves names behind, and nothing about them says what they once were.
    for socket in paths.sockets() {
        std::fs::write(socket, b"debris").expect("cannot leave a file behind");
    }
    std::fs::write(paths.lock(), b"4242\n").expect("cannot leave a lock file behind");

    let daemon = Daemon::bind(paths.clone(), Settings::default())
        .expect("a directory left in a mess could not be reused");

    // Both are sockets this daemon bound rather than the files that were in the
    // way: nothing of what was there survived being bound over.
    for socket in daemon.paths().sockets() {
        assert!(socket.exists(), "{} was not rebound", socket.display());
        assert_ne!(
            std::fs::read(socket).ok(),
            Some(b"debris".to_vec()),
            "{} is still the file that was in the way",
            socket.display()
        );
    }
}

#[tokio::test]
async fn a_daemon_that_is_dropped_takes_its_lock_file_with_it() {
    let dir = tempfile::tempdir().expect("cannot make a temporary directory");
    let paths = SocketPaths::in_dir(dir.path().join("agentbus"));

    let daemon = Daemon::bind(paths.clone(), Settings::default()).expect("cannot start the daemon");
    assert!(paths.lock().exists());
    drop(daemon);

    assert!(!paths.lock().exists());
    Daemon::bind(paths, Settings::default()).expect("the directory was not released");
}
