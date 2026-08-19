//! `agentbus daemon` run the way anything else would run it: as a process,
//! given an environment, and killed when it is no longer wanted.
//!
//! Where the daemon listens is decided from the environment, and the tests that
//! matter for that have to be able to set one — which a test running inside the
//! daemon's own process cannot safely do. So the precedence rules are checked
//! here, on a real child process, rather than by unit tests calling `resolve`.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
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
    match bus_dir {
        Some(dir) => command.env("AGENTBUS_DIR", dir),
        None => command.env_remove("AGENTBUS_DIR"),
    };
    match runtime_dir {
        Some(dir) => command.env("XDG_RUNTIME_DIR", dir),
        None => command.env_remove("XDG_RUNTIME_DIR"),
    };
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
