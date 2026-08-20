//! `agentbus subscribe` and `agentbus status` run the way a person or a script
//! runs them: as processes, against a real daemon, reading real stdout.
//!
//! What is being checked here is the command surface rather than the protocol —
//! that the stream really does reach stdout a line at a time, that a subscriber
//! which is killed and started again is told where things stand, and that
//! neither command hangs or says something unhelpful when there is no bus at
//! all.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long a test will wait for something that should happen immediately.
const PATIENCE: Duration = Duration::from_secs(10);

/// A child process, killed when the test ends however it ends.
struct Process(Option<Child>);

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Process {
    /// The process itself.
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("the process has already exited")
    }
}

/// An `agentbus` command on `dir`, with nothing inherited from whoever is
/// running the tests.
fn command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
    command
        .args(args)
        .arg("--dir")
        .arg(dir)
        .env_remove("AGENTBUS_DIR")
        .env_remove("AGENTBUS_LOG")
        .env_remove("AGENTBUS_PROC_ROOT")
        .env_remove("XDG_RUNTIME_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// A directory for one test's bus, removed with the test.
fn bus_dir() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("cannot make a temporary directory");
    let dir = temp.path().join("bus");
    (temp, dir)
}

/// Starts a daemon on `dir` and waits until it is publishing.
///
/// With no process table to read, so that it says nothing whatever about the
/// foreground. These tests assert on exactly what comes down the stream, and a
/// daemon watching this machine's own table would report the correlated shells
/// that happen to be open on it — somebody else's terminal, arriving in the
/// middle of a test's output and failing it on a machine that is merely in use.
fn start_daemon(dir: &Path) -> Process {
    let mut daemon = command(dir, &["daemon"]);
    let daemon = Process(Some(
        daemon
            .env("AGENTBUS_PROC_ROOT", dir.with_file_name("no-process-table"))
            .stdout(Stdio::null())
            .spawn()
            .expect("cannot run agentbus"),
    ));
    let socket = dir.join("sub.sock");
    let deadline = Instant::now() + PATIENCE;
    while UnixStream::connect(&socket).is_err() {
        assert!(
            Instant::now() < deadline,
            "{} never started serving",
            socket.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    daemon
}

/// Sends one event, the way an emitter does.
fn emit(dir: &Path, session: &str, kind: &str, extra: &str) {
    let line =
        format!(r#"{{"v":1,"agent":"claude","session":"{session}","kind":"{kind}"{extra}}}"#);
    let mut stream = UnixStream::connect(dir.join("emit.sock")).expect("cannot connect");
    stream.write_all(line.as_bytes()).expect("cannot write");
}

/// Sends one event and waits for the daemon to have folded it, by asking the bus
/// what it knows.
fn emit_and_settle(dir: &Path, session: &str, kind: &str, extra: &str) {
    emit(dir, session, kind, extra);
    let deadline = Instant::now() + PATIENCE;
    while !snapshot(dir).to_string().contains(session) {
        assert!(Instant::now() < deadline, "{session} never reached the bus");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The daemon's snapshot, as `agentbus status --json` reports it.
fn snapshot(dir: &Path) -> Value {
    let output = command(dir, &["status", "--json"])
        .output()
        .expect("cannot run agentbus");
    assert!(output.status.success(), "{}", said(&output));
    serde_json::from_slice(&output.stdout).expect("that was not a snapshot")
}

/// Everything a finished command said, for a failure message.
fn said(output: &Output) -> String {
    format!(
        "exited with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A running `agentbus subscribe`, and its stdout arriving line by line.
///
/// The lines are read on a thread of their own so that a test can wait for the
/// next one with a deadline: a command that has stopped producing output is the
/// failure being tested for, and a test that blocked on it forever would report
/// that as a hang rather than as a result.
struct Subscriber {
    process: Process,
    lines: Receiver<String>,
}

fn subscribe(dir: &Path) -> Subscriber {
    let mut child = command(dir, &["subscribe"])
        .spawn()
        .expect("cannot run agentbus");
    let stdout = child.stdout.take().expect("no stdout");
    let (sender, lines) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                return;
            }
        }
    });
    Subscriber {
        process: Process(Some(child)),
        lines,
    }
}

impl Subscriber {
    /// The next line, failing the test if none arrives.
    fn line(&mut self) -> Value {
        let line = match self.lines.recv_timeout(PATIENCE) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => panic!("nothing arrived on stdout"),
            Err(RecvTimeoutError::Disconnected) => panic!("the command stopped"),
        };
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error} in {line:?}"))
    }

    /// The next line of a kind a test is waiting for.
    fn line_of_kind(&mut self, kind: &str) -> Value {
        loop {
            let line = self.line();
            if line["kind"] == kind {
                return line;
            }
        }
    }

    /// Kills it, the way anything supervising it would.
    fn kill(mut self) {
        let child = self.process.child();
        child.kill().expect("cannot kill it");
        child.wait().expect("cannot reap it");
    }
}

#[test]
fn subscribe_prints_a_snapshot_and_then_the_stream() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);
    emit_and_settle(&dir, "abc123", "tool_start", "");

    let mut subscriber = subscribe(&dir);

    let snapshot = subscriber.line();
    assert_eq!(snapshot["kind"], "snapshot");
    assert_eq!(snapshot["sessions"][0]["session"], "abc123");
    assert_eq!(snapshot["sessions"][0]["status"], "working");

    emit(&dir, "abc123", "blocked", "");

    let event = subscriber.line_of_kind("blocked");
    assert_eq!(event["session"], "abc123");
    assert!(
        event["seq"].as_u64() > snapshot["seq"].as_u64(),
        "{event} did not follow the snapshot"
    );
}

