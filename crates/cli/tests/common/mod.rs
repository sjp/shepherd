//! A bus driven the way the people who use one drive it: a daemon process in a
//! directory of its own, recorded hook payloads pushed through the real
//! `agentbus emit`, and a subscriber reading whatever comes out the far end.
//!
//! Nothing here calls into the library. What these tests are about is the
//! pipeline — process start, socket, fold, publish, snapshot — so every step
//! goes through the binary, and what they assert is what somebody at a shell
//! would have seen. The lines are parsed with the published protocol types
//! because that is what a subscriber does; the tests that pin the bytes on the
//! wire live next door.
//!
//! Not every test binary that includes this module reaches for all of it. One
//! harness serving several is the point of having a harness, so a helper only
//! one of them calls is not dead code.

#![allow(dead_code)]

pub mod tree;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use agentbus_protocol::{
    Event, ForegroundChange, ForegroundEntry, SessionEntry, SessionStatus, Snapshot, StreamLine,
};

/// How long a test waits for something that should happen immediately.
///
/// Generous on purpose: it is the difference between a failing test and a
/// hanging one, never a measurement. Anything actually being timed says so.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// The environment variables that would otherwise decide, behind a test's back,
/// which bus a command talks to and how it behaves.
const INHERITED: &[&str] = &[
    "AGENTBUS_CONFIG_DIR",
    "AGENTBUS_DIR",
    "AGENTBUS_LOG",
    "AGENTBUS_PANE",
    "AGENTBUS_PROC_ROOT",
    "AGENTBUS_STALE_SECS",
    "AGENTBUS_DONE_RETENTION_SECS",
    "XDG_RUNTIME_DIR",
];

