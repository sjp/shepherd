//! Attaching to a daemon that says exactly what a test told it to say.
//!
//! The far end here is a shell script producing a stream, which is the smallest
//! thing that is a real process with a real pipe: the interesting parts of
//! merging are what happens to a line on the way through, and what happens when
//! a stream stops, and both of those want a stream that can be cut rather than a
//! function that can be told to return.

use std::path::Path;
use std::sync::Mutex;

use agentbus_protocol::{
    Agent, DaemonIdentity, Event, ForegroundChange, ForegroundEntry, ForegroundState, Heartbeat,
    Kind, OriginHop, SessionEntry, SessionStatus, Snapshot, Source, Timestamp, UnstampedEvent,
    observed_session_id,
};
use serde_json::json;
use tokio::sync::broadcast::error::TryRecvError;

use super::{Attachment, Settings, State, remembering};
use crate::bus::{Bus, Published};
use crate::remote::transport::{Backoff, Error, Running, Transport};

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a test waits for something that should happen almost at once.
const PATIENCE: Duration = Duration::from_secs(10);

/// The opaque string every correlated thing in these tests carries.
const SLOT: &str = "w9:p3";

/// A far end that says what it was told to and then holds the line open, or
/// hangs up, depending on the script it was given.
///
/// One script per attempt, the last one repeating, so that a test can say what
/// the far end looks like *after* a reconnection as easily as before one.
#[derive(Debug)]
struct Canned {
    scripts: Vec<String>,
    attempts: Mutex<usize>,
    identity: Option<String>,
    name: String,
}

impl Canned {
    /// A far end whose stream is `lines`, held open afterwards so that it stops
    /// only when something stops it.
    fn saying(lines: &[String]) -> Self {
        Self::running(&[held(lines)])
    }

    /// A far end running one shell script per attempt.
    fn running(scripts: &[String]) -> Self {
        Self {
            scripts: scripts.to_vec(),
            attempts: Mutex::new(0),
            identity: Some("canned".to_owned()),
            name: "canned".to_owned(),
        }
    }

    /// The same, under another name and with no identity of its own, for a test
    /// about what the far end says it is.
    fn anonymous(mut self, name: &str) -> Self {
        self.identity = None;
        self.name = name.to_owned();
        self
    }

    /// The same, with an identity it is sure of.
    fn identified(mut self, id: &str, name: &str) -> Self {
        self.identity = Some(id.to_owned());
        self.name = name.to_owned();
        self
    }

    /// How many times a stream has been asked for.
    fn attempts(&self) -> usize {
        *self.attempts.lock().expect("the count was poisoned")
    }
}

impl Transport for Canned {
    fn kind(&self) -> &'static str {
        "canned"
    }

    fn label(&self) -> String {
        self.name.clone()
    }

    fn identity(&self) -> Option<String> {
        self.identity.clone()
    }

    fn install_path(&self, _version: &str) -> String {
        "/nowhere/agentbus".to_owned()
    }

    /// Runs the script this attempt is owed, whatever it was asked to run: what
    /// a test cares about is the stream that comes back.
    fn run(&self, _command: &str, _args: &[&str], _stdin: Option<&str>) -> Result<Running, Error> {
        let mut attempts = self.attempts.lock().expect("the count was poisoned");
        let script = self.scripts[(*attempts).min(self.scripts.len() - 1)].clone();
        *attempts += 1;
        drop(attempts);
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        Running::spawn(&mut command, None).map_err(|source| Error::Run {
            label: self.label(),
            command: "sh".to_owned(),
            source,
        })
    }

    fn copy_in(&self, _local: &Path, _remote: &str) -> Result<(), Error> {
        unreachable!("a canned far end is never provisioned")
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

/// A far end that cannot be reached at all.
#[derive(Debug)]
struct Refusing {
    recoverable: bool,
    delay: Duration,
}

impl Transport for Refusing {
    fn kind(&self) -> &'static str {
        "refusing"
    }

    fn label(&self) -> String {
        "refusing".to_owned()
    }

    fn identity(&self) -> Option<String> {
        Some("refusing".to_owned())
    }

    fn install_path(&self, _version: &str) -> String {
        "/nowhere/agentbus".to_owned()
    }

    fn run(&self, command: &str, _args: &[&str], _stdin: Option<&str>) -> Result<Running, Error> {
        Err(Error::Run {
            label: self.label(),
            command: command.to_owned(),
            source: std::io::Error::other("there is nothing there"),
        })
    }

    fn copy_in(&self, _local: &Path, _remote: &str) -> Result<(), Error> {
        unreachable!("a far end that cannot be reached is never provisioned")
    }

    fn recoverable(&self, _failure: &dyn std::error::Error) -> bool {
        self.recoverable
    }

    fn backoff(&self) -> Backoff {
        Backoff {
            initial: self.delay,
            max: self.delay,
            multiplier: 1.0,
            jitter: 0.0,
        }
    }
}

