//! Attaching to a daemon on another endpoint, where the other endpoint is this
//! machine.
//!
//! The far end here is a directory rather than a container or a host, and
//! everything else about it is real: a real `agentbus` binary found by the real
//! bootstrap script, a real detached daemon serving a bus directory of its own,
//! real hooks emitted into it, and a real `agentbus subscribe` whose stdout is
//! the stream being merged. What that buys is the two failures this is actually
//! for — the stream dying while the daemon behind it lives, and the daemon
//! itself dying — which cannot be staged with a function that returns lines.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentbus_daemon::remote::attach::{Settings, State};
use agentbus_daemon::remote::transport::{Backoff, Error, Running, Transport};
use agentbus_daemon::remote::{Attachment, Bootstrap, Release};
use agentbus_daemon::{Bus, VERSION};
use agentbus_protocol::{Event, OriginHop, SessionEntry, SessionStatus};
use serde_json::{Value, json};

/// How long a test waits for something that should happen immediately.
const PATIENCE: Duration = Duration::from_secs(15);

/// The opaque string the far end's sessions are correlated to. Its shape means
/// nothing to anything here.
const SLOT: &str = "w9:p3";

/// The environment variables that would otherwise decide, behind a test's back,
/// which bus the far end is.
const INHERITED: &[&str] = &[
    "AGENTBUS_CONFIG_DIR",
    "AGENTBUS_DIR",
    "AGENTBUS_LOG",
    "AGENTBUS_PANE",
    "AGENTBUS_PROC_ROOT",
    "AGENTBUS_REMOTE_BINARY",
    "AGENTBUS_STALE_SECS",
    "AGENTBUS_DONE_RETENTION_SECS",
    "AGENTBUS_ASSERT_HOLD_SECS",
    "XDG_RUNTIME_DIR",
];

/// A far end that happens to be this machine: its own bus directory, its own
/// home, and this build of `agentbus` where the bootstrap script looks first.
///
/// It records the process id of whatever it was last asked to run, which is how
/// a test kills the stream without touching the daemon behind it — the
/// distinction the whole reconnection story turns on.
#[derive(Debug)]
struct Elsewhere {
    temp: tempfile::TempDir,
    dir: PathBuf,
}

