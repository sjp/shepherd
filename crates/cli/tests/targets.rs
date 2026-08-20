//! Declaring endpoints at a shell, and being told what became of them.
//!
//! Everything here goes through the built binary against directories of the
//! test's own, because what these are about is the thing a person actually does:
//! type a command, look at a file, type another, and get an answer that makes
//! sense whether or not a bus happens to be running at the time.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long a test waits for something that should happen immediately.
const PATIENCE: Duration = Duration::from_secs(10);

/// The environment variables that would otherwise decide, behind a test's back,
/// which bus a command talks to and where it keeps its declarations.
const INHERITED: &[&str] = &[
    "AGENTBUS_CONFIG_DIR",
    "AGENTBUS_DIR",
    "AGENTBUS_LOG",
    "AGENTBUS_PANE",
    "AGENTBUS_PROC_ROOT",
    "AGENTBUS_STALE_SECS",
    "AGENTBUS_DONE_RETENTION_SECS",
    "AGENTBUS_ASSERT_HOLD_SECS",
    "XDG_CONFIG_HOME",
    "XDG_RUNTIME_DIR",
    "HOME",
];

/// One machine: somewhere to keep declarations, and somewhere a daemon would
/// serve.
struct Machine {
    temp: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        Self {
            temp: tempfile::tempdir().expect("cannot make a temporary directory"),
        }
    }

    /// Where the declarations are kept.
    fn config(&self) -> PathBuf {
        self.temp.path().join("config")
    }

    /// Where a daemon would serve.
    fn dir(&self) -> PathBuf {
        self.temp.path().join("bus")
    }

    /// The file the declarations are in.
    fn targets(&self) -> PathBuf {
        self.config().join("targets.json")
    }

    /// The file a running daemon writes what it is attached to in.
    fn attachments(&self) -> PathBuf {
        self.dir().join("attachments.json")
    }

    /// An `agentbus` command against this machine and nothing else.
    ///
    /// Where the declarations are goes in directly after the subcommand, which
    /// is the only place it can: the commands that declare something take
    /// everything after their own options as the words a transport was given,
    /// and a flag written after those would be one of them.
    fn command(&self, args: &[&str]) -> Command {
        let (subcommand, rest) = args.split_first().expect("no subcommand");
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
        command
            .arg(subcommand)
            .arg("--config-dir")
            .arg(self.config())
            .args(rest)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for variable in INHERITED {
            command.env_remove(variable);
        }
        command
    }

    /// Runs one command and hands back what it printed, insisting it succeeded.
    fn run(&self, args: &[&str]) -> String {
        let output = self.command(args).output().expect("cannot run agentbus");
        assert!(output.status.success(), "{}", said(&output));
        String::from_utf8(output.stdout).expect("what it printed is not text")
    }

    /// The same, for a command that is expected to be refused.
    fn refused(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().expect("cannot run agentbus");
        assert!(!output.status.success(), "{}", said(&output));
        output
    }

    /// What `agentbus targets --json` says, parsed.
    fn reported(&self) -> Value {
        let printed = self.run(&[
            "targets",
            "--dir",
            &self.dir().display().to_string(),
            "--json",
        ]);
        serde_json::from_str(&printed).expect("what it printed is not json")
    }

    /// Starts a daemon serving this machine, with no process table to read.
    fn daemon(&self) -> Daemon {
        let child = self
            .command(&[
                "daemon",
                "--dir",
                &self.dir().display().to_string(),
                "--proc-root",
                &self.temp.path().join("no-such-proc").display().to_string(),
            ])
            .spawn()
            .expect("cannot run agentbus daemon");
        until("the daemon never started", || self.attachments().exists());
        Daemon { child }
    }
}

/// A running daemon, stopped politely or killed with the test.
struct Daemon {
    child: Child,
}

