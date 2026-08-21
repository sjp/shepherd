//! What an installation looks like on a machine that runs its scripts by
//! extension.
//!
//! Every agent this program installs for is supported on both kinds of machine,
//! so every one of them has a Windows answer to four questions: where its
//! configuration is, how its command is found, which file is written into it,
//! and how it is told to run that file. The answers differ per agent and were
//! built one agent at a time, so they are checked here in one place, against one
//! described machine, for all seventeen at once — a machine of that family
//! described from this one, which is what the [`Environment`] exists for.
//!
//! The home directory these tests describe is spelled the way that machine
//! spells one, separators and all. Nothing here takes a path apart or puts one
//! together out of text: resolution only ever joins, so whatever spelling a home
//! directory arrives in survives into every path built from it, and a test that
//! described a machine of one family in the punctuation of the other would be
//! checking the wrong thing.
//!
//! What is deliberately *not* here is anything about running: no wrapper is
//! executed and no command is spawned. These are questions about what would be
//! written and what it would say, which is the half of the story a machine of
//! the other family can answer.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agentbus_install::agent::{
    AGENTS, Agent, CLAUDE_CONFIG_DIR_VAR, HERMES_HOME_VAR, PI_CONFIG_DIR_VAR,
};
use agentbus_install::paths::{Environment, LOCAL_APP_DATA_VAR, Platform, USER_PROFILE_VAR};
use agentbus_install::{Change, Installer, State, installers};

/// The profile directory the described machine gives its user, spelled as that
/// machine spells one.
const PROFILE: &str = r"Users\u";

/// Where that machine keeps the binary the hooks hand their events to. A
/// directory with a space in it, because the quoting of one is the difference
/// between a hook that runs and a hook that fails where nobody is looking.
const BINARY: &str = r"C:\Program Files\agentbus\agentbus.exe";

/// What runs the file installed for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// The interpreter is named and handed the path of the script.
    Named,
    /// The whole command is handed over encoded, because what runs it puts what
    /// it is given through a shell of its own first.
    Encoded,
    /// Nothing on the machine runs it: the agent loads it with the runtime it
    /// brought with it.
    Loaded,
}

/// What one agent's installation on such a machine is made of.
struct Expected {
    agent: Agent,
    /// The extension of the file whose mark says which generation the hooks
    /// are — the one that has to be the machine's own kind of script wherever
    /// the machine is what runs it.
    extension: &'static str,
    /// How the agent is told to run what was installed.
    run: Run,
    /// Where the agent keeps its configuration, below the home directory.
    config_dir: &'static [&'static str],
}

/// Every agent, and what a machine that runs its scripts by extension gets.
const EXPECTED: [Expected; 17] = [
    Expected {
        agent: Agent::Antigravity,
        extension: "ps1",
        run: Run::Named,
        config_dir: &[".gemini", "config"],
    },
    Expected {
        agent: Agent::Claude,
        extension: "ps1",
        run: Run::Named,
        config_dir: &[".claude"],
    },
    Expected {
        agent: Agent::Codex,
        extension: "ps1",
        run: Run::Named,
        config_dir: &[".codex"],
    },
    Expected {
        agent: Agent::Cursor,
        extension: "ps1",
        run: Run::Named,
        config_dir: &[".cursor"],
    },
    Expected {
        agent: Agent::Devin,
        extension: "ps1",
        run: Run::Named,
        config_dir: &[".config", "devin"],
    },
    Expected {
        agent: Agent::Droid,
        extension: "ps1",
        run: Run::Named,
        config_dir: &[".factory"],
    },
    Expected {
        agent: Agent::GithubCopilot,
        extension: "ps1",
        run: Run::Named,
        config_dir: &[".copilot"],
    },
    Expected {
        agent: Agent::Grok,
        extension: "ps1",
        run: Run::Named,
        config_dir: &[".grok"],
    },
    Expected {
        agent: Agent::Hermes,
        extension: "py",
        run: Run::Loaded,
        config_dir: &[".hermes"],
    },
    Expected {
        agent: Agent::Kilo,
        extension: "js",
        run: Run::Loaded,
        config_dir: &[".config", "kilo"],
    },
    Expected {
        agent: Agent::Kimi,
        extension: "ps1",
        run: Run::Named,
        config_dir: &[".kimi-code"],
    },
    Expected {
        agent: Agent::Mastracode,
        extension: "ps1",
        run: Run::Encoded,
        config_dir: &[".mastracode"],
    },
    Expected {
        agent: Agent::Omp,
        extension: "ts",
        run: Run::Loaded,
        config_dir: &[".omp", "agent"],
    },
    Expected {
        agent: Agent::OpenCode,
        extension: "js",
        run: Run::Loaded,
        config_dir: &[".config", "opencode"],
    },
    Expected {
        agent: Agent::Pi,
        extension: "ts",
        run: Run::Loaded,
        config_dir: &[".pi", "agent"],
    },
    Expected {
        agent: Agent::QoderCli,
        extension: "ps1",
        run: Run::Named,
        config_dir: &[".qoder"],
    },
    Expected {
        agent: Agent::Qwen,
        extension: "ps1",
        run: Run::Named,
        config_dir: &[".qwen"],
    },
];

