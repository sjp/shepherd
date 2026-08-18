//! `agentbus emit` against a real bus.
//!
//! The safety tests next door are about what a hook must never do. These are
//! about the ordinary case working: a payload the way an agent delivers it goes
//! in one end, and the session it describes comes out of `agentbus status` at
//! the other, with the environment's opaque correlation carried across
//! untouched. Both ways in are covered — an agent's own hook, and the ingestion
//! path any program that watches a terminal can use.

use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long a test will wait for something that should happen immediately.
const PATIENCE: Duration = Duration::from_secs(10);

/// The observation the ingestion path documents, in full.
const OBSERVATION: &str = r#"{"kind":"blocked","correlation":"w9:p3","agent":"unknown","cwd":"/x","detail":{"confidence":"low"}}"#;

/// A child process, killed when the test ends however it ends.
struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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
        .env_remove("AGENTBUS_PANE")
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

/// Starts a daemon on `dir` and waits until it is serving.
fn start_daemon(dir: &Path) -> Process {
    let daemon = Process(
        command(dir, &["daemon"])
            .stdout(Stdio::null())
            .spawn()
            .expect("cannot run agentbus"),
    );
    let socket = dir.join("emit.sock");
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

/// Runs one hook: `payload` on stdin, `AGENTBUS_PANE` set to `pane` if it is
/// given, exactly as an agent would run it.
fn emit(dir: &Path, args: &[&str], pane: Option<&str>, payload: &[u8]) -> Output {
    let mut command = command(dir, &["emit"]);
    command.args(args).stdin(Stdio::piped());
    if let Some(pane) = pane {
        command.env("AGENTBUS_PANE", pane);
    }
    let mut child = command.spawn().expect("cannot run agentbus");
    let mut stdin = child.stdin.take().expect("no stdin");
    stdin.write_all(payload).expect("cannot write the payload");
    drop(stdin);
    let output = child.wait_with_output().expect("cannot wait for agentbus");
    assert!(output.status.success(), "{}", said(&output));
    assert!(output.stdout.is_empty(), "{}", said(&output));
    output
}

/// One of the payloads captured from a real agent.
fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks/claude")
        .join(format!("{name}.json"));
    std::fs::read(&path).unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// The daemon's snapshot, as `agentbus status --json` reports it.
fn snapshot(dir: &Path) -> Value {
    let output = command(dir, &["status", "--json"])
        .output()
        .expect("cannot run agentbus");
    assert!(output.status.success(), "{}", said(&output));
    serde_json::from_slice(&output.stdout).expect("that was not a snapshot")
}

/// The one session the bus knows about, once it knows about one.
fn only_session(dir: &Path) -> Value {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let sessions = snapshot(dir)["sessions"].clone();
        match sessions.as_array().expect("sessions is not an array").len() {
            0 => assert!(
                Instant::now() < deadline,
                "nothing ever reached the bus: {sessions}"
            ),
            1 => return sessions[0].clone(),
            _ => panic!("more sessions than were sent: {sessions}"),
        }
        std::thread::sleep(Duration::from_millis(10));
    }
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

#[test]
fn a_hook_payload_becomes_the_session_it_describes() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    emit(
        &dir,
        &["--agent", "claude"],
        None,
        &fixture("UserPromptSubmit"),
    );

    let session = only_session(&dir);
    assert_eq!(session["session"], "9f2c1b7a-4d5e-4a91-8c33-1e6b0d7f2a48");
    assert_eq!(session["agent"], "claude");
    assert_eq!(session["status"], "working");
    assert_eq!(session["source"], "hook");
    assert_eq!(session["cwd"], "/srv/project");
    assert_eq!(session["correlation"], Value::Null);
}

#[test]
fn the_environments_correlation_is_carried_across_verbatim() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    // Whatever set the variable decides what it means. This one is deliberately
    // not the tidy shape anything downstream might be tempted to expect: the
    // client copies the bytes and forms no opinion about them.
    let slot = "  w9:p3 / not yours to parse  ";
    emit(
        &dir,
        &["--agent", "claude"],
        Some(slot),
        &fixture("PreToolUse"),
    );

    assert_eq!(only_session(&dir)["correlation"], slot);
}

#[test]
fn an_empty_correlation_is_the_same_as_none_at_all() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    emit(&dir, &["--agent", "claude"], Some(""), &fixture("Stop"));

    assert_eq!(only_session(&dir)["correlation"], Value::Null);
}

