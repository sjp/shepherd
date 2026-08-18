//! The life of a daemon process: starting one where another is already running,
//! starting one after another was killed outright, and stopping one politely.
//!
//! None of this can be checked inside the daemon's own process. Being the only
//! daemon in a directory is a claim about other processes, a crash is something
//! only a real process can suffer, and a signal has to be sent to a pid. So
//! these tests run the binary, and assert on what a shell would see: what is in
//! the directory, what is on stderr, and what the exit status was.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long a test will wait for something that should happen immediately.
const PATIENCE: Duration = Duration::from_secs(10);

/// How long a signalled daemon has to be gone.
const SHUTDOWN: Duration = Duration::from_secs(1);

/// A daemon process, killed when the test ends however it ends.
///
/// The child is optional only because collecting its output consumes it; while
/// the daemon is running it is always there.
struct Daemon {
    child: Option<Child>,
    dir: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Daemon {
    /// Starts `command` as a daemon serving `dir`.
    fn spawn(command: &mut Command, dir: &Path) -> Self {
        Self {
            child: Some(command.spawn().expect("cannot run agentbus")),
            dir: dir.to_owned(),
        }
    }

    /// The process itself.
    fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("the daemon has already exited")
    }

    /// The socket emitters connect to.
    fn emit(&self) -> PathBuf {
        self.dir.join("emit.sock")
    }

