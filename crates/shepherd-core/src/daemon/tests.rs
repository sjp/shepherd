//! When a daemon is started, when another one is, and when this stops asking.
//!
//! Three kinds of test, for three questions. Most go through a stand-in that
//! records what it was asked to start and answers what the test told it to,
//! because the question is *when* a daemon is started and when the asking
//! stops, and a real bus would answer that no better while making the
//! interesting cases — a command that is not there, one that exits at once —
//! impossible to arrange on purpose.
//!
//! The next kind runs real processes: a script of this test's own standing in
//! for the bus's command, in a directory standing in for the machine's `PATH`.
//! That is where what gets *typed* is asserted, because a stand-in cannot tell
//! anybody whether the words would have run.
//!
//! The last kind runs the real bus, and only where this workspace has been
//! built: see [`built`]. Those are the tests that say the lock is respected,
//! because nothing else on this side of the boundary knows what the lock does.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;

use agentbus_protocol::Snapshot;
use tempfile::TempDir;

use crate::bus::Subscriber;

use super::*;

/// How long a test waits for a real process to do something before it concludes
/// it never will.
const PATIENCE: Duration = Duration::from_secs(20);

/// How often it looks while waiting.
const GLANCE: Duration = Duration::from_millis(10);

/// A bus command that is never run: it records what it was asked to start and
/// answers what the test told it to.
#[derive(Debug, Default)]
struct Stand {
    started: Vec<PathBuf>,
    answers: VecDeque<DaemonError>,
    ends: VecDeque<Ended>,
}

impl Stand {
    /// One that starts a daemon whenever it is asked.
    fn willing() -> Self {
        Self::default()
    }

    /// One with no bus command on it at all.
    fn absent() -> Self {
        let mut stand = Self::willing();
        stand.answers.extend([DaemonError::NotInstalled]);
        stand
    }

    /// Says that the daemon it started has ended, to be reported at the next
    /// look.
    fn ends(&mut self, ended: Ended) {
        self.ends.push_back(ended);
    }

    /// How many daemons it has been asked to start.
    fn starts(&self) -> usize {
        self.started.len()
    }
}

impl Daemons for Stand {
    fn start(&mut self, dir: &Path) -> Result<(), DaemonError> {
        self.started.push(dir.to_owned());
        match self.answers.pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn ended(&mut self) -> Option<Ended> {
        self.ends.pop_front()
    }
}

/// A lifecycle for a directory nothing else is using, with waits short enough
/// to be stepped past by arithmetic rather than by sleeping.
fn lifecycle(dir: &TempDir) -> Lifecycle {
    Lifecycle::new(SocketPaths::in_dir(dir.path()))
        .with_grace(Duration::from_millis(500))
        .with_patience(Duration::from_secs(5))
        .with_backoff(Duration::from_secs(2))
}

/// A directory for a test that is never served.
fn dir() -> TempDir {
    tempfile::tempdir().expect("a directory")
}

/// What the bus says when it begins a stream.
fn snapshot() -> Update {
    Update::Reset(Snapshot::new(1, Vec::new()))
}

#[test]
fn a_bus_that_is_already_there_is_never_started() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir);
    let mut stand = Stand::willing();
    let start = Instant::now();

    // The subscriber connects long before the grace is up, which is the whole
    // reason there is one.
    lifecycle.tick(&mut stand, start);
    lifecycle.heard(&snapshot(), start + Duration::from_millis(20));
    lifecycle.tick(&mut stand, start + Duration::from_secs(60));

    assert_eq!(stand.starts(), 0);
    assert_eq!(lifecycle.presence(), &Presence::Running);
}

#[test]
fn a_bus_that_is_not_there_is_started_once_the_grace_is_up() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir);
    let mut stand = Stand::willing();
    let start = Instant::now();

    lifecycle.tick(&mut stand, start);
    lifecycle.tick(&mut stand, start + Duration::from_millis(400));
    assert_eq!(stand.starts(), 0, "started before waiting for a stream");

    lifecycle.tick(&mut stand, start + Duration::from_millis(500));
    assert_eq!(stand.started, vec![dir.path().to_owned()]);
    assert_eq!(lifecycle.presence(), &Presence::Waiting);

    // Started, and not started again while it is being waited for.
    lifecycle.tick(&mut stand, start + Duration::from_secs(1));
    lifecycle.tick(&mut stand, start + Duration::from_secs(2));
    assert_eq!(stand.starts(), 1);

    lifecycle.heard(&snapshot(), start + Duration::from_secs(3));
    assert_eq!(lifecycle.presence(), &Presence::Running);
    assert_eq!(lifecycle.attempts(), 0, "a stream settles the count");
}

