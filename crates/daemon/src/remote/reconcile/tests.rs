//! Reconciling, driven by a transport whose far end is a shell script.
//!
//! What is being tested is the loop between two files and a set of attachments,
//! so the far end only has to be a real process producing a real stream that can
//! be cut: what makes an attachment interesting — that it reaches something,
//! that it can be stopped, that stopping it withdraws what it said — is the same
//! whether the process is a container on another machine or a shell here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use agentbus_protocol::{
    Agent, DaemonIdentity, SessionEntry, SessionStatus, Snapshot, Source, Timestamp,
};

use super::{Plan, Reconciling};
use crate::bus::Bus;
use crate::clock;
use crate::remote::attach;
use crate::remote::attachments::{Attachments, Entry, Sharing, State};
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

/// What a test says one set of words reaches: how it would be got to, and what
/// the daemon at the far end says it is.
///
/// The two are what the layers of identity are made of, and a test sets them
/// apart from each other on purpose. Several names may share a way in and be one
/// machine, share a way in and turn out to be two, or reach one machine by ways
/// in that look nothing alike — and each of those is a different answer.
#[derive(Debug, Clone)]
struct Endpoint {
    way_in: Option<String>,
    id: String,
}

impl Endpoint {
    /// One reached by `way_in`, where the daemon says it is `id`.
    fn new(way_in: &str, id: &str) -> Self {
        Self {
            way_in: Some(way_in.to_owned()),
            id: id.to_owned(),
        }
    }
}

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
    /// What this transport knows it reached before reaching it, which for one
    /// that cannot know is nothing.
    sure: Option<String>,
    /// How it would be got to.
    way_in: Option<String>,
    /// What the daemon at the far end says it is.
    id: String,
    /// Where every host told to leave its way in alone is written down.
    kept: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    /// One that knows what it reached the moment it is made, as a transport
    /// talking to something on this machine does.
    fn of(args: &[String]) -> Made {
        Self::made(args, None)
    }

    fn made(args: &[String], gate: Option<PathBuf>) -> Made {
        let name = Self::named(args)?;
        Ok(Arc::new(Self {
            sure: Some(name.clone()),
            id: name.clone(),
            name,
            gate,
            way_in: None,
            kept: Arc::new(Mutex::new(Vec::new())),
        }) as Arc<dyn Transport>)
    }

    /// One that knows only how it would get there, as a transport reaching
    /// across a network does, reaching whatever the test said those words reach.
    fn reaching(
        args: &[String],
        endpoints: &BTreeMap<String, Endpoint>,
        gate: Option<PathBuf>,
        kept: &Arc<Mutex<Vec<String>>>,
    ) -> Made {
        let name = Self::named(args)?;
        let endpoint = endpoints
            .get(&name)
            .unwrap_or_else(|| panic!("this test never said what {name} reaches"));
        Ok(Arc::new(Self {
            name,
            gate,
            sure: None,
            way_in: endpoint.way_in.clone(),
            id: endpoint.id.clone(),
            kept: Arc::clone(kept),
        }) as Arc<dyn Transport>)
    }

    fn named(args: &[String]) -> Result<String, String> {
        args.first()
            .cloned()
            .ok_or_else(|| "a fake endpoint has to be named".to_owned())
    }

    /// What the far end runs: wait, if it was told to, then say what it knows
    /// and stay on the line.
    fn script(&self) -> String {
        let snapshot = serde_json::to_string(&snapshot(&self.id)).expect("cannot write a line");
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
        self.sure.clone()
    }

    fn way_in(&self) -> Option<String> {
        self.way_in.clone()
    }

    fn keep_open(&self) {
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(self.name.clone());
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
///
/// The session belongs to the daemon rather than to the words that reached it,
/// so two sets of words that reach one daemon report one session — which is the
/// whole reason for not wanting to be attached to it twice.
fn snapshot(id: &str) -> Snapshot {
    let mut snapshot = Snapshot::new(
        1,
        vec![SessionEntry {
            session: format!("session-at-{id}"),
            agent: Agent::Claude,
            status: SessionStatus::Blocked,
            source: Source::Hook,
            cwd: None,
            correlation: None,
            origin: Vec::new(),
            since: at("2026-08-17T10:00:00.000Z"),
        }],
    );
    snapshot.daemon = Some(DaemonIdentity::new(id));
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

    /// A reconciler that reaches whatever this test says each set of words
    /// reaches, through transports that learn what they reached only by asking.
    fn reaching(&self, endpoints: &[(&str, Endpoint)]) -> Reaching {
        self.reaching_after(endpoints, None)
    }

    /// The same, saying nothing until `gate` exists, so that a test can look at
    /// what is known before any far end has answered.
    fn reaching_after(&self, endpoints: &[(&str, Endpoint)], gate: Option<PathBuf>) -> Reaching {
        let endpoints: BTreeMap<String, Endpoint> = endpoints
            .iter()
            .map(|(name, endpoint)| ((*name).to_owned(), endpoint.clone()))
            .collect();
        let kept = Arc::new(Mutex::new(Vec::new()));
        let registry = Registry::new().with(FAKE, {
            let kept = Arc::clone(&kept);
            move |args: &[String]| Fake::reaching(args, &endpoints, gate.clone(), &kept)
        });
        Reaching {
            _reconciling: self.with(registry, QUICKLY),
            kept,
        }
    }

    /// The one entry listing `name` among the words that reach it, whatever
    /// other names it is listed under.
    fn holding(&self, name: &str) -> Option<Entry> {
        self.entries()?
            .into_iter()
            .find(|entry| entry.aliases.contains(&words(&[name])))
    }

    /// How many endpoints are written down, or nothing before anything has been.
    fn count(&self) -> Option<usize> {
        Some(self.entries()?.len())
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

/// A running reconciler, and what the endpoints it reached were told about the
/// connections they were holding.
struct Reaching {
    _reconciling: Reconciling,
    kept: Arc<Mutex<Vec<String>>>,
}

impl Reaching {
    /// Every endpoint told to leave its way in open because something else was
    /// still reaching through it.
    fn kept(&self) -> Vec<String> {
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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

#[test]
fn three_names_for_one_machine_are_one_attachment_listing_all_three() {
    let bench = Bench::new();
    for name in ["fileserver", "192.168.0.4", "fs.example.net"] {
        bench.declare(name);
    }

    // The first two are obviously one endpoint before anything is reached; the
    // third is one only the daemon over there could have told anybody about.
    let reaching = bench.reaching(&[
        (
            "fileserver",
            Endpoint::new("bob@fileserver:22", "9f3c:1000"),
        ),
        (
            "192.168.0.4",
            Endpoint::new("bob@fileserver:22", "9f3c:1000"),
        ),
        (
            "fs.example.net",
            Endpoint::new("bob@fs.example.net:22", "9f3c:1000"),
        ),
    ]);

    until("the three names never became one endpoint", || {
        bench.count() == Some(1)
    });
    let entry = bench
        .holding("fileserver")
        .expect("nothing was written down");
    assert_eq!(entry.identity.as_deref(), Some("9f3c:1000"));
    assert_eq!(
        entry.aliases,
        vec![
            words(&["fileserver"]),
            words(&["192.168.0.4"]),
            words(&["fs.example.net"])
        ],
        "the names are listed in the order they were declared in"
    );
    // Not one connection: one of the three is reached by a way in that looks
    // nothing like the others', and only the far end's own account joined them.
    assert_eq!(entry.sharing, Some(Sharing::Separate));
    assert_eq!(entry.state, State::Attached);
    // The one that is left reading it is the one that was declared first.
    assert_eq!(entry.label, "fileserver");
    // And what that daemon knows is still on this bus: letting go of the two
    // streams reading the same daemon withdrew none of it.
    let session = bench
        .session_of("9f3c:1000")
        .expect("the session went missing");
    assert_eq!(session.status, SessionStatus::Blocked);
    assert_eq!(
        bench.bus.sessions().len(),
        1,
        "one daemon reached three ways is one daemon's worth of sessions"
    );
    assert_eq!(session.origin.len(), 1);
    assert_eq!(session.origin[0].id, "9f3c:1000");
    // The two that were let go share their connection with the one that was
    // kept, so they were told to leave it alone. The third does not.
    let kept = reaching.kept();
    assert_eq!(kept, vec!["192.168.0.4".to_owned()], "{kept:?}");
}

#[test]
fn two_machines_that_say_they_are_different_stay_two_attachments() {
    let bench = Bench::new();
    bench.declare("fileserver");
    bench.declare("buildbox");

    let _reaching = bench.reaching(&[
        (
            "fileserver",
            Endpoint::new("bob@fileserver:22", "9f3c:1000"),
        ),
        ("buildbox", Endpoint::new("bob@buildbox:22", "77a1:1000")),
    ]);

    until("they were never both attached", || {
        bench
            .entries()
            .is_some_and(|entries| entries.iter().all(|entry| entry.identity.is_some()))
            && bench.count() == Some(2)
    });
    assert_eq!(
        bench.holding("fileserver").unwrap().aliases,
        vec![words(&["fileserver"])]
    );
    assert_eq!(
        bench.holding("buildbox").unwrap().identity.as_deref(),
        Some("77a1:1000")
    );
}

#[test]
fn two_names_that_looked_alike_and_turn_out_to_be_two_machines_are_split() {
    let bench = Bench::new();
    bench.declare("fileserver");
    bench.declare("fileserver.old");
    let gate = bench._run.path().join("answer-now");

    // Both resolve to the same place, so before either has answered they are one
    // endpoint as far as anything here can tell.
    let _reaching = bench.reaching_after(
        &[
            (
                "fileserver",
                Endpoint::new("bob@fileserver:22", "9f3c:1000"),
            ),
            (
                "fileserver.old",
                Endpoint::new("bob@fileserver:22", "77a1:1000"),
            ),
        ],
        Some(gate.clone()),
    );

    until("they were never taken for one endpoint", || {
        bench.count() == Some(1)
    });
    let provisional = bench.holding("fileserver.old").expect("nothing is written");
    assert_eq!(provisional.identity, None, "nothing has said what it is");
    assert_eq!(provisional.way_in.as_deref(), Some("bob@fileserver:22"));
    assert_eq!(provisional.sharing, Some(Sharing::Shared));
    assert_eq!(provisional.aliases.len(), 2);

    std::fs::write(&gate, []).expect("cannot open the gate");

    // And the moment they say what they are, the guess is overturned.
    until("they were never told apart", || bench.count() == Some(2));
    assert_eq!(
        bench.holding("fileserver").unwrap().identity.as_deref(),
        Some("9f3c:1000")
    );
    assert_eq!(
        bench.holding("fileserver.old").unwrap().identity.as_deref(),
        Some("77a1:1000")
    );
    // Neither of them was ever let go, so neither had anything to say about the
    // connection they share.
    assert!(_reaching.kept().is_empty(), "{:?}", _reaching.kept());
}

#[test]
fn one_machine_reached_as_two_users_is_two_daemons() {
    let bench = Bench::new();
    bench.declare("root@fileserver");
    bench.declare("fileserver");

    let _reaching = bench.reaching(&[
        (
            "root@fileserver",
            Endpoint::new("root@fileserver:22", "9f3c:0"),
        ),
        (
            "fileserver",
            Endpoint::new("bob@fileserver:22", "9f3c:1000"),
        ),
    ]);

    until("they were never both attached", || {
        bench
            .entries()
            .is_some_and(|entries| entries.iter().all(|entry| entry.identity.is_some()))
    });
    // One machine, and the sockets on it are per-user, so these are two daemons
    // holding two sets of sessions.
    assert_eq!(bench.count(), Some(2));
    assert_eq!(
        bench
            .holding("root@fileserver")
            .unwrap()
            .identity
            .as_deref(),
        Some("9f3c:0")
    );
    assert_eq!(
        bench.holding("fileserver").unwrap().identity.as_deref(),
        Some("9f3c:1000")
    );
}

#[test]
fn the_last_name_left_for_an_endpoint_takes_over_reaching_it() {
    let bench = Bench::new();
    bench.declare("fileserver");
    bench.declare("192.168.0.4");
    let _reaching = bench.reaching(&[
        (
            "fileserver",
            Endpoint::new("bob@fileserver:22", "9f3c:1000"),
        ),
        (
            "192.168.0.4",
            Endpoint::new("bob@fileserver:22", "9f3c:1000"),
        ),
    ]);
    until("they never became one endpoint", || {
        bench.count() == Some(1)
    });

    bench.undeclare("fileserver");

    until("the name that was left never took over", || {
        bench.entry("192.168.0.4").map(|entry| entry.state) == Some(State::Attached)
    });
    let entry = bench
        .entry("192.168.0.4")
        .expect("nothing was written down");
    assert_eq!(entry.aliases, vec![words(&["192.168.0.4"])]);
    assert_eq!(entry.identity.as_deref(), Some("9f3c:1000"));
    // And the sessions of the daemon it reaches are on this bus, reported now by
    // the only attachment left.
    assert_eq!(
        bench.session_of("9f3c:1000").map(|entry| entry.status),
        Some(SessionStatus::Blocked)
    );
}
