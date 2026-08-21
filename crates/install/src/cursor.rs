//! Installing for Cursor's command line.
//!
//! The smallest hook entry of any agent here: an object whose one required field
//! is `command`. Everything else an entry may carry — a type, a timeout, whether
//! a failure blocks the action — has a default this program is happy with, and
//! writing them out would be stating a preference where it has none.
//!
//! The file itself is not quite the smallest, because it opens by saying which
//! dialect it is written in. That declaration is not an entry and is not marked
//! as one: a file this program writes from nothing carries it because that is
//! how the agent's own documentation writes the file, and a file that already
//! carries one keeps whatever it says. See [`crate::merge::Declaration`].
//!
//! # The event is spelled the agent's way
//!
//! Every other agent that hooks a session beginning calls it `SessionStart`.
//! This one calls it `sessionStart`, and that is the name written here, because
//! an installer's job is to write the file the agent reads rather than the file
//! the other agents would recognize. The mapping for this agent is written
//! against the same spelling, since that is what its payload carries.
//!
//! # The wrapper sits in the configuration directory itself
//!
//! There is no directory below it to drop generated files into, and making one
//! would be inventing a layout for somebody else's program. So the wrapper is
//! written beside the hooks file, which is also where a person looking for it
//! would look.
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
use crate::state::State;
use crate::status::HookStatus;
use crate::{
    Declaration, Error, Installer, Placement, assets, command, file, json, merge, sentinel,
};

/// The file the entry goes into.
const HOOKS_FILE: &str = "hooks.json";

/// The key in that file that the event names hang from.
const HOOKS_KEY: &str = "hooks";

/// The event that says a session has begun, in this agent's own spelling.
const SESSION_START: &str = "sessionStart";

/// The key that says which dialect the hooks file is written in.
const DIALECT_KEY: &str = "version";

/// The dialect this program writes.
const DIALECT: u64 = 1;

/// What the wrapper says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// Cursor's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor;

impl Installer for Cursor {
    fn agent(&self) -> Agent {
        Agent::Cursor
    }

    /// Writes the wrapper, then points the hooks file at it.
    ///
    /// In that order. The entry names a file, and a hooks file that named one
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
        Ok(vec![
            plan_wrapper(env, binary)?,
            merge::plan_install_declaring(
                &hooks_file(env),
                &[Placement::new([HOOKS_KEY, SESSION_START], entry(env)?)],
                &declarations(),
            )?,
        ])
    }

    /// Takes the entry out, then the wrapper.
    ///
    /// In that order for the same reason reversed: the entry is what runs the
    /// file, so it goes first and nothing is ever pointed at something that is
    /// no longer there. What the file says about itself is never touched — a
    /// file this program merely added to keeps it, and one this program created
    /// goes away whole.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        let mut changes = vec![merge::plan_uninstall_declaring(
            &hooks_file(env),
            state,
            &declarations(),
        )?];

        let path = wrapper(env);
        // A file without the mark is passed over here rather than refused,
        // unlike on the way in: an uninstall that stopped because something of
        // the user's is in the way would be refusing to do the one thing it can
        // certainly do safely, which is nothing.
        changes.push(match read(&path)? {
            Some(text) if sentinel::is_generated(&text) => Change::Delete { path },
            _ => Change::Keep { path },
        });
        Ok(changes)
    }

    /// Reads the wrapper, and then looks for the entry that runs it.
    ///
    /// The wrapper is the file that says which generation this is, because it is
    /// the one whose content is the installation. A wrapper nothing calls is a
    /// working file in an installation that never runs, which is exactly the
    /// case worth telling somebody about.
    fn status(&self, env: &Environment) -> Result<HookStatus, Error> {
        let path = wrapper(env);
        let Some(text) = read(&path)? else {
            return Ok(HookStatus::NotInstalled);
        };
        if !sentinel::is_generated(&text) {
            return Ok(HookStatus::NotInstalled);
        }
        let status = HookStatus::of_text(self.agent(), &text);
        Ok(status.confirmed(is_pointed_at(env)?))
    }
}

/// Where the agent keeps its configuration.
fn config_dir(env: &Environment) -> PathBuf {
    Agent::Cursor.config_dir(env)
}

/// The wrapper this program drops in there.
fn wrapper(env: &Environment) -> PathBuf {
    config_dir(env).join(match env.platform() {
        Platform::Unix => "agentbus.sh",
        Platform::Windows => "agentbus.ps1",
    })
}

/// The hooks file the entry goes into.
fn hooks_file(env: &Environment) -> PathBuf {
    config_dir(env).join(HOOKS_FILE)
}

/// What the hooks file has to say about itself.
fn declarations() -> [Declaration; 1] {
    [Declaration::new(DIALECT_KEY, DIALECT)]
}

