//! Installing for Grok's command line.
//!
//! The simplest installation here, and the agent is the reason: it reads every
//! JSON file in its `hooks` directory and merges what they say, so there is no
//! shared file to add to. This program writes a file of its own beside the
//! user's, and the two never meet. Nothing is parsed on the way in, nothing is
//! merged, and an uninstall is two deletions.
//!
//! So the pair of files installed here is a wrapper script and the small
//! configuration file that runs it, both wholly this program's. The wrapper is
//! where the generation mark lives, because it is the file whose content is the
//! installation; the configuration file is what makes the wrapper ever run.
//!
//! # The file's name is the claim on it
//!
//! There is nowhere in a hooks file this agent reads to write the mark every
//! entry in somebody else's document carries — a key it did not expect could be
//! a key it refuses, and a file this program owns outright has nothing to
//! distinguish itself *from*. What says the file is this program's is that it
//! is named after this program, in a directory whose whole purpose is to hold
//! one such file per tool. Anything else in there is somebody else's and is
//! never read, written or counted.
//!
//! # The wrapper stays inside the older shell standard
//!
//! It is named to `sh` rather than to `bash` by the entry that runs it. Nothing
//! in it needs more, and a machine with no `bash` on it is a machine where the
//! other spelling would install perfectly and then quietly never run.
//!
//! # A missing configuration directory is a refusal
//!
//! The directory is never made here. One that is not there means this agent has
//! never run on the machine, and an installer that made it would be creating
//! another program's home on the strength of a guess about its layout. The
//! `hooks` directory below it *is* made, because that one is where the agent
//! documents these files as going and a user who has never written a hook will
//! not have it.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::change::Change;
use crate::paths::{Environment, Platform};
use crate::state::{Ownership, State};
use crate::status::HookStatus;
use crate::{Error, Installer, assets, command, file, json, sentinel};

/// The directory below the agent's own that it reads hook files out of.
const HOOKS_DIR: &str = "hooks";

/// The file this program's own hook is declared in.
const CONFIG_FILE: &str = "agentbus.json";

/// Where in that file the event names hang from.
const HOOKS_KEY: &str = "hooks";

/// The event the wrapper is run on.
///
/// The one this program needs at install time: which session is running. What
/// every other event of this agent's means is decided from the payload the
/// wrapper forwards, and registering for the rest would be a cost paid on every
/// session for events nothing is waiting for.
const EVENT: &str = "SessionStart";

/// How long the agent is asked to allow the wrapper, in seconds.
///
/// Far longer than the wrapper can take, and there so that a machine which has
/// somehow made it slow cannot make somebody's session slow with it.
const TIMEOUT: u64 = 5;

/// What the wrapper says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// Grok's hooks: a wrapper script and the file that declares it, side by side
/// in a directory the agent reads whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grok;

impl Installer for Grok {
    fn agent(&self) -> Agent {
        Agent::Grok
    }

    /// Writes the wrapper, then the file that runs it.
    ///
    /// In that order. The declaration names a file, and a declaration naming one
    /// which is not there yet would be a session's worth of hooks that do
    /// nothing — a window that is short, avoidable, and hard to explain
    /// afterwards.
    fn plan_install(
        &self,
        env: &Environment,
        _state: &State,
        binary: &Path,
    ) -> Result<Vec<Change>, Error> {
        let home = config_dir(env);
        if !home.is_dir() {
            return Err(Error::Absent {
                agent: self.agent(),
                path: home,
            });
        }
        let mut changes = Vec::new();
        // Writing a file makes the directory above it anyway; this is here so
        // that the making is recorded, which is the one thing the uninstall
        // cannot work out for itself later.
        let hooks = hooks_dir(env);
        if !hooks.is_dir() {
            changes.push(Change::Make { path: hooks });
        }
        changes.push(plan_wrapper(env, binary)?);
        changes.push(plan_config(env)?);
        Ok(changes)
    }