#[test]
fn the_wait_runs_from_the_first_tick_rather_than_from_construction() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir);
    let mut stand = Stand::willing();
    let built = Instant::now();

    // Nothing was ticked for a minute after this was made, and the grace is
    // still a grace.
    lifecycle.tick(&mut stand, built + Duration::from_secs(60));
    assert_eq!(stand.starts(), 0);

    lifecycle.tick(&mut stand, built + Duration::from_millis(60_400));
    assert_eq!(stand.starts(), 0);

    lifecycle.tick(&mut stand, built + Duration::from_millis(60_500));
    assert_eq!(stand.starts(), 1);
}

#[test]
fn a_stream_that_blinks_does_not_start_a_second_daemon() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir);
    let mut stand = Stand::willing();
    let start = Instant::now();

    lifecycle.heard(&snapshot(), start);
    lifecycle.heard(&Update::Disconnected, start + Duration::from_secs(1));
    assert_eq!(lifecycle.presence(), &Presence::Lost);

    // The subscriber reconnected on its own, well inside the grace.
    lifecycle.tick(&mut stand, start + Duration::from_millis(1_100));
    lifecycle.heard(&snapshot(), start + Duration::from_millis(1_200));
    lifecycle.tick(&mut stand, start + Duration::from_secs(30));

    assert_eq!(stand.starts(), 0);
    assert_eq!(lifecycle.presence(), &Presence::Running);
}

#[test]
fn a_stream_that_stays_away_gets_another_daemon() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir);
    let mut stand = Stand::willing();
    let start = Instant::now();

    lifecycle.heard(&snapshot(), start);
    lifecycle.heard(&Update::Disconnected, start + Duration::from_secs(1));
    lifecycle.tick(&mut stand, start + Duration::from_millis(1_400));
    assert_eq!(stand.starts(), 0);

    lifecycle.tick(&mut stand, start + Duration::from_millis(1_500));
    assert_eq!(stand.starts(), 1);
    assert_eq!(
        lifecycle.presence(),
        &Presence::Lost,
        "a bus being started is not a bus"
    );
}

#[test]
fn a_stream_that_ends_while_a_daemon_is_starting_does_not_start_another() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir);
    let mut stand = Stand::willing();
    let start = Instant::now();

    lifecycle.tick(&mut stand, start);
    lifecycle.tick(&mut stand, start + Duration::from_millis(500));
    assert_eq!(stand.starts(), 1);

    // The connection this had before was to whatever was there previously, and
    // its ending says nothing about the daemon that has just been started —
    // which is already being waited for.
    lifecycle.heard(&Update::Disconnected, start + Duration::from_millis(600));
    lifecycle.tick(&mut stand, start + Duration::from_secs(2));
    lifecycle.tick(&mut stand, start + Duration::from_secs(4));
    assert_eq!(stand.starts(), 1);
}

#[test]
fn a_daemon_that_stood_down_for_another_one_is_not_a_failure() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir);
    let mut stand = Stand::willing();
    let start = Instant::now();

    lifecycle.tick(&mut stand, start);
    lifecycle.tick(&mut stand, start + Duration::from_millis(500));
    assert_eq!(stand.starts(), 1);

    // It found the directory's lock held and exited at once. There is a bus;
    // this is simply not the process serving it, so nothing else is started and
    // the stream is waited for as before.
    stand.ends(Ended::AlreadyRunning);
    lifecycle.tick(&mut stand, start + Duration::from_millis(600));
    lifecycle.tick(&mut stand, start + Duration::from_secs(3));
    assert_eq!(stand.starts(), 1);
    assert_eq!(lifecycle.presence(), &Presence::Waiting);

    lifecycle.heard(&snapshot(), start + Duration::from_secs(4));
    assert_eq!(lifecycle.presence(), &Presence::Running);
}

