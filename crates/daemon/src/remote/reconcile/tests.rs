//! Reconciling, driven by a transport whose far end is a shell script.
//!
//! What is being tested is the loop between two files and a set of attachments,
//! so the far end only has to be a real process producing a real stream that can
//! be cut: what makes an attachment interesting — that it reaches something,
//! that it can be stopped, that stopping it withdraws what it said — is the same
//! whether the process is a container on another machine or a shell here.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentbus_protocol::{
    Agent, DaemonIdentity, SessionEntry, SessionStatus, Snapshot, Source, Timestamp,
};

use super::{Plan, Reconciling};
use crate::bus::Bus;
use crate::clock;
use crate::remote::attach;
use crate::remote::attachments::{Attachments, Entry, State};
use crate::remote::bootstrap::Bootstrap;
use crate::remote::targets::Targets;
use crate::remote::transport::{
    Backoff, Error as TransportError, Made, Registry, Running, Transport,
};

/// How long a test waits for something that should happen almost at once.
const PATIENCE: Duration = Duration::from_secs(10);

/// How often these reconcilers look, which is far faster than a daemon's own
/// cadence so that a test is not spent waiting one out.
const QUICKLY: Duration = Duration::from_millis(20);

/// The name the far end of these tests is declared under.
const FAKE: &str = "fake";

/// A far end that says it is a daemon and then holds the line open.
///
/// It is handed the words it was declared with, which is what a real transport
/// is given, and it takes its name from the first of them so that a test can
/// tell two declarations apart in what gets written down.
///
/// It can also be made to say nothing until a file appears, which is how a test
/// holds an endpoint in the state before it has answered for as long as it wants
/// to look at it.
#[derive(Debug)]
struct Fake {
    name: String,
    gate: Option<PathBuf>,
}

impl Fake {
    fn of(args: &[String]) -> Made {
        Self::made(args, None)
    }

    fn made(args: &[String], gate: Option<PathBuf>) -> Made {
        match args.first() {
            Some(name) => Ok(Arc::new(Self {
                name: name.clone(),
                gate,
            }) as Arc<dyn Transport>),
            None => Err("a fake endpoint has to be named".to_owned()),
        }
    }

    /// What the far end runs: wait, if it was told to, then say what it knows
    /// and stay on the line.
    fn script(&self) -> String {
        let snapshot = serde_json::to_string(&snapshot(&self.name)).expect("cannot write a line");
        let waiting = match &self.gate {
            Some(gate) => format!("while [ ! -e '{}' ]; do sleep 0.01; done\n", gate.display()),
            None => String::new(),
        };
        format!("{waiting}printf '%s\\n' '{snapshot}'\nexec sleep 3600\n")
    }
}

impl Transport for Fake {
    fn kind(&self) -> &'static str {
        FAKE
    }

    fn label(&self) -> String {
        self.name.clone()
    }

    fn identity(&self) -> Option<String> {
        Some(self.name.clone())
    }

    fn install_path(&self, _version: &str) -> String {
        "/nowhere/agentbus".to_owned()
    }

    /// Prints a snapshot and then waits to be killed, whatever it was asked to
    /// run: what a reconciler cares about is the stream that comes back.
    fn run(
        &self,
        _command: &str,
        _args: &[&str],
        _stdin: Option<&str>,
    ) -> Result<Running, TransportError> {
        let mut command = Command::new("sh");
        command.arg("-c").arg(self.script());
        Running::spawn(&mut command, None).map_err(|source| TransportError::Run {
            label: self.label(),
            command: "sh".to_owned(),
            source,
        })
    }

    fn copy_in(&self, _local: &Path, _remote: &str) -> Result<(), TransportError> {
        unreachable!("a fake endpoint is never provisioned")
    }

    fn backoff(&self) -> Backoff {
        Backoff {
            initial: Duration::from_millis(20),
            max: Duration::from_millis(20),
            multiplier: 1.0,
            jitter: 0.0,
        }
    }
}

/// What the daemon at the far end says it knows: one session, so that a test can
/// watch it arrive and watch it be withdrawn.
fn snapshot(name: &str) -> Snapshot {
    let mut snapshot = Snapshot::new(
        1,
        vec![SessionEntry {
            session: format!("session-at-{name}"),
            agent: Agent::Claude,
            status: SessionStatus::Blocked,
            source: Source::Hook,
            cwd: None,
            correlation: None,
            origin: Vec::new(),
            since: at("2026-08-17T10:00:00.000Z"),
        }],
    );
    snapshot.daemon = Some(DaemonIdentity::new(name));
    snapshot
}

fn at(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("not a timestamp")
}

fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// A daemon's worth of state, and the two files a reconciler works between.
struct Bench {
    _config: tempfile::TempDir,
    _run: tempfile::TempDir,
    targets: Targets,
    attachments: Attachments,
    bus: Arc<Bus>,
}

impl Bench {
    fn new() -> Self {
        let config = tempfile::tempdir().expect("cannot make a temporary directory");
        let run = tempfile::tempdir().expect("cannot make a temporary directory");
        Self {
            targets: Targets::in_dir(config.path()),
            attachments: Attachments::in_dir(run.path()),
            bus: Arc::new(Bus::new()),
            _config: config,
            _run: run,
        }
    }