/// A shell script that prints `lines` and then holds the line open until it is
/// stopped.
///
/// `exec` matters: the process that is left waiting is the one the transport
/// handed back, so killing that handle stops it rather than orphaning a sleep.
fn held(lines: &[String]) -> String {
    format!("{}exec sleep 3600\n", printing(lines))
}

/// The same, but hanging up once it has said its piece.
fn hangs_up(lines: &[String]) -> String {
    printing(lines)
}

fn printing(lines: &[String]) -> String {
    let mut script = String::new();
    for line in lines {
        assert!(!line.contains('\''), "a canned line cannot contain a quote");
        script.push_str(&format!("printf '%s\\n' '{line}'\n"));
    }
    script
}

fn at(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("not a timestamp")
}

fn now() -> Timestamp {
    at("2026-08-17T10:32:01.412Z")
}

/// A session as another daemon reports it.
fn reported(session: &str, status: SessionStatus, origin: Vec<OriginHop>) -> SessionEntry {
    SessionEntry {
        session: session.to_owned(),
        agent: Agent::Claude,
        status,
        source: Source::Hook,
        cwd: Some("/srv/project".to_owned()),
        correlation: Some(SLOT.to_owned()),
        origin,
        since: now(),
    }
}

/// Something another daemon saw in front of one of its shells.
fn watching(pid: u32, origin: Vec<OriginHop>) -> ForegroundEntry {
    ForegroundEntry {
        origin,
        state: Some(ForegroundState::Foreground),
        ..ForegroundEntry::new(pid, "claude", "claude --resume", now()).with_correlation(SLOT)
    }
}

fn line(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("cannot write a line")
}

fn container() -> OriginHop {
    OriginHop::new(OriginHop::CONTAINER, "9f3c", "build")
}

/// The hop a far end called `canned` puts on everything it relays.
fn canned_hop() -> OriginHop {
    OriginHop::new("canned", "canned", "canned")
}

/// An attachment to `transport`, with timings a test does not have to wait out.
fn attached(bus: &Arc<Bus>, transport: Arc<dyn Transport>) -> Attachment {
    attached_with(
        bus,
        transport,
        Settings {
            liveness: Duration::from_millis(200),
            stable: Duration::from_secs(3_600),
        },
    )
}

fn attached_with(bus: &Arc<Bus>, transport: Arc<dyn Transport>, settings: Settings) -> Attachment {
    Attachment::start(
        transport,
        crate::remote::Bootstrap::new(crate::VERSION),
        Arc::clone(bus),
        settings,
    )
}

