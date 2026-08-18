//! What `agentbus emit` promises the agent it runs inside.
//!
//! These are the tests to keep if every other test in this repository were
//! thrown away. Everything else here is about a bus being useful; this file is
//! about a coding agent that somebody is relying on, into which this binary is
//! injected on every tool call, and which reads what its hooks print and what
//! they exit with as decisions about that person's work. A byte on stdout can
//! rewrite what the agent believes. A non-zero exit can deny a tool call the
//! user asked for. A hook that waits is an agent that has stopped.
//!
//! So every test below runs the real binary as a real process and asserts the
//! same three things about it, whatever it was given: it exited zero, it said
//! nothing on stdout, and it was finished within a budget an agent would never
//! notice. The cases are the ways this is likely to go wrong — no daemon, a
//! daemon that has stopped listening, one that hangs up mid-sentence, a payload
//! that is nonsense, an agent nobody has written an adapter for, a command line
//! that is a typo, and a panic.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The whole of one invocation's wall-clock allowance, as seen from outside.
///
/// The client budgets itself 100 ms from the moment it gets control; the rest is
/// what it costs to start a process at all.
const LIMIT: Duration = Duration::from_millis(150);

/// A payload the Claude adapter has something to say about, so that a test is
/// never passing merely because there was nothing to send.
const HOOK_PAYLOAD: &str =
    r#"{"session_id":"abc123","hook_event_name":"UserPromptSubmit","cwd":"/srv/project"}"#;

/// What the caller of a hook can see of one run of it.
struct Ran {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    took: Duration,
}

impl Ran {
    /// Asserts the contract, which is the same for every one of these.
    fn is_harmless(&self) -> &Self {
        assert_eq!(
            self.status.code(),
            Some(0),
            "exited with {} (stderr: {})",
            self.status,
            String::from_utf8_lossy(&self.stderr)
        );
        assert!(
            self.stdout.is_empty(),
            "it said something on stdout: {}",
            String::from_utf8_lossy(&self.stdout)
        );
        assert!(
            self.took < LIMIT,
            "it took {:?}, which is over the {LIMIT:?} an agent would wait",
            self.took
        );
        self
    }
}

/// A directory for one test's bus, removed with the test.
fn bus_dir() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("cannot make a temporary directory");
    let dir = temp.path().join("bus");
    std::fs::create_dir(&dir).expect("cannot make the bus directory");
    (temp, dir)
}

/// `agentbus emit` on `dir`, with nothing inherited from whoever is running the
/// tests.
///
/// The directory is given in the environment rather than as a flag, because that
/// is how a hook is told where the bus is: the command line an agent runs is
/// fixed when the hook is installed, and the environment is the only part of it
/// that can still say anything.
fn command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
    command
        .arg("emit")
        .args(args)
        .env("AGENTBUS_DIR", dir)
        .env_remove("AGENTBUS_LOG")
        .env_remove("AGENTBUS_LOG_FILE")
        .env_remove("AGENTBUS_PANE")
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Runs one hook: hands it `payload` on stdin the way an agent does, and waits
/// for it.
///
/// The payload is written from a thread of its own and its failure is ignored,
/// because a client that has decided it has heard enough is entitled to stop
/// reading — an agent writing into a hook that has moved on sees a broken pipe,
/// not a hang, and neither should this.
fn emit(dir: &Path, args: &[&str], payload: &[u8]) -> Ran {
    let started = Instant::now();
    let mut child = command(dir, args).spawn().expect("cannot run agentbus");
    let mut stdin = child.stdin.take().expect("no stdin");
    let payload = payload.to_vec();
    let writing = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
    });
    let output = child.wait_with_output().expect("cannot wait for agentbus");
    let took = started.elapsed();
    let _ = writing.join();
    Ran {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
        took,
    }
}

/// Puts something on the emit socket that answers connections in a chosen way.
///
/// The listener is owned by a thread that outlives the test; the socket goes
/// away with the temporary directory.
fn listen(dir: &Path, answer: fn(UnixStream)) {
    let listener = UnixListener::bind(dir.join("emit.sock")).expect("cannot bind");
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            match connection {
                Ok(stream) => answer(stream),
                Err(_) => return,
            }
        }
    });
}

