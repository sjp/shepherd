//! Being told to read the declarations again.
//!
//! `SIGHUP` is a signal a real process gets from something outside it, and what
//! it has to do here is reach a thread that is otherwise asleep until its next
//! look. Neither half of that can be staged inside the daemon, so this is a
//! whole daemon, running, signalled the way a shell or a supervisor would signal
//! it.
//!
//! This is the only test in this binary on purpose: a signal is delivered to the
//! process rather than to a test, so a second one running beside it would be
//! signalled too.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentbus_daemon::remote::attachments::Attachments;
use agentbus_daemon::remote::targets::Targets;
use agentbus_daemon::remote::transport::{Backoff, Error, Made, Registry, Running, Transport};
use agentbus_daemon::{Daemon, Settings, SocketPaths, clock};
use agentbus_protocol::Snapshot;

/// How long a test waits for something that should happen immediately.
const PATIENCE: Duration = Duration::from_secs(10);

/// The name the far end is declared under.
const FAKE: &str = "fake";

/// A far end that says it is a daemon with nothing to report, and then holds
/// the line open until it is stopped.
#[derive(Debug)]
struct Fake;

impl Fake {
    fn of(_args: &[String]) -> Made {
        Ok(Arc::new(Self) as Arc<dyn Transport>)
    }
}

impl Transport for Fake {
    fn kind(&self) -> &'static str {
        FAKE
    }

    fn label(&self) -> String {
        FAKE.to_owned()
    }

    fn identity(&self) -> Option<String> {
        Some(FAKE.to_owned())
    }

    fn install_path(&self, _version: &str) -> String {
        "/nowhere/agentbus".to_owned()
    }

    fn run(&self, _command: &str, _args: &[&str], _stdin: Option<&str>) -> Result<Running, Error> {
        let snapshot =
            serde_json::to_string(&Snapshot::new(1, Vec::new())).expect("cannot write a line");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("printf '%s\\n' '{snapshot}'\nexec sleep 3600\n"));
        Running::spawn(&mut command, None).map_err(|source| Error::Run {
            label: self.label(),
            command: "sh".to_owned(),
            source,
        })
    }

    fn copy_in(&self, _local: &Path, _remote: &str) -> Result<(), Error> {
        unreachable!("a fake endpoint is never provisioned")
    }

    fn backoff(&self) -> Backoff {
        Backoff {
            initial: Duration::from_secs(3_600),
            max: Duration::from_secs(3_600),
            multiplier: 1.0,
            jitter: 0.0,
        }
    }
}

/// Waits for `wanted`, or fails the test saying what it was waiting for.
fn until(what: &str, wanted: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !wanted() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Sends `signal` to this process, which is the process the daemon is in.
fn signal(signal: libc::c_int) {
    // Safe by construction: a signal number and this process's own id.
    let sent = unsafe { libc::kill(libc::getpid(), signal) };
    assert_eq!(sent, 0, "cannot signal this process");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_told_to_reload_reads_the_declarations_without_waiting_for_its_next_look() {
    let run = tempfile::tempdir().expect("cannot make a temporary directory");
    let config = tempfile::tempdir().expect("cannot make a temporary directory");
    let paths = SocketPaths::in_dir(run.path().join("agentbus"));
    let targets = Targets::in_dir(config.path());
    let attachments = Attachments::in_dir(paths.dir());
    let daemon = Daemon::bind(
        paths.clone(),
        Settings {
            // Long enough that a pass which happens at all happened because it
            // was asked for.
            reconcile_every: Duration::from_secs(3_600),
            proc_root: run.path().join("no-such-proc"),
            ..Settings::default()
        },
    )
    .expect("cannot start the daemon")
    .declaring(targets.clone())
    .discovering(Vec::new())
    .reaching(Registry::new().with(FAKE, Fake::of));
    let serving = tokio::spawn(daemon.run());

    until("the daemon never said what it was attached to", || {
        attachments.read().expect("cannot read what is attached") == Some(Vec::new())
    });
    targets
        .declare(FAKE, &["fileserver".to_owned()], &clock::now())
        .expect("cannot declare");

    signal(libc::SIGHUP);

    until("the declaration was never noticed", || {
        attachments
            .read()
            .expect("cannot read what is attached")
            .is_some_and(|entries| entries.len() == 1)
    });

    // And the signal is a reload rather than a stop: the daemon is still there
    // to be stopped afterwards.
    signal(libc::SIGTERM);
    serving.await.expect("the daemon panicked");
    assert!(!attachments.path().exists());
    assert_eq!(targets.list().expect("cannot read").len(), 1);
}