#[test]
fn a_daemon_that_stops_without_serving_is_tried_again_and_then_given_up_on() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir).with_attempts(3);
    let mut stand = Stand::willing();
    let mut now = Instant::now();

    lifecycle.tick(&mut stand, now);
    now += Duration::from_millis(500);
    lifecycle.tick(&mut stand, now);
    assert_eq!(stand.starts(), 1);

    // First failure: another follows, after the backoff and not before it.
    stand.ends(Ended::Stopped { status: Some(1) });
    lifecycle.tick(&mut stand, now);
    lifecycle.tick(&mut stand, now + Duration::from_millis(1_900));
    assert_eq!(stand.starts(), 1);
    now += Duration::from_secs(2);
    lifecycle.tick(&mut stand, now);
    assert_eq!(stand.starts(), 2);

    // Second failure: the wait doubles.
    stand.ends(Ended::Stopped { status: Some(1) });
    lifecycle.tick(&mut stand, now);
    lifecycle.tick(&mut stand, now + Duration::from_millis(3_900));
    assert_eq!(stand.starts(), 2);
    now += Duration::from_secs(4);
    lifecycle.tick(&mut stand, now);
    assert_eq!(stand.starts(), 3);

    // Third: that was the last one allowed, so what went wrong becomes the
    // answer rather than the reason for another go.
    stand.ends(Ended::Stopped { status: Some(1) });
    lifecycle.tick(&mut stand, now);
    assert_eq!(
        lifecycle.presence(),
        &Presence::Unavailable(Unavailable::Stopped { status: Some(1) })
    );

    // And it stays the answer: an hour of ticks starts nothing.
    for minute in 1..=60 {
        lifecycle.tick(&mut stand, now + Duration::from_secs(minute * 60));
    }
    assert_eq!(stand.starts(), 3);
}

#[test]
fn a_daemon_that_never_serves_is_given_up_on() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir).with_attempts(1);
    let mut stand = Stand::willing();
    let start = Instant::now();

    lifecycle.tick(&mut stand, start);
    lifecycle.tick(&mut stand, start + Duration::from_millis(500));
    assert_eq!(stand.starts(), 1);

    // It is still running, and nothing has come from it.
    lifecycle.tick(&mut stand, start + Duration::from_secs(5));
    assert_eq!(lifecycle.presence(), &Presence::Waiting);

    lifecycle.tick(&mut stand, start + Duration::from_millis(5_500));
    assert_eq!(
        lifecycle.presence(),
        &Presence::Unavailable(Unavailable::NeverServed)
    );
}

#[test]
fn a_machine_without_the_bus_is_told_so_at_once_and_asked_no_further() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir);
    let mut stand = Stand::absent();
    let start = Instant::now();

    lifecycle.tick(&mut stand, start);
    lifecycle.tick(&mut stand, start + Duration::from_millis(500));
    assert_eq!(
        lifecycle.presence(),
        &Presence::Unavailable(Unavailable::NotInstalled),
        "a machine with no bus on it should say so"
    );

    // No backoff, no second attempt: installing it is somebody's decision, and
    // it is not going to happen because this asked again.
    for second in 1..=600 {
        lifecycle.tick(&mut stand, start + Duration::from_secs(second));
    }
    assert_eq!(stand.starts(), 1);
}

#[test]
fn a_bus_somebody_starts_by_hand_is_the_answer_however_this_had_given_up() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir);
    let mut stand = Stand::absent();
    let start = Instant::now();

    lifecycle.tick(&mut stand, start);
    lifecycle.tick(&mut stand, start + Duration::from_millis(500));
    assert!(matches!(lifecycle.presence(), Presence::Unavailable(_)));

    lifecycle.heard(&snapshot(), start + Duration::from_secs(90));
    assert_eq!(lifecycle.presence(), &Presence::Running);
}

#[test]
fn retrying_starts_another_daemon_without_waiting_out_a_grace() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir);
    let mut stand = Stand::absent();
    let start = Instant::now();

    lifecycle.tick(&mut stand, start);
    lifecycle.tick(&mut stand, start + Duration::from_millis(500));
    assert_eq!(stand.starts(), 1);

    // Somebody installed it and said so.
    let asked = start + Duration::from_secs(120);
    lifecycle.retry(asked);
    lifecycle.tick(&mut stand, asked);
    assert_eq!(stand.starts(), 2);
    assert_eq!(lifecycle.presence(), &Presence::Waiting);
}

#[test]
fn a_daemon_that_stops_while_the_stream_is_live_is_not_a_failure() {
    let dir = dir();
    let mut lifecycle = lifecycle(&dir);
    let mut stand = Stand::willing();
    let start = Instant::now();

    lifecycle.tick(&mut stand, start);
    lifecycle.tick(&mut stand, start + Duration::from_millis(500));
    lifecycle.heard(&snapshot(), start + Duration::from_secs(1));

    // The daemon this started lost a race for the directory and exited after
    // the stream had already arrived from the one that won it. The stream is
    // what says whether there is a bus, and it is still saying yes.
    stand.ends(Ended::Stopped { status: Some(1) });
    lifecycle.tick(&mut stand, start + Duration::from_secs(2));
    assert_eq!(lifecycle.presence(), &Presence::Running);
    assert_eq!(stand.starts(), 1);
}

