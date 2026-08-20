//! Subscribing to a bus on a machine where nobody has started one.
//!
//! The subject is what happens to the daemon rather than what arrives on the
//! stream, so these tests are about processes: that one is started, that it is
//! still there after the subscriber that wanted it has gone, and that the second
//! caller gets the first caller's daemon rather than another one.
//!
//! Everything goes through the built binary. A detached daemon is made out of
//! `fork`, `setsid` and an exec, none of which mean anything in a test that
//! calls a library function.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use agentbus_protocol::StreamLine;

/// How long a test waits for something that should happen immediately.
const PATIENCE: Duration = Duration::from_secs(10);

/// The environment variables that would otherwise decide, behind a test's back,
/// which bus a command talks to.
const INHERITED: &[&str] = &[
    "AGENTBUS_DIR",
    "AGENTBUS_LOG",
    "AGENTBUS_PANE",
    "AGENTBUS_PROC_ROOT",
    "AGENTBUS_STALE_SECS",
    "AGENTBUS_DONE_RETENTION_SECS",
    "AGENTBUS_ASSERT_HOLD_SECS",
    "XDG_RUNTIME_DIR",
];

/// A bus directory nobody is serving, and whatever ends up serving it.
///
/// The daemon these tests start is deliberately not a child of anything, so
/// nothing reaps it when a test ends. This is what does instead, and it has to:
/// a daemon left holding a directory that is about to be deleted would go on
/// running for as long as the test binary did.
struct Somewhere {
    dir: PathBuf,
    _temp: tempfile::TempDir,
}

impl Drop for Somewhere {
    fn drop(&mut self) {
        if let Some(pid) = self.serving() {
            // Safe by construction: a signal number and a pid this test read
            // from the lock file of a daemon it started itself.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

impl Somewhere {
    /// An empty directory, with no daemon and no sockets in it.
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("cannot make a temporary directory");
        let dir = temp.path().join("bus");
        Self { dir, _temp: temp }
    }

    fn dir(&self) -> &Path {
        &self.dir
    }

    /// An `agentbus` command against this directory, with nothing inherited from
    /// whoever is running the tests.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
        command
            .args(args)
            .arg("--dir")
            .arg(&self.dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for variable in INHERITED {
            command.env_remove(variable);
        }
        command
    }

    /// Starts a subscriber that will start a daemon if it has to.
    fn subscribe(&self, args: &[&str]) -> Subscriber {
        let mut child = self
            .command(args)
            .spawn()
            .expect("cannot run agentbus subscribe");
        let stdout = child.stdout.take().expect("no stdout");
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        Subscriber { child, lines }
    }

    /// The pid of the daemon serving this directory, as its lock file names it,
    /// or nothing if no daemon has claimed it.
    fn serving(&self) -> Option<libc::pid_t> {
        let written = std::fs::read_to_string(self.dir.join("daemon.lock")).ok()?;
        written.trim().parse().ok()
    }

    /// Whether anything is listening on the subscribe socket right now.
    fn answering(&self) -> bool {
        UnixStream::connect(self.dir.join("sub.sock")).is_ok()
    }

    /// What the daemon said about itself, for a failure message.
    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("daemon.log")).unwrap_or_default()
    }
}

/// A running `agentbus subscribe`, and the stream arriving from it.
struct Subscriber {
    child: Child,
    lines: Receiver<String>,
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Subscriber {
    /// The first line of the stream, which is always the snapshot.
    fn snapshot(&mut self) -> StreamLine {
        let line = match self.lines.recv_timeout(PATIENCE) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => panic!("nothing arrived on the stream"),
            Err(RecvTimeoutError::Disconnected) => panic!("the subscriber said nothing at all"),
        };
        let parsed: StreamLine =
            serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error} in {line:?}"));
        assert!(
            matches!(parsed, StreamLine::Snapshot(_)),
            "the stream began with {parsed:?}"
        );
        parsed
    }

    /// Stops the subscriber the way a killed one stops: without saying goodbye.
    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Waits for `wanted` to be true, or fails the test saying what it was waiting