    /// Takes the declaration away, then the wrapper.
    ///
    /// In that order for the same reason reversed: the declaration is what runs
    /// the file, so it goes first and nothing is ever pointed at something that
    /// is no longer there.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        // The declaration is removed because it is there, not because anything
        // in it was recognised. It is this program's file by its name, and a
        // record that has been lost — a home restored from a backup, a machine
        // whose configuration directory was copied from somewhere else — must
        // not be the difference between an uninstall that finishes and one that
        // leaves a hook behind still naming a wrapper.
        let path = config(env);
        let mut changes = vec![match path.exists() {
            true => Change::Delete { path },
            false => Change::Keep { path },
        }];

        let path = wrapper(env);
        // A file without the mark is passed over here rather than refused,
        // unlike on the way in: an uninstall that stopped because something of
        // the user's is in the way would be refusing to do the one thing it can
        // certainly do safely, which is nothing.
        changes.push(match read(&path)? {
            Some(text) if sentinel::is_generated(&text) => Change::Delete { path },
            _ => Change::Keep { path },
        });

        // Only the directory the record says this program made: one the user
        // already had is theirs however empty this leaves it, and it holds
        // their own hook files in any case.
        let hooks = hooks_dir(env);
        if state.ownership(&hooks) == Some(Ownership::Created) {
            changes.push(Change::Clear { path: hooks });
        }
        Ok(changes)
    }

    /// Reads the wrapper, and then looks for the declaration that runs it.
    ///
    /// The wrapper is the file that says which generation this is, because it is
    /// the one whose content is the installation. A wrapper nothing declares is
    /// a working file in an installation that never runs, which is exactly the
    /// case worth telling somebody about — and so is a declaration left over
    /// from an installation whose wrapper has since moved, because it names the
    /// path it was written with and not wherever one is now.
    fn status(&self, env: &Environment) -> Result<HookStatus, Error> {
        let path = self.asset(env);
        let Some(text) = read(&path)? else {
            return Ok(HookStatus::NotInstalled);
        };
        if !sentinel::is_generated(&text) {
            return Ok(HookStatus::NotInstalled);
        }
        let status = HookStatus::of_text(self.agent(), &text);
        Ok(status.confirmed(is_declared(env)?))
    }

    /// The wrapper this program drops in, which is the file the mark is in.
    fn asset(&self, env: &Environment) -> PathBuf {
        wrapper(env)
    }
}

/// Where the agent keeps its configuration.
fn config_dir(env: &Environment) -> PathBuf {
    Agent::Grok.config_dir(env)
}

/// The directory it reads hook files out of.
fn hooks_dir(env: &Environment) -> PathBuf {
    config_dir(env).join(HOOKS_DIR)
}

/// The wrapper this program drops in there.
fn wrapper(env: &Environment) -> PathBuf {
    hooks_dir(env).join(match env.platform() {
        Platform::Unix => "agentbus.sh",
        Platform::Windows => "agentbus.ps1",
    })
}

/// The file that declares it, beside it.
fn config(env: &Environment) -> PathBuf {
    hooks_dir(env).join(CONFIG_FILE)
}

/// What the wrapper should hold, with the binary written into it.
fn generated(platform: Platform, binary: &Path) -> Result<String, Error> {
    let named = match platform {
        Platform::Unix => command::in_shell(binary)?,
        Platform::Windows => command::in_powershell(binary)?,
    };
    Ok(assets::GROK_WRAPPER
        .text(platform)
        .replace(BINARY_MARK, &named))
}

/// What writing the wrapper would do, given whatever is at its path now.
///
/// A file of that name this program did not write stops the plan. It is the one
/// case where doing nothing is worse than saying so: silently leaving it would
/// report a successful installation of hooks that are not installed, and
/// silently replacing it would delete something a user wrote.
fn plan_wrapper(env: &Environment, binary: &Path) -> Result<Change, Error> {
    let path = wrapper(env);
    let contents = generated(env.platform(), binary)?;
    Ok(match read(&path)? {
        None => Change::Create {
            path,
            contents,
            executable: true,
        },
        Some(text) if !sentinel::is_generated(&text) => return Err(Error::NotOurs { path }),
        Some(text) if text == contents => Change::Keep { path },
        Some(_) => Change::Rewrite {
            path,
            contents,
            executable: true,
        },
    })
}

