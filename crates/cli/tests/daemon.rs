//! `agentbus daemon` run the way anything else would run it: as a process,
//! given an environment, and killed when it is no longer wanted.
//!
//! Where the daemon listens is decided from the environment, and the tests that
//! matter for that have to be able to set one — which a test running inside the
//! daemon's own process cannot safely do. So the precedence rules are checked
//! here, on a real child process, rather than by unit tests calling `resolve`.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kills the daemon when the test ends, however it ends.
struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `agentbus daemon` with `AGENTBUS_DIR` and `XDG_RUNTIME_DIR` set to
/// exactly what is given, and nothing inherited from whoever is running the
/// tests.
fn start(bus_dir: Option<&Path>, runtime_dir: Option<&Path>) -> Killed {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
    command.arg("daemon");
    // Somewhere there is no process table, since these tests are about where a
    // daemon puts its files rather than about what it can see. The path is
    // never created; a daemon that cannot read one simply reports nothing.
    command.env(
        "AGENTBUS_PROC_ROOT",
        std::env::temp_dir().join(format!("agentbus-no-process-table-{}", std::process::id())),
    );
    match bus_dir {
        Some(dir) => command.env("AGENTBUS_DIR", dir),
        None => command.env_remove("AGENTBUS_DIR"),
    };
    match runtime_dir {
        Some(dir) => command.env("XDG_RUNTIME_DIR", dir),
        None => command.env_remove("XDG_RUNTIME_DIR"),
    };
    // These tests are about where a daemon puts its files. Nothing here is to
    // reach the network on its own or write into the manifests of whoever is
    // running the tests.
    command.env("AGENTBUS_UPDATE_MANIFESTS", "0");
    Killed(command.spawn().expect("failed to run agentbus"))
}

/// Waits for `path` to exist, failing the test if it never does.
fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The permission bits of `path`.
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_daemon_listens_where_it_is_told_and_never_answers() {
    let temp = tempfile::tempdir().expect("cannot make a temporary directory");
    let bus = temp.path().join("bus");
    let runtime = temp.path().join("run");
    std::fs::create_dir(&runtime).expect("cannot make a runtime directory");
    let _daemon = start(Some(&bus), Some(&runtime));

    let socket = bus.join("emit.sock");
    wait_for(&socket);
    // Whatever else a daemon keeps for this user under the runtime directory,
    // the sockets are where it was told to put them and nowhere else.
    assert!(
        !runtime.join("agentbus").join("emit.sock").exists(),
        "an explicit directory did not win"
    );
    assert_eq!(mode(&bus), 0o700);
    assert_eq!(mode(&socket), 0o600);

    let mut stream = UnixStream::connect(&socket).expect("cannot connect");
    stream
        .write_all(br#"{"v":1,"agent":"claude","session":"abc123","kind":"tool_start"}"#)
        .expect("cannot write");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("cannot close");

    // The emitter's half of this exchange is over the moment it has written its
    // line: there is nothing to wait for, and nothing ever arrives.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("cannot set a read timeout");
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).expect("cannot read");
    assert!(reply.is_empty(), "the daemon replied with {reply:?}");
}

#[test]
fn without_an_explicit_directory_the_daemon_uses_the_runtime_directory() {
    let temp = tempfile::tempdir().expect("cannot make a temporary directory");
    let _daemon = start(None, Some(temp.path()));

    let dir = temp.path().join("agentbus");
    wait_for(&dir.join("emit.sock"));
    assert_eq!(mode(&dir), 0o700);
}

#[test]
fn the_help_says_how_to_stop_a_daemon_fetching_manifests() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentbus"))
        .args(["daemon", "--help"])
        .output()
        .expect("failed to run agentbus");
    let help = String::from_utf8_lossy(&output.stdout);

    // The flag and the variable behind it: whatever supervises a daemon often
    // cannot choose its arguments, and has to be able to find the other way.
    for word in ["--no-update-manifests", "AGENTBUS_UPDATE_MANIFESTS"] {
        assert!(help.contains(word), "the help omits {word:?}: {help}");
    }
}

/// The line a daemon logs about itself as it starts, from a child given exactly
/// `environment` on top of a directory of its own.
///
/// Reading it back is the only way to see what a real command line and a real
/// environment settled on, which is the thing being asserted: a test in this
/// process could set neither safely.
fn starting_line(environment: &[(&str, &str)]) -> String {
    let temp = tempfile::tempdir().expect("cannot make a temporary directory");
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
    command
        .arg("daemon")
        .env("AGENTBUS_DIR", temp.path().join("bus"))
        .env("AGENTBUS_PROC_ROOT", temp.path().join("no-process-table"))
        .env_remove("AGENTBUS_UPDATE_MANIFESTS")
        .stderr(Stdio::piped());
    for (name, value) in environment {
        command.env(name, value);
    }
    let mut daemon = Killed(command.spawn().expect("failed to run agentbus"));
    let stderr = daemon.0.stderr.take().expect("the daemon has no stderr");

    for line in BufReader::new(stderr).lines().take(20) {
        let line = line.expect("cannot read what the daemon said");
        if line.contains("starting") {
            return line;
        }
    }
    panic!("the daemon never said what it was starting with");
}

#[test]
fn whether_manifests_are_checked_for_is_settable_from_the_environment() {
    // A daemon that is checking is killed long before its first check, which is
    // a minute away; nothing here reaches the network.
    assert!(
        starting_line(&[]).contains("update_manifests=true"),
        "a daemon given nothing does not check for newer manifests",
    );

    for said in ["0", "false", "off", "no"] {
        let line = starting_line(&[("AGENTBUS_UPDATE_MANIFESTS", said)]);
        assert!(
            line.contains("update_manifests=false"),
            "{said:?} did not turn the checks off: {line}",
        );
    }
    assert!(
        starting_line(&[("AGENTBUS_UPDATE_MANIFESTS", "1")]).contains("update_manifests=true"),
        "a daemon told to check does not",
    );
}

#[test]
fn a_daemon_told_not_to_update_manifests_still_starts() {
    let temp = tempfile::tempdir().expect("cannot make a temporary directory");
    let bus = temp.path().join("bus");
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
    command
        .arg("daemon")
        .arg("--no-update-manifests")
        .env("AGENTBUS_DIR", &bus)
        .env("AGENTBUS_PROC_ROOT", temp.path().join("no-process-table"))
        // Deliberately contradicting the flag, which is the way round that
        // matters: whoever typed the flag meant it.
        .env("AGENTBUS_UPDATE_MANIFESTS", "1");
    let _daemon = Killed(command.spawn().expect("failed to run agentbus"));

    wait_for(&bus.join("emit.sock"));
}
