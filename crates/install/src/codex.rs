//! Installing for Codex CLI.
//!
//! Three things go on the machine. A wrapper script is dropped into a `hooks`
//! directory inside Codex's configuration directory, one entry is added to the
//! `hooks.json` beside it telling Codex to run the wrapper when a session
//! starts, and one setting is switched on in `config.toml` — because a current
//! Codex does not read `hooks.json` at all until it is.
//!
//! The wrapper is where the generation mark lives, so a machine can be asked
//! what it is carrying. The entry is what makes the wrapper ever run. The
//! setting is what makes the entry ever be read; without it an installation
//! looks complete, changes the right files, and does nothing whatsoever.
//!
//! The one event hooked is the start of a session, because what this program
//! needs from Codex at install time is the identity of the session that is
//! running. What every other event *means* is decided from the payload the
//! wrapper forwards, and registering more of them is a cost paid on every tool
//! call for events nothing is waiting on.
//!
//! # The setting
//!
//! `config.toml` is a file a user keeps by hand, comments and all, and this
//! program writes exactly one key into it: `hooks` under `[features]`. It goes
//! in through the line-by-line editor in [`crate::toml_text`], which changes the
//! line it is changing and copies every other byte through, and which refuses
//! outright rather than guess at a file it cannot read that way.
//!
//! Whether the setting was already on when this program arrived is the one
//! thing about it that cannot be read back off the disk later: `hooks = true`
//! looks the same whoever wrote it, and there is nowhere in a line like that to
//! hang the mark this program's entries in a document carry. So the answer is
//! recorded when it is still known, and an uninstall switches the setting off
//! only where the record says this program switched it on.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::change::Change;
use crate::command;
use crate::paths::{Environment, Platform};
use crate::state::{Ownership, State};
use crate::status::HookStatus;
use crate::{Error, Installer, Placement, assets, file, json, merge, sentinel, toml_text};

/// The directory inside Codex's configuration that the wrapper is dropped
/// into.
const HOOKS_DIR: &str = "hooks";

/// The file Codex reads its hooks from, inside its configuration directory.
///
/// A file separate from the `config.toml` a user keeps their settings in, which
/// is why the entry goes here and not into the `[hooks]` table Codex documents
/// as an alternative: on a machine where this one does not exist there is
/// nothing to merge with and nothing that can be damaged, and where it does, it
/// is JSON, which this program can take its own entries back out of exactly.
const HOOKS_FILE: &str = "hooks.json";

/// The key in that file that everything below is hung from.
const HOOKS_KEY: &str = "hooks";

/// The event the wrapper is run on.
const EVENT: &str = "SessionStart";

/// The file the setting that switches hooks on lives in.
const CONFIG_FILE: &str = "config.toml";

/// The section of that file it lives in.
const FEATURES: &str = "features";

/// The setting itself, which Codex reads as permission to look for hooks at
/// all.
const HOOKS_FLAG: &str = "hooks";

/// How long Codex is asked to allow the wrapper, in seconds.
///
/// Far longer than the wrapper can take, and there so that a machine which has
/// somehow made it slow cannot make somebody's session slow with it.
const TIMEOUT: u64 = 5;

/// What the wrapper says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// Codex CLI's hooks: a wrapper script, one entry in its drop-in file that runs
/// it, and the setting that makes the file be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codex;

impl Installer for Codex {
    fn agent(&self) -> Agent {
        Agent::Codex
    }