/// What this program's own hook file says: run the wrapper when a session
/// begins.
///
/// The group carries no matcher. This agent matches a pattern against the name
/// of a tool, the event has no tool in it, and inventing a pattern for that
/// would be saying something this program does not mean.
fn declaration(env: &Environment) -> Result<Value, Error> {
    let mut hook = Map::new();
    hook.insert("type".to_owned(), Value::from("command"));
    hook.insert(
        "command".to_owned(),
        Value::from(command::hook_command(
            Agent::Grok,
            env.platform(),
            &wrapper(env),
            None,
        )?),
    );
    hook.insert("timeout".to_owned(), Value::from(TIMEOUT));

    let mut group = Map::new();
    group.insert("hooks".to_owned(), Value::Array(vec![Value::Object(hook)]));

    let mut events = Map::new();
    events.insert(EVENT.to_owned(), Value::Array(vec![Value::Object(group)]));

    let mut document = Map::new();
    document.insert(HOOKS_KEY.to_owned(), Value::Object(events));
    Ok(Value::Object(document))
}

/// What writing that file would do, given whatever is at its path now.
///
/// Written whole, whatever is there. Nothing of the user's can be in it: the
/// directory it sits in is one file per tool, and this is the file named after
/// this one. What is compared is what the two files *say* rather than the bytes
/// they say it in, so that a copy an older build laid out differently is still
/// recognised as saying the same thing and left alone.
fn plan_config(env: &Environment) -> Result<Change, Error> {
    let path = config(env);
    let ours = declaration(env)?;
    let contents = json::render(&ours, json::DEFAULT_INDENT);
    Ok(match read(&path)? {
        None => Change::Create {
            path,
            contents,
            executable: false,
        },
        Some(text) if says(&text) == Some(ours) => Change::Keep { path },
        Some(_) => Change::Rewrite {
            path,
            contents,
            executable: false,
        },
    })
}

/// Whether the file on this machine declares the wrapper this program would
/// install now.
fn is_declared(env: &Environment) -> Result<bool, Error> {
    let Some(text) = read(&config(env))? else {
        return Ok(false);
    };
    Ok(says(&text) == Some(declaration(env)?))
}

/// What a hook file says, or nothing where it cannot be read as JSON at all.
fn says(text: &str) -> Option<Value> {
    json::parse(text).ok()
}