impl Daemon {
    /// Asks it to stop and waits for it, so that whatever it takes away is gone
    /// by the time this returns.
    fn stop(mut self) {
        // Safe by construction: a signal number and the id of a child this test
        // started itself.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        let finished = self.child.wait().expect("cannot wait for the daemon");
        assert!(finished.success(), "the daemon exited with {finished}");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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

/// Waits for `wanted`, or fails the test saying what it was waiting for.
fn until(what: &str, wanted: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !wanted() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The mode a file is kept at.
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("no such file")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn declaring_and_undeclaring_go_round_and_the_file_is_the_owners_alone() {
    let machine = Machine::new();

    assert_eq!(machine.run(&["targets"]), "no targets\n");

    assert_eq!(
        machine.run(&["attach", "--", "-p", "2222", "bob@fs.example.net"]),
        "declared: ssh -p 2222 bob@fs.example.net\n"
    );

    assert_eq!(mode(&machine.targets()), 0o600);
    assert_eq!(mode(&machine.config()), 0o700);
    let listed = machine.reported();
    assert_eq!(listed["targets"].as_array().expect("no targets").len(), 1);
    assert_eq!(
        listed["targets"][0]["aliases"][0],
        serde_json::json!(["-p", "2222", "bob@fs.example.net"])
    );
    assert_eq!(listed["targets"][0]["transport"], "ssh");

    // Saying it again is what somebody's shell history does, and it changes
    // nothing.
    assert_eq!(
        machine.run(&["attach", "--", "-p", "2222", "bob@fs.example.net"]),
        "already declared\n"
    );
    assert_eq!(machine.reported()["targets"].as_array().unwrap().len(), 1);

    assert_eq!(
        machine.run(&["detach", "--", "-p", "2222", "bob@fs.example.net"]),
        "removed\n"
    );
    assert_eq!(
        machine.run(&["detach", "--", "-p", "2222", "bob@fs.example.net"]),
        "not declared\n"
    );
    assert!(machine.reported()["targets"].as_array().unwrap().is_empty());
}

#[test]
fn a_container_is_declared_by_naming_it_and_a_host_by_what_reaches_it() {
    let machine = Machine::new();

    machine.run(&["attach", "docker", "eager_mclean"]);
    machine.run(&["attach", "ssh", "--", "fileserver"]);

    let listed = machine.reported();
    let transports: Vec<&str> = listed["targets"]
        .as_array()
        .expect("no targets")
        .iter()
        .map(|target| target["transport"].as_str().expect("no transport"))
        .collect();
    assert_eq!(transports, ["docker", "ssh"]);
    assert_eq!(
        listed["targets"][0]["aliases"][0],
        serde_json::json!(["eager_mclean"])
    );
    assert_eq!(
        listed["targets"][1]["aliases"][0],
        serde_json::json!(["fileserver"])
    );

    // And a container has to be named, rather than being guessed at.
    let refused = machine.refused(&["attach", "docker"]);
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("needs something to reach"),
        "{}",
        said(&refused)
    );
}

#[test]
fn a_declaration_with_no_daemon_says_so_rather_than_failing() {
    let machine = Machine::new();
    machine.run(&["attach", "--", "fileserver"]);

    let printed = machine.run(&["targets", "--dir", &machine.dir().display().to_string()]);

    assert!(printed.contains("daemon not running"), "{printed}");
    assert_eq!(machine.reported()["daemon"], false);
}

#[test]
fn a_daemon_says_what_it_is_attached_to_while_it_runs_and_takes_it_away_afterwards() {
    let machine = Machine::new();
    // Words that could never reach anything, so that this is about what a
    // daemon says rather than about a machine somebody would have to own: ssh's
    // own grammar reads the last word as a command to run over there, which is
    // not a thing a target may carry.
    machine.run(&["attach", "--", "fileserver", "vim"]);

    let daemon = machine.daemon();

    let listed = machine.reported();
    assert_eq!(listed["daemon"], true);
    // It is being reported on rather than reached: the declaration was refused
    // before anything was connected to, which is a different answer from there
    // being no daemon to attach anything.
    assert_eq!(listed["targets"][0]["state"], "needs_attention");
    assert!(
        listed["targets"][0]["last_error"]
            .as_str()
            .expect("nothing was said about it")
            .contains("carries a command to run"),
        "{listed}"
    );

    daemon.stop();

    assert!(!machine.attachments().exists());
    // And what was declared is still declared: it belongs to the person who
    // declared it, not to the daemon that was acting on it.
    assert_eq!(
        machine.reported()["targets"][0]["state"],
        "daemon_not_running"
    );
    assert_eq!(machine.reported()["targets"][0]["declared"], true);
}

#[test]
fn declaring_from_several_shells_at_once_leaves_one_readable_file() {
    let machine = Machine::new();
    machine.run(&["attach", "--", "first"]);

    let racing: Vec<Child> = (0..8)
        .map(|index| {
            machine
                .command(&["attach", "--", &format!("host{index}")])
                .spawn()
                .expect("cannot run agentbus attach")
        })
        .collect();
    for mut declaring in racing {
        let finished = declaring.wait().expect("cannot wait for agentbus attach");
        assert!(finished.success(), "attach exited with {finished}");
    }

    // Every write is a whole file renamed over the last, so whatever survived
    // the race is a document, and the declaration that was there before them all
    // is still in it.
    let listed = machine.reported();
    let targets = listed["targets"].as_array().expect("no targets");
    assert!(!targets.is_empty(), "{listed}");
    assert!(
        targets
            .iter()
            .any(|target| target["aliases"][0] == serde_json::json!(["first"])),
        "{listed}"
    );
}