/// What such a machine is told to run a script with.
const POWERSHELL: &str = "powershell -NoProfile -ExecutionPolicy Bypass";

/// A machine of that family, with a home directory of its own and nothing
/// installed on it.
///
/// The temporary directory is handed back with it because the machine only
/// exists while it does.
fn machine() -> (tempfile::TempDir, Environment) {
    let root = tempfile::tempdir().expect("cannot make a temporary directory");
    let home = root.path().join(PROFILE);
    fs::create_dir_all(&home).unwrap();
    let env = Environment::rooted(&home)
        .with_platform(Platform::Windows)
        .with_var(USER_PROFILE_VAR, &home);
    (root, env)
}

/// Gives the machine every agent's configuration directory, so that each of
/// them has somewhere to install into.
fn configured(env: &Environment) {
    for agent in AGENTS {
        fs::create_dir_all(agent.config_dir(env)).unwrap();
    }
}

/// The directory commands are looked up in, added to `env`.
fn with_path(env: Environment, root: &Path) -> (PathBuf, Environment) {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let env = env.with_path([&bin]);
    (bin, env)
}

/// What one agent's installer says it would do.
fn plan(installer: &dyn Installer, env: &Environment) -> Vec<Change> {
    installer
        .plan_install(env, &State::default(), Path::new(BINARY))
        .unwrap_or_else(|error| panic!("{}: {error}", installer.agent()))
}

/// Everything a plan would write, run together, so that a question can be asked
/// of an installation as a whole rather than of each of its files.
fn written(changes: &[Change]) -> String {
    changes
        .iter()
        .filter_map(|change| match change {
            Change::Create { contents, .. } | Change::Rewrite { contents, .. } => Some(&**contents),
            _ => None,
        })
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Whether an installation says `command` somewhere, however the file it went
/// into spells a string.
///
/// A command is written into a document as often as it is written into a
/// script, and a document escapes the separators and the quotes in it. Both
/// families of file this program writes a command into — the JSON ones and the
/// ones with tables in — escape those two characters the same way, so there are
/// exactly two spellings to look for and this asks about both.
fn says(text: &str, command: &str) -> bool {
    let quoted = serde_json::Value::from(command).to_string();
    let escaped = &quoted[1..quoted.len() - 1];
    text.contains(command) || text.contains(escaped)
}

/// The installers, by the agent each installs for.
fn by_agent() -> BTreeMap<Agent, &'static dyn Installer> {
    installers()
        .into_iter()
        .map(|installer| (installer.agent(), installer))
        .collect()
}

#[test]
fn every_agent_is_answered_for_here() {
    let listed: Vec<Agent> = EXPECTED.iter().map(|expected| expected.agent).collect();

    assert_eq!(listed, AGENTS, "an agent is missing from this file");
}

#[test]
fn every_agent_keeps_its_configuration_below_the_home_directory_that_machine_gave_it() {
    let (_root, env) = machine();

    for expected in EXPECTED {
        let dir = expected
            .config_dir
            .iter()
            .fold(env.home().to_owned(), |dir, segment| dir.join(segment));

        assert_eq!(expected.agent.config_dir(&env), dir, "{}", expected.agent);
        assert!(
            expected.agent.config_dir(&env).starts_with(env.home()),
            "{} left the home directory it was given",
            expected.agent
        );
    }
}

#[test]
fn a_directory_a_variable_named_is_taken_as_that_machine_spells_it() {
    let (_root, env) = machine();
    let moved = env.home().join(r"elsewhere\claude");
    let env = env
        .with_var(CLAUDE_CONFIG_DIR_VAR, &moved)
        .with_var(PI_CONFIG_DIR_VAR, r"my\omp");

    assert_eq!(Agent::Claude.config_dir(&env), moved);
    assert_eq!(
        Agent::Omp.config_dir(&env),
        env.home().join(r"my\omp").join("agent"),
        "a directory named below the home directory is joined to it, not spliced into it"
    );
}