/// The word that asks a script written by [`command_in`] to do nothing but
/// exist.
///
/// Not a word the bus has, and never passed by anything but the check below:
/// what a test asserts about the words a daemon is started with is worth
/// nothing if the checking put one of them there.
const PROBE: &str = "--is-this-runnable-yet";

/// A directory holding a script standing in for the bus's command, and a
/// [`Host`] that looks for commands in that directory and nowhere else.
///
/// The script is run once before it is returned, with [`PROBE`], which every
/// script written here answers by exiting at once whatever its body would
/// otherwise do. A file written and then executed immediately can be refused
/// while the handle that wrote it is still open — this process's own writer is
/// closed by then, but a forked child of a test running beside this one may
/// hold an inherited copy until it execs something of its own — so this waits
/// for it to become runnable rather than letting a test fail on somebody else's
/// timing.
fn command_in(dir: &Path, body: &str) -> Host {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(COMMAND);
    fs::write(
        &path,
        format!("#!/bin/sh\ncase \"$1\" in {PROBE}) exit 0 ;; esac\n{body}\n"),
    )
    .expect("a script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("a script anybody may run");

    let deadline = Instant::now() + PATIENCE;
    while let Err(problem) = Command::new(&path)
        .arg(PROBE)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        assert!(
            Instant::now() < deadline,
            "the command written for this test never became runnable: {problem}"
        );
        thread::sleep(GLANCE);
    }
    Host::searching([dir])
}

/// Waits for `ended` to say something, or fails saying what it was waiting for.
fn wait_for_end(host: &mut Host, expectation: &str) -> Ended {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(ended) = host.ended() {
            return ended;
        }
        assert!(
            Instant::now() < deadline,
            "waited {PATIENCE:?} for {expectation}"
        );
        thread::sleep(GLANCE);
    }
}

#[test]
fn a_machine_without_the_command_says_so_before_anything_is_started() {
    let dir = dir();
    let mut host = Host::searching(Vec::<PathBuf>::new());

    assert!(matches!(host.command(), Err(DaemonError::NotInstalled)));
    assert!(matches!(
        host.start(dir.path()),
        Err(DaemonError::NotInstalled)
    ));

    // A directory that has everything except the command.
    let host = Host::searching([dir.path()]);
    assert!(matches!(host.command(), Err(DaemonError::NotInstalled)));
}

#[test]
fn what_is_started_is_what_somebody_would_type() {
    let bin = dir();
    let served = dir();
    let record = bin.path().join("asked");
    let mut host = command_in(
        bin.path(),
        &format!(
            "for word in \"$@\"; do printf '%s\\n' \"$word\" >> {}; done",
            record.display()
        ),
    );

    host.start(served.path()).expect("a daemon");
    wait_for_end(&mut host, "the script to finish");

    let asked: Vec<String> = fs::read_to_string(&record)
        .expect("what the command was asked")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        asked,
        vec![
            "daemon".to_owned(),
            "--dir".to_owned(),
            served.path().display().to_string(),
        ],
        "the bus should be run exactly as somebody would run it by hand"
    );
}

#[test]
fn nothing_of_this_application_is_put_into_the_daemon_environment() {
    let bin = dir();
    let served = dir();
    let record = bin.path().join("environment");
    let mut host = command_in(bin.path(), &format!("env > {}", record.display()));

    host.start(served.path()).expect("a daemon");
    wait_for_end(&mut host, "the script to finish");

    // What the shell running the script sets for itself. Anything else the
    // daemon can see that this process cannot came from here, and nothing
    // should.
    const THE_SHELL_S_OWN: [&str; 3] = ["PWD", "SHLVL", "_"];

    let ours: HashSet<OsString> = std::env::vars_os().map(|(name, _)| name).collect();
    let theirs = fs::read_to_string(&record).expect("the daemon's environment");
    let added: Vec<&str> = theirs
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, _)| name)
        .filter(|name| !ours.contains(&OsString::from(name)))
        .filter(|name| !THE_SHELL_S_OWN.contains(name))
        .collect();

    assert!(
        added.is_empty(),
        "the daemon was given variables of this application's own: {added:?}"
    );
}

#[test]
fn a_daemon_that_exits_is_reported_by_how_it_exited() {
    let bin = dir();
    let mut host = command_in(bin.path(), "exit 1");
    host.start(dir().path()).expect("a daemon");
    assert_eq!(
        wait_for_end(&mut host, "the daemon to exit"),
        Ended::Stopped { status: Some(1) }
    );
    assert_eq!(host.ended(), None, "an end is reported once");

    // The status the bus keeps for a daemon that found the directory taken.
    let bin = dir();
    let mut host = command_in(bin.path(), "exit 3");
    host.start(dir().path()).expect("a daemon");
    assert_eq!(
        wait_for_end(&mut host, "the daemon to stand down"),
        Ended::AlreadyRunning
    );
}

