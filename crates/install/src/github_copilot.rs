//! Installing for GitHub Copilot's command line.
//!
//! Its settings file holds a `hooks` object of event names, which looks like the
//! shape four other agents here share — and then the entry underneath is a
//! different thing entirely. There is no array of invocations inside an entry:
//! the entry *is* the invocation, and the field the command goes in is the name
//! of the interpreter that will run it. `bash` on one kind of machine,
//! `powershell` on the other, with a `command` field beside them for a line that
//! suits either.
//!
//! That is why this is a module of its own rather than another description in
//! [`crate::nested_json`]. Normalizing it into the nested shape would produce a
//! file this agent reads and finds no command in, which is the quietest way an
//! installer can fail.
//!
//! # Which spelling of the event, and why it decides the payload
//!
//! This agent accepts two spellings of every event name, and the spelling that
//! is registered decides the shape of the payload the hook is handed: one
//! carries `hook_event_name` and `session_id`, the other `sessionId` and no
//! field naming the event at all. Registering the first is what lets the mapping
//! for this agent be written the way every other mapping here is written, and it
//! is the reason the choice is worth a paragraph rather than a constant.
//!
//! # The entry names the interpreter twice
//!
//! The field says which interpreter the command is for, and the command names
//! that interpreter again, because what is being run is a script file rather
//! than a line of shell. Naming it is how a script is run everywhere else in
//! this crate — see [`crate::command::hook_command`] — and it means the wrapper
//! needs no first line about itself and cannot be run by whatever a user's login
//! shell happens to be.
//!
//! # A missing configuration directory is a refusal
//!
//! The directory is never made here. One that is not there means this agent has
//! never run on the machine, and an installer that made it would be creating
//! another program's home on the strength of a guess about its layout.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::change::Change;
use crate::paths::{Environment, Platform};
use crate::state::{Ownership, State};
use crate::status::HookStatus;
use crate::{Error, Installer, Placement, assets, command, file, json, merge, sentinel};

/// The directory inside the configuration directory the wrapper is dropped
/// into.
///
/// Not because the entry needs it there — it names the wrapper by an absolute
/// path — but because this is where a person looking for an installed hook would
/// look, and it keeps a generated file out of the top of a directory somebody
/// reads.
const HOOKS_DIR: &str = "hooks";

/// The file the entry goes into.
///
/// The one this agent reads a user's own settings out of. It reads hooks from
/// several other places as well — a repository's own, a machine-wide policy —
/// and none of those is this program's to write to: they belong to a project or
/// to an administrator, and a user asking for their own hooks has not asked for
/// either.
const SETTINGS: &str = "settings.json";

/// The key in the settings file that the event names hang from.
const HOOKS_KEY: &str = "hooks";

/// The event that says a session has begun, in the spelling whose payload names
/// itself.
const SESSION_START: &str = "SessionStart";

/// How long the agent is asked to allow the wrapper, in the unit its entries
/// count in.
///
/// Far longer than the wrapper can take, and there so that a machine which has
/// somehow made it slow cannot make somebody's session slow with it. Written out
/// rather than left to the agent's own default, which is longer still.
const TIMEOUT_SECONDS: u64 = 5;

/// What the wrapper says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// GitHub Copilot's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GithubCopilot;

impl Installer for GithubCopilot {
    fn agent(&self) -> Agent {
        Agent::GithubCopilot
    }

