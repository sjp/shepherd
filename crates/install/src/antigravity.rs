//! Installing for Antigravity's command line.
//!
//! Two things go on the machine. A wrapper script is dropped into a `hooks`
//! directory below the agent's configuration, and one top-level key in
//! `hooks.json` tells the agent to run it. The wrapper is where the generation
//! mark lives, so a machine can be asked what it is carrying; the key is what
//! makes the wrapper ever run.
//!
//! # One key, owned outright
//!
//! This agent keys its hooks file by the *name of a hook* rather than by the
//! name of an event: everything one tool registers hangs below one key of that
//! tool's choosing. That is a better bargain than the one the other agents
//! offer, and this takes it. The key named after this program is this program's
//! whole, rewritten as a unit and removed as a unit, and no other key in the
//! file is read or written. There is no mark on it and none is needed — the
//! key *is* the claim, the way a file named after this program is in a directory
//! of one file per tool.
//!
//! Everything else in the file is somebody's, though, so it is edited through
//! [`crate::cst`] rather than parsed and written back: their key order, their
//! indentation and their line endings are theirs, and an installer that
//! reformatted the file would be handing them a diff of the whole thing where
//! they asked for one key.
//!
//! # The event's name comes from the entry, not from the payload
//!
//! Alone among the agents here, this one's hook payload does not say which
//! event it is about. It delivers one shape per event and expects whoever
//! registered the hook to remember which event they registered for. So the
//! entry written here passes the name to the wrapper, and the wrapper passes it
//! to `agentbus emit` beside the payload — which travels untouched, as it does
//! everywhere else, so that the mapping for this agent stays written against
//! the agent's own documented shape.
//!
//! The one event registered for is the one fired when the user's input has been
//! submitted and before the model is called, because that is the one that
//! carries the conversation this program needs to know about. The other four
//! are a cost paid on every turn for events nothing is waiting for.
//!
//! # A missing configuration directory is a refusal
//!
//! The directory is never made here. One that is not there means this agent has
//! never run on the machine, and an installer that made it would be creating
//! another program's home on the strength of a guess about its layout. The
//! `hooks` directory below it *is* made, because that one is where a generated
//! script belongs and a user who has never written a hook will not have it.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::change::Change;
use crate::cst::CstDocument;
use crate::paths::{Environment, Platform};
use crate::state::{Ownership, State};
use crate::status::HookStatus;
use crate::{Error, Installer, assets, command, file, json, sentinel};

/// The directory below the agent's own that the wrapper is dropped into.
const HOOKS_DIR: &str = "hooks";

/// The file the key goes into.
const HOOKS_FILE: &str = "hooks.json";

/// The top-level key this program owns, which is what the agent calls this
/// hook.
///
/// Named after this program without the leading underscore that marks an entry
/// in somebody else's array, because this is not metadata about the document:
/// it is the hook's own name, and the agent shows it to a user who asks what
/// hooks they have.
const HOOK_NAME: &str = "agentbus";

/// The event the wrapper is run on, in this agent's own spelling.
const EVENT: &str = "PreInvocation";

/// How long the agent is asked to allow the wrapper, in seconds.
///
/// Far longer than the wrapper can take, and there so that a machine which has
/// somehow made it slow cannot make somebody's session slow with it. The agent's
/// own default is six times that.
const TIMEOUT: u64 = 5;

/// What the wrapper says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// Antigravity's hooks: a wrapper script, and one top-level key in the hooks
/// file that runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Antigravity;

impl Installer for Antigravity {
    fn agent(&self) -> Agent {
        Agent::Antigravity
    }

    /// Writes the wrapper, then points the hooks file at it.
    ///
    /// In that order. The key names a file, and a hooks file that named one
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
        changes.push(plan_key(env)?);
        Ok(changes)
    }

    /// Takes the key out, then the wrapper.
    ///
    /// In that order for the same reason reversed: the key is what runs the
    /// file, so it goes first and nothing is ever pointed at something that is
    /// no longer there.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        let mut changes = vec![plan_key_removal(env, state)?];

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
        // already had is theirs however empty this leaves it.
        let hooks = hooks_dir(env);
        if state.ownership(&hooks) == Some(Ownership::Created) {
            changes.push(Change::Clear { path: hooks });
        }
        Ok(changes)
    }

    /// Reads the wrapper, and then looks for the key that runs it.
    ///
    /// The wrapper is the file that says which generation this is, because it is
    /// the one whose content is the installation. A wrapper nothing calls is a
    /// working file in an installation that never runs, which is exactly the
    /// case worth telling somebody about — and so is a key left over from an
    /// installation whose wrapper has since moved, because it names the path it
    /// was written with and not wherever one is now.
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
    Agent::Antigravity.config_dir(env)
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