    /// Writes the wrapper, points an entry at it, and then switches hooks on.
    ///
    /// In that order, outwards. Each step is what makes the one before it
    /// count, so a run interrupted between any two of them leaves a machine
    /// that does nothing rather than one that runs half an installation.
    fn plan_install(
        &self,
        env: &Environment,
        state: &State,
        binary: &Path,
    ) -> Result<Vec<Change>, Error> {
        // Writing a file makes the directories above it anyway; these are here
        // so that the making is recorded, which is the one thing the uninstall
        // cannot work out for itself later. Both, because an agent found by its
        // command alone may have no configuration directory yet, and one this
        // program made is one it has to be able to take away again.
        let mut changes: Vec<Change> = [Agent::Codex.config_dir(env), hooks_dir(env)]
            .into_iter()
            .filter(|dir| !dir.is_dir())
            .map(|path| Change::Make { path })
            .collect();
        changes.push(plan_wrapper(env, binary)?);
        changes.push(merge::plan_install(&hooks(env), &[placement(env)?])?);
        changes.extend(plan_flag(env, state)?);
        Ok(changes)
    }

    /// Switches hooks off, takes the entry out, and then takes the wrapper
    /// away.
    ///
    /// The same order reversed, inwards: nothing is ever left pointing at
    /// something that is no longer there.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        let mut changes = plan_flag_removal(env, state)?;
        changes.push(merge::plan_uninstall(&hooks(env), state)?);

        let path = wrapper(env);
        // A file without the mark is passed over here rather than refused,
        // unlike on the way in: an uninstall that stopped because something of
        // the user's is in the way would be refusing to do the one thing it can
        // certainly do safely, which is nothing.
        changes.push(match read(&path)? {
            Some(text) if sentinel::is_generated(&text) => Change::Delete { path },
            _ => Change::Keep { path },
        });
        // Deepest first, and only the directories the record says this program
        // made: one the user already had is theirs however empty this leaves
        // it.
        for dir in [hooks_dir(env), Agent::Codex.config_dir(env)] {
            if state.ownership(&dir) == Some(Ownership::Created) {
                changes.push(Change::Clear { path: dir });
            }
        }
        Ok(changes)
    }

    /// Reads the wrapper, and then looks for the two things that have to be
    /// true for it ever to run.
    ///
    /// The wrapper is the file that says which generation this is, because it
    /// is the one whose content is the installation. Either of the other two
    /// halves missing is the case worth telling somebody about: an entry that
    /// no longer names the wrapper this build would install, and a setting
    /// somebody has switched back off, both leave a perfectly current file that
    /// nothing ever runs.
    fn status(&self, env: &Environment) -> Result<HookStatus, Error> {
        let path = self.asset(env);
        let Some(text) = read(&path)? else {
            return Ok(HookStatus::NotInstalled);
        };
        if !sentinel::is_generated(&text) {
            return Ok(HookStatus::NotInstalled);
        }
        let status = HookStatus::of_text(self.agent(), &text);
        Ok(status.confirmed(is_pointed_at(env)? && is_switched_on(env)?))
    }

    /// The wrapper this program drops in, which is the file the mark is in.
    fn asset(&self, env: &Environment) -> PathBuf {
        wrapper(env)
    }
}

/// The directory the wrapper is dropped into.
fn hooks_dir(env: &Environment) -> PathBuf {
    Agent::Codex.config_dir(env).join(HOOKS_DIR)
}

/// The wrapper this program drops in there.
fn wrapper(env: &Environment) -> PathBuf {
    hooks_dir(env).join(match env.platform() {
        Platform::Unix => "agentbus.sh",
        Platform::Windows => "agentbus.ps1",
    })
}

/// The drop-in file the entry goes in.
fn hooks(env: &Environment) -> PathBuf {
    Agent::Codex.config_dir(env).join(HOOKS_FILE)
}

/// The file the setting lives in.
fn config(env: &Environment) -> PathBuf {
    Agent::Codex.config_dir(env).join(CONFIG_FILE)
}

/// How the setting is named in the record of what this program has switched
/// on.
///
/// Spelled the way somebody would say where it is, so that a person reading the
/// record can find the line it is about.
fn flag_setting() -> String {
    format!("{FEATURES}.{HOOKS_FLAG}")
}

/// How the section is named there, for the case where this program wrote the
/// section as well as the key.
///
/// A separate fact from the key's, because taking the key away again is not the
/// same question as taking away the header above it: a `[features]` that was
/// already in the file holds settings this program has never heard of.
fn section_setting() -> String {
    format!("[{FEATURES}]")
}