    /// Writes the wrapper, then points the settings at it.
    ///
    /// In that order. The entry names a file, and a settings file that named one
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
        let hooks_dir = hooks_dir(env);
        if !hooks_dir.is_dir() {
            changes.push(Change::Make { path: hooks_dir });
        }
        changes.push(plan_wrapper(env, binary)?);
        changes.push(merge::plan_install(
            &settings(env),
            &[Placement::new([HOOKS_KEY, SESSION_START], entry(env)?)],
        )?);
        Ok(changes)
    }

    /// Takes the entry out, then the wrapper.
    ///
    /// In that order for the same reason reversed: the entry is what runs the
    /// file, so it goes first and nothing is ever pointed at something that is
    /// no longer there.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        let mut changes = vec![merge::plan_uninstall(&settings(env), state)?];

        let path = wrapper(env);
        // A file without the mark is passed over here rather than refused,
        // unlike on the way in: an uninstall that stopped because something of
        // the user's is in the way would be refusing to do the one thing it can
        // certainly do safely, which is nothing.
        changes.push(match read(&path)? {
            Some(text) if sentinel::is_generated(&text) => Change::Delete { path },
            _ => Change::Keep { path },
        });
        // Only the directory the record says this program made. The agent's own
        // configuration directory is never among them, because an installation
        // that found it missing refused rather than making one.
        let hooks_dir = hooks_dir(env);
        if state.ownership(&hooks_dir) == Some(Ownership::Created) {
            changes.push(Change::Clear { path: hooks_dir });
        }
        Ok(changes)
    }

    /// Reads the wrapper, and then looks for the entry that runs it.
    ///
    /// The wrapper is the file that says which generation this is, because it is
    /// the one whose content is the installation. A wrapper nothing calls is a
    /// working file in an installation that never runs, which is exactly the
    /// case worth telling somebody about.
    fn status(&self, env: &Environment) -> Result<HookStatus, Error> {
        let path = self.asset(env);
        let Some(text) = read(&path)? else {
            return Ok(HookStatus::NotInstalled);
        };
        if !sentinel::is_generated(&text) {
            return Ok(HookStatus::NotInstalled);
        }
        let status = HookStatus::of_text(self.agent(), &text);
        Ok(status.confirmed(is_pointed_at(env)?))
    }

    /// The wrapper this program drops in, which is the file the mark is in.
    fn asset(&self, env: &Environment) -> PathBuf {
        wrapper(env)
    }
}

/// Where the agent keeps its configuration.
fn config_dir(env: &Environment) -> PathBuf {
    Agent::GithubCopilot.config_dir(env)
}

/// The directory the wrapper is dropped into.
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

/// The settings file the entry goes into.
fn settings(env: &Environment) -> PathBuf {
    config_dir(env).join(SETTINGS)
}

/// Which field of an entry the command goes in on `platform`.
///
/// The agent runs the command with the interpreter the field is named after, so
/// this is not a preference: a command in the wrong field is one the machine
/// cannot run.
fn command_field(platform: Platform) -> &'static str {
    match platform {
        Platform::Unix => "bash",
        Platform::Windows => "powershell",
    }
}

/// The entry itself, before the merge marks it as this program's.
fn entry(env: &Environment) -> Result<Map<String, Value>, Error> {
    let platform = env.platform();
    let mut entry = Map::new();
    entry.insert("type".to_owned(), Value::from("command"));
    entry.insert(
        command_field(platform).to_owned(),
        Value::from(command::hook_command(
            Agent::GithubCopilot,
            platform,
            &wrapper(env),
            None,
        )?),
    );
    entry.insert("timeoutSec".to_owned(), Value::from(TIMEOUT_SECONDS));
    // No matcher. The event carries no tool name to match on, and a pattern for
    // something with nothing to match would be saying more than is meant.
    Ok(entry)
}