    /// Blocks until the daemon is accepting connections.
    fn wait_until_serving(&self) {
        let socket = self.emit();
        let deadline = Instant::now() + PATIENCE;
        while UnixStream::connect(&socket).is_err() {
            assert!(
                Instant::now() < deadline,
                "{} never started serving",
                socket.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Asks the daemon to stop, the way a supervisor does.
    fn terminate(&mut self) {
        self.signal(libc::SIGTERM);
    }

    /// Sends `signal` to the daemon.
    fn signal(&mut self, signal: i32) {
        let pid = i32::try_from(self.child().id()).expect("implausible pid");
        // Safe by construction: `kill` is given a pid this test owns and a
        // signal number, and reports failure through its return value.
        let sent = unsafe { libc::kill(pid, signal) };
        assert_eq!(sent, 0, "cannot signal the daemon");
    }

    /// Kills the daemon the way a machine losing power would.
    fn crash(&mut self) {
        self.child().kill().expect("cannot kill the daemon");
        self.child().wait().expect("cannot reap the daemon");
    }

    /// Waits for the daemon to exit, failing the test if it takes longer than
    /// `within`.
    fn wait_for_exit(&mut self, within: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child().try_wait().expect("cannot wait") {
                return status;
            }
            assert!(Instant::now() < deadline, "the daemon is still running");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for the daemon to exit and collects everything it said.
    fn output(mut self) -> Output {
        self.child
            .take()
            .expect("the daemon has already exited")
            .wait_with_output()
            .expect("cannot collect the output")
    }
}

/// An `agentbus daemon` command on `dir`, with nothing inherited from whoever is
/// running the tests.
fn command(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
    command
        .arg("daemon")
        .arg("--dir")
        .arg(dir)
        .env_remove("AGENTBUS_DIR")
        .env_remove("AGENTBUS_LOG")
        .env_remove("AGENTBUS_STALE_SECS")
        .env_remove("AGENTBUS_DONE_RETENTION_SECS")
        .env_remove("XDG_RUNTIME_DIR")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

/// Starts a daemon on `dir` with no flags beyond the directory, and waits for it
/// to serve.
fn start(dir: &Path) -> Daemon {
    let daemon = Daemon::spawn(&mut command(dir), dir);
    daemon.wait_until_serving();
    daemon
}

/// A directory for one test's bus, removed with the test.
fn bus_dir() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("cannot make a temporary directory");
    let dir = temp.path().join("bus");
    (temp, dir)
}

/// Connects, writes one event and closes, the way an emitter does.
fn emit_one(socket: &Path) {
    let mut stream = UnixStream::connect(socket).expect("cannot connect");
    stream
        .write_all(br#"{"v":1,"agent":"claude","session":"abc123","kind":"tool_start"}"#)
        .expect("cannot write");
}

#[test]
fn a_second_daemon_on_the_same_directory_refuses_to_start() {
    let (_temp, dir) = bus_dir();
    let mut first = start(&dir);

    let refused = command(&dir).output().expect("cannot run agentbus");

    assert_eq!(
        refused.status.code(),
        Some(3),
        "a second daemon exited with {:?}",
        refused.status
    );
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(
        said.contains(&format!(
            "agentbus daemon: another daemon is already running in {}",
            dir.display()
        )),
        "unhelpful refusal: {said}"
    );
    // Whatever the second one did, the first one is still the daemon here.
    emit_one(&first.emit());
    first.terminate();
}

#[test]
fn a_daemon_started_after_a_crash_needs_no_manual_cleanup() {
    let (_temp, dir) = bus_dir();
    let mut crashed = start(&dir);
    let socket = crashed.emit();

    crashed.crash();

    assert!(
        socket.exists(),
        "the crash cleaned up after itself, so this proves nothing"
    );
    assert!(dir.join("daemon.lock").exists(), "the lock file vanished");
    let _restarted = start(&dir);
}

#[test]
fn a_terminated_daemon_leaves_nothing_behind() {
    let (_temp, dir) = bus_dir();
    let mut daemon = start(&dir);

    daemon.terminate();

    let status = daemon.wait_for_exit(SHUTDOWN);
    assert!(status.success(), "exited with {status}");
    for left in ["emit.sock", "sub.sock", "daemon.lock"] {
        assert!(!dir.join(left).exists(), "{left} was left behind");
    }
    assert!(dir.exists(), "the directory itself was taken away");
}

#[test]
fn an_interrupted_daemon_leaves_nothing_behind_either() {
    let (_temp, dir) = bus_dir();
    let mut daemon = start(&dir);

    daemon.signal(libc::SIGINT);

    let output = daemon.output();
    assert!(output.status.success(), "exited with {}", output.status);
    assert!(
        !dir.join("emit.sock").exists(),
        "the socket was left behind"
    );
    assert!(
        !dir.join("daemon.lock").exists(),
        "the lock file was left behind"
    );
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("SIGINT"),
        "it did not say why it stopped: {said}"
    );
}

#[test]
fn a_directory_that_cannot_be_made_is_reported_and_fails() {
    let mut refused = command(Path::new("/proc/nowhere/bus"));

    let output = refused.output().expect("cannot run agentbus");

    assert_eq!(
        output.status.code(),
        Some(1),
        "exited with {}",
        output.status
    );
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("agentbus daemon: cannot prepare the bus directory /proc/nowhere/bus"),
        "unhelpful failure: {said}"
    );
}

#[test]
fn the_daemon_reports_how_it_was_started_and_why_it_stopped() {
    let (_temp, dir) = bus_dir();
    let mut command = command(&dir);
    command.args(["--stale-secs", "5", "--done-retention-secs", "7"]);
    let mut daemon = Daemon::spawn(&mut command, &dir);
    daemon.wait_until_serving();

    daemon.terminate();

    let output = daemon.output();
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "exited with {}", output.status);
    for expected in [
        "starting",
        env!("CARGO_PKG_VERSION"),
        &dir.display().to_string(),
        "stale_secs=5",
        "done_retention_secs=7",
        "SIGTERM",
    ] {
        assert!(
            said.contains(expected),
            "{expected:?} is missing from {said}"
        );
    }
}

#[test]
fn the_settings_can_be_given_in_the_environment_instead() {
    let (_temp, dir) = bus_dir();
    let mut command = command(&dir);
    command
        .env("AGENTBUS_STALE_SECS", "5")
        .env("AGENTBUS_DONE_RETENTION_SECS", "7");
    let mut daemon = Daemon::spawn(&mut command, &dir);
    daemon.wait_until_serving();

    daemon.terminate();

    let said = String::from_utf8_lossy(&daemon.output().stderr).into_owned();
    assert!(said.contains("stale_secs=5"), "{said}");
    assert!(said.contains("done_retention_secs=7"), "{said}");
}

#[test]
fn a_daemon_told_to_say_nothing_says_nothing() {
    let (_temp, dir) = bus_dir();
    let mut command = command(&dir);
    command.args(["--log-level", "off"]);
    let mut daemon = Daemon::spawn(&mut command, &dir);
    daemon.wait_until_serving();

    daemon.terminate();

    let output = daemon.output();
    assert!(output.status.success(), "exited with {}", output.status);
    assert!(
        output.stderr.is_empty(),
        "it said {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