/// The entry itself, before the merge marks it as this program's.
fn entry(env: &Environment) -> Result<Map<String, Value>, Error> {
    let mut entry = Map::new();
    entry.insert(
        "command".to_owned(),
        Value::from(command::hook_command(
            Agent::Cursor,
            env.platform(),
            &wrapper(env),
            None,
        )?),
    );
    Ok(entry)
}

/// What the wrapper should hold, with the binary written into it.
fn generated(platform: Platform, binary: &Path) -> Result<String, Error> {
    let named = match platform {
        Platform::Unix => command::in_shell(binary)?,
        Platform::Windows => command::in_powershell(binary)?,
    };
    Ok(assets::CURSOR_WRAPPER
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

/// Whether the hooks file on this machine runs the wrapper this program would
/// install now.
///
/// A file that cannot be read as this program reads it counts as not pointing
/// anywhere: it may well be the agent's to understand, but it is not one an
/// install could add to either, so an installation resting on it is one that
/// needs a person.
fn is_pointed_at(env: &Environment) -> Result<bool, Error> {
    let Some(text) = read(&hooks_file(env))? else {
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
    const AGENT: Agent = Agent::Cursor;

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
        Cursor
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

    /// The hooks file as it stands.
    fn hooks_text(env: &Environment) -> Option<String> {
        fs::read_to_string(hooks_file(env)).ok()
    }

    /// The hooks file, read as a document.
    fn hooks_document(env: &Environment) -> Value {
        serde_json::from_str(&hooks_text(env).expect("no hooks file"))
            .expect("the hooks file is not JSON")
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
        is_well_formed(AGENT, &assets::CURSOR_WRAPPER);
    }

    #[test]
    fn a_fresh_install_writes_the_wrapper_beside_the_hooks_file_and_an_entry_that_runs_it() {
        let (_home, env) = machine_with_agent();

        install(&env);

        assert_eq!(
            wrapper(&env).parent(),
            Some(config_dir(&env).as_path()),
            "the wrapper is not beside the hooks file",
        );
        let written = fs::read_to_string(wrapper(&env)).expect("no wrapper");
        assert!(sentinel::is_generated(&written));
        assert!(
            written.contains("/opt/bin/agentbus"),
            "the wrapper does not name the binary"
        );

        let entries = ours(&hooks_document(&env));
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0]["command"]
                .as_str()
                .expect("a command")
                .contains("agentbus.sh"),
            "the entry does not run the wrapper: {:?}",
            entries[0],
        );
    }

    #[test]
    fn the_entry_says_nothing_but_what_it_has_to() {
        let (_home, env) = machine_with_agent();

        install(&env);

        let entries = ours(&hooks_document(&env));
        let named: Vec<&str> = entries[0]
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .filter(|key| *key != sentinel::KEY)
            .collect();
        assert_eq!(named, ["command"], "{:?}", entries[0]);
    }

    #[test]
    fn a_hooks_file_written_from_nothing_says_which_dialect_it_is() {
        let (_home, env) = machine_with_agent();

        install(&env);

        assert_eq!(hooks_document(&env)[DIALECT_KEY], Value::from(DIALECT));
    }

    #[test]
    fn a_hooks_file_that_already_says_which_dialect_it_is_is_believed() {
        let (_home, env) = machine_with_agent();
        fs::write(
            hooks_file(&env),
            "{\n  \"version\": 99,\n  \"theirs\": 1\n}\n",
        )
        .unwrap();

        install(&env);

        let document = hooks_document(&env);
        assert_eq!(document[DIALECT_KEY], Value::from(99));
        assert_eq!(document["theirs"], Value::from(1));
    }

    #[test]
    fn an_agent_that_is_not_on_the_machine_is_refused_and_nothing_is_written() {
        let (_home, env) = machine();

        let refusal = Cursor
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
        let theirs = "{\n  \"version\": 1,\n  \"theirs\": true,\n  \"hooks\": {\n    \
                      \"sessionStart\": [\n      {\n        \"command\": \"their-own-script\"\n  \
                      }\n    ]\n  }\n}\n";
        fs::write(hooks_file(&env), theirs).unwrap();

        let mut state = install(&env);
        let document = hooks_document(&env);
        assert_eq!(document["theirs"], Value::from(true));
        assert_eq!(
            document[HOOKS_KEY][SESSION_START]
                .as_array()
                .expect("an array")
                .len(),
            2,
        );

        let changes = Cursor.plan_uninstall(&env, &state).expect("planning");
        apply(&changes, &mut state);

        let after = hooks_document(&env);
        assert_eq!(after["theirs"], Value::from(true));
        assert_eq!(after[DIALECT_KEY], Value::from(1));
        assert!(ours(&after).is_empty());
        assert_eq!(
            after[HOOKS_KEY][SESSION_START][0]["command"],
            Value::from("their-own-script"),
        );
    }

    #[test]
    fn a_hooks_file_the_user_wrote_keeps_its_dialect_when_this_program_leaves() {
        let (_home, env) = machine_with_agent();
        fs::write(hooks_file(&env), "{\n  \"version\": 1\n}\n").unwrap();
        let mut state = install(&env);

        let changes = Cursor.plan_uninstall(&env, &state).expect("planning");
        apply(&changes, &mut state);

        assert_eq!(
            hooks_text(&env).as_deref(),
            Some("{\n  \"version\": 1\n}\n"),
            "a file this program did not create was removed or rewritten",
        );
    }

    #[test]
    fn a_hooks_file_that_cannot_be_rewritten_safely_is_refused() {
        for held in ["{not json at all}", "{\"hooks\": {}, \"hooks\": {}}"] {
            let (_home, env) = machine_with_agent();
            fs::write(hooks_file(&env), held).unwrap();

            let refusal = Cursor
                .plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"))
                .expect_err("a file that cannot be rewritten has to stop the plan");

            assert!(
                matches!(refusal, Error::NotRewritable { .. }),
                "{refusal:?}"
            );
            assert_eq!(
                hooks_text(&env).as_deref(),
                Some(held),
                "the hooks file was changed anyway",
            );
        }
    }

    #[test]
    fn a_hooks_file_that_is_not_an_object_is_refused() {
        let (_home, env) = machine_with_agent();
        fs::write(hooks_file(&env), "[]\n").unwrap();

        let refusal = Cursor
            .plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"))
            .expect_err("a document with nowhere to put any of this has to stop the plan");

        assert!(matches!(refusal, Error::Conflict { .. }), "{refusal:?}");
        assert_eq!(hooks_text(&env).as_deref(), Some("[]\n"));
    }

    #[test]
    fn a_wrapper_somebody_else_wrote_stops_the_plan() {
        let (_home, env) = machine_with_agent();
        fs::write(wrapper(&env), "# something a user wrote\n").unwrap();

        let refusal = Cursor
            .plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"))
            .expect_err("somebody else's file has to stop the plan");

        assert!(matches!(refusal, Error::NotOurs { .. }), "{refusal:?}");
    }

    #[test]
    fn installing_twice_changes_nothing_the_second_time() {
        let (_home, env) = machine_with_agent();
        let state = install(&env);
        let before = hooks_text(&env);

        let again = Cursor
            .plan_install(&env, &state, Path::new("/opt/bin/agentbus"))
            .expect("planning failed");

        assert!(
            again
                .iter()
                .all(|change| matches!(change, Change::Keep { .. })),
            "{again:?}",
        );
        assert_eq!(hooks_text(&env), before);
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

        let changes = Cursor.plan_uninstall(&env, &state).expect("planning");
        apply(&changes, &mut state);

        assert!(!wrapper(&env).exists());
        assert!(
            !hooks_file(&env).exists(),
            "a hooks file this program created was kept for the sake of the one \
             line it wrote in it",
        );
        assert!(
            config_dir(&env).is_dir(),
            "a directory that was never this program's was removed",
        );
    }

    #[test]
    fn what_is_installed_is_what_the_machine_is_asked_about() {
        let (_home, env) = machine_with_agent();
        assert_eq!(Cursor.status(&env).unwrap(), HookStatus::NotInstalled);

        let mut state = install(&env);
        let expected = crate::version::expected_version(AGENT);
        assert_eq!(Cursor.status(&env).unwrap(), HookStatus::Current(expected));

        // A hooks file emptied of this program's entry leaves the wrapper
        // standing with nothing to run it.
        fs::write(hooks_file(&env), "{\n  \"version\": 1\n}\n").unwrap();
        assert_eq!(
            Cursor.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected),
        );

        let changes = Cursor.plan_uninstall(&env, &state).expect("planning");
        apply(&changes, &mut state);
        assert_eq!(Cursor.status(&env).unwrap(), HookStatus::NotInstalled);
    }

    #[test]
    fn an_upgrade_replaces_what_an_earlier_build_wrote_rather_than_adding_to_it() {
        let (_home, env) = machine_with_agent();
        install(&env);

        // What an older build would have left: the same kind of entry, marked,
        // naming a wrapper somewhere this build no longer writes one.
        let mut document = hooks_document(&env);
        let mut stale = Map::new();
        stale.insert(
            "command".to_owned(),
            Value::from("bash '/gone/agentbus.sh'"),
        );
        sentinel::mark(&mut stale);
        document[HOOKS_KEY][SESSION_START]
            .as_array_mut()
            .expect("an array")
            .push(Value::Object(stale));
        fs::write(
            hooks_file(&env),
            json::render(&document, json::DEFAULT_INDENT),
        )
        .unwrap();

        let mut state = State::default();
        let changes = plan(&env);
        apply(&changes, &mut state);

        assert_eq!(ours(&hooks_document(&env)).len(), 1, "entries accumulated");
    }
}