/// A payload big enough that it cannot be handed over in one write, so that
/// what a receiver does in the middle of one is observable.
fn oversized_hook_payload(bytes: usize) -> Vec<u8> {
    format!(
        r#"{{"session_id":"abc123","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{{"command":"{}"}}}}"#,
        "x".repeat(bytes)
    )
    .into_bytes()
}

#[test]
fn a_machine_with_no_bus_running_notices_nothing() {
    let (_temp, dir) = bus_dir();
    emit(&dir, &["--agent", "claude"], HOOK_PAYLOAD.as_bytes()).is_harmless();
}

#[test]
fn a_socket_file_with_nobody_behind_it_is_the_same_as_no_socket() {
    let (_temp, dir) = bus_dir();
    // A daemon that was killed without cleaning up leaves exactly this: the
    // socket file outlives the listener, and connecting to it is refused.
    let listener = UnixListener::bind(dir.join("emit.sock")).expect("cannot bind");
    drop(listener);
    assert!(dir.join("emit.sock").exists(), "the socket file vanished");

    emit(&dir, &["--agent", "claude"], HOOK_PAYLOAD.as_bytes()).is_harmless();
}

#[test]
fn a_bus_that_accepts_and_then_never_reads_is_given_up_on() {
    let (_temp, dir) = bus_dir();
    listen(&dir, |stream| {
        // Held open, unread, for longer than any agent would wait.
        std::thread::sleep(Duration::from_secs(30));
        drop(stream);
    });

    emit(
        &dir,
        &["--agent", "claude"],
        &oversized_hook_payload(512 * 1024),
    )
    .is_harmless();
}

#[test]
fn a_bus_that_hangs_up_in_the_middle_of_an_event_is_survived() {
    let (_temp, dir) = bus_dir();
    listen(&dir, |mut stream| {
        let mut swallowed = [0_u8; 64];
        let _ = stream.read(&mut swallowed);
        drop(stream);
    });

    // Large enough that the write cannot finish in one syscall, so the client is
    // certainly still writing when the far end disappears. Nothing here may
    // reach the process as a signal: a hook killed by `SIGPIPE` is a hook that
    // exited non-zero.
    emit(
        &dir,
        &["--agent", "claude"],
        &oversized_hook_payload(512 * 1024),
    )
    .is_harmless();
}

#[test]
fn a_payload_that_is_not_json_is_not_an_event() {
    let (_temp, dir) = bus_dir();
    for payload in [
        &b"this is not JSON"[..],
        &b""[..],
        &b"{\"session_id\": "[..],
        &b"[1, 2, 3]"[..],
        &b"\x00\x01\x02"[..],
    ] {
        emit(&dir, &["--agent", "claude"], payload).is_harmless();
    }
}

#[test]
fn a_payload_over_the_bound_is_not_an_event() {
    let (_temp, dir) = bus_dir();
    // Larger than the client will read, and never sent anywhere: the assertion
    // is that being handed more than it agreed to read costs it nothing.
    let over = oversized_hook_payload(2 * 1024 * 1024);
    listen(&dir, |mut stream| {
        let mut swallowed = Vec::new();
        let _ = stream.read_to_end(&mut swallowed);
        assert!(swallowed.is_empty(), "an over-long payload was forwarded");
    });

    emit(&dir, &["--agent", "claude"], &over).is_harmless();
}

#[test]
fn an_agent_nobody_has_written_an_adapter_for_is_not_an_event() {
    let (_temp, dir) = bus_dir();
    for agent in ["codex", "opencode", "an-agent-from-the-future", ""] {
        emit(&dir, &["--agent", agent], HOOK_PAYLOAD.as_bytes()).is_harmless();
    }
}

#[test]
fn a_command_line_that_makes_no_sense_still_exits_zero_and_says_nothing() {
    let (_temp, dir) = bus_dir();
    for args in [
        &["--invented-flag"][..],
        &["--agent"][..],
        &["--source", "guessed"][..],
        // The two ways of naming what an event is, at once.
        &["--agent", "claude", "--source", "observed"][..],
        // Neither of them.
        &[][..],
    ] {
        let ran = emit(&dir, args, HOOK_PAYLOAD.as_bytes());
        ran.is_harmless();
    }
}

