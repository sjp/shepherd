//! `agentbus install` and `agentbus uninstall` against a described machine.
//!
//! These run the real binary with `HOME` and `PATH` pointed into a temporary
//! directory, so what they exercise is the whole path a user's invocation takes:
//! the argument parser, the detection rules reading the environment this process
//! set up, and the report. What is deliberately not exercised is any agent's
//! configuration, because this build knows how to install into none of them yet
//! — which is itself something a user is entitled to be told rather than left to
//! infer from an empty report.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A home directory, a search path and a state directory, all under one
/// temporary directory that is removed with the test.
struct Machine {
    _root: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    state: PathBuf,
}

impl Machine {
    /// A machine with no coding agent on it.
    fn new() -> Self {
        let root = tempfile::tempdir().expect("cannot make a temporary directory");
        let (home, bin, state) = (
            root.path().join("home"),
            root.path().join("bin"),
            root.path().join("state"),
        );
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        Self {
            _root: root,
            home,
            bin,
            state,
        }
    }

    /// Gives the machine a configuration directory for `agent`.
    fn configured(self, agent: &str) -> Self {
        let dir = match agent {
            "opencode" => self.home.join(".config").join(agent),
            _ => self.home.join(format!(".{agent}")),
        };
        fs::create_dir_all(dir).unwrap();
        self
    }

    /// Puts `agent`'s command on the machine's search path.
    fn installed(self, agent: &str) -> Self {
        let command = self.bin.join(agent);
        fs::write(&command, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();
        self
    }

    /// Runs the binary on this machine, with nothing inherited from whoever is
    /// running the tests.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agentbus"))
            .args(args)
            .env("HOME", &self.home)
            .env("PATH", &self.bin)
            .env("XDG_STATE_HOME", &self.state)
            .output()
            .expect("cannot run agentbus")
    }
}

/// What a run printed on stdout, once it has been checked for having succeeded.
fn succeeds(output: &Output) -> &str {
    assert!(output.status.success(), "{output:?}");
    std::str::from_utf8(&output.stdout).expect("output is not UTF-8")
}

/// Whether anything at all was written below `path`.
fn is_untouched(path: &Path) -> bool {
    !path.exists()
}

#[test]
fn a_machine_with_no_agent_on_it_is_told_so() {
    let machine = Machine::new();

    let output = machine.run(&["install"]);

    assert_eq!(
        succeeds(&output),
        "no coding agent found on this machine\n\
         nothing to install: this build has no installer for any agent\n"
    );
}

#[test]
fn an_agent_is_found_by_its_configuration_directory() {
    let machine = Machine::new().configured("claude");

    let report = succeeds(&machine.run(&["install"])).to_owned();

    assert!(
        report.starts_with("found claude (configuration directory"),
        "{report}"
    );
    assert!(report.contains(".claude)"), "{report}");
}

#[test]
fn an_agent_is_found_by_its_command() {
    let machine = Machine::new().installed("codex");

    let report = succeeds(&machine.run(&["install"])).to_owned();

    assert!(report.starts_with("found codex (command "), "{report}");
}

#[test]
fn every_agent_on_the_machine_is_reported() {
    let machine = Machine::new()
        .configured("claude")
        .installed("codex")
        .configured("opencode")
        .installed("opencode");

    let report = succeeds(&machine.run(&["uninstall"])).to_owned();

    assert!(report.contains("found claude"), "{report}");
    assert!(report.contains("found codex"), "{report}");
    assert!(
        report.contains("found opencode (configuration directory"),
        "{report}"
    );
    assert!(report.contains("nothing to uninstall"), "{report}");
}

#[test]
fn a_run_that_has_nothing_to_do_writes_nothing() {
    let machine = Machine::new().configured("claude");

    machine.run(&["install"]);
    machine.run(&["install", "--dry-run"]);
    machine.run(&["uninstall"]);

    assert!(
        is_untouched(&machine.state),
        "a run with nothing to do left something behind"
    );
}

#[test]
fn a_dry_run_is_accepted_and_says_what_it_would_do() {
    let machine = Machine::new().installed("claude");

    let report = succeeds(&machine.run(&["install", "--dry-run"])).to_owned();

    assert!(report.contains("found claude"), "{report}");
    assert!(report.contains("nothing to install"), "{report}");
}

#[test]
fn an_agent_can_be_named_and_several_can_be_named_at_once() {
    let machine = Machine::new();

    let output = machine.run(&["install", "--agent", "claude", "--agent", "codex"]);

    assert!(succeeds(&output).contains("nothing to install"));
}

#[test]
fn a_name_that_is_not_an_agent_is_refused_with_the_names_that_are() {
    let machine = Machine::new();

    let output = machine.run(&["install", "--agent", "emacs"]);

    assert!(!output.status.success(), "{output:?}");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(complaint.contains("emacs"), "{complaint}");
    assert!(complaint.contains("claude"), "{complaint}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn a_run_with_nowhere_to_look_says_which_variable_is_missing() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentbus"))
        .arg("uninstall")
        .env_remove("HOME")
        .env("PATH", "")
        .output()
        .expect("cannot run agentbus");

    assert!(!output.status.success(), "{output:?}");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(
        complaint.starts_with("agentbus uninstall: HOME"),
        "{complaint}"
    );
}