/// for.
fn until(what: &str, wanted: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !wanted() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn subscribing_to_a_directory_nobody_serves_is_a_failure() {
    let somewhere = Somewhere::new();

    let output = somewhere
        .command(&["subscribe"])
        .output()
        .expect("cannot run agentbus");

    assert!(!output.status.success());
    assert!(somewhere.serving().is_none());
}

#[test]
fn asking_for_a_daemon_starts_one_and_subscribes_to_it() {
    let somewhere = Somewhere::new();

    let mut subscriber = somewhere.subscribe(&["subscribe", "--ensure-daemon"]);

    subscriber.snapshot();
    assert!(
        somewhere.serving().is_some(),
        "nothing claimed the directory: {}",
        somewhere.log()
    );
    assert!(
        !somewhere.log().is_empty(),
        "the daemon said nothing at all"
    );
}

#[test]
fn the_daemon_that_was_started_outlives_the_subscriber_that_wanted_it() {
    let somewhere = Somewhere::new();
    let mut subscriber = somewhere.subscribe(&["subscribe", "--ensure-daemon"]);
    subscriber.snapshot();
    let started = somewhere.serving().expect("nothing claimed the directory");

    subscriber.kill();

    // The subscriber's death is not an event the daemon reports, so there is
    // nothing to wait for: what is being asserted is that nothing changes.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        somewhere.answering(),
        "the daemon stopped with its subscriber"
    );
    assert_eq!(somewhere.serving(), Some(started));
}

#[test]
fn a_second_caller_gets_the_daemon_the_first_one_started() {
    let somewhere = Somewhere::new();
    let mut first = somewhere.subscribe(&["subscribe", "--ensure-daemon"]);
    first.snapshot();
    let started = somewhere.serving().expect("nothing claimed the directory");

    let mut second = somewhere.subscribe(&["subscribe", "--ensure-daemon"]);
    second.snapshot();

    assert_eq!(
        somewhere.serving(),
        Some(started),
        "a second daemon took the directory: {}",
        somewhere.log()
    );
}

#[test]
fn two_callers_at_once_end_up_with_one_daemon_between_them() {
    let somewhere = Somewhere::new();

    let mut first = somewhere.subscribe(&["subscribe", "--ensure-daemon"]);
    let mut second = somewhere.subscribe(&["subscribe", "--ensure-daemon"]);

    first.snapshot();
    second.snapshot();
    let serving = somewhere.serving().expect("nothing claimed the directory");
    // The daemon that lost the race for the lock said so and exited; the one
    // holding the lock is the one both subscribers are reading from.
    until("the loser never exited", || somewhere.answering());
    assert_eq!(somewhere.serving(), Some(serving));
}

#[test]
fn a_daemon_started_this_way_is_an_ordinary_one() {
    let somewhere = Somewhere::new();
    let mut subscriber = somewhere.subscribe(&["subscribe", "--ensure-daemon"]);
    subscriber.snapshot();

    let output = somewhere
        .command(&["status", "--json"])
        .output()
        .expect("cannot run agentbus status");

    assert!(output.status.success(), "{}", somewhere.log());
    let snapshot: StreamLine =
        serde_json::from_slice(&output.stdout).expect("that was not a snapshot");
    assert!(matches!(snapshot, StreamLine::Snapshot(_)), "{snapshot:?}");
}

#[test]
fn the_bootstrap_script_finds_this_build_and_runs_it_with_the_arguments_it_was_given() {
    let somewhere = Somewhere::new();

    // Exactly what a transport does at the far end: the script on stdin, the
    // wanted version first, and the command to run after it. Naming this build
    // outright is the script's first candidate, and the only one a test can
    // point at a binary it knows the version of.
    let mut child = Command::new("sh")
        .args(["-s", "--", env!("CARGO_PKG_VERSION")])
        .args(["subscribe", "--ensure-daemon", "--dir"])
        .arg(somewhere.dir())
        .env("AGENTBUS_REMOTE_BINARY", env!("CARGO_BIN_EXE_agentbus"))
        .env_remove("AGENTBUS_DIR")
        .env_remove("AGENTBUS_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cannot run the bootstrap script");
    child
        .stdin
        .take()
        .expect("no stdin")
        .write_all(agentbus_daemon::remote::SCRIPT.as_bytes())
        .expect("cannot send the script");

    let mut stream = BufReader::new(child.stdout.take().expect("no stdout"));
    let mut first = String::new();
    stream
        .read_line(&mut first)
        .expect("cannot read the stream");
    let _ = child.kill();
    let _ = child.wait();

    let parsed: StreamLine = serde_json::from_str(first.trim_end())
        .unwrap_or_else(|error| panic!("{error} in {first:?}"));
    assert!(matches!(parsed, StreamLine::Snapshot(_)), "{parsed:?}");
    assert!(somewhere.serving().is_some(), "{}", somewhere.log());
}