#[test]
fn a_daemon_that_is_still_running_has_not_ended() {
    let bin = dir();
    let mut host = command_in(bin.path(), "exec sleep 60");
    host.start(dir().path()).expect("a daemon");

    assert_eq!(host.ended(), None);
    let Some(pid) = host.started() else {
        panic!("a daemon that was started should say which process it is");
    };

    // Nothing here stops a daemon, so this test does what a person would.
    stop(pid);
    assert!(matches!(
        wait_for_end(&mut host, "the daemon to stop"),
        Ended::Stopped { .. }
    ));
}

/// Asks a process to stop, the way somebody at a terminal would.
fn stop(pid: u32) {
    // Safe: a signal to a process this test started, with a number the platform
    // defines and an integer it can only refuse.
    let sent = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    assert_eq!(sent, 0, "cannot stop the process this test started");
}

/// The bus's own binary, where this workspace has been built.
///
/// The tests below need the real thing: what the lock does when two daemons
/// want one directory is knowable only from the daemon that implements it, and
/// a stand-in asserting the answer this side already believes would assert
/// nothing. A checkout that has not built it skips them rather than failing,
/// because a test binary can be run from a tree where only this crate was
/// built.
fn built() -> Option<PathBuf> {
    let path = std::env::current_exe()
        .ok()?
        // …/target/<profile>/deps/<this test binary>
        .parent()?
        .parent()?
        .join(COMMAND);
    path.is_file().then_some(path)
}

/// Runs a real daemon for `dir` the way somebody at a terminal would, and waits
/// for it to serve.
fn daemon_for(bus: &Path, dir: &Path) -> Child {
    let started = Command::new(bus)
        .arg(DAEMON)
        .arg(DIR)
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("a daemon");

    let paths = SocketPaths::in_dir(dir);
    let deadline = Instant::now() + PATIENCE;
    while std::os::unix::net::UnixStream::connect(paths.sub()).is_err() {
        assert!(
            Instant::now() < deadline,
            "the daemon this test started never served {}",
            dir.display()
        );
        thread::sleep(GLANCE);
    }
    started
}

#[test]
fn a_real_bus_is_started_where_there_is_none() {
    let Some(bus) = built() else {
        return;
    };
    let served = dir();
    let mut host = Host::searching([bus.parent().expect("a directory")]);
    let mut lifecycle = Lifecycle::new(SocketPaths::in_dir(served.path()))
        .with_grace(Duration::from_millis(100))
        .with_patience(PATIENCE);

    // The subscriber is the only thing that says whether there is a bus, so the
    // test drives exactly what an application would: everything it said, and a
    // look at the clock whether or not it said anything.
    let bus = Subscriber::at(SocketPaths::in_dir(served.path()))
        .with_backoff(Duration::from_millis(50), Duration::from_millis(200))
        .spawn();
    let deadline = Instant::now() + PATIENCE;
    while !lifecycle.presence().is_running() {
        match bus.updates().recv_timeout(GLANCE) {
            Ok(update) => lifecycle.heard(&update, Instant::now()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("the subscriber stopped"),
        }
        lifecycle.tick(&mut host, Instant::now());
        assert!(
            Instant::now() < deadline,
            "waited {PATIENCE:?} for a bus to be started in {}: {:?}",
            served.path().display(),
            lifecycle.presence()
        );
    }

    let pid = host
        .started()
        .expect("the bus should be the daemon this started, not one already there");
    stop(pid);
    assert!(matches!(
        wait_for_end(&mut host, "the daemon to stop"),
        Ended::Stopped { .. }
    ));
}

#[test]
fn a_second_real_daemon_stands_down_for_the_one_already_serving() {
    let Some(bus) = built() else {
        return;
    };
    let served = dir();
    let mut running = daemon_for(&bus, served.path());

    let mut host = Host::searching([bus.parent().expect("a directory")]);
    host.start(served.path()).expect("a daemon");
    assert_eq!(
        wait_for_end(&mut host, "the second daemon to stand down"),
        Ended::AlreadyRunning,
        "the bus allows one daemon per directory and the second should say so"
    );

    // The one that was already there is untouched by any of it.
    assert!(
        std::os::unix::net::UnixStream::connect(SocketPaths::in_dir(served.path()).sub()).is_ok(),
        "the daemon that was already serving should still be"
    );
    stop(running.id());
    let _ = running.wait();
}