impl Drop for Elsewhere {
    fn drop(&mut self) {
        if let Some(pid) = self.serving() {
            // Safe by construction: a signal number and a pid this test read
            // from the lock file of a daemon started under its own directory.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

impl Elsewhere {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("cannot make a temporary directory");
        let dir = temp.path().join("bus");
        Self { temp, dir }
    }

    /// Where the process id of the last thing run over there is written.
    fn pidfile(&self) -> PathBuf {
        self.temp.path().join("ran.pid")
    }

    /// The process id of the last thing run over there, if anything has been.
    fn ran(&self) -> Option<libc::pid_t> {
        std::fs::read_to_string(self.pidfile())
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The process id of the daemon serving the far end, as its lock file names
    /// it, or nothing if no daemon has claimed it.
    fn serving(&self) -> Option<libc::pid_t> {
        std::fs::read_to_string(self.dir.join("daemon.lock"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// What the daemon over there said about itself, for a failure message.
    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("daemon.log")).unwrap_or_default()
    }

    /// An `agentbus` command run over there, against its bus and nothing else.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
        command
            .args(args)
            .env("HOME", self.temp.path())
            .env("AGENTBUS_DIR", &self.dir)
            // Nothing over there is watching a process table, so what arrives on
            // the stream is what this test put into it, on any machine.
            .env("AGENTBUS_PROC_ROOT", self.temp.path().join("no-such-proc"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for variable in INHERITED {
            if *variable != "AGENTBUS_DIR" && *variable != "AGENTBUS_PROC_ROOT" {
                command.env_remove(variable);
            }
        }
        command
    }

    /// Gets a daemon running over there the way anything arriving on a machine
    /// does, and leaves it running.
    fn started(&self) {
        let mut subscriber = self
            .command(&["subscribe", "--ensure-daemon"])
            .spawn()
            .expect("cannot run agentbus subscribe");
        let stdout = subscriber.stdout.take().expect("no stdout");
        let mut first = String::new();
        BufReader::new(stdout)
            .read_line(&mut first)
            .expect("cannot read the stream");
        let _ = subscriber.kill();
        let _ = subscriber.wait();
        assert!(
            first.contains("snapshot"),
            "the far end said {first:?}: {}",
            self.log()
        );
    }

    /// Sends one Claude hook payload into the bus over there.
    fn emit(&self, payload: &Value) {
        let mut child = self
            .command(&["emit", "--agent", "claude"])
            .env("AGENTBUS_PANE", SLOT)
            .stdin(Stdio::piped())
            .spawn()
            .expect("cannot run agentbus emit");
        child
            .stdin
            .take()
            .expect("no stdin")
            .write_all(payload.to_string().as_bytes())
            .expect("cannot write the payload");
        let output = child.wait_with_output().expect("cannot wait for agentbus");
        assert!(
            output.status.success(),
            "emit exited with {}",
            output.status
        );
    }
}

impl Transport for Elsewhere {
    fn kind(&self) -> &'static str {
        "loopback"
    }

    fn label(&self) -> String {
        "elsewhere".to_owned()
    }

    fn identity(&self) -> Option<String> {
        Some("elsewhere".to_owned())
    }

    fn install_path(&self, _version: &str) -> String {
        self.temp
            .path()
            .join(".local/bin/agentbus")
            .display()
            .to_string()
    }

    /// Runs a command over there, through a shell that first writes down which
    /// process it is going to become.
    ///
    /// `exec` is what makes that worth writing down: the shell is replaced by
    /// the command, so the recorded id is the id of the thing whose stream is
    /// being read, and killing it kills exactly that.
    fn run(&self, command: &str, args: &[&str], stdin: Option<&str>) -> Result<Running, Error> {
        let mut script = format!(
            "printf %s $$ > {}\nexec",
            quoted(&self.pidfile().display().to_string())
        );
        for word in std::iter::once(command).chain(args.iter().copied()) {
            script.push(' ');
            script.push_str(&quoted(word));
        }
        let mut process = Command::new("sh");
        process
            .arg("-c")
            .arg(script)
            .env("HOME", self.temp.path())
            .env("AGENTBUS_DIR", &self.dir)
            .env("AGENTBUS_PROC_ROOT", self.temp.path().join("no-such-proc"))
            // Where the bootstrap script looks before anything else, which is
            // what makes this build the one the far end runs.
            .env("AGENTBUS_REMOTE_BINARY", env!("CARGO_BIN_EXE_agentbus"));
        for variable in INHERITED {
            if ![
                "AGENTBUS_DIR",
                "AGENTBUS_PROC_ROOT",
                "AGENTBUS_REMOTE_BINARY",
            ]
            .contains(variable)
            {
                process.env_remove(variable);
            }
        }
        Running::spawn(&mut process, stdin).map_err(|source| Error::Run {
            label: self.label(),
            command: command.to_owned(),
            source,
        })
    }

    fn copy_in(&self, local: &Path, remote: &str) -> Result<(), Error> {
        let failed = |source| Error::Copy {
            label: self.label(),
            local: local.to_owned(),
            remote: remote.to_owned(),
            source,
        };
        if let Some(parent) = Path::new(remote).parent() {
            std::fs::create_dir_all(parent).map_err(failed)?;
        }
        std::fs::copy(local, remote).map(|_| ()).map_err(failed)
    }

    fn backoff(&self) -> Backoff {
        Backoff {
            initial: Duration::from_millis(50),
            max: Duration::from_millis(200),
            multiplier: 2.0,
            jitter: 0.0,
        }
    }
}

/// One word of a shell command, as a shell will read it back unchanged.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The hop everything relayed from the far end carries.
fn hop() -> OriginHop {
    OriginHop::new("loopback", "elsewhere", "elsewhere")
}

/// A daemon on this machine, attached to the one over there.
fn attached(bus: &Arc<Bus>, far: &Arc<Elsewhere>) -> Attachment {
    Attachment::start(
        Arc::clone(far) as Arc<dyn Transport>,
        // Nowhere to fetch from: a test that unexpectedly reaches the fetch path
        // fails here rather than going to the network.
        Bootstrap::new(VERSION).fetching(Release::at("file:///no/such/release", VERSION)),
        Arc::clone(bus),
        Settings {
            liveness: Duration::from_secs(30),
            stable: Duration::from_secs(60),
        },
    )
}

/// What a Claude session does when it starts a tool.
fn tool_start(session: &str) -> Value {
    json!({
        "session_id": session,
        "hook_event_name": "PreToolUse",
        "cwd": "/srv/project",
        "tool_name": "Bash",
    })
}

/// What it does when it is waiting for a person.
fn blocked(session: &str) -> Value {
    json!({
        "session_id": session,
        "hook_event_name": "Notification",
        "notification_type": "permission_prompt",
    })
}

/// Waits for `wanted`, or fails the test saying what it was waiting for.
fn until(what: &str, wanted: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !wanted() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What the local bus says about one session, if it says anything.
fn session_of(bus: &Bus, session: &str) -> Option<SessionEntry> {
    let mut found: Vec<SessionEntry> = bus
        .sessions()
        .into_iter()
        .filter(|entry| entry.session == session)
        .collect();
    assert!(found.len() <= 1, "one session is two rows: {found:?}");
    found.pop()
}

/// The events merged from the far end, in the order they were numbered here.
fn relayed(bus: &Bus, session: &str) -> Vec<Event> {
    bus.recent()
        .into_iter()
        .filter(|event| event.session == session)
        .collect()
}

/// A session of this daemon's own, so that a test can see that merging changed
/// nothing about it.
fn local(bus: &Bus) {
    bus.ingest(
        &serde_json::to_vec(
            &json!({"v": 1, "agent": "codex", "session": "here", "kind": "tool_start"}),
        )
        .unwrap(),
    )
    .expect("the local event was dropped");
}

#[test]
fn attaching_brings_back_what_the_daemon_over_there_already_knew() {
    let far = Arc::new(Elsewhere::new());
    far.started();
    far.emit(&blocked("abc123"));
    let bus = Arc::new(Bus::new());
    local(&bus);

    let attachment = attached(&bus, &far);

    until("it never attached", || {
        attachment.state() == State::Attached
    });
    until("nothing was seeded", || {
        session_of(&bus, "abc123").is_some()
    });

    let session = session_of(&bus, "abc123").expect("the session went missing");
    // Seeded from what the far end had already folded, not replayed: it was
    // blocked before this end had heard of it, and it still is.
    assert_eq!(session.status, SessionStatus::Blocked);
    assert_eq!(session.correlation.as_deref(), Some(SLOT));
    assert_eq!(session.origin, vec![hop()]);
    // And this daemon's own sessions are still this daemon's own.
    let here = session_of(&bus, "here").expect("the local session went missing");
    assert!(here.origin.is_empty(), "{:?}", here.origin);
}

#[test]
fn an_event_emitted_over_there_arrives_here_numbered_here() {
    let far = Arc::new(Elsewhere::new());
    let bus = Arc::new(Bus::new());
    local(&bus);
    let attachment = attached(&bus, &far);
    until("it never attached", || {
        attachment.state() == State::Attached
    });
    let before = bus.last_seq();

    far.emit(&tool_start("def456"));

    until("the event never arrived", || {
        !relayed(&bus, "def456").is_empty()
    });

    let events = relayed(&bus, "def456");
    let event = &events[0];
    assert_eq!(event.origin, vec![hop()]);
    // Numbered in this daemon's sequence, after everything it had already said.
    assert!(event.seq > before, "{} is not after {before}", event.seq);
    assert_eq!(bus.last_seq(), event.seq);
    // And the number it had over there is still readable, which is how the two
    // streams are lined up against each other afterwards.
    let raw = event.raw.as_ref().expect("the payload went missing");
    assert!(
        raw.get("remote_seq").and_then(Value::as_u64).is_some(),
        "{raw:?}"
    );
    // The agent's own payload came through beside it.
    assert_eq!(raw.get("tool_name"), Some(&json!("Bash")));
    assert_eq!(
        session_of(&bus, "def456").expect("no session").status,
        SessionStatus::Working
    );
}

#[test]
fn a_session_blocked_before_the_stream_broke_is_still_blocked_after_it() {
    let far = Arc::new(Elsewhere::new());
    let bus = Arc::new(Bus::new());
    let attachment = attached(&bus, &far);
    until("it never attached", || {
        attachment.state() == State::Attached
    });

    far.emit(&blocked("abc123"));
    until("the session never blocked", || {
        session_of(&bus, "abc123").map(|entry| entry.status) == Some(SessionStatus::Blocked)
    });

    // The stream dies; the daemon behind it does not. This is the network blip,
    // and it is the case the whole arrangement exists for.
    let daemon = far.serving().expect("nothing is serving over there");
    let stream = far.ran().expect("nothing has been run over there");
    // Safe by construction: a signal number and a pid this test read from a file
    // written by a process it started itself.
    unsafe { libc::kill(stream, libc::SIGKILL) };

    until("the stream was never picked up again", || {
        far.ran() != Some(stream) && attachment.state() == State::Attached
    });

    assert_eq!(
        far.serving(),
        Some(daemon),
        "the daemon over there restarted: {}",
        far.log()
    );
    let session = session_of(&bus, "abc123").expect("the session went missing");
    assert_eq!(session.status, SessionStatus::Blocked);
    assert_eq!(session.origin, vec![hop()]);
}

#[test]
fn a_daemon_that_dies_takes_the_sessions_it_knew_with_it_and_a_fresh_one_is_started() {
    let far = Arc::new(Elsewhere::new());
    let bus = Arc::new(Bus::new());
    let attachment = attached(&bus, &far);
    until("it never attached", || {
        attachment.state() == State::Attached
    });

    far.emit(&tool_start("ghi789"));
    until("the session never arrived", || {
        session_of(&bus, "ghi789").map(|entry| entry.status) == Some(SessionStatus::Working)
    });

    let daemon = far.serving().expect("nothing is serving over there");
    // Safe by construction: a signal number and a pid this test read from the
    // lock file of a daemon started under its own directory.
    unsafe { libc::kill(daemon, libc::SIGKILL) };

    // Nobody over there is going to start another one, so the reconnection has
    // to, and what it finds is a daemon that has never heard of anything.
    until("no fresh daemon was started", || {
        far.serving().is_some_and(|serving| serving != daemon)
    });
    until("the sessions of the dead daemon were never ended", || {
        session_of(&bus, "ghi789").map(|entry| entry.status) == Some(SessionStatus::Done)
    });
    until("it never attached again", || {
        attachment.state() == State::Attached
    });

    // And the fresh daemon is an ordinary one that this end is really attached
    // to: what is emitted into it arrives here.
    far.emit(&blocked("jkl012"));
    until("the fresh daemon's events never arrived", || {
        session_of(&bus, "jkl012").map(|entry| entry.status) == Some(SessionStatus::Blocked)
    });
}

#[test]
fn detaching_ends_the_sessions_over_there_and_leaves_the_daemon_running() {
    let far = Arc::new(Elsewhere::new());
    let bus = Arc::new(Bus::new());
    let attachment = attached(&bus, &far);
    until("it never attached", || {
        attachment.state() == State::Attached
    });
    far.emit(&blocked("abc123"));
    until("the session never blocked", || {
        session_of(&bus, "abc123").map(|entry| entry.status) == Some(SessionStatus::Blocked)
    });
    let daemon = far.serving().expect("nothing is serving over there");
    let stream = far.ran().expect("nothing has been run over there");

    attachment.detach();

    assert_eq!(
        session_of(&bus, "abc123").map(|entry| entry.status),
        Some(SessionStatus::Done)
    );
    // The daemon over there is untouched — that is the whole point of having put
    // it there — and the stream this end was reading is not.
    assert_eq!(far.serving(), Some(daemon));
    assert_eq!(
        unsafe { libc::kill(stream, 0) },
        -1,
        "the far end's subscriber is still running"
    );
}