#[test]
fn a_subscriber_that_is_killed_and_started_again_is_told_where_things_stand() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);
    emit_and_settle(&dir, "abc123", "blocked", "");

    let mut first = subscribe(&dir);
    assert_eq!(first.line()["kind"], "snapshot");
    first.kill();

    // The session neither ended nor changed because nothing was watching it, and
    // what happened while nothing was is in the next snapshot too.
    emit_and_settle(&dir, "def456", "tool_start", "");
    let mut second = subscribe(&dir);

    let snapshot = second.line();
    assert_eq!(snapshot["kind"], "snapshot");
    let sessions = snapshot["sessions"].as_array().expect("no sessions array");
    assert_eq!(sessions.len(), 2, "{snapshot}");
    let blocked = sessions
        .iter()
        .find(|session| session["session"] == "abc123")
        .expect("the blocked session was forgotten");
    assert_eq!(blocked["status"], "blocked");
}

#[test]
fn status_renders_what_the_bus_knows() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);
    emit_and_settle(
        &dir,
        "abc123",
        "blocked",
        r#","cwd":"/workspaces/foo","correlation":"w9:p3""#,
    );
    emit_and_settle(
        &dir,
        "observed:w9:p4",
        "tool_start",
        r#","source":"observed""#,
    );

    let output = command(&dir, &["status"])
        .output()
        .expect("cannot run agentbus");

    assert!(output.status.success(), "{}", said(&output));
    let table = String::from_utf8(output.stdout).expect("not text");
    let mut lines = table.lines();
    assert_eq!(
        lines
            .next()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<&str>>(),
        [
            "AGENT",
            "SESSION",
            "STATUS",
            "SOURCE",
            "ORIGIN",
            "CWD",
            "CORRELATION",
            "SINCE"
        ]
    );
    assert_eq!(lines.count(), 2, "{table}");
    for expected in ["claude", "abc123", "blocked", "/workspaces/foo", "w9:p3"] {
        assert!(
            table.contains(expected),
            "{expected:?} is missing from {table}"
        );
    }
    // A session the bus only inferred has to be visibly the weaker claim.
    assert!(table.contains("observed"), "{table}");
    // Not a terminal, so nothing that only a terminal understands.
    assert!(!table.contains('\x1b'), "{table:?}");
}

#[test]
fn status_on_a_quiet_bus_says_there_is_nothing_to_report() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    let output = command(&dir, &["status"])
        .output()
        .expect("cannot run agentbus");

    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "no sessions\n");
}