/// Reads a file that may not be there, naming it if reading fails.
fn read(path: &Path) -> Result<Option<String>, Error> {
    file::read(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::assets::tests::is_well_formed;

    /// The agent this module installs for.
    const AGENT: Agent = Agent::Grok;

    /// Where this program's own binary is, as far as these tests are concerned.
    const BINARY: &str = "/opt/bin/agentbus";

    /// A machine with a home directory that really exists and holds nothing.
    fn machine() -> (tempfile::TempDir, Environment) {
        let home = tempfile::tempdir().expect("cannot make a temporary directory");
        let env = Environment::rooted(home.path());
        (home, env)
    }

    /// The same machine, with the agent already on it.
    fn machine_with_agent() -> (tempfile::TempDir, Environment) {
        let (home, env) = machine();
        fs::create_dir_all(config_dir(&env)).expect("cannot make the agent's directory");
        (home, env)
    }

    /// What installing on `env` would do, with nothing installed before.
    fn plan(env: &Environment) -> Vec<Change> {
        Grok.plan_install(env, &State::default(), Path::new(BINARY))
            .expect("planning failed")
    }

    /// Carries `changes` out, so that the next plan sees the machine they left.
    fn apply(changes: &[Change], state: &mut State) {
        for change in changes {
            change
                .apply(AGENT, state)
                .expect("carrying out a step failed");
        }
    }

    /// Installs for real, and answers with the record it left.
    fn install(env: &Environment) -> State {
        let mut state = State::default();
        let changes = plan(env);
        apply(&changes, &mut state);
        state
    }

    /// Uninstalls for real, from the record an install left.
    fn uninstall(env: &Environment, state: &mut State) {
        let changes = Grok.plan_uninstall(env, state).expect("planning failed");
        apply(&changes, state);
    }

    /// The hook file as it stands.
    fn config_text(env: &Environment) -> Option<String> {
        fs::read_to_string(config(env)).ok()
    }

    /// The hook file, read as a document.
    fn config_document(env: &Environment) -> Value {
        serde_json::from_str(&config_text(env).expect("no hook file"))
            .expect("the hook file is not JSON")
    }

    /// The one command the installed hook file runs.
    fn declared_command(env: &Environment) -> String {
        config_document(env)[HOOKS_KEY][EVENT][0]["hooks"][0]["command"]
            .as_str()
            .expect("a command")
            .to_owned()
    }

    #[test]
    fn the_wrapper_obeys_the_rules_every_wrapper_obeys() {
        is_well_formed(AGENT, &assets::GROK_WRAPPER);
    }

    #[test]
    fn a_fresh_install_writes_the_wrapper_and_a_hook_file_that_runs_it() {
        let (_home, env) = machine_with_agent();

        install(&env);

        let written = fs::read_to_string(wrapper(&env)).expect("no wrapper");
        assert!(sentinel::is_generated(&written));
        assert!(
            written.contains(BINARY),
            "the wrapper does not name the binary"
        );
        assert_eq!(
            wrapper(&env).parent(),
            config(&env).parent(),
            "the two files this program owns are not beside each other",
        );
        assert!(
            declared_command(&env).contains("agentbus.sh"),
            "the hook file does not run the wrapper",
        );
    }

    #[test]
    fn the_installed_command_runs_the_wrapper_with_the_shell_every_machine_has() {
        let (_home, env) = machine_with_agent();

        install(&env);

        let command = declared_command(&env);
        assert!(
            command.starts_with("sh "),
            "{command:?} does not start with the shell this agent's wrapper is written for",
        );
    }

    #[test]
    fn an_agent_that_is_not_on_the_machine_is_refused_and_nothing_is_written() {
        let (_home, env) = machine();

        let refusal = Grok
            .plan_install(&env, &State::default(), Path::new(BINARY))
            .expect_err("a missing configuration directory has to stop the plan");

        let said = refusal.to_string();
        let home = config_dir(&env);
        assert!(
            said.contains(&home.display().to_string()) && said.contains(AGENT.name()),
            "{said:?} says neither where nor which agent",
        );
        assert!(!home.exists(), "the agent had its home made for it");
    }

    #[test]
    fn the_hook_files_of_other_tools_are_never_read_or_written() {
        let (_home, env) = machine_with_agent();
        fs::create_dir_all(hooks_dir(&env)).unwrap();
        let theirs = hooks_dir(&env).join("their-own-tool.json");
        // Not JSON at all, so a run that so much as parsed it would say so.
        fs::write(&theirs, "{ not json, and not this program's }").unwrap();

        let mut state = install(&env);
        assert_eq!(
            fs::read_to_string(&theirs).unwrap(),
            "{ not json, and not this program's }",
        );

        uninstall(&env, &mut state);
        assert!(theirs.exists(), "somebody else's hook file was removed");
        assert!(
            hooks_dir(&env).is_dir(),
            "a directory that was never this program's was removed",
        );
    }

    #[test]
    fn a_wrapper_somebody_else_wrote_stops_the_plan() {
        let (_home, env) = machine_with_agent();
        fs::create_dir_all(hooks_dir(&env)).unwrap();
        fs::write(wrapper(&env), "# something a user wrote\n").unwrap();

        let refusal = Grok
            .plan_install(&env, &State::default(), Path::new(BINARY))
            .expect_err("somebody else's file has to stop the plan");

        assert!(matches!(refusal, Error::NotOurs { .. }), "{refusal:?}");
    }

    #[test]
    fn installing_twice_changes_nothing_the_second_time() {
        let (_home, env) = machine_with_agent();
        let state = install(&env);
        let before = config_text(&env);

        let again = Grok
            .plan_install(&env, &state, Path::new(BINARY))
            .expect("planning failed");

        assert!(
            again
                .iter()
                .all(|change| matches!(change, Change::Keep { .. })),
            "{again:?}",
        );
        assert_eq!(config_text(&env), before);
    }

    #[test]
    fn a_hook_file_that_says_the_same_thing_in_a_different_shape_is_left_alone() {
        let (_home, env) = machine_with_agent();
        let state = install(&env);
        let compact = serde_json::to_string(&config_document(&env)).unwrap();
        fs::write(config(&env), &compact).unwrap();

        let again = Grok
            .plan_install(&env, &state, Path::new(BINARY))
            .expect("planning failed");

        assert!(
            again
                .iter()
                .all(|change| matches!(change, Change::Keep { .. })),
            "{again:?}",
        );
        assert_eq!(config_text(&env).as_deref(), Some(compact.as_str()));
    }

    #[test]
    fn a_dry_run_says_exactly_what_a_real_one_would_do() {
        let (_home, env) = machine_with_agent();
        let described = plan(&env);

        assert!(!wrapper(&env).exists());
        assert!(!config(&env).exists());

        let carried_out = plan(&env);
        assert_eq!(described, carried_out);

        let mut state = State::default();
        apply(&carried_out, &mut state);
        assert!(wrapper(&env).exists() && config(&env).exists());
    }

    #[test]
    fn uninstalling_leaves_nothing_of_its_own_behind() {
        let (_home, env) = machine_with_agent();
        let mut state = install(&env);

        uninstall(&env, &mut state);

        assert!(!wrapper(&env).exists());
        assert!(!config(&env).exists());
        assert!(
            !hooks_dir(&env).exists(),
            "a directory this program made was left standing empty",
        );
        assert!(
            config_dir(&env).is_dir(),
            "a directory that was never this program's was removed",
        );
    }

    #[test]
    fn a_hook_file_left_by_a_run_this_program_has_forgotten_is_still_removed() {
        let (_home, env) = machine_with_agent();
        install(&env);

        // The record gone, as it would be on a home restored from a backup.
        let mut state = State::default();
        uninstall(&env, &mut state);

        assert!(!config(&env).exists(), "a hook of this program's was left");
    }

    #[test]
    fn what_is_installed_is_what_the_machine_is_asked_about() {
        let (_home, env) = machine_with_agent();
        assert_eq!(Grok.status(&env).unwrap(), HookStatus::NotInstalled);

        let mut state = install(&env);
        let expected = crate::version::expected_version(AGENT);
        assert_eq!(Grok.status(&env).unwrap(), HookStatus::Current(expected));

        // A hook file edited by hand leaves the wrapper standing with nothing
        // to run it, and installing again puts it back.
        fs::write(config(&env), "{\n  \"hooks\": {}\n}\n").unwrap();
        assert_eq!(
            Grok.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected)
        );
        apply(&plan(&env), &mut state);
        assert_eq!(Grok.status(&env).unwrap(), HookStatus::Current(expected));

        // One that is not JSON at all is no more of a declaration than one
        // saying the wrong thing.
        fs::write(config(&env), "half a file").unwrap();
        assert_eq!(
            Grok.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected)
        );

        apply(&plan(&env), &mut state);
        uninstall(&env, &mut state);
        assert_eq!(Grok.status(&env).unwrap(), HookStatus::NotInstalled);
    }

    #[test]
    fn a_wrapper_moved_since_it_was_declared_is_an_installation_that_needs_repairing() {
        let (_home, env) = machine_with_agent();
        install(&env);
        let mut document = config_document(&env);
        document[HOOKS_KEY][EVENT][0]["hooks"][0]["command"] =
            Value::from("sh '/gone/agentbus.sh'");
        fs::write(config(&env), json::render(&document, json::DEFAULT_INDENT)).unwrap();

        assert_eq!(
            Grok.status(&env).unwrap(),
            HookStatus::NeedsRepair(crate::version::expected_version(AGENT)),
        );
    }
}