#[test]
fn an_observation_becomes_a_session_no_agent_reported() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    // The observer states what it was watching, so the environment's idea of
    // that is ignored: it may not have been watching this process at all.
    emit(
        &dir,
        &["--source", "observed"],
        Some("somewhere-else"),
        OBSERVATION.as_bytes(),
    );

    let session = only_session(&dir);
    assert_eq!(session["session"], "observed:w9:p3");
    assert_eq!(session["agent"], "unknown");
    assert_eq!(session["status"], "blocked");
    assert_eq!(session["source"], "observed");
    assert_eq!(session["cwd"], "/x");
    assert_eq!(session["correlation"], "w9:p3");
}

#[test]
fn a_payload_the_adapter_has_nothing_to_say_about_reaches_the_bus_as_nothing() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    // Claude's idle notification: a real payload, deliberately not an event.
    emit(
        &dir,
        &["--agent", "claude"],
        None,
        br#"{"session_id":"abc123","hook_event_name":"Notification","notification_type":"idle"}"#,
    );
    // Something that is an event, so that the assertion below is about what was
    // dropped rather than about the bus being slow.
    emit(&dir, &["--agent", "claude"], None, &fixture("SessionStart"));

    let session = only_session(&dir);
    assert_eq!(session["session"], "9f2c1b7a-4d5e-4a91-8c33-1e6b0d7f2a48");
    assert_eq!(session["status"], "starting");
}

#[test]
fn the_whole_run_of_a_session_folds_the_way_the_bus_reports_it() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    for (name, expected) in [
        ("SessionStart", "starting"),
        ("UserPromptSubmit", "working"),
        ("PreToolUse", "working"),
        ("PostToolUse", "working"),
        ("Notification", "blocked"),
        ("Stop", "idle"),
        ("SessionEnd", "done"),
    ] {
        emit(&dir, &["--agent", "claude"], Some("w9:p3"), &fixture(name));
        let deadline = Instant::now() + PATIENCE;
        while only_session(&dir)["status"] != expected {
            assert!(
                Instant::now() < deadline,
                "{name} never became {expected}: {}",
                only_session(&dir)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn the_bus_can_be_named_in_the_environment_instead_of_on_the_command_line() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    // How an installed hook is told where the bus is: its command line was
    // fixed when it was installed, and the environment is the only part of the
    // invocation that can still say anything.
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentbus"))
        .args(["emit", "--agent", "claude"])
        .env("AGENTBUS_DIR", &dir)
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cannot run agentbus");
    let mut stdin = child.stdin.take().expect("no stdin");
    stdin
        .write_all(&fixture("UserPromptSubmit"))
        .expect("cannot write the payload");
    drop(stdin);
    let output = child.wait_with_output().expect("cannot wait for agentbus");
    assert!(output.status.success(), "{}", said(&output));

    assert_eq!(only_session(&dir)["status"], "working");
}

#[test]
fn a_payload_delivered_without_being_finished_is_still_sent() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    let mut command = command(&dir, &["emit"]);
    let mut child = command
        .args(["--agent", "claude"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("cannot run agentbus");
    let mut stdin = child.stdin.take().expect("no stdin");
    stdin
        .write_all(&fixture("UserPromptSubmit"))
        .expect("cannot write the payload");
    // Deliberately still open while the client runs: an agent that writes its
    // payload and then holds the pipe has said everything it had to say, and the
    // event is worth sending even though nothing announced the end of it.
    let status = child.wait().expect("cannot wait for agentbus");
    assert!(status.success(), "exited with {status}");
    drop(stdin);

    assert_eq!(only_session(&dir)["status"], "working");
}

#[test]
fn the_client_never_waits_for_the_bus_to_answer() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    emit(
        &dir,
        &["--agent", "claude"],
        None,
        &fixture("UserPromptSubmit"),
    );
    // The daemon's side of the exchange is one-directional too, which is what
    // makes exiting without reading safe: a client that closed its end of a
    // connection nobody wrote to has missed nothing.
    let stream = UnixStream::connect(dir.join("emit.sock")).expect("cannot connect");
    stream.shutdown(Shutdown::Both).expect("cannot close");

    assert_eq!(only_session(&dir)["status"], "working");
}