    /// A reconciler over these files, knowing one transport and looking often.
    fn reconciling(&self) -> Reconciling {
        self.looking_every(QUICKLY)
    }

    fn looking_every(&self, every: Duration) -> Reconciling {
        self.with(Registry::new().with(FAKE, Fake::of), every)
    }

    /// A reconciler whose far ends say nothing until `gate` exists.
    fn waiting_for(&self, gate: PathBuf) -> Reconciling {
        self.with(
            Registry::new().with(FAKE, move |args| Fake::made(args, Some(gate.clone()))),
            QUICKLY,
        )
    }

    fn with(&self, transports: Registry, every: Duration) -> Reconciling {
        Reconciling::start(Plan {
            targets: self.targets.clone(),
            attachments: self.attachments.clone(),
            transports,
            // Nothing is found here: what these tests are about is the loop
            // between the declarations and the attachments.
            discoveries: Vec::new(),
            bus: Arc::clone(&self.bus),
            bootstrap: Bootstrap::new(crate::VERSION),
            attach: attach::Settings {
                liveness: Duration::from_secs(30),
                stable: Duration::from_secs(3_600),
            },
            every,
        })
    }

    fn declare(&self, name: &str) {
        self.targets
            .declare(FAKE, &words(&[name]), &clock::now())
            .expect("cannot declare");
    }

    fn undeclare(&self, name: &str) {
        assert!(
            self.targets
                .undeclare(FAKE, &words(&[name]))
                .expect("cannot undeclare")
        );
    }

    /// What is written down about the endpoint called `name`, if anything.
    fn entry(&self, name: &str) -> Option<Entry> {
        self.entries()?
            .into_iter()
            .find(|entry| entry.args == words(&[name]))
    }

    /// Everything written down, or nothing when nothing has been.
    fn entries(&self) -> Option<Vec<Entry>> {
        self.attachments
            .read()
            .expect("cannot read what is attached")
    }

    /// What the bus says about the session the endpoint called `name` reported.
    fn session_of(&self, name: &str) -> Option<SessionEntry> {
        self.bus
            .sessions()
            .into_iter()
            .find(|entry| entry.session == format!("session-at-{name}"))
    }
}