/// What the wrapper should hold, with the binary written into it.
fn generated(platform: Platform, binary: &Path) -> Result<String, Error> {
    let named = match platform {
        Platform::Unix => command::in_shell(binary)?,
        Platform::Windows => command::in_powershell(binary)?,
    };
    Ok(assets::CODEX_WRAPPER
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

/// The entry that tells Codex to run the wrapper, and where it belongs.
///
/// The event carries no tool name, so the entry carries no matcher: Codex
/// documents an omitted one as matching everything, and inventing a pattern for
/// an event that has nothing to match would be saying something this program
/// does not mean.
fn placement(env: &Environment) -> Result<Placement, Error> {
    Ok(Placement::new([HOOKS_KEY, EVENT], entry(env)?))
}

/// The entry itself, before the merge marks it as this program's.
fn entry(env: &Environment) -> Result<Map<String, Value>, Error> {
    let mut hook = Map::new();
    hook.insert("type".to_owned(), Value::from("command"));
    hook.insert(
        "command".to_owned(),
        Value::from(command::hook_command(
            Agent::Codex,
            env.platform(),
            &wrapper(env),
            None,
        )?),
    );
    // Nothing the agent does may wait on this. The wrapper is quick and bounds
    // itself, but an agent that waits for it at all has been slowed down by
    // something installed behind its user's back.
    hook.insert("async".to_owned(), Value::Bool(true));
    hook.insert("timeout".to_owned(), Value::from(TIMEOUT));

    let mut entry = Map::new();
    entry.insert("hooks".to_owned(), Value::Array(vec![Value::Object(hook)]));
    Ok(entry)
}

/// What switching hooks on would do, and who the setting belongs to afterwards.
///
/// Three steps come back: what happens to the file, and the two facts about it
/// that the file itself will not be able to answer later — whether this program
/// switched the setting on, and whether it wrote the section the setting sits
/// in. A setting the record already claims stays claimed, because a second
/// install finds it on and would otherwise conclude it had always been so.
fn plan_flag(env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
    let path = config(env);
    // A file that is not there is read as an empty one, so that writing the
    // setting into nothing goes through the same editor as writing it into
    // somebody's file — one spelling of `[features]`, arrived at one way.
    let existing = read(&path)?;
    let before = existing.as_deref().unwrap_or_default();
    let was_on = toml_text::flag(before, FEATURES, HOOKS_FLAG).map_err(problem(&path))?;
    let after = toml_text::set_flag(before, FEATURES, HOOKS_FLAG).map_err(problem(&path))?;

    let file = match existing {
        None => Change::Create {
            path: path.clone(),
            contents: after.text,
            executable: false,
        },
        Some(text) if text == after.text => Change::Keep { path: path.clone() },
        Some(_) => Change::Rewrite {
            path: path.clone(),
            contents: after.text,
            executable: false,
        },
    };
    Ok(vec![
        file,
        claim(
            &path,
            flag_setting(),
            !was_on || state.claimed(&path, &flag_setting()),
        ),
        claim(
            &path,
            section_setting(),
            after.created_section || state.claimed(&path, &section_setting()),
        ),
    ])
}

/// What switching it back off would do, and nothing at all where the record
/// says it was never this program's.
///
/// A file this program created and has just emptied goes, by the same rule that
/// governs the drop-in file: an empty file this program made is litter, and one
/// it merely added a line to is the user's however little of it is left.
fn plan_flag_removal(env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
    let path = config(env);
    let file = match read(&path)? {
        Some(text) if state.claimed(&path, &flag_setting()) => {
            let contents = toml_text::clear_flag(
                &text,
                FEATURES,
                HOOKS_FLAG,
                state.claimed(&path, &section_setting()),
            )
            .map_err(problem(&path))?;
            let created = state.ownership(&path) == Some(Ownership::Created);
            match contents {
                _ if created && contents.trim().is_empty() => Change::Delete { path: path.clone() },
                ref same if *same == text => Change::Keep { path: path.clone() },
                contents => Change::Rewrite {
                    path: path.clone(),
                    contents,
                    executable: false,
                },
            }
        }
        _ => Change::Keep { path: path.clone() },
    };
    // Given up whichever way that went, including where the file itself has
    // gone: a record still claiming a setting in a file nobody can find is the
    // one thing an uninstall would otherwise leave behind.
    Ok(vec![
        file,
        claim(&path, flag_setting(), false),
        claim(&path, section_setting(), false),
    ])
}

/// One step saying who a setting belongs to.
fn claim(path: &Path, setting: String, ours: bool) -> Change {
    Change::Setting {
        path: path.to_owned(),
        setting,
        ours,
    }
}

/// Whether the drop-in file on this machine runs the wrapper this program would
/// install now.
///
/// A file that cannot be read as this program reads it counts as not pointing
/// anywhere. It may well be Codex's to understand, but it is not one an install
/// could add to either, so an installation resting on it is one that needs a
/// person.
fn is_pointed_at(env: &Environment) -> Result<bool, Error> {
    let Some(text) = read(&hooks(env))? else {
        return Ok(false);
    };
    let Ok(document) = json::parse(&text) else {
        return Ok(false);
    };
    let Some(Value::Array(entries)) = document.get(HOOKS_KEY).and_then(|at| at.get(EVENT)) else {
        return Ok(false);
    };
    let mut ours = entry(env)?;
    sentinel::mark(&mut ours);
    let ours = Value::Object(ours);
    let mut marked = entries.iter().filter(|entry| sentinel::is_marked(entry));
    Ok(marked.next() == Some(&ours) && marked.next().is_none())
}

/// Whether Codex on this machine is set to read its hooks at all.
///
/// A `config.toml` this program cannot read a line at a time counts as switched
/// off, for the same reason: an installation whose last step could not be taken
/// is one somebody has to look at.
fn is_switched_on(env: &Environment) -> Result<bool, Error> {
    let Some(text) = read(&config(env))? else {
        return Ok(false);
    };
    Ok(toml_text::flag(&text, FEATURES, HOOKS_FLAG).unwrap_or(false))
}

/// How a file that cannot be edited a line at a time is refused, named.
fn problem(path: &Path) -> impl Fn(toml_text::Problem) -> Error {
    let path = path.to_owned();
    move |problem| Error::NotEditable {
        path: path.clone(),
        problem,
    }
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

    /// A machine with a home directory that really exists and holds nothing.
    fn machine() -> (tempfile::TempDir, Environment) {
        let home = tempfile::tempdir().expect("cannot make a temporary directory");
        let env = Environment::rooted(home.path());
        (home, env)
    }

    /// What installing on `env` would do, with nothing installed before.
    fn plan(env: &Environment) -> Vec<Change> {
        plan_with(env, &State::default())
    }

    /// The same, on a machine this program has already written to.
    fn plan_with(env: &Environment, state: &State) -> Vec<Change> {
        Codex
            .plan_install(env, state, Path::new("/opt/bin/agentbus"))
            .expect("planning failed")
    }

    /// The step a plan has for `path`.
    fn step<'a>(changes: &'a [Change], path: &Path) -> &'a Change {
        changes
            .iter()
            .find(|change| change.path() == Some(path))
            .unwrap_or_else(|| panic!("no step for {}: {changes:?}", path.display()))
    }

    /// What a step would leave in the file it is about, given what is there
    /// now.
    fn left(change: &Change, before: Option<&str>) -> String {
        match change {
            Change::Create { contents, .. } | Change::Rewrite { contents, .. } => contents.clone(),
            Change::Keep { .. } => before.expect("kept a file that is not there").to_owned(),
            other => panic!("the file was {other:?}"),
        }
    }

    /// Writes `text` to `path`, making the directories above it.
    fn given(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// The drop-in file as a plan would leave it, parsed.
    fn registered(env: &Environment, before: Option<&str>) -> Value {
        if let Some(text) = before {
            given(&hooks(env), text);
        }
        let changes = plan(env);
        let text = left(step(&changes, &hooks(env)), before);
        serde_json::from_str(&text).expect("what would be written is not JSON")
    }

    /// The one entry registered for the event this program hooks.
    fn only_entry(document: &Value) -> &Value {
        let entries = document[HOOKS_KEY][EVENT]
            .as_array()
            .unwrap_or_else(|| panic!("nothing is registered for {EVENT}: {document}"));
        match entries.as_slice() {
            [entry] => entry,
            many => panic!("{EVENT} has {} entries", many.len()),
        }
    }

    /// Whether a plan says the setting is this program's.
    fn claims(changes: &[Change], setting: &str) -> bool {
        changes.iter().any(|change| {
            matches!(
                change,
                Change::Setting { setting: named, ours: true, .. } if named == setting
            )
        })
    }

    /// A record in which this program has switched the setting on, and written
    /// the section too if `section` says so.
    fn claimed(env: &Environment, section: bool) -> State {
        let mut state = State::default();
        state.claim(&config(env), &flag_setting());
        if section {
            state.claim(&config(env), &section_setting());
        }
        state
    }

    #[test]
    fn the_wrapper_obeys_the_rules_every_wrapper_obeys() {
        assets::tests::is_well_formed(Agent::Codex, &assets::CODEX_WRAPPER);
    }

    #[test]
    fn everything_installed_goes_inside_the_agents_own_configuration_directory() {
        let env = Environment::rooted("/home/u");

        assert_eq!(
            wrapper(&env),
            PathBuf::from("/home/u/.codex/hooks/agentbus.sh")
        );
        assert_eq!(hooks(&env), PathBuf::from("/home/u/.codex/hooks.json"));
        assert_eq!(config(&env), PathBuf::from("/home/u/.codex/config.toml"));
    }

    #[test]
    fn installing_writes_a_runnable_wrapper_and_an_entry_that_runs_it() {
        let (_home, env) = machine();

        let changes = plan(&env);

        let Change::Create {
            contents,
            executable,
            ..
        } = step(&changes, &wrapper(&env))
        else {
            panic!("the wrapper was not written: {changes:?}");
        };
        assert!(*executable, "the wrapper has to be runnable");
        assert!(
            contents.contains("'/opt/bin/agentbus'"),
            "the wrapper does not name the binary: {contents}"
        );
        assert!(!contents.contains(BINARY_MARK), "{contents}");

        let registered = registered(&env, None);
        let hook = &only_entry(&registered)["hooks"][0];
        assert_eq!(hook["type"], Value::from("command"));
        assert_eq!(
            hook["command"],
            Value::from(format!("bash '{}'", wrapper(&env).display()))
        );
        assert_eq!(hook["async"], Value::Bool(true));
        assert_eq!(hook["timeout"], Value::from(TIMEOUT));
    }

    #[test]
    fn the_only_event_hooked_is_the_one_that_says_which_session_this_is() {
        let (_home, env) = machine();

        let registered = registered(&env, None);

        let events: Vec<&String> = registered[HOOKS_KEY]
            .as_object()
            .expect("nothing was registered")
            .keys()
            .collect();
        assert_eq!(events, vec![EVENT]);
    }

    #[test]
    fn the_entry_carries_the_mark_that_makes_it_findable_again_and_no_matcher() {
        let (_home, env) = machine();

        let entry = only_entry(&registered(&env, None)).clone();

        assert!(sentinel::is_marked(&entry), "{entry}");
        assert_eq!(entry.get("matcher"), None, "{entry}");
    }

    #[test]
    fn what_an_earlier_build_registered_is_replaced_rather_than_added_to() {
        let (_home, env) = machine();
        // What installing used to write: an entry per event, each running the
        // binary directly, all of them marked as this program's.
        let mut events = Map::new();
        for event in ["SessionStart", "PreToolUse", "Stop", "SessionEnd"] {
            let mut hook = Map::new();
            hook.insert("type".to_owned(), Value::from("command"));
            hook.insert(
                "command".to_owned(),
                Value::from("/opt/bin/agentbus emit --agent codex"),
            );
            let mut entry = Map::new();
            sentinel::mark(&mut entry);
            entry.insert("hooks".to_owned(), Value::Array(vec![Value::Object(hook)]));
            events.insert(event.to_owned(), Value::Array(vec![Value::Object(entry)]));
        }
        let mut before = Map::new();
        before.insert(HOOKS_KEY.to_owned(), Value::Object(events));
        let before = format!("{:#}\n", Value::Object(before));

        let registered = registered(&env, Some(&before));

        let events = registered[HOOKS_KEY].as_object().expect("{registered}");
        assert_eq!(
            events[EVENT].as_array().map(Vec::len),
            Some(1),
            "the old entry was left beside the new one: {registered}"
        );
        for (event, entries) in events {
            let entries = entries.as_array().expect(event);
            assert!(
                event == EVENT || entries.is_empty(),
                "{event} still holds {entries:?}"
            );
        }
        assert!(
            only_entry(&registered)["hooks"][0]["command"]
                .as_str()
                .is_some_and(|command| command.starts_with("bash ")),
            "{registered}"
        );
    }

    #[test]
    fn installing_switches_hooks_on_and_says_the_setting_is_this_programs() {
        let (_home, env) = machine();

        let changes = plan(&env);

        assert_eq!(
            left(step(&changes, &config(&env)), None),
            "[features]\nhooks = true\n"
        );
        assert!(claims(&changes, &flag_setting()));
        assert!(claims(&changes, &section_setting()));
    }

    #[test]
    fn a_setting_somebody_else_switched_on_is_left_alone_and_never_claimed() {
        let (_home, env) = machine();
        let before = "# mine\n[features]\nhooks = true\nsomething = 1\n";
        given(&config(&env), before);

        let changes = plan(&env);

        assert!(
            matches!(step(&changes, &config(&env)), Change::Keep { .. }),
            "{changes:?}"
        );
        assert!(!claims(&changes, &flag_setting()));
        assert!(!claims(&changes, &section_setting()));
    }

    #[test]
    fn a_setting_this_program_switched_on_stays_its_own_when_it_installs_again() {
        let (_home, env) = machine();
        given(&config(&env), "[features]\nhooks = true\n");

        let changes = plan_with(&env, &claimed(&env, true));

        assert!(claims(&changes, &flag_setting()));
        assert!(claims(&changes, &section_setting()));
    }

    #[test]
    fn a_section_already_in_the_file_has_the_setting_added_and_nothing_else() {
        let (_home, env) = machine();
        let before = "[features]\n# theirs\nother = false\n\n[tui]\ntheme = \"dark\"\n";
        given(&config(&env), before);

        let changes = plan(&env);

        assert_eq!(
            left(step(&changes, &config(&env)), Some(before)),
            "[features]\nhooks = true\n# theirs\nother = false\n\n[tui]\ntheme = \"dark\"\n"
        );
        assert!(claims(&changes, &flag_setting()));
        assert!(
            !claims(&changes, &section_setting()),
            "a section that was already there is not this program's"
        );
    }

    #[test]
    fn a_config_this_cannot_read_a_line_at_a_time_stops_the_plan() {
        let (_home, env) = machine();
        let before = "[features]\nhooks = false\n\n[features]\n";
        given(&config(&env), before);

        let refused = Codex.plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"));

        assert!(
            matches!(refused, Err(Error::NotEditable { .. })),
            "{refused:?}"
        );
        assert_eq!(fs::read_to_string(config(&env)).unwrap(), before);
    }

    #[test]
    fn uninstalling_switches_off_only_a_setting_the_record_says_is_this_programs() {
        let (_home, env) = machine();
        let before = "[features]\nhooks = true\n";
        given(&config(&env), before);

        let theirs = Codex
            .plan_uninstall(&env, &State::default())
            .expect("planning failed");
        assert!(
            matches!(step(&theirs, &config(&env)), Change::Keep { .. }),
            "{theirs:?}"
        );

        let ours = Codex
            .plan_uninstall(&env, &claimed(&env, true))
            .expect("planning failed");
        assert_eq!(left(step(&ours, &config(&env)), Some(before)), "");
    }

    #[test]
    fn uninstalling_gives_back_a_config_the_install_only_added_a_line_to() {
        let (_home, env) = machine();
        let before = "# mine\n[features]\nother = false\n";
        given(&config(&env), before);
        let after = left(step(&plan(&env), &config(&env)), Some(before));
        given(&config(&env), &after);

        let changes = Codex
            .plan_uninstall(&env, &claimed(&env, false))
            .expect("planning failed");

        assert_eq!(left(step(&changes, &config(&env)), Some(&after)), before);
    }

    #[test]
    fn uninstalling_gives_up_the_setting_whichever_way_it_went() {
        let (_home, env) = machine();
        given(&config(&env), "[features]\nhooks = true\n");

        for state in [State::default(), claimed(&env, true)] {
            let changes = Codex.plan_uninstall(&env, &state).expect("planning failed");

            assert!(!claims(&changes, &flag_setting()), "{changes:?}");
            assert!(!claims(&changes, &section_setting()), "{changes:?}");
        }
    }

    #[test]
    fn nothing_is_installed_where_there_is_no_wrapper_or_where_it_is_not_ours() {
        let (_home, env) = machine();

        assert_eq!(Codex.status(&env).unwrap(), HookStatus::NotInstalled);

        given(&wrapper(&env), "#!/bin/sh\n# mine\nexit 0\n");
        assert_eq!(Codex.status(&env).unwrap(), HookStatus::NotInstalled);
    }

    /// Puts a whole installation on the machine, and answers with what its
    /// status is then.
    fn installed(env: &Environment) -> HookStatus {
        let mut state = State::default();
        for change in plan(env) {
            change.apply(Agent::Codex, &mut state).expect("applying");
        }
        Codex.status(env).expect("cannot read the status")
    }

    #[test]
    fn a_whole_installation_is_current_and_each_half_missing_needs_repairing() {
        let (_home, env) = machine();
        let expected = crate::version::expected_version(Agent::Codex);

        assert_eq!(installed(&env), HookStatus::Current(expected));

        let entries = fs::read_to_string(hooks(&env)).unwrap();
        fs::write(hooks(&env), "{}\n").unwrap();
        assert_eq!(
            Codex.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected),
            "an entry that no longer runs the wrapper"
        );

        fs::write(hooks(&env), &entries).unwrap();
        fs::write(config(&env), "[features]\nhooks = false\n").unwrap();
        assert_eq!(
            Codex.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected),
            "a setting somebody has switched back off"
        );
    }

    #[test]
    fn an_installation_from_before_the_wrapper_is_behind_rather_than_current() {
        let (_home, env) = machine();
        let expected = crate::version::expected_version(Agent::Codex);
        let text = assets::CODEX_WRAPPER
            .text(Platform::Unix)
            .replace(&format!("AGENTBUS_HOOK_VERSION={expected}"), "hooks");
        given(&wrapper(&env), &text);

        assert_eq!(
            Codex.status(&env).unwrap(),
            HookStatus::Outdated {
                found: None,
                expected
            }
        );
    }

    #[test]
    fn a_path_that_is_not_text_is_refused_rather_than_approximated() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let binary = PathBuf::from(OsStr::from_bytes(b"/opt/\xff/agentbus"));

        assert!(matches!(
            Codex.plan_install(&Environment::rooted("/home/u"), &State::default(), &binary),
            Err(Error::Unwritable { .. })
        ));
    }
}
