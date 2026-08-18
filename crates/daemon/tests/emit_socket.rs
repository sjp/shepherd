//! What a daemon does with what arrives on its emit socket.
//!
//! These drive the socket the way an emitter does — connect, write bytes, close
//! — rather than calling the ingest function directly, because most of what is
//! being asserted here is about connections rather than about events: that one
//! client cannot spoil another's, that nothing is ever written back, and that
//! the socket is nobody else's business.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentbus_daemon::bus::Bus;
use agentbus_daemon::emit::MAX_LINE;
use agentbus_daemon::{Daemon, SocketPaths};
use agentbus_protocol::SessionStatus;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// A daemon listening in a directory of its own, and the state behind it.
struct Running {
    bus: Arc<Bus>,
    paths: SocketPaths,
    _dir: tempfile::TempDir,
}

/// Starts a daemon on a temporary directory.
fn start() -> Running {
    let dir = tempfile::tempdir().expect("cannot make a temporary directory");
    let daemon = Daemon::bind(SocketPaths::in_dir(dir.path().join("agentbus")))
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

/// One valid event line, for a session named by the caller.
fn event(session: &str) -> Vec<u8> {
    format!(r#"{{"v":1,"agent":"claude","session":"{session}","kind":"tool_start"}}"#).into_bytes()
}

/// Connects, writes `bytes`, and closes without waiting for anything — what an
/// emitter does.
async fn emit(socket: &Path, bytes: &[u8]) {
    let mut stream = UnixStream::connect(socket).await.expect("cannot connect");
    stream.write_all(bytes).await.expect("cannot write");
    stream.shutdown().await.expect("cannot close");
}

/// Waits for the bus to have ingested `seq` events, failing the test if it does
/// not get there.
async fn wait_for_seq(bus: &Bus, seq: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while bus.last_seq() < seq {
        assert!(
            Instant::now() < deadline,
            "only {} of {seq} events were ingested",
            bus.last_seq()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        bus.last_seq(),
        seq,
        "more events were ingested than were sent"
    );
}

#[tokio::test]
async fn a_line_becomes_a_session_and_the_next_one_gets_the_next_sequence_number() {
    let running = start();

    emit(running.paths.emit(), &event("abc123")).await;
    wait_for_seq(&running.bus, 1).await;

    let sessions = running.bus.sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session, "abc123");
    assert_eq!(sessions[0].status, SessionStatus::Working);
    assert_eq!(running.bus.recent()[0].seq, 1);

    emit(running.paths.emit(), &event("def456")).await;
    wait_for_seq(&running.bus, 2).await;

    assert_eq!(running.bus.recent()[1].seq, 2);
    assert_eq!(running.bus.sessions().len(), 2);
}

#[tokio::test]
async fn a_trailing_newline_is_optional() {
    let running = start();

    let mut line = event("abc123");
    line.push(b'\n');
    emit(running.paths.emit(), &line).await;

    wait_for_seq(&running.bus, 1).await;
}

#[tokio::test]
async fn nothing_is_ever_written_back() {
    let running = start();
    let mut stream = UnixStream::connect(running.paths.emit())
        .await
        .expect("cannot connect");

    stream
        .write_all(&event("abc123"))
        .await
        .expect("cannot write");
    stream.shutdown().await.expect("cannot close");

    let mut reply = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut reply))
        .await
        .expect("the daemon never closed its end")
        .expect("cannot read");
    assert_eq!(read, 0);
    assert!(reply.is_empty(), "the daemon replied with {reply:?}");
    wait_for_seq(&running.bus, 1).await;
}

#[tokio::test]
async fn a_malformed_line_does_not_stop_the_next_one() {
    let running = start();

    emit(running.paths.emit(), b"this is not an event\n").await;
    emit(running.paths.emit(), br#"{"v":1,"agent":"claude"}"#).await;
    emit(running.paths.emit(), &event("abc123")).await;

    wait_for_seq(&running.bus, 1).await;
    assert_eq!(running.bus.sessions().len(), 1);
}

#[tokio::test]
async fn an_over_long_line_does_not_stop_the_next_one() {
    let running = start();

    let mut stream = UnixStream::connect(running.paths.emit())
        .await
        .expect("cannot connect");
    // No newline anywhere in it, so the daemon has to stop on the bound rather
    // than on a line ending. The write itself may fail once the daemon has given
    // up on this connection, which is the point.
    let _ = stream.write_all(&vec![b'x'; MAX_LINE + 4096]).await;
    drop(stream);

    emit(running.paths.emit(), &event("abc123")).await;

    wait_for_seq(&running.bus, 1).await;
    assert_eq!(running.bus.sessions()[0].session, "abc123");
}

#[tokio::test]
async fn a_client_that_connects_and_says_nothing_does_not_stop_the_next_one() {
    let running = start();

    let stalled = UnixStream::connect(running.paths.emit())
        .await
        .expect("cannot connect");

    emit(running.paths.emit(), &event("abc123")).await;
    wait_for_seq(&running.bus, 1).await;

    // Still stalled, still holding its connection open, and still ignored.
    emit(running.paths.emit(), &event("def456")).await;
    wait_for_seq(&running.bus, 2).await;
    drop(stalled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_clients_at_once_all_get_through() {
    let running = start();

    let clients: Vec<_> = (0..200)
        .map(|index| {
            let socket = running.paths.emit().to_owned();
            tokio::spawn(async move { emit(&socket, &event(&format!("session-{index}"))).await })
        })
        .collect();
    for client in clients {
        client.await.expect("a client panicked");
    }

    wait_for_seq(&running.bus, 200).await;
    assert_eq!(running.bus.sessions().len(), 200);

    let mut seqs: Vec<u64> = running.bus.recent().iter().map(|event| event.seq).collect();
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=200).collect::<Vec<u64>>());
}

#[tokio::test]
async fn the_socket_and_its_directory_are_the_owner_s_alone() {
    let running = start();

    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(running.paths.dir()), 0o700);
    assert_eq!(mode(running.paths.emit()), 0o600);
}

#[tokio::test]
async fn a_second_daemon_cannot_take_over_a_bound_socket() {
    let running = start();

    let error = Daemon::bind(running.paths.clone()).expect_err("two daemons bound one socket");

    assert!(
        error.to_string().contains("emit.sock"),
        "unhelpful error: {error}"
    );
}