/// Waits for `wanted`, or fails the test saying what it was waiting for.
fn until(what: &str, wanted: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !wanted() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_declared_endpoint_is_attached_to_and_written_down() {
    let bench = Bench::new();
    bench.declare("fileserver");

    let _reconciling = bench.reconciling();

    until("it never attached", || {
        bench.entry("fileserver").map(|entry| entry.state) == Some(State::Attached)
    });
    let entry = bench.entry("fileserver").expect("nothing was written down");
    assert_eq!(entry.transport, FAKE);
    assert_eq!(entry.identity.as_deref(), Some("fileserver"));
    assert_eq!(entry.label, "fileserver");
    assert_eq!(entry.aliases, vec![words(&["fileserver"])]);
    assert!(!entry.auto);
    assert_eq!(entry.last_error, None);
    // And what the far end knew is on this bus.
    assert_eq!(
        bench.session_of("fileserver").map(|entry| entry.status),
        Some(SessionStatus::Blocked)
    );
}

#[test]
fn one_declared_after_the_reconciler_started_is_picked_up_by_looking_again() {
    let bench = Bench::new();
    let _reconciling = bench.reconciling();
    until("nothing was ever written down", || {
        bench.entries().is_some()
    });
    assert_eq!(bench.entries(), Some(Vec::new()));

    bench.declare("fileserver");

    until("the new declaration was never noticed", || {
        bench.entry("fileserver").map(|entry| entry.state) == Some(State::Attached)
    });
}

#[test]
fn one_that_is_no_longer_declared_is_detached_and_its_sessions_ended() {
    let bench = Bench::new();
    bench.declare("fileserver");
    let _reconciling = bench.reconciling();
    until("it never attached", || {
        bench.session_of("fileserver").map(|entry| entry.status) == Some(SessionStatus::Blocked)
    });

    bench.undeclare("fileserver");

    until("it was never detached", || {
        bench.entry("fileserver").is_none()
    });
    // The sessions it was speaking for end with it: this daemon can no longer
    // say anything about them.
    assert_eq!(
        bench.session_of("fileserver").map(|entry| entry.status),
        Some(SessionStatus::Done)
    );
}

#[test]
fn a_pass_asked_for_now_happens_without_waiting_for_the_next_look() {
    let bench = Bench::new();
    // An interval a test that waited it out would never finish inside.
    let reconciling = bench.looking_every(Duration::from_secs(3_600));
    until("nothing was ever written down", || {
        bench.entries().is_some()
    });

    bench.declare("fileserver");
    reconciling.wake().now();

    until("the reconciler never looked again", || {
        bench.entry("fileserver").is_some()
    });
}

#[test]
fn a_declaration_nothing_can_be_made_of_says_so_and_is_left_alone() {
    let bench = Bench::new();
    bench
        .targets
        .declare(FAKE, &[], &clock::now())
        .expect("cannot declare");

    let _reconciling = bench.reconciling();

    until("it was never written down", || {
        bench.entries().is_some_and(|entries| !entries.is_empty())
    });
    let entry = bench
        .entries()
        .unwrap()
        .pop()
        .expect("nothing was written down");
    assert_eq!(entry.state, State::NeedsAttention);
    assert_eq!(
        entry.last_error.as_deref(),
        Some("a fake endpoint has to be named")
    );
    assert_eq!(entry.identity, None);
}

#[test]
fn a_declaration_for_a_transport_this_build_has_never_heard_of_is_ignored() {
    let bench = Bench::new();
    bench
        .targets
        .declare("telepathy", &words(&["fileserver"]), &clock::now())
        .expect("cannot declare");
    bench.declare("fileserver");

    let _reconciling = bench.reconciling();

    until("the one it does know was never attached", || {
        bench.entry("fileserver").is_some()
    });
    // The one it cannot act on is simply not there, and nothing about it stopped
    // the other one being attached.
    assert_eq!(bench.entries().unwrap().len(), 1);
}

#[test]
fn stopping_detaches_everything_and_takes_the_record_away() {
    let bench = Bench::new();
    bench.declare("fileserver");
    let reconciling = bench.reconciling();
    until("it never attached", || {
        bench.session_of("fileserver").map(|entry| entry.status) == Some(SessionStatus::Blocked)
    });

    drop(reconciling);

    assert_eq!(bench.entries(), None);
    assert_eq!(
        bench.session_of("fileserver").map(|entry| entry.status),
        Some(SessionStatus::Done)
    );
    // And the declarations are untouched: what somebody asked for outlives the
    // daemon that was doing it.
    assert_eq!(bench.targets.list().unwrap().len(), 1);
}

#[test]
fn an_endpoint_is_reported_as_being_reached_before_it_is_reported_as_reached() {
    let bench = Bench::new();
    let gate = bench._run.path().join("answer-now");
    bench.declare("fileserver");

    let _reconciling = bench.waiting_for(gate.clone());

    // Nothing over there has said anything yet, and that is a state of its own
    // rather than an absence: somebody looking now is told it is being reached.
    until("it was never written down", || {
        bench.entry("fileserver").is_some()
    });
    let reaching = bench.entry("fileserver").expect("nothing was written down");
    assert_eq!(reaching.state, State::Connecting);

    std::fs::write(&gate, []).expect("cannot open the gate");

    until("it never attached", || {
        bench.entry("fileserver").map(|entry| entry.state) == Some(State::Attached)
    });
    let attached = bench.entry("fileserver").expect("nothing was written down");
    // And the moment it changed is what is stamped on it, not the moment it was
    // declared.
    assert!(attached.since >= reaching.since);

    bench.undeclare("fileserver");

    until("it was never taken off the list", || {
        bench.entry("fileserver").is_none()
    });
}

#[test]
fn what_was_declared_is_attached_to_again_by_whatever_reconciles_next() {
    let bench = Bench::new();
    bench.declare("fileserver");
    let reconciling = bench.reconciling();
    until("it never attached", || {
        bench.entry("fileserver").map(|entry| entry.state) == Some(State::Attached)
    });
    drop(reconciling);
    assert_eq!(bench.entries(), None);

    // A declaration outlives whatever was acting on it, which is the whole
    // reason it is written down somewhere a daemon does not own.
    let _restarted = bench.reconciling();

    until("it never attached again", || {
        bench.entry("fileserver").map(|entry| entry.state) == Some(State::Attached)
    });
}

#[test]
fn a_file_a_later_build_wrote_leaves_the_attachments_alone() {
    let bench = Bench::new();
    bench.declare("fileserver");
    let _reconciling = bench.reconciling();
    until("it never attached", || {
        bench.entry("fileserver").map(|entry| entry.state) == Some(State::Attached)
    });

    std::fs::write(bench.targets.path(), r#"{"v":99,"targets":[]}"#).expect("cannot write");

    // Nothing is torn down on the strength of a file that cannot be read: the
    // endpoint is still reached, and still says so.
    std::thread::sleep(QUICKLY * 10);
    assert_eq!(
        bench.entry("fileserver").map(|entry| entry.state),
        Some(State::Attached)
    );
}

#[test]
fn what_is_written_down_is_only_rewritten_when_something_changes() {
    let bench = Bench::new();
    bench.declare("fileserver");
    let _reconciling = bench.reconciling();
    until("it never attached", || {
        bench.entry("fileserver").map(|entry| entry.state) == Some(State::Attached)
    });

    let written = written_at(&bench);
    std::thread::sleep(QUICKLY * 10);

    assert_eq!(written_at(&bench), written);
}

/// When what is attached was last written down.
fn written_at(bench: &Bench) -> std::time::SystemTime {
    std::fs::metadata(bench.attachments.path())
        .and_then(|meta| meta.modified())
        .expect("cannot read when it was written")
}
