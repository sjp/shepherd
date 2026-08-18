//! Finding the coding agents on a machine.
//!
//! Each test describes a machine — a home directory with some configuration
//! directories in it, a search path with some commands on it — and asks what was
//! found there. Both kinds of evidence are covered separately, because a machine
//! usually has only one of them: a user who has run their agent once has the
//! configuration directory, and one who installed it system-wide but has not run
//! it yet has only the command.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use agentbus_install::agent::{AGENTS, Agent, DetectedAgent, detect};
use agentbus_install::paths::Environment;

/// A machine with nothing on it.
fn machine() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().expect("cannot make a temporary directory");
    let home = root.path().join("home");
    let bin = root.path().join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin).unwrap();
    (root, home, bin)
}

/// Gives the machine `agent`'s configuration directory.
fn configured(home: &Path, agent: Agent) -> PathBuf {
    let dir = agent.config_dir(home);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Puts `agent`'s command on the machine's search path.
fn installed(bin: &Path, agent: Agent) -> PathBuf {
    let command = bin.join(agent.name());
    fs::write(&command, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();
    command
}

#[test]
fn a_configuration_directory_is_enough() {
    let (_root, home, bin) = machine();
    let dir = configured(&home, Agent::Claude);

    let found = detect(&Environment::rooted(&home).with_path([&bin]));

    assert_eq!(
        found,
        vec![DetectedAgent {
            agent: Agent::Claude,
            config_dir: Some(dir),
            command: None,
        }]
    );
}

#[test]
fn a_command_on_the_search_path_is_enough() {
    let (_root, home, bin) = machine();
    let command = installed(&bin, Agent::Codex);

    let found = detect(&Environment::rooted(&home).with_path([&bin]));

    assert_eq!(
        found,
        vec![DetectedAgent {
            agent: Agent::Codex,
            config_dir: None,
            command: Some(command),
        }]
    );
}

#[test]
fn both_are_reported_when_both_are_there() {
    let (_root, home, bin) = machine();
    let dir = configured(&home, Agent::OpenCode);
    let command = installed(&bin, Agent::OpenCode);

    let found = detect(&Environment::rooted(&home).with_path([&bin]));

    assert_eq!(
        found,
        vec![DetectedAgent {
            agent: Agent::OpenCode,
            config_dir: Some(dir),
            command: Some(command),
        }]
    );
}

#[test]
fn a_bare_machine_has_nothing_on_it() {
    let (_root, home, bin) = machine();

    assert!(detect(&Environment::rooted(&home).with_path([&bin])).is_empty());
}

#[test]
fn a_search_path_that_was_never_read_finds_no_commands() {
    let (_root, home, bin) = machine();
    installed(&bin, Agent::Claude);

    assert!(detect(&Environment::rooted(&home)).is_empty());
}

#[test]
fn every_agent_can_be_found() {
    let (_root, home, bin) = machine();
    for agent in AGENTS {
        configured(&home, agent);
        installed(&bin, agent);
    }

    let found = detect(&Environment::rooted(&home).with_path([&bin]));

    assert_eq!(
        found.iter().map(|found| found.agent).collect::<Vec<_>>(),
        AGENTS
    );
}

#[test]
fn agents_do_not_find_each_other() {
    let (_root, home, bin) = machine();
    configured(&home, Agent::Claude);
    installed(&bin, Agent::Codex);

    let found = detect(&Environment::rooted(&home).with_path([&bin]));

    assert_eq!(
        found.iter().map(|found| found.agent).collect::<Vec<_>>(),
        vec![Agent::Claude, Agent::Codex]
    );
}
