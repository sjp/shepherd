//! `agentbus emit` against a real bus.
//!
//! The safety tests next door are about what a hook must never do. These are
//! about the ordinary case working: a payload the way an agent delivers it goes
//! in one end, and the session it describes comes out of `agentbus status` at
//! the other, with the environment's opaque correlation carried across
//! untouched. Both ways in are covered — an agent's own hook, and the ingestion
//! path any program that watches a terminal can use.
//!
//! What each payload is taken to mean is the mapping inside the binary: every
//! command here is given an empty home directory of its own, so that a copy on
//! the machine running the tests cannot answer for the agents in them.

use std::io::{BufRead, BufReader, Write};
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
        .env("HOME", dir.with_file_name("home"))
        .env_remove("AGENTBUS_DIR")
        .env_remove("AGENTBUS_LOG")
        .env_remove("AGENTBUS_PANE")
        .env_remove("AGENTBUS_PROC_ROOT")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("XDG_STATE_HOME")
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

/// Starts a daemon on `dir` and waits until it is serving, with no process
/// table to read: what these tests read on the stream is what they put there.
fn start_daemon(dir: &Path) -> Process {
    let daemon = Process(
        command(dir, &["daemon"])
            .env("AGENTBUS_PROC_ROOT", dir.with_file_name("no-process-table"))
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
    let environment: Vec<(&str, &str)> = pane
        .map(|pane| ("AGENTBUS_PANE", pane))
        .into_iter()
        .collect();
    emit_in(dir, args, &environment, payload)
}

/// The same, in whatever environment the hook is being said to have inherited.
fn emit_in(dir: &Path, args: &[&str], environment: &[(&str, &str)], payload: &[u8]) -> Output {
    let mut command = command(dir, &["emit"]);
    command.args(args).stdin(Stdio::piped());
    command.envs(environment.iter().copied());
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

/// A subscriber reading the daemon's stream, for the tests that are about what
/// an event carries rather than about the session it folds into.
struct Subscriber {
    process: Process,
    printed: BufReader<std::process::ChildStdout>,
}

/// Starts one, past the snapshot every stream begins with.
fn subscribe(dir: &Path) -> Subscriber {
    let mut process = Process(
        command(dir, &["subscribe"])
            .spawn()
            .expect("cannot run agentbus"),
    );
    let mut printed = BufReader::new(process.0.stdout.take().expect("no stdout"));
    let mut snapshot = String::new();
    printed.read_line(&mut snapshot).expect("cannot read");
    assert!(snapshot.contains(r#""kind":"snapshot""#), "{snapshot:?}");
    Subscriber { process, printed }
}

impl Subscriber {
    /// The next event on the stream, past anything else that arrives first.
    ///
    /// An event's `kind` is the kind of thing that happened, so it is the lines
    /// that are *not* events which have to be named here.
    fn event(&mut self) -> Value {
        const NOT_EVENTS: [&str; 3] = ["snapshot", "heartbeat", "foreground_change"];

        let deadline = Instant::now() + PATIENCE;
        loop {
            assert!(Instant::now() < deadline, "no event ever arrived");
            let mut line = String::new();
            self.printed.read_line(&mut line).expect("cannot read");
            let line: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("{line:?}"));
            if !NOT_EVENTS.contains(&line["kind"].as_str().unwrap_or_default()) {
                let _ = &self.process;
                return line;
            }
        }
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
        .env("HOME", dir.with_file_name("home"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("XDG_STATE_HOME")
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

/// The variable `sshd` sets in every remote session.
const SSH_CONNECTION: &str = "SSH_CONNECTION";

/// The value it holds, in the four fields `sshd` documents.
const CONNECTION: &str = "10.0.0.5 51234 10.0.0.9 22";

#[test]
fn a_hook_with_no_correlation_reports_the_connection_it_was_reached_over() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);
    let mut stream = subscribe(&dir);

    emit_in(
        &dir,
        &["--agent", "claude"],
        &[(SSH_CONNECTION, CONNECTION)],
        &fixture("PreToolUse"),
    );

    let event = stream.event();
    assert_eq!(event["correlation"], Value::Null);
    assert_eq!(event["detail"]["ssh_connection"], CONNECTION);
}

#[test]
fn a_hook_that_has_a_correlation_says_that_and_says_nothing_about_the_connection() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);
    let mut stream = subscribe(&dir);

    emit_in(
        &dir,
        &["--agent", "claude"],
        &[("AGENTBUS_PANE", "w9:p3"), (SSH_CONNECTION, CONNECTION)],
        &fixture("PreToolUse"),
    );

    let event = stream.event();
    assert_eq!(event["correlation"], "w9:p3");
    assert_eq!(
        event["detail"]["ssh_connection"],
        Value::Null,
        "the shell said which shell it was, so the connection adds nothing"
    );
}

#[test]
fn the_second_correlation_name_is_read_where_the_first_is_unset() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    emit_in(
        &dir,
        &["--agent", "claude"],
        &[("LC_AGENTBUS_PANE", "w9:p3"), (SSH_CONNECTION, CONNECTION)],
        &fixture("PreToolUse"),
    );

    assert_eq!(only_session(&dir)["correlation"], "w9:p3");
}

#[test]
fn the_first_correlation_name_is_the_one_that_counts_where_both_are_set() {
    let (_temp, dir) = bus_dir();
    let _daemon = start_daemon(&dir);

    emit_in(
        &dir,
        &["--agent", "claude"],
        &[
            ("AGENTBUS_PANE", "the-one-here"),
            ("LC_AGENTBUS_PANE", "the-one-that-arrived"),
        ],
        &fixture("PreToolUse"),
    );

    assert_eq!(only_session(&dir)["correlation"], "the-one-here");
}