/// What the wrapper should hold, with the binary written into it.
fn generated(platform: Platform, binary: &Path) -> Result<String, Error> {
    let named = match platform {
        Platform::Unix => command::in_shell(binary)?,
        Platform::Windows => command::in_powershell(binary)?,
    };
    Ok(assets::GITHUB_COPILOT_WRAPPER
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

/// Whether the settings on this machine run the wrapper this program would
/// install now.
///
/// A settings file that cannot be read as this program reads it counts as not
/// pointing anywhere: it may well be the agent's to understand, but it is not
/// one an install could add to either, so an installation resting on it is one
/// that needs a person.
fn is_pointed_at(env: &Environment) -> Result<bool, Error> {
    let Some(text) = read(&settings(env))? else {
        return Ok(false);
    };
    let Ok(document) = json::parse(&text) else {
        return Ok(false);
    };
    let mut ours = entry(env)?;
    sentinel::mark(&mut ours);
    let ours = Value::Object(ours);
    let Some(Value::Array(entries)) = document.get(HOOKS_KEY).and_then(|at| at.get(SESSION_START))
    else {
        return Ok(false);
    };
    let mut marked = entries.iter().filter(|entry| sentinel::is_marked(entry));
    Ok(marked.next() == Some(&ours) && marked.next().is_none())
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
    const AGENT: Agent = Agent::GithubCopilot;

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
        GithubCopilot
            .plan_install(env, &State::default(), Path::new("/opt/bin/agentbus"))
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

    /// The settings file as it stands.
    fn settings_text(env: &Environment) -> Option<String> {
        fs::read_to_string(settings(env)).ok()
    }

    /// The settings file, read as a document.
    fn settings_document(env: &Environment) -> Value {
        serde_json::from_str(&settings_text(env).expect("no settings file"))
            .expect("the settings file is not JSON")
    }

    /// This program's own entries under the event it registers for.
    fn ours(document: &Value) -> Vec<Value> {
        let Some(Value::Array(entries)) =
            document.get(HOOKS_KEY).and_then(|at| at.get(SESSION_START))
        else {
            return Vec::new();
        };
        entries
            .iter()
            .filter(|entry| sentinel::is_marked(entry))
            .cloned()
            .collect()
    }

    #[test]
    fn the_wrapper_obeys_the_rules_every_wrapper_obeys() {
        is_well_formed(AGENT, &assets::GITHUB_COPILOT_WRAPPER);
    }

    #[test]
    fn a_fresh_install_writes_the_wrapper_and_an_entry_that_runs_it() {
        let (_home, env) = machine_with_agent();

        install(&env);

        let written = fs::read_to_string(wrapper(&env)).expect("no wrapper");
        assert!(sentinel::is_generated(&written));
        assert!(
            written.contains("/opt/bin/agentbus"),
            "the wrapper does not name the binary"
        );

        let entries = ours(&settings_document(&env));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["type"], Value::from("command"));
        assert_eq!(entries[0]["timeoutSec"], Value::from(TIMEOUT_SECONDS));
        assert!(
            entries[0]["bash"]
                .as_str()
                .expect("a command")
                .contains("agentbus.sh"),
            "the entry does not run the wrapper: {:?}",
            entries[0],
        );
    }

    #[test]
    fn the_command_goes_in_the_field_named_after_the_interpreter_that_runs_it() {
        let expected = [
            (Platform::Unix, "bash", "powershell", "agentbus.sh"),
            (Platform::Windows, "powershell", "bash", "agentbus.ps1"),
        ];

        for (platform, field, other, script) in expected {
            let (_home, env) = machine();
            let env = env.with_platform(platform);
            fs::create_dir_all(config_dir(&env)).expect("cannot make the agent's directory");

            install(&env);

            let entries = ours(&settings_document(&env));
            assert_eq!(entries.len(), 1, "{platform:?}");
            let command = entries[0][field].as_str().expect("a command");
            assert!(command.contains(script), "{platform:?}: {command:?}");
            assert_eq!(entries[0].get(other), None, "{platform:?}: {command:?}");
        }
    }

    #[test]
    fn an_agent_that_is_not_on_the_machine_is_refused_and_nothing_is_written() {
        let (_home, env) = machine();

        let refusal = GithubCopilot
            .plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"))
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
    fn foreign_entries_and_settings_survive_an_install_and_an_uninstall() {
        let (_home, env) = machine_with_agent();
        let theirs = "{\n  \"theirs\": true,\n  \"hooks\": {\n    \"SessionStart\": [\n      \
                      {\n        \"type\": \"command\",\n        \"bash\": \"their-own-script\"\n \
                      }\n    ]\n  }\n}\n";
        fs::write(settings(&env), theirs).unwrap();

        let mut state = install(&env);
        let document = settings_document(&env);
        assert_eq!(document["theirs"], Value::from(true));
        assert_eq!(
            document[HOOKS_KEY][SESSION_START]
                .as_array()
                .expect("an array")
                .len(),
            2,
        );

        let changes = GithubCopilot
            .plan_uninstall(&env, &state)
            .expect("planning");
        apply(&changes, &mut state);

        let after = settings_document(&env);
        assert_eq!(after["theirs"], Value::from(true));
        assert!(ours(&after).is_empty());
        assert_eq!(
            after[HOOKS_KEY][SESSION_START][0]["bash"],
            Value::from("their-own-script"),
        );
    }

    #[test]
    fn a_settings_file_that_cannot_be_rewritten_safely_is_refused() {
        for held in ["{not json at all}", "{\"hooks\": {}, \"hooks\": {}}"] {
            let (_home, env) = machine_with_agent();
            fs::write(settings(&env), held).unwrap();

            let refusal = GithubCopilot
                .plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"))
                .expect_err("a file that cannot be rewritten has to stop the plan");

            assert!(
                matches!(refusal, Error::NotRewritable { .. }),
                "{refusal:?}"
            );
            assert_eq!(
                settings_text(&env).as_deref(),
                Some(held),
                "the settings were changed anyway",
            );
        }
    }

    #[test]
    fn a_wrapper_somebody_else_wrote_stops_the_plan() {
        let (_home, env) = machine_with_agent();
        let path = wrapper(&env);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "# something a user wrote\n").unwrap();

        let refusal = GithubCopilot
            .plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"))
            .expect_err("somebody else's file has to stop the plan");

        assert!(matches!(refusal, Error::NotOurs { .. }), "{refusal:?}");
    }

    #[test]
    fn installing_twice_changes_nothing_the_second_time() {
        let (_home, env) = machine_with_agent();
        let state = install(&env);
        let before = settings_text(&env);

        let again = GithubCopilot
            .plan_install(&env, &state, Path::new("/opt/bin/agentbus"))
            .expect("planning failed");

        assert!(
            again
                .iter()
                .all(|change| matches!(change, Change::Keep { .. })),
            "{again:?}",
        );
        assert_eq!(settings_text(&env), before);
    }

    #[test]
    fn a_dry_run_says_exactly_what_a_real_one_would_do() {
        let (_home, env) = machine_with_agent();
        let described = plan(&env);

        assert!(!wrapper(&env).exists());

        let carried_out = plan(&env);
        assert_eq!(described, carried_out);

        let mut state = State::default();
        apply(&carried_out, &mut state);
        assert!(wrapper(&env).exists());
    }

    #[test]
    fn uninstalling_leaves_nothing_of_its_own_behind() {
        let (_home, env) = machine_with_agent();
        let mut state = install(&env);

        let changes = GithubCopilot
            .plan_uninstall(&env, &state)
            .expect("planning");
        apply(&changes, &mut state);

        assert!(!wrapper(&env).exists());
        assert!(
            !settings(&env).exists(),
            "a settings file this program created was kept",
        );
        assert!(
            !hooks_dir(&env).exists(),
            "a directory this program made was kept",
        );
        assert!(
            config_dir(&env).is_dir(),
            "a directory that was never this program's was removed",
        );
    }

    #[test]
    fn a_directory_the_user_already_had_survives_an_uninstall() {
        let (_home, env) = machine_with_agent();
        fs::create_dir_all(hooks_dir(&env)).unwrap();
        let mut state = install(&env);

        let changes = GithubCopilot
            .plan_uninstall(&env, &state)
            .expect("planning");
        apply(&changes, &mut state);

        assert!(
            hooks_dir(&env).is_dir(),
            "a directory that was already there was removed",
        );
    }

    #[test]
    fn what_is_installed_is_what_the_machine_is_asked_about() {
        let (_home, env) = machine_with_agent();
        assert_eq!(
            GithubCopilot.status(&env).unwrap(),
            HookStatus::NotInstalled
        );

        let mut state = install(&env);
        let expected = crate::version::expected_version(AGENT);
        assert_eq!(
            GithubCopilot.status(&env).unwrap(),
            HookStatus::Current(expected),
        );

        // A settings file emptied of this program's entry leaves the wrapper
        // standing with nothing to run it.
        fs::write(settings(&env), "{}\n").unwrap();
        assert_eq!(
            GithubCopilot.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected),
        );

        let changes = GithubCopilot
            .plan_uninstall(&env, &state)
            .expect("planning");
        apply(&changes, &mut state);
        assert_eq!(
            GithubCopilot.status(&env).unwrap(),
            HookStatus::NotInstalled
        );
    }

    #[test]
    fn an_upgrade_replaces_what_an_earlier_build_wrote_rather_than_adding_to_it() {
        let (_home, env) = machine_with_agent();
        install(&env);

        // What an older build would have left: the same kind of entry, marked,
        // naming a wrapper somewhere this build no longer writes one.
        let mut document = settings_document(&env);
        let mut stale = Map::new();
        stale.insert("type".to_owned(), Value::from("command"));
        stale.insert("bash".to_owned(), Value::from("bash '/gone/agentbus.sh'"));
        sentinel::mark(&mut stale);
        document[HOOKS_KEY][SESSION_START]
            .as_array_mut()
            .expect("an array")
            .push(Value::Object(stale));
        fs::write(
            settings(&env),
            json::render(&document, json::DEFAULT_INDENT),
        )
        .unwrap();

        let mut state = State::default();
        let changes = plan(&env);
        apply(&changes, &mut state);

        assert_eq!(
            ours(&settings_document(&env)).len(),
            1,
            "entries accumulated",
        );
    }
}