/// An `agentbus` command against the bus in `dir`, with nothing inherited from
/// whoever is running the tests.
fn command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
    command
        .args(args)
        .arg("--dir")
        .arg(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for variable in INHERITED {
        command.env_remove(variable);
    }
    command
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

/// A running bus: a daemon process serving a directory that goes away with the
/// test.
pub struct Bus {
    daemon: Child,
    dir: PathBuf,
    _temp: tempfile::TempDir,
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

impl Bus {
    /// Starts a daemon with no process table to read.
    ///
    /// That is what a test about sessions wants: such a daemon says nothing
    /// whatever about the foreground, so what these tests read on the stream is
    /// what they themselves put into it, on any machine.
    pub fn start() -> Self {
        Self::started(None, &[])
    }

    /// Starts a daemon watching the process table under `proc_root`.
    pub fn watching(proc_root: &Path) -> Self {
        Self::started(Some(proc_root), &[])
    }

    /// The same, with `settings` handed to the daemon as well, for a test whose
    /// subject is a timeout it would otherwise have to wait out.
    pub fn watching_with(proc_root: &Path, settings: &[&str]) -> Self {
        Self::started(Some(proc_root), settings)
    }

    /// Starts a daemon on a fresh directory and returns once both of its sockets
    /// answer, so that a test never races the thing it is about to talk to.
    fn started(proc_root: Option<&Path>, settings: &[&str]) -> Self {
        let temp = tempfile::tempdir().expect("cannot make a temporary directory");
        let dir = temp.path().join("bus");
        let unreadable = temp.path().join("no-process-table");
        let daemon = command(&dir, &["daemon"])
            .args(settings)
            .env(
                "AGENTBUS_PROC_ROOT",
                proc_root.unwrap_or(unreadable.as_path()),
            )
            .stdout(Stdio::null())
            .spawn()
            .expect("cannot run agentbus");
        let bus = Self {
            daemon,
            dir,
            _temp: temp,
        };
        for socket in ["emit.sock", "sub.sock"] {
            let path = bus.dir.join(socket);
            let deadline = Instant::now() + PATIENCE;
            while UnixStream::connect(&path).is_err() {
                assert!(
                    Instant::now() < deadline,
                    "{} never started serving",
                    path.display()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        bus
    }

    /// The directory the sockets are in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// An `agentbus` command against this bus, for a test that wants to run one
    /// the harness has no opinion about.
    pub fn command(&self, args: &[&str]) -> Command {
        command(&self.dir, args)
    }

    /// Runs one `agentbus` command to completion, failing the test if it did not
    /// succeed.
    pub fn run(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().expect("cannot run agentbus");
        assert!(output.status.success(), "{args:?}: {}", said(&output));
        output
    }

    /// Runs one hook: `payload` on stdin, `AGENTBUS_PANE` set to `pane` where
    /// something set it, exactly as an installed hook is run by its agent.
    ///
    /// The client's contract is checked on the way past, because every call in
    /// every test is another sample of it: nothing on stdout, and a zero exit.
    pub fn emit(&self, agent: &str, pane: Option<&str>, payload: &[u8]) -> Output {
        self.sent(&["--agent", agent], pane, payload)
    }

    /// Sends one observation, the way a program that was watching a terminal
    /// does. An observation states for itself what it was watching, so there is
    /// no pane in the environment to set.
    pub fn observe(&self, payload: &[u8]) -> Output {
        self.sent(&["--source", "observed"], None, payload)
    }

    /// Runs one `emit`, whatever it is being told to send.
    fn sent(&self, what: &[&str], pane: Option<&str>, payload: &[u8]) -> Output {
        let mut args = vec!["emit"];
        args.extend_from_slice(what);
        let mut command = self.command(&args);
        command.stdin(Stdio::piped());
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

    /// What the bus knows, as `agentbus status --json` reports it.
    pub fn snapshot(&self) -> Snapshot {
        let output = self.run(&["status", "--json"]);
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "that was not a snapshot: {error} in {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    /// Waits until the bus reports `session` as `status`, and returns the
    /// snapshot that says so.
    ///
    /// Something has to wait: the client writes its line and exits without being
    /// told the bus read it, so a hook that has finished running is not yet a
    /// session anybody can see. Everywhere else in these tests the wait is a
    /// subscriber reading the event; this is the one for when nothing is
    /// subscribed, which is the situation the restart test is entirely about.
    pub fn wait_for(&self, session: &str, status: SessionStatus) -> Snapshot {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let snapshot = self.snapshot();
            let reached = snapshot
                .sessions
                .iter()
                .any(|entry| entry.session == session && entry.status == status);
            if reached {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "{session} never became {status}: {:?}",
                snapshot.sessions
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits until the bus reports an observation for `correlation`, and returns
    /// the snapshot that says so.
    ///
    /// Something has to wait here too: the daemon looks at the process table on
    /// its own schedule, and a test that wrote a file has not thereby been
    /// observed.
    pub fn wait_for_foreground(&self, correlation: &str) -> Snapshot {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let snapshot = self.snapshot();
            let observed = snapshot
                .foreground
                .as_deref()
                .expect("this daemon is not watching a process table");
            if observed
                .iter()
                .any(|entry| entry.correlation == correlation)
            {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "nothing was ever observed for {correlation}: {observed:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Subscribes on the socket, the way a program that has embedded the bus
    /// does.
    pub fn attach(&self) -> Subscriber {
        let socket = UnixStream::connect(self.dir.join("sub.sock")).expect("cannot connect");
        let reading = socket.try_clone().expect("cannot clone the connection");
        Subscriber::reading(reading, Connection::Socket(socket))
    }

    /// Subscribes by running `agentbus subscribe`, the way a shell does.
    pub fn subscribe(&self) -> Subscriber {
        let mut child = self
            .command(&["subscribe"])
            .spawn()
            .expect("cannot run agentbus");
        let stdout = child.stdout.take().expect("no stdout");
        Subscriber::reading(stdout, Connection::Command(child))
    }
}

/// How a subscriber is connected, and how it goes away.
///
/// Going away is the interesting half: the restart test needs a subscriber that
/// disappears the way a killed one does, rather than one that politely says
/// goodbye — the bus is not told either way.
enum Connection {
    Socket(UnixStream),
    Command(Child),
}

impl Drop for Connection {
    fn drop(&mut self) {
        match self {
            Self::Socket(socket) => {
                let _ = socket.shutdown(Shutdown::Both);
            }
            Self::Command(child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// A subscriber, and the stream arriving from it line by line.
///
/// The lines are read on a thread of its own so that a test can wait for the
/// next one with a deadline. A stream that stopped producing is one of the
/// things being tested for, and a test that blocked on it forever would report
/// that as a hang rather than as a result.
pub struct Subscriber {
    lines: Receiver<String>,
    connection: Connection,
}

impl Subscriber {
    /// Starts reading `source`, keeping `connection` for as long as the
    /// subscriber is wanted.
    fn reading(source: impl Read + Send + 'static, connection: Connection) -> Self {
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(source).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        Self { lines, connection }
    }

    /// The next line, failing the test if none arrives. `what` is what the test
    /// was waiting for: a stream that has gone quiet is a failure whose entire
    /// content is which step it went quiet on.
    fn line(&mut self, what: &str) -> StreamLine {
        let line = match self.lines.recv_timeout(PATIENCE) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => panic!("nothing arrived on the stream: {what}"),
            Err(RecvTimeoutError::Disconnected) => panic!("the stream ended: {what}"),
        };
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error} in {line:?}"))
    }

    /// The first line of the stream, which is always the snapshot.
    pub fn snapshot(&mut self) -> Snapshot {
        match self.line("the snapshot") {
            StreamLine::Snapshot(snapshot) => snapshot,
            other => panic!("the stream began with {other:?} instead of a snapshot"),
        }
    }

    /// Reads lines until `wanted` recognizes one, failing the test if none does
    /// before the patience runs out.
    pub fn until<T>(&mut self, what: &str, wanted: impl Fn(&StreamLine) -> Option<T>) -> T {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let line = self.line(what);
            if let Some(found) = wanted(&line) {
                return found;
            }
            assert!(Instant::now() < deadline, "the stream never carried {what}");
        }
    }

    /// The next event, skipping the heartbeats and anything else the bus says
    /// while a test is waiting.
    pub fn event(&mut self, what: &str) -> Event {
        self.until(what, |line| match line {
            StreamLine::Event(event) => Some(event.clone()),
            _ => None,
        })
    }

    /// The next change to what is in front of a correlated shell, skipping
    /// everything else the bus says while a test is waiting.
    pub fn foreground_change(&mut self, what: &str) -> ForegroundChange {
        self.until(what, |line| match line {
            StreamLine::ForegroundChange(change) => Some(change.clone()),
            _ => None,
        })
    }

    /// Whatever the bus says in the next `patience`, heartbeats aside, or
    /// nothing at all if it stays quiet — for a test whose subject *is* the
    /// quiet, where waiting the full patience is the measurement rather than a
    /// delay to be avoided.
    pub fn quiet_for(&mut self, patience: Duration) -> Option<StreamLine> {
        let deadline = Instant::now() + patience;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(remaining) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => panic!("the stream ended"),
            };
            match serde_json::from_str(&line) {
                Ok(StreamLine::Heartbeat(_)) => {}
                Ok(line) => return Some(line),
                Err(error) => panic!("{error} in {line:?}"),
            }
        }
    }

    /// Disconnects, the way a subscriber that has been killed does.
    pub fn close(self) {
        drop(self.connection);
    }
}

/// The one foreground observation in `snapshot` for this correlation, failing
/// the test if the bus has none or has several.
pub fn foreground_of<'a>(snapshot: &'a Snapshot, correlation: &str) -> &'a ForegroundEntry {
    let observed = snapshot
        .foreground
        .as_deref()
        .expect("this daemon is not watching a process table");
    let found: Vec<&ForegroundEntry> = observed
        .iter()
        .filter(|entry| entry.correlation == correlation)
        .collect();
    match found.as_slice() {
        [entry] => entry,
        [] => panic!("{correlation} is not in {observed:?}"),
        many => panic!(
            "{correlation} is in the snapshot {} times: {many:?}",
            many.len()
        ),
    }
}

/// The one session in `snapshot` with this id, failing the test if the bus has
/// forgotten it or knows it twice over.
pub fn session_of<'a>(snapshot: &'a Snapshot, session: &str) -> &'a SessionEntry {
    let found: Vec<&SessionEntry> = snapshot
        .sessions
        .iter()
        .filter(|entry| entry.session == session)
        .collect();
    match found.as_slice() {
        [entry] => entry,
        [] => panic!("{session} is not in {:?}", snapshot.sessions),
        many => panic!(
            "{session} is in the snapshot {} times: {many:?}",
            many.len()
        ),
    }
}

/// Where the recorded hook payloads live.
fn recordings_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks")
}

/// The file in an agent's directory that says which payloads to replay, in what
/// order, and what the bus should say after each one.
const SEQUENCE: &str = "sequence.txt";

/// The status token that means "this recording does not say yet".
const UNKNOWN: &str = "?";

/// One agent's recorded payloads and the session they add up to.
pub struct Recording {
    /// The agent the directory is named after, which is what `--agent` is given.
    pub agent: String,
    /// The steps, in the order they were recorded.
    pub steps: Vec<Step>,
}

/// One step of a recording: a payload to send, and what the bus should say about
/// the session once it has.
pub struct Step {
    /// The payload file.
    pub payload: PathBuf,
    /// The status expected afterwards, or `None` where the recording does not
    /// claim one yet.
    pub expected: Option<SessionStatus>,
}

impl Step {
    /// The payload, as it would arrive on the hook command's stdin.
    pub fn read(&self) -> Vec<u8> {
        std::fs::read(&self.payload)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", self.payload.display()))
    }

    /// What to call this step in a failure message.
    pub fn name(&self) -> String {
        self.payload
            .file_name()
            .unwrap_or(self.payload.as_os_str())
            .to_string_lossy()
            .into_owned()
    }
}

/// Every recording found beside these tests.
///
/// Discovered rather than listed: an agent is added to the replay by dropping
/// its directory in, and nothing here has to learn its name.
pub fn recordings() -> Vec<Recording> {
    let dir = recordings_dir();
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()));
    let mut recordings: Vec<Recording> = entries
        .map(|entry| entry.expect("cannot read a directory entry").path())
        .filter(|path| path.is_dir())
        .map(|path| read_recording(&path))
        .collect();
    recordings.sort_by(|one, other| one.agent.cmp(&other.agent));
    assert!(!recordings.is_empty(), "no recordings in {}", dir.display());
    recordings
}

/// The recording for one named agent.
pub fn recording(agent: &str) -> Recording {
    read_recording(&recordings_dir().join(agent))
}

/// One recorded payload, for a test scripting its own sequence rather than
/// replaying a recorded one.
pub fn payload(agent: &str, name: &str) -> Vec<u8> {
    let path = recordings_dir().join(agent).join(format!("{name}.json"));
    std::fs::read(&path).unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// Reads one agent's directory.
fn read_recording(dir: &Path) -> Recording {
    let agent = dir
        .file_name()
        .expect("a directory with no name")
        .to_string_lossy()
        .into_owned();
    let sequence = dir.join(SEQUENCE);
    let text = std::fs::read_to_string(&sequence).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}\n\
             every directory of recorded payloads needs a {SEQUENCE} beside them saying \
             which to replay and what the bus should say after each one",
            sequence.display()
        )
    });
    let steps = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !is_blank_or_comment(line))
        .map(|(number, line)| read_step(dir, line, &sequence, number + 1))
        .collect();
    Recording { agent, steps }
}

/// Whether a line of a sequence file says nothing.
fn is_blank_or_comment(line: &str) -> bool {
    let line = line.trim_start();
    line.is_empty() || line.starts_with('#')
}

/// Reads one `<payload-file> <expected-status>` line.
fn read_step(dir: &Path, line: &str, sequence: &Path, number: usize) -> Step {
    let at = format!("{}:{number}", sequence.display());
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [file, status] = fields.as_slice() else {
        panic!("{at}: expected `<payload-file> <expected-status>`, found {line:?}");
    };
    let payload = dir.join(file);
    assert!(payload.is_file(), "{at}: there is no {}", payload.display());
    let expected = match *status {
        UNKNOWN => None,
        named => Some(
            SessionStatus::ALL
                .into_iter()
                .find(|status| status.as_str() == named)
                .unwrap_or_else(|| panic!("{at}: {named:?} is not a status")),
        ),
    };
    Step { payload, expected }
}