#[test]
fn status_in_json_prints_the_snapshot_line_and_nothing_else() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);
    emit_and_settle(&dir, "abc123", "tool_start", "");

    let output = command(&dir, &["status", "--json"])
        .output()
        .expect("cannot run agentbus");

    assert!(output.status.success(), "{}", said(&output));
    let printed = String::from_utf8(output.stdout).expect("not text");
    assert_eq!(printed.lines().count(), 1, "{printed}");
    assert!(printed.ends_with('\n'), "{printed:?}");

    // The same line a subscriber would have read, byte for byte.
    let mut subscriber = subscribe(&dir);
    let first = subscriber.line();
    let snapshot: Value = serde_json::from_str(&printed).expect("that was not JSON");
    assert_eq!(snapshot["kind"], "snapshot");
    assert_eq!(snapshot["sessions"], first["sessions"]);
}

#[test]
fn status_can_follow_the_stream_for_a_moment_after_the_table() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);
    emit_and_settle(&dir, "abc123", "tool_start", "");

    let child = command(&dir, &["status", "--recent", "5"])
        .spawn()
        .expect("cannot run agentbus");
    // The tail is live rather than a replay of anything the daemon kept, so the
    // event has to happen while the command is watching.
    std::thread::sleep(Duration::from_millis(250));
    emit(&dir, "abc123", "blocked", "");

    let output = child.wait_with_output().expect("cannot wait");

    assert!(output.status.success(), "{}", said(&output));
    let printed = String::from_utf8(output.stdout).expect("not text");
    let tail: Vec<&str> = printed.lines().skip(2).collect();
    assert_eq!(tail.len(), 1, "{printed}");
    let event: Value = serde_json::from_str(tail[0]).expect("that was not an event");
    assert_eq!(event["kind"], "blocked");
    assert_eq!(event["session"], "abc123");
}

#[test]
fn recent_follows_the_stream_briefly_when_it_is_given_no_time() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    let started = Instant::now();
    let output = command(&dir, &["status", "--recent"])
        .output()
        .expect("cannot run agentbus");
    let took = started.elapsed();

    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "no sessions\n");
    // Long enough to catch what someone is waiting to see, short enough that the
    // command is still a one-shot.
    assert!(
        took > Duration::from_millis(1_500) && took < Duration::from_secs(5),
        "it followed the stream for {took:?}"
    );
}

#[test]
fn subscribe_is_a_pipe_that_ends_when_the_bus_does() {
    let (_temp, dir) = bus_dir();
    let mut daemon = start_daemon(&dir);
    emit_and_settle(&dir, "abc123", "tool_start", "");
    let mut subscriber = Process(Some(
        command(&dir, &["subscribe"])
            .spawn()
            .expect("cannot run agentbus"),
    ));
    let mut printed = BufReader::new(subscriber.child().stdout.take().expect("no stdout"));

    // Reading its first line is what proves it is subscribed, and therefore that
    // what happens next is the daemon going away underneath it.
    let mut first = String::new();
    printed.read_line(&mut first).expect("cannot read");
    let first: Value = serde_json::from_str(&first).unwrap_or_else(|_| panic!("{first:?}"));
    assert_eq!(first["kind"], "snapshot");

    daemon.child().kill().expect("cannot kill the daemon");
    daemon.child().wait().expect("cannot reap the daemon");

    // The stream ends rather than stalls, and ending is not a failure: whoever
    // wanted it to come back is the one who restarts it.
    let mut rest = String::new();
    printed.read_to_string(&mut rest).expect("cannot read");
    let status = subscriber.child().wait().expect("cannot wait");
    assert!(status.success(), "it exited with {status}");
}

#[test]
fn without_a_daemon_both_commands_say_so_at_once() {
    let (_temp, dir) = bus_dir();
    std::fs::create_dir_all(&dir).expect("cannot make the directory");

    for args in [["subscribe"], ["status"]] {
        let started = Instant::now();
        let output = command(&dir, &args).output().expect("cannot run agentbus");
        let took = started.elapsed();

        assert_eq!(output.status.code(), Some(1), "{}", said(&output));
        assert!(output.stdout.is_empty(), "{}", said(&output));
        let complaint = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            complaint.lines().count(),
            1,
            "more than one line of complaint: {complaint}"
        );
        assert!(
            complaint.contains(&format!("no daemon running in {}", dir.display())),
            "unhelpful complaint: {complaint}"
        );
        // Nothing was waited for: there is nothing there to wait for.
        assert!(took < Duration::from_millis(500), "{args:?} took {took:?}");
    }
}