#[test]
#[cfg(debug_assertions)]
fn a_panic_anywhere_inside_is_still_a_silent_exit_zero() {
    let (_temp, dir) = bus_dir();
    // A build with debug assertions on carries one agent name that panics, so
    // that this guarantee can be tested against the real process rather than
    // against something standing in for it.
    let ran = emit(
        &dir,
        &["--agent", "panic-on-purpose"],
        HOOK_PAYLOAD.as_bytes(),
    );
    ran.is_harmless();
    assert!(
        ran.stderr.is_empty(),
        "a panic announced itself: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
}

#[test]
fn a_payload_that_never_arrives_is_not_waited_for_forever() {
    let (_temp, dir) = bus_dir();
    let started = Instant::now();
    let mut child = command(&dir, &["--agent", "claude"])
        .spawn()
        .expect("cannot run agentbus");
    // Deliberately not written to and deliberately not closed: an agent that
    // spawned a hook, handed it a pipe and then went off to think about
    // something else leaves exactly this, and the hook may not wait for it.
    let stdin = child.stdin.take().expect("no stdin");
    let output = child.wait_with_output().expect("cannot wait for agentbus");
    let ran = Ran {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
        took: started.elapsed(),
    };
    drop(stdin);
    ran.is_harmless();
}

#[test]
fn diagnostics_are_off_by_default_and_never_reach_stdout_when_they_are_on() {
    let (_temp, dir) = bus_dir();
    let quiet = emit(&dir, &["--agent", "claude"], b"not JSON at all");
    quiet.is_harmless();
    assert!(
        quiet.stderr.is_empty(),
        "it said something nobody asked for: {}",
        String::from_utf8_lossy(&quiet.stderr)
    );

    let asked = {
        let started = Instant::now();
        let mut child = command(&dir, &["--agent", "claude"])
            .env("AGENTBUS_LOG", "debug")
            .spawn()
            .expect("cannot run agentbus");
        let mut stdin = child.stdin.take().expect("no stdin");
        let _ = stdin.write_all(b"not JSON at all");
        drop(stdin);
        let output = child.wait_with_output().expect("cannot wait for agentbus");
        Ran {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
            took: started.elapsed(),
        }
    };
    asked.is_harmless();
    assert!(
        !asked.stderr.is_empty(),
        "it was asked to explain itself and did not"
    );
}

#[test]
fn diagnostics_can_be_sent_somewhere_an_agent_will_not_swallow_them() {
    let (_temp, dir) = bus_dir();
    let log = dir.join("emit.log");
    let started = Instant::now();
    let mut child = command(&dir, &["--agent", "claude"])
        .env("AGENTBUS_LOG", "debug")
        .env("AGENTBUS_LOG_FILE", &log)
        .spawn()
        .expect("cannot run agentbus");
    let mut stdin = child.stdin.take().expect("no stdin");
    let _ = stdin.write_all(b"not JSON at all");
    drop(stdin);
    let output = child.wait_with_output().expect("cannot wait for agentbus");
    Ran {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
        took: started.elapsed(),
    }
    .is_harmless();

    let written = std::fs::read_to_string(&log).expect("nothing was written to the log file");
    assert!(written.contains("agentbus emit"), "the log says: {written}");
}

#[test]
fn starting_up_with_no_bus_to_talk_to_costs_almost_nothing() {
    let (_temp, dir) = bus_dir();
    // Ten runs, reported as the best of them: what is being measured is the cost
    // of the client, and the fastest run is the one least contaminated by
    // whatever else the machine running the tests is doing.
    let mut runs: Vec<Duration> = (0..10)
        .map(|_| {
            emit(&dir, &["--agent", "claude"], HOOK_PAYLOAD.as_bytes())
                .is_harmless()
                .took
        })
        .collect();
    runs.sort();
    println!(
        "cold start with no bus running: best {:?}, median {:?} (of {} runs)",
        runs[0],
        runs[runs.len() / 2],
        runs.len()
    );
}
