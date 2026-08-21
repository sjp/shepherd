//! Finding the coding agents on a machine.
//!
//! Each test describes a machine — a home directory with some configuration
//! directories in it, a search path with some commands on it — and asks what was
//! found there. Both kinds of evidence are covered separately, because a machine
//! usually has only one of them: a user who has run their agent once has the
//! configuration directory, and one who installed it system-wide but has not run
//! it yet has only the command.
//!
//! A machine of either family can be described from either, which is the whole
//! point of describing one: the rules for what a command is called and what
//! makes it runnable differ between them, and both sets have to be exercised
//! wherever these tests happen to be run.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use agentbus_install::agent::{AGENTS, Agent, DetectedAgent, detect};
use agentbus_install::paths::{Environment, Platform};

/// A machine with nothing on it.
fn machine() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().expect("cannot make a temporary directory");
    let home = root.path().join("home");
    let bin = root.path().join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin).unwrap();
    (root, home, bin)
}

/// A unix machine with `home` and `bin` on it.
fn described(home: &Path, bin: &Path) -> Environment {
    Environment::rooted(home)
        .with_path([bin])
        .with_platform(Platform::Unix)
}

/// Gives the machine `agent`'s configuration directory.
fn configured(env: &Environment, agent: Agent) -> PathBuf {
    let dir = agent.config_dir(env);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Puts `agent`'s command on the machine's search path.
fn installed(bin: &Path, agent: Agent) -> PathBuf {
    command(bin, agent.commands(Platform::Unix)[0])
}

/// Puts a runnable file called `name` on the machine's search path.
fn command(bin: &Path, name: &str) -> PathBuf {
    let command = bin.join(name);
    fs::write(&command, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();
    command
}

/// Which agents were found, in the order they were reported in.
fn agents(found: &[DetectedAgent]) -> Vec<Agent> {
    found.iter().map(|found| found.agent).collect()
}

#[test]
fn a_configuration_directory_is_enough() {
    let (_root, home, bin) = machine();
    let env = described(&home, &bin);
    let dir = configured(&env, Agent::Claude);

    let found = detect(&env);

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
    let env = described(&home, &bin);
    let command = installed(&bin, Agent::Codex);

    let found = detect(&env);

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
    let env = described(&home, &bin);
    let dir = configured(&env, Agent::OpenCode);
    let command = installed(&bin, Agent::OpenCode);

    let found = detect(&env);

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

    assert!(detect(&described(&home, &bin)).is_empty());
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
    let env = described(&home, &bin);
    for agent in AGENTS {
        configured(&env, agent);
        installed(&bin, agent);
    }

    let found = detect(&env);

    assert_eq!(agents(&found), AGENTS);
}

#[test]
fn agents_do_not_find_each_other() {
    let (_root, home, bin) = machine();
    let env = described(&home, &bin);
    configured(&env, Agent::Claude);
    installed(&bin, Agent::Codex);

    assert_eq!(agents(&detect(&env)), vec![Agent::Claude, Agent::Codex]);
}

#[test]
fn an_agent_run_by_a_name_that_is_not_its_own_is_still_found() {
    let (_root, home, bin) = machine();
    let env = described(&home, &bin);
    let command = command(&bin, "cursor-agent");

    let found = detect(&env);

    assert_eq!(
        found,
        vec![DetectedAgent {
            agent: Agent::Cursor,
            config_dir: None,
            command: Some(command),
        }]
    );
}

#[test]
fn an_agent_shipped_under_more_than_one_name_is_found_under_either() {
    let (_root, home, bin) = machine();
    let env = described(&home, &bin);
    let command = command(&bin, "kilo-code");

    let found = detect(&env);

    assert_eq!(agents(&found), vec![Agent::Kilo]);
    assert_eq!(found[0].command, Some(command));
}

#[test]
fn a_file_nobody_can_run_is_not_a_command() {
    let (_root, home, bin) = machine();
    fs::write(bin.join("droid"), "#!/bin/sh\n").unwrap();

    assert!(detect(&described(&home, &bin)).is_empty());
}

#[test]
fn an_agent_is_looked_for_where_its_own_variable_says() {
    let (_root, home, bin) = machine();
    let elsewhere = home.join("elsewhere");
    let env = described(&home, &bin).with_var(agentbus_install::agent::CODEX_HOME_VAR, &elsewhere);
    fs::create_dir_all(&elsewhere).unwrap();

    let found = detect(&env);

    assert_eq!(
        found,
        vec![DetectedAgent {
            agent: Agent::Codex,
            config_dir: Some(elsewhere),
            command: None,
        }],
        "the default directory is not what was looked at"
    );
}

#[test]
fn a_windows_machine_is_found_by_the_shims_its_commands_are_installed_as() {
    let (_root, home, bin) = machine();
    let env = described(&home, &bin).with_platform(Platform::Windows);
    // No executable bit anywhere, which is what these files look like on the
    // machine they belong to.
    fs::write(bin.join("agy.exe"), "").unwrap();
    fs::write(bin.join("qoder.cmd"), "").unwrap();

    let found = detect(&env);

    assert_eq!(agents(&found), vec![Agent::Antigravity, Agent::QoderCli]);
    assert_eq!(found[0].command, Some(bin.join("agy.exe")));
    assert_eq!(found[1].command, Some(bin.join("qoder.cmd")));
}

#[test]
fn what_a_windows_machine_is_run_by_is_not_what_a_unix_one_is() {
    let (_root, home, bin) = machine();
    command(&bin, "qoder");

    assert!(
        detect(&described(&home, &bin)).is_empty(),
        "a name that only exists on the other kind of machine is not looked for here"
    );
}