/// Waits for `wanted`, or fails the test saying what it was waiting for.
fn until(what: &str, wanted: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !wanted() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Everything published so far, in the order it was published.
fn published(events: &mut tokio::sync::broadcast::Receiver<Published>) -> Vec<Published> {
    let mut lines = Vec::new();
    loop {
        match events.try_recv() {
            Ok(line) => lines.push(line),
            Err(TryRecvError::Empty | TryRecvError::Closed) => return lines,
            Err(TryRecvError::Lagged(_)) => {}
        }
    }
}

/// The events among them.
fn events(lines: Vec<Published>) -> Vec<Event> {
    lines
        .into_iter()
        .filter_map(|line| match line {
            Published::Event(event) => Some(event),
            Published::Foreground(_) => None,
        })
        .collect()
}

/// The one session the bus is reporting.
fn only_session(bus: &Bus) -> SessionEntry {
    let sessions = bus.sessions();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    sessions.into_iter().next().expect("there was one")
}

#[test]
fn what_the_far_end_already_knew_is_seeded_with_the_way_to_it() {
    let bus = Arc::new(Bus::new());
    let far = Canned::saying(&[line(&Snapshot::new(
        41,
        vec![reported("abc123", SessionStatus::Blocked, Vec::new())],
    ))]);
    let _attachment = attached(&bus, Arc::new(far));

    until("nothing was seeded", || !bus.sessions().is_empty());

    let session = only_session(&bus);
    assert_eq!(session.session, "abc123");
    // Seeded, not replayed: the far end folded these events and this end takes
    // its word for the answer, including how long it has been that way.
    assert_eq!(session.status, SessionStatus::Blocked);
    assert_eq!(session.since, now());
    assert_eq!(session.cwd.as_deref(), Some("/srv/project"));
    assert_eq!(session.correlation.as_deref(), Some(SLOT));
    assert_eq!(session.origin, vec![canned_hop()]);
}

#[test]
fn a_session_of_this_daemons_own_is_still_here_and_still_local() {
    let bus = Arc::new(Bus::new());
    bus.ingest(
        &serde_json::to_vec(
            &json!({"v": 1, "agent": "claude", "session": "local", "kind": "tool_start"}),
        )
        .unwrap(),
    )
    .expect("the local event was dropped");
    let far = Canned::saying(&[line(&Snapshot::new(
        41,
        vec![reported("abc123", SessionStatus::Blocked, Vec::new())],
    ))]);
    let _attachment = attached(&bus, Arc::new(far));

    until("nothing was seeded", || bus.sessions().len() == 2);

    let local = bus
        .sessions()
        .into_iter()
        .find(|session| session.session == "local")
        .expect("the local session went missing");
    assert!(local.origin.is_empty(), "{:?}", local.origin);
}

#[test]
fn an_event_from_there_is_renumbered_here_and_keeps_the_number_it_had() {
    let bus = Arc::new(Bus::new());
    let mut watching = bus.events();
    let far = Canned::saying(&[
        line(&Snapshot::new(41, Vec::new())),
        line(
            &UnstampedEvent::new(Agent::Claude, "abc123", Kind::ToolStart)
                .with_correlation(SLOT)
                .with_raw(json!({"tool": "Bash"}))
                .stamp(77, now()),
        ),
        line(&UnstampedEvent::new(Agent::Claude, "abc123", Kind::TurnEnd).stamp(78, now())),
        line(
            &UnstampedEvent::new(Agent::Claude, "abc123", Kind::Error)
                .with_raw(json!("it went wrong"))
                .stamp(79, now()),
        ),
    ]);
    let _attachment = attached(&bus, Arc::new(far));

    until("the events never arrived", || {
        bus.last_seq() >= 3 && bus.recent().len() == 3
    });

    let arrived = events(published(&mut watching));
    assert_eq!(arrived.len(), 3, "{arrived:?}");
    // Numbered here, in this daemon's own sequence, which is what a local
    // subscriber is promised.
    assert_eq!(
        arrived.iter().map(|event| event.seq).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    for event in &arrived {
        assert_eq!(event.origin, vec![canned_hop()]);
    }
    assert_eq!(
        arrived[0].raw,
        Some(json!({"tool": "Bash", "remote_seq": 77}))
    );
    assert_eq!(arrived[1].raw, Some(json!({"remote_seq": 78})));
    assert_eq!(
        arrived[2].raw,
        Some(json!({"remote_seq": 79, "value": "it went wrong"}))
    );
    // And they were folded and kept, exactly as a local emit would have been.
    assert_eq!(only_session(&bus).status, SessionStatus::Idle);
    assert_eq!(bus.recent().len(), 3);
}

#[test]
fn a_payload_keeps_whatever_it_was_beside_the_number_it_came_with() {
    assert_eq!(remembering(None, 7), Some(json!({"remote_seq": 7})));
    assert_eq!(
        remembering(Some(json!({"tool": "Bash"})), 7),
        Some(json!({"tool": "Bash", "remote_seq": 7}))
    );
    assert_eq!(
        remembering(Some(json!([1, 2])), 7),
        Some(json!({"remote_seq": 7, "value": [1, 2]}))
    );
    // Even a payload that already had the key: what the far end sent is what is
    // kept, and it is the number this end is about to replace.
    assert_eq!(
        remembering(Some(json!({"remote_seq": 1})), 7),
        Some(json!({"remote_seq": 7}))
    );
}

#[test]
fn a_heartbeat_from_there_is_never_passed_on() {
    let bus = Arc::new(Bus::new());
    let mut watching = bus.events();
    let far = Canned::saying(&[
        line(&Snapshot::new(41, Vec::new())),
        line(&Heartbeat::new(42, now())),
        line(&Heartbeat::new(43, now())),
        line(&UnstampedEvent::new(Agent::Claude, "abc123", Kind::ToolStart).stamp(44, now())),
    ]);
    let _attachment = attached(&bus, Arc::new(far));

    until("the event never arrived", || bus.last_seq() >= 1);

    let arrived = published(&mut watching);
    assert_eq!(arrived.len(), 1, "{arrived:?}");
    // The one line that was published is the event, and its number is one: the
    // heartbeats did not consume a place in this daemon's sequence either.
    assert_eq!(arrived[0].seq(), 1);
}

#[test]
fn a_chain_the_far_end_already_carried_stays_behind_this_one() {
    let bus = Arc::new(Bus::new());
    let mut watching = bus.events();
    let far = Canned::saying(&[
        line(&Snapshot::new(
            41,
            vec![reported(
                "abc123",
                SessionStatus::Working,
                vec![container()],
            )],
        )),
        line(
            &UnstampedEvent::new(Agent::Claude, "abc123", Kind::ToolStart)
                .with_origin(vec![container()])
                .stamp(42, now()),
        ),
    ]);
    let _attachment = attached(&bus, Arc::new(far));

    until("the event never arrived", || bus.last_seq() >= 1);

    // Outermost first: the way a subscriber here would go to reach it.
    assert_eq!(only_session(&bus).origin, vec![canned_hop(), container()]);
    assert_eq!(
        events(published(&mut watching))[0].origin,
        vec![canned_hop(), container()]
    );
}

#[test]
fn what_the_far_end_watches_is_reported_where_this_machine_watches_nothing() {
    // A bus with no process table under it, which is what a daemon on a machine
    // that has none is.
    let bus = Arc::new(Bus::new());
    assert_eq!(bus.foreground(), None);
    let mut watching_bus = bus.events();
    let far = Canned::saying(&[
        line(&Snapshot::new(41, Vec::new()).with_foreground(vec![watching(4471, Vec::new())])),
        line(&ForegroundChange::observed(
            42,
            now(),
            watching(4472, vec![container()]),
        )),
        line(&ForegroundChange::withdrawn(43, now(), SLOT)),
    ]);
    let _attachment = attached(&bus, Arc::new(far));

    until("the withdrawal never arrived", || bus.last_seq() >= 3);

    let lines = published(&mut watching_bus);
    assert_eq!(lines.len(), 3, "{lines:?}");
    let Published::Foreground(seeded) = &lines[0] else {
        panic!("{lines:?}")
    };
    assert_eq!(
        seeded.foreground.as_ref().expect("an observation").origin,
        vec![canned_hop()]
    );
    let Published::Foreground(observed) = &lines[1] else {
        panic!("{lines:?}")
    };
    assert_eq!(
        observed.foreground.as_ref().expect("an observation").origin,
        vec![canned_hop(), container()]
    );
    // Withdrawn, and this daemon says so rather than saying nothing: it is
    // relaying an answer, not giving one of its own.
    assert_eq!(bus.foreground(), Some(Vec::new()));
}

#[test]
fn a_far_end_that_watches_nothing_makes_this_one_claim_nothing() {
    let bus = Arc::new(Bus::new());
    let far = Canned::saying(&[line(&Snapshot::new(
        41,
        vec![reported("abc123", SessionStatus::Working, Vec::new())],
    ))]);
    let _attachment = attached(&bus, Arc::new(far));

    until("nothing was seeded", || !bus.sessions().is_empty());

    assert_eq!(bus.foreground(), None, "nobody in the chain is looking");
}

#[test]
fn a_stream_that_goes_quiet_is_dropped_and_picked_up_again() {
    let bus = Arc::new(Bus::new());
    let far = Arc::new(Canned::running(&[
        held(&[line(&Snapshot::new(
            41,
            vec![
                reported("abc123", SessionStatus::Blocked, Vec::new()),
                reported("def456", SessionStatus::Working, Vec::new()),
            ],
        ))]),
        held(&[line(&Snapshot::new(
            9,
            vec![reported("abc123", SessionStatus::Blocked, Vec::new())],
        ))]),
    ]));
    let attachment = attached(&bus, Arc::clone(&far) as Arc<dyn Transport>);

    until("it never attached", || {
        attachment.state() == State::Attached
    });
    // It says nothing at all after its snapshot, not even a heartbeat, which is
    // the whole of the evidence that it is gone.
    until("it never gave up on the quiet stream", || {
        far.attempts() >= 2
    });
    until("it never attached again", || {
        attachment.state() == State::Attached
            && status_of(&bus, "def456") == Some(SessionStatus::Done)
    });

    // The far end never died, so what it still knows it still says, and this end
    // takes it back exactly as it was.
    let sessions = bus.sessions();
    assert_eq!(
        sessions.len(),
        2,
        "a reconnection made a second row: {sessions:?}"
    );
    let session = sessions
        .iter()
        .find(|session| session.session == "abc123")
        .expect("the session it still knows went missing");
    assert_eq!(session.status, SessionStatus::Blocked);
    assert_eq!(session.since, now());
    assert_eq!(session.origin, vec![canned_hop()]);
}

/// What the bus says one session is doing, if it says anything about it.
fn status_of(bus: &Bus, session: &str) -> Option<SessionStatus> {
    bus.sessions()
        .into_iter()
        .find(|entry| entry.session == session)
        .map(|entry| entry.status)
}

#[test]
fn a_far_end_that_hangs_up_is_reached_again() {
    let bus = Arc::new(Bus::new());
    let far = Arc::new(Canned::running(&[hangs_up(&[line(&Snapshot::new(
        41,
        vec![reported("abc123", SessionStatus::Blocked, Vec::new())],
    ))])]));
    let attachment = attached(&bus, Arc::clone(&far) as Arc<dyn Transport>);

    until("it never tried again", || far.attempts() >= 3);
    assert_eq!(only_session(&bus).status, SessionStatus::Blocked);
    drop(attachment);
}

#[test]
fn something_that_does_not_begin_by_saying_what_it_knows_is_not_a_daemon() {
    let bus = Arc::new(Bus::new());
    let far = Arc::new(Canned::running(&[held(&[
        line(&Heartbeat::new(1, now())),
        line(&Snapshot::new(
            2,
            vec![reported("abc123", SessionStatus::Blocked, Vec::new())],
        )),
    ])]));
    let attachment = attached(&bus, Arc::clone(&far) as Arc<dyn Transport>);

    until("it never tried again", || far.attempts() >= 2);
    assert_ne!(attachment.state(), State::Attached);
    assert!(bus.sessions().is_empty(), "{:?}", bus.sessions());
}

#[test]
fn a_failure_that_trying_again_would_not_help_is_left_for_a_person() {
    let bus = Arc::new(Bus::new());
    let attachment = attached(
        &bus,
        Arc::new(Refusing {
            recoverable: false,
            delay: Duration::from_millis(20),
        }),
    );

    until("it never asked for help", || {
        matches!(attachment.state(), State::NeedsAttention { .. })
    });
    // And it stays there rather than retrying something that will fail the same
    // way, until it is detached.
    std::thread::sleep(Duration::from_millis(100));
    assert!(matches!(attachment.state(), State::NeedsAttention { .. }));
    attachment.detach();
}

#[test]
fn a_failure_worth_trying_again_is_tried_again() {
    let bus = Arc::new(Bus::new());
    let attachment = attached(
        &bus,
        Arc::new(Refusing {
            recoverable: true,
            delay: Duration::from_millis(60),
        }),
    );

    until(
        "it never counted a second attempt",
        || matches!(attachment.state(), State::Reconnecting { attempt } if attempt >= 1),
    );
}

#[test]
fn a_connection_that_lasted_puts_the_schedule_of_delays_back_to_the_beginning() {
    let counted = |stable: Duration| {
        let bus = Arc::new(Bus::new());
        let far = Canned::running(&[hangs_up(&[line(&Snapshot::new(41, Vec::new()))])]);
        let attachment = attached_with(
            &bus,
            Arc::new(far),
            Settings {
                liveness: Duration::from_millis(200),
                stable,
            },
        );
        let mut furthest = 0;
        let until = Instant::now() + Duration::from_millis(700);
        while Instant::now() < until {
            if let State::Reconnecting { attempt } = attachment.state() {
                furthest = furthest.max(attempt);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        furthest
    };

    // Every connection counts as having worked, so nothing ever accumulates.
    assert_eq!(counted(Duration::ZERO), 0);
    // None of them does, so the failures add up.
    assert!(counted(Duration::from_secs(3_600)) >= 2);
}

#[test]
fn detaching_ends_what_the_far_end_was_speaking_for_and_leaves_it_running() {
    let bus = Arc::new(Bus::new());
    let far = Canned::saying(&[line(
        &Snapshot::new(
            41,
            vec![reported("abc123", SessionStatus::Blocked, Vec::new())],
        )
        .with_foreground(vec![watching(4471, Vec::new())]),
    )]);
    let attachment = attached(&bus, Arc::new(far));
    until("nothing was seeded", || !bus.sessions().is_empty());

    attachment.detach();

    assert_eq!(only_session(&bus).status, SessionStatus::Done);
    assert_eq!(bus.foreground(), None, "nobody is looking any more");
}

#[test]
fn the_deeper_of_two_views_of_one_slot_is_the_one_that_is_reported() {
    let bus = Arc::new(Bus::new());
    let observed = |agent: &str, origin: Vec<OriginHop>| SessionEntry {
        session: observed_session_id(SLOT),
        agent: agent.into(),
        status: SessionStatus::Working,
        source: Source::Observed,
        cwd: None,
        correlation: Some(SLOT.to_owned()),
        origin,
        since: now(),
    };
    // One daemon can see that something is running in the slot; the one a level
    // further in can see that it is claude. Both are guesses about one slot.
    let near = Canned::running(&[held(&[line(&Snapshot::new(
        1,
        vec![observed(Agent::UNKNOWN, Vec::new())],
    ))])])
    .identified("near", "near");
    let far = Canned::running(&[held(&[line(&Snapshot::new(
        1,
        vec![observed("claude", vec![container()])],
    ))])])
    .identified("far", "far");

    let _near = attached(&bus, Arc::new(near));
    let _far = attached(&bus, Arc::new(far));

    until("both views never arrived", || {
        bus.sessions().len() == 1 && bus.sessions()[0].origin.len() == 2
    });

    // One row, and it is the one closest to what is running: through the far
    // end, and then through the container it saw it in.
    let session = only_session(&bus);
    assert_eq!(
        session.origin,
        vec![OriginHop::new("canned", "far", "far"), container()]
    );
}

#[test]
fn the_far_ends_own_account_of_what_it_is_settles_the_hop_when_nothing_else_can() {
    let bus = Arc::new(Bus::new());
    let far = Canned::saying(&[line(
        &Snapshot::new(
            41,
            vec![reported("abc123", SessionStatus::Working, Vec::new())],
        )
        .with_daemon(DaemonIdentity::new("9f3c1000:1000")),
    )])
    .anonymous("box");
    let _attachment = attached(&bus, Arc::new(far));

    until("nothing was seeded", || !bus.sessions().is_empty());

    assert_eq!(
        only_session(&bus).origin,
        vec![OriginHop::new("canned", "9f3c1000:1000", "box")]
    );
}

#[test]
fn an_attachment_takes_the_far_ends_word_for_what_it_is_where_it_has_none_of_its_own() {
    let bus = Arc::new(Bus::new());
    let far = Canned::saying(&[line(
        &Snapshot::new(
            41,
            vec![reported("abc123", SessionStatus::Working, Vec::new())],
        )
        .with_daemon(DaemonIdentity::new("9f3c1000:1000")),
    )])
    .anonymous("fileserver");
    let attachment = attached(&bus, Arc::new(far));

    // Nothing before it has been reached, because nothing has said anything.
    until("nothing was seeded", || !bus.sessions().is_empty());

    assert_eq!(attachment.identity().as_deref(), Some("9f3c1000:1000"));
    // Which is what lets two of these be compared: what a transport was asked
    // to reach is not what it found there.
    assert_eq!(attachment.label(), "fileserver");
}

#[test]
fn a_far_end_that_says_nothing_about_itself_leaves_an_attachment_with_no_identity() {
    let bus = Arc::new(Bus::new());
    let far = Canned::saying(&[line(&Snapshot::new(
        41,
        vec![reported("abc123", SessionStatus::Working, Vec::new())],
    ))])
    .anonymous("fileserver");
    let attachment = attached(&bus, Arc::new(far));

    until("nothing was seeded", || !bus.sessions().is_empty());

    // Rather than a name from this side, which would compare equal to the name
    // this side made up for somewhere else.
    assert_eq!(attachment.identity(), None);
}

#[test]
fn a_transport_that_knows_what_it_reached_is_not_told_otherwise() {
    let bus = Arc::new(Bus::new());
    let far = Canned::saying(&[line(
        &Snapshot::new(
            41,
            vec![reported("abc123", SessionStatus::Working, Vec::new())],
        )
        .with_daemon(DaemonIdentity::new("somebody-else")),
    )])
    .identified("9f3c", "build");
    let _attachment = attached(&bus, Arc::new(far));

    until("nothing was seeded", || !bus.sessions().is_empty());

    assert_eq!(
        only_session(&bus).origin,
        vec![OriginHop::new("canned", "9f3c", "build")]
    );
}

#[test]
fn a_line_of_a_kind_this_build_has_never_heard_of_is_skipped() {
    let bus = Arc::new(Bus::new());
    let far = Canned::saying(&[
        line(&Snapshot::new(41, Vec::new())),
        line(&json!({"v": 1, "kind": "invented_later", "seq": 42})),
        String::from("this is not json at all"),
        line(&UnstampedEvent::new(Agent::Claude, "abc123", Kind::ToolStart).stamp(43, now())),
    ]);
    let _attachment = attached(&bus, Arc::new(far));

    until("the event after them never arrived", || bus.last_seq() >= 1);
    assert_eq!(only_session(&bus).status, SessionStatus::Working);
}