#[test]
fn the_agent_that_keeps_its_configuration_in_the_application_data_goes_there() {
    let (root, env) = machine();
    let data = root.path().join(r"Users\u\AppData\Local");
    let env = env.with_var(LOCAL_APP_DATA_VAR, &data);

    assert_eq!(
        Agent::Hermes.config_dir(&env),
        data.join("hermes"),
        "a user with nothing but the profile directory the machine made them"
    );

    // A user who also has a home directory in the unix sense — one their shell
    // brought with it — finds it where it is on every other machine they use.
    let owned = env.clone().with_var(USER_PROFILE_VAR, r"C:\Users\somebody");
    assert_eq!(Agent::Hermes.config_dir(&owned), env.home().join(".hermes"));

    // And the agent's own variable wins over both, as it does everywhere.
    let named = env.with_var(HERMES_HOME_VAR, r"D:\hermes");
    assert_eq!(Agent::Hermes.config_dir(&named), Path::new(r"D:\hermes"));
}

#[test]
fn every_agents_command_is_found_under_the_extension_it_was_installed_with() {
    let (root, env) = machine();
    let (bin, env) = with_path(env, root.path());

    for agent in AGENTS {
        for name in agent.commands(Platform::Windows) {
            for shim in [".exe", ".cmd", ".bat", ".ps1"] {
                let command = bin.join(format!("{name}{shim}"));
                // Written without the executable bit, which is what a file on
                // a machine that has no such thing looks like from here.
                fs::write(&command, "").unwrap();

                assert_eq!(
                    agent.command(&env),
                    Some(command.clone()),
                    "{agent} is not found as {name}{shim}"
                );

                fs::remove_file(&command).unwrap();
            }
        }
        assert_eq!(agent.command(&env), None, "{agent} was found twice over");
    }
}

#[test]
fn every_agent_gets_the_kind_of_file_that_machine_can_read() {
    let (_root, env) = machine();
    configured(&env);
    let installers = by_agent();

    for expected in EXPECTED {
        let installer = installers[&expected.agent];
        let asset = installer.asset(&env);
        let changes = plan(installer, &env);

        assert_eq!(
            asset.extension().and_then(std::ffi::OsStr::to_str),
            Some(expected.extension),
            "{} installs {}",
            expected.agent,
            asset.display()
        );
        assert!(
            changes
                .iter()
                .any(|change| change.path() == Some(asset.as_path())),
            "{} plans nothing for {}: {changes:?}",
            expected.agent,
            asset.display()
        );
    }
}

#[test]
fn every_agent_is_told_to_run_what_was_installed_the_way_that_machine_runs_it() {
    let (_root, env) = machine();
    configured(&env);
    let installers = by_agent();

    for expected in EXPECTED {
        let installer = installers[&expected.agent];
        let text = written(&plan(installer, &env));
        let agent = expected.agent;
        let script = format!("{POWERSHELL} -File \"{}\"", installer.asset(&env).display());
        let encoded = format!("{POWERSHELL} -EncodedCommand ");

        match expected.run {
            Run::Named => assert!(says(&text, &script), "{agent} is not told to run {script}"),
            Run::Encoded => {
                assert!(
                    says(&text, &encoded),
                    "{agent} is not handed its command encoded"
                );
                assert!(
                    !says(&text, &script),
                    "{agent} is handed its command twice over"
                );
            }
            Run::Loaded => assert!(
                !text.contains(POWERSHELL),
                "{agent} loads its own file and is told to run nothing"
            ),
        }
    }
}

#[test]
fn every_installed_file_names_the_binary_the_way_whatever_reads_it_does() {
    let (_root, env) = machine();
    configured(&env);

    for installer in installers() {
        let agent = installer.agent();
        let text = written(&plan(installer, &env));

        assert!(
            !text.contains("@BINARY@"),
            "{agent} left the mark the binary goes over"
        );
        // Where the binary is named at all it is named in full, in the spelling
        // the file's own reader gives a path that is not to be interpreted: a
        // script this machine runs quotes it as it stands, and a plugin's
        // runtime reads a string in which the separators are escaped.
        let escaped = BINARY.replace('\\', r"\\");
        assert!(
            text.contains(&format!("'{BINARY}'")) || text.contains(&escaped),
            "{agent} does not name {BINARY} in any spelling a reader of its files would understand"
        );
    }
}

#[test]
fn what_this_program_keeps_for_itself_sits_below_that_home_directory_too() {
    let (_root, env) = machine();

    for path in [env.state_file(), env.data_dir().to_owned()] {
        assert!(
            path.starts_with(env.home()),
            "{} is not below the home directory",
            path.display()
        );
    }
}