/// The hooks file the key goes into.
fn hooks_file(env: &Environment) -> PathBuf {
    config_dir(env).join(HOOKS_FILE)
}

/// What the wrapper should hold, with the binary written into it.
fn generated(platform: Platform, binary: &Path) -> Result<String, Error> {
    let named = match platform {
        Platform::Unix => command::in_shell(binary)?,
        Platform::Windows => command::in_powershell(binary)?,
    };
    Ok(assets::ANTIGRAVITY_WRAPPER
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

/// What this program's key in the hooks file holds: run the wrapper before the
/// model is called, and tell it which event that was.
///
/// Nothing says the hook is enabled, because the agent documents an omitted
/// answer as yes and writing it out would be stating a preference where this
/// program has none. Nothing says which tool to match on either: this agent
/// matches a pattern against the name of a tool, and the event has no tool in
/// it.
fn block(env: &Environment) -> Result<Value, Error> {
    let mut hook = Map::new();
    hook.insert("type".to_owned(), Value::from("command"));
    hook.insert(
        "command".to_owned(),
        Value::from(command::hook_command(
            Agent::Antigravity,
            env.platform(),
            &wrapper(env),
            Some(EVENT),
        )?),
    );
    hook.insert("timeout".to_owned(), Value::from(TIMEOUT));

    let mut block = Map::new();
    block.insert(EVENT.to_owned(), Value::Array(vec![Value::Object(hook)]));
    Ok(Value::Object(block))
}

/// What putting the key into the hooks file would do.
///
/// A file that is not there is written from nothing, which is the case worth
/// having: no merge, no user content, nothing that can go wrong. A file that is
/// there has this program's key set to what it should be and every other key
/// left exactly as it was, down to the bytes.
fn plan_key(env: &Environment) -> Result<Change, Error> {
    let path = hooks_file(env);
    let ours = block(env)?;

    let Some(text) = read(&path)? else {
        let mut document = Map::new();
        document.insert(HOOK_NAME.to_owned(), ours);
        return Ok(Change::Create {
            path,
            contents: json::render(&Value::Object(document), json::DEFAULT_INDENT),
            executable: false,
        });
    };

    let mut document = CstDocument::parse_strict(&path, &text)?;
    document.set(HOOK_NAME, ours)?;
    let contents = document.render();
    Ok(match contents == text {
        true => Change::Keep { path },
        false => Change::Rewrite {
            path,
            contents,
            executable: false,
        },
    })
}

/// What taking this program's key back out of the hooks file would do.
///
/// A file that does not so much as spell this program's name is left alone
/// without being parsed. That is not a shortcut around the strictness: a file
/// with nothing of this program's in it is a file an uninstall has nothing to do
/// to, and refusing to finish an uninstall over a hooks file this program never
/// wrote into would be refusing to remove everything else on the machine as
/// well.
fn plan_key_removal(env: &Environment, state: &State) -> Result<Change, Error> {
    let path = hooks_file(env);
    let Some(text) = read(&path)? else {
        return Ok(Change::Keep { path });
    };
    if !text.contains(HOOK_NAME) {
        return Ok(Change::Keep { path });
    }

    let mut document = CstDocument::parse_strict(&path, &text)?;
    if document.get(&[HOOK_NAME]).is_none() {
        return Ok(Change::Keep { path });
    }
    document.remove(HOOK_NAME)?;
    // A file this program created and has just emptied is litter. One it merely
    // added a key to is the user's, however little of it is left.
    let ours = state.ownership(&path) == Some(Ownership::Created);
    if ours && document.get(&[]).is_some_and(sentinel::is_vacant) {
        return Ok(Change::Delete { path });
    }
    Ok(Change::Rewrite {
        path,
        contents: document.render(),
        executable: false,
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
    let path = hooks_file(env);
    let Some(text) = read(&path)? else {
        return Ok(false);
    };
    let Ok(document) = CstDocument::parse_strict(&path, &text) else {
        return Ok(false);
    };
    Ok(document.get(&[HOOK_NAME]) == Some(&block(env)?))
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
    const AGENT: Agent = Agent::Antigravity;

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
        Antigravity
            .plan_install(env, &State::default(), Path::new(BINARY))
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
        let changes = Antigravity
            .plan_uninstall(env, state)
            .expect("planning failed");
        apply(&changes, state);
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

    /// The one command this program's key runs.
    fn our_command(env: &Environment) -> String {
        hooks_document(env)[HOOK_NAME][EVENT][0]["command"]
            .as_str()
            .expect("a command")
            .to_owned()
    }

    #[test]
    fn the_wrapper_obeys_the_rules_every_wrapper_obeys() {
        is_well_formed(AGENT, &assets::ANTIGRAVITY_WRAPPER);
    }

    #[test]
    fn a_fresh_install_writes_the_wrapper_and_one_key_that_runs_it() {
        let (_home, env) = machine_with_agent();

        install(&env);

        let written = fs::read_to_string(wrapper(&env)).expect("no wrapper");
        assert!(sentinel::is_generated(&written));
        assert!(
            written.contains(BINARY),
            "the wrapper does not name the binary"
        );

        let document = hooks_document(&env);
        let named: Vec<&str> = document
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(named, [HOOK_NAME], "{document:?}");
        assert!(
            our_command(&env).contains("agentbus.sh"),
            "the key does not run the wrapper",
        );
    }

    #[test]
    fn the_wrapper_is_told_which_event_it_is_being_run_for() {
        let (_home, env) = machine_with_agent();

        install(&env);

        let command = our_command(&env);
        assert!(
            command.ends_with(&format!(" {EVENT}")),
            "{command:?} does not end by naming the event",
        );
    }

    #[test]
    fn an_agent_that_is_not_on_the_machine_is_refused_and_nothing_is_written() {
        let (_home, env) = machine();

        let refusal = Antigravity
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
    fn every_other_hook_in_the_file_survives_an_install_and_an_uninstall() {
        let (_home, env) = machine_with_agent();
        let theirs = "{\n  \"lint-checker\": {\n    \"PreInvocation\": [\n      {\n        \
                      \"command\": \"their-own-script\"\n      }\n    ]\n  }\n}\n";
        fs::write(hooks_file(&env), theirs).unwrap();

        let mut state = install(&env);
        let document = hooks_document(&env);
        assert_eq!(
            document["lint-checker"][EVENT][0]["command"],
            Value::from("their-own-script"),
        );
        assert!(document[HOOK_NAME].is_object());

        uninstall(&env, &mut state);

        assert_eq!(
            hooks_text(&env).as_deref(),
            Some(theirs),
            "a file this program only added a key to was not put back as it was",
        );
    }

    #[test]
    fn a_hooks_file_that_cannot_be_rewritten_safely_is_refused() {
        for held in [
            "{not json at all}",
            "{\"agentbus\": {}, \"agentbus\": {}}",
            "{\"a\": 1, \"a\": 2}",
        ] {
            let (_home, env) = machine_with_agent();
            fs::write(hooks_file(&env), held).unwrap();

            let refusal = Antigravity
                .plan_install(&env, &State::default(), Path::new(BINARY))
                .expect_err("a file that cannot be rewritten has to stop the plan");

            assert!(
                matches!(refusal, Error::NotRewritable { .. }),
                "{held:?}: {refusal:?}",
            );
            assert_eq!(
                hooks_text(&env).as_deref(),
                Some(held),
                "the hooks file was changed anyway",
            );
        }
    }

    #[test]
    fn an_uninstall_leaves_a_hooks_file_with_nothing_of_this_programs_in_it_alone() {
        let (_home, env) = machine_with_agent();
        // Unreadable to this program, and none of its business either: an
        // uninstall that stopped here would refuse to remove anything else on
        // the machine.
        let theirs = "{not json at all}";
        fs::write(hooks_file(&env), theirs).unwrap();

        let changes = Antigravity
            .plan_uninstall(&env, &State::default())
            .expect("a file with nothing of this program's in it stops nothing");

        assert!(
            changes
                .iter()
                .all(|change| matches!(change, Change::Keep { .. })),
            "{changes:?}",
        );
        assert_eq!(hooks_text(&env).as_deref(), Some(theirs));
    }

    #[test]
    fn a_hooks_file_that_is_not_an_object_is_refused() {
        let (_home, env) = machine_with_agent();
        fs::write(hooks_file(&env), "[]\n").unwrap();

        let refusal = Antigravity
            .plan_install(&env, &State::default(), Path::new(BINARY))
            .expect_err("a document with nowhere to put any of this has to stop the plan");

        assert!(matches!(refusal, Error::Conflict { .. }), "{refusal:?}");
        assert_eq!(hooks_text(&env).as_deref(), Some("[]\n"));
    }

    #[test]
    fn a_wrapper_somebody_else_wrote_stops_the_plan() {
        let (_home, env) = machine_with_agent();
        fs::create_dir_all(hooks_dir(&env)).unwrap();
        fs::write(wrapper(&env), "# something a user wrote\n").unwrap();

        let refusal = Antigravity
            .plan_install(&env, &State::default(), Path::new(BINARY))
            .expect_err("somebody else's file has to stop the plan");

        assert!(matches!(refusal, Error::NotOurs { .. }), "{refusal:?}");
    }

    #[test]
    fn installing_twice_changes_nothing_the_second_time() {
        let (_home, env) = machine_with_agent();
        let state = install(&env);
        let before = hooks_text(&env);

        let again = Antigravity
            .plan_install(&env, &state, Path::new(BINARY))
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
    fn an_upgrade_replaces_what_an_earlier_build_wrote_rather_than_adding_to_it() {
        let (_home, env) = machine_with_agent();
        fs::write(
            hooks_file(&env),
            "{\n  \"agentbus\": {\n    \"Stop\": [\n      {\n        \"command\": \
             \"bash '/gone/agentbus.sh'\"\n      }\n    ]\n  }\n}\n",
        )
        .unwrap();

        install(&env);

        let document = hooks_document(&env);
        let named: Vec<&str> = document[HOOK_NAME]
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(named, [EVENT], "what an earlier build wrote was kept");
    }

    #[test]
    fn a_dry_run_says_exactly_what_a_real_one_would_do() {
        let (_home, env) = machine_with_agent();
        let described = plan(&env);

        assert!(!wrapper(&env).exists());
        assert!(!hooks_file(&env).exists());

        let carried_out = plan(&env);
        assert_eq!(described, carried_out);

        let mut state = State::default();
        apply(&carried_out, &mut state);
        assert!(wrapper(&env).exists() && hooks_file(&env).exists());
    }

    #[test]
    fn uninstalling_leaves_nothing_of_its_own_behind() {
        let (_home, env) = machine_with_agent();
        let mut state = install(&env);

        uninstall(&env, &mut state);

        assert!(!wrapper(&env).exists());
        assert!(
            !hooks_file(&env).exists(),
            "a hooks file this program created was kept for the sake of the one \
             key it wrote in it",
        );
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
    fn what_is_installed_is_what_the_machine_is_asked_about() {
        let (_home, env) = machine_with_agent();
        assert_eq!(Antigravity.status(&env).unwrap(), HookStatus::NotInstalled);

        let mut state = install(&env);
        let expected = crate::version::expected_version(AGENT);
        assert_eq!(
            Antigravity.status(&env).unwrap(),
            HookStatus::Current(expected)
        );

        // A key edited by hand leaves the wrapper standing with nothing to run
        // it, and installing again puts it back.
        let mut document = hooks_document(&env);
        document[HOOK_NAME][EVENT][0]["command"] = Value::from("bash '/gone/agentbus.sh'");
        fs::write(
            hooks_file(&env),
            json::render(&document, json::DEFAULT_INDENT),
        )
        .unwrap();
        assert_eq!(
            Antigravity.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected),
        );
        apply(&plan(&env), &mut state);
        assert_eq!(
            Antigravity.status(&env).unwrap(),
            HookStatus::Current(expected)
        );

        uninstall(&env, &mut state);
        assert_eq!(Antigravity.status(&env).unwrap(), HookStatus::NotInstalled);
    }
}
