//! Installing for MastraCode.
//!
//! The one agent here whose hooks file has no wrapper key at all: `hooks.json`
//! is a top-level object whose keys *are* the event names, each holding an array
//! of flat `{type, command, timeout, description}` entries. Everything else in
//! this crate that writes JSON writes it a level or two down, so the placements
//! below name one key rather than two — which is all the difference amounts to.
//!
//! # Every event, one mapping
//!
//! The wrapper is registered for the whole of the agent's lifecycle, not for the
//! start of a session alone. The mapping this program carries turns exactly one
//! of those events into something it understands today and passes over the rest,
//! and that is on purpose: registering is the half that touches a user's
//! machine, and doing it once means the day a mapping learns another event is a
//! day nobody has to reinstall. An event nobody has written a mapping for
//! reaches the emit path and is dropped there, quietly, which is what the emit
//! path does with everything it was not taught.
//!
//! # The mark rides inside the entry
//!
//! As everywhere else in this crate, an entry this program wrote says so with a
//! key of its own — see [`crate::sentinel`]. This agent reads an entry by asking
//! two questions of it, whether its `type` is `command` and whether its
//! `command` is a string, and carries whatever else it finds. So the mark is
//! invisible to it and the file stays one this program can find its own work in
//! again later.
//!
//! # The allowance is counted in milliseconds
//!
//! This agent reads the `timeout` field of an entry as milliseconds and kills
//! the hook when it runs out. A number is only ever as good as the unit it is
//! read in, so the one written here is spelled in the unit this agent reads.
//!
//! # The command is handed over encoded on the machine that needs it
//!
//! Both kinds of machine are given the whole invocation as one string, and on
//! one of them that string goes through the machine's own command processor
//! before anything runs — where a quoted path with a space or an apostrophe in
//! it does not survive intact. The encoded spelling from
//! [`command::encoded_hook_command`] leaves nothing for anything to reinterpret.
//! The other kind of machine hands the string to a shell whose quoting survives
//! being read, and gets the ordinary spelling.
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

/// The file the entries go into.
const HOOKS_FILE: &str = "hooks.json";

/// The directory inside the configuration directory the wrapper is dropped
/// into.
///
/// Not required by the agent — every entry names the wrapper by an absolute
/// path — but it is where a person looking for an installed hook would look, and
/// it keeps a generated file out of the top of a directory somebody reads.
const HOOKS_DIR: &str = "hooks";

/// The events the wrapper is registered for.
///
/// Written out in the order the agent's lifecycle runs in rather than
/// alphabetically, because that is the order somebody reading the installed file
/// will want to check it in.
///
/// Three of the events the agent publishes are deliberately not here. A tool
/// call *finishing* is the far side of one that is, and hooking both sides of
/// the busiest event any agent produces doubles what this program costs a
/// session in exchange for a transition the next event already implies. A
/// notification is not a change in what the agent is doing. A session *ending*
/// arrives while the agent is on its way out, which is the moment a hook is
/// least likely to finish and the moment its answer is worth least.
const EVENTS: [&str; 11] = [
    SESSION_START,
    "UserPromptSubmit",
    "AgentStart",
    "PreToolUse",
    "PermissionRequest",
    "PermissionResult",
    "SubagentStart",
    "SubagentEnd",
    "Interrupt",
    "AgentEnd",
    "Stop",
];

/// The event that says a session has begun.
const SESSION_START: &str = "SessionStart";

/// How long the agent is asked to allow the wrapper, in the unit it reads.
///
/// Far longer than the wrapper can take, and there so that a machine which has
/// somehow made it slow cannot make somebody's session slow with it.
const TIMEOUT_MILLISECONDS: u64 = 5_000;

/// What the entry says it is for, where the agent lists its hooks to a user.
///
/// A user who opens that list is entitled to a line telling them what a hook
/// they did not write is doing there.
const DESCRIPTION: &str = "Hand this event to agentbus";

/// What the wrapper says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// MastraCode's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mastracode;

impl Installer for Mastracode {
    fn agent(&self) -> Agent {
        Agent::Mastracode
    }

    /// Writes the wrapper, then points the hooks file at it.
    ///
    /// In that order. The entries name a file, and a hooks file that named one
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
        changes.push(merge::plan_install(&hooks_file(env), &placements(env)?)?);
        Ok(changes)
    }

    /// Takes the entries out, then the wrapper.
    ///
    /// In that order for the same reason reversed: the entries are what run the
    /// file, so they go first and nothing is ever pointed at something that is
    /// no longer there. An event name this program introduced goes with the last
    /// of its entries, so a file it merely added to is left holding what it held
    /// before.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        let mut changes = vec![merge::plan_uninstall(&hooks_file(env), state)?];

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

    /// Reads the wrapper, and then looks for the entries that run it.
    ///
    /// The wrapper is the file that says which generation this is, because it is
    /// the one whose content is the installation. A wrapper nothing calls is a
    /// working file in an installation that never runs, which is exactly the
    /// case worth telling somebody about — and so is an installation missing one
    /// of its events, which is one that will be silent about whatever that event
    /// was for.
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
    Agent::Mastracode.config_dir(env)
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

/// The hooks file the entries go into.
fn hooks_file(env: &Environment) -> PathBuf {
    config_dir(env).join(HOOKS_FILE)
}

/// One entry per event, and where each belongs.
///
/// The path is one key long, because the event names are the top of this file
/// rather than something hanging from it.
fn placements(env: &Environment) -> Result<Vec<Placement>, Error> {
    let entry = entry(env)?;
    Ok(EVENTS
        .iter()
        .map(|event| Placement::new([*event], entry.clone()))
        .collect())
}

/// The entry itself, before the merge marks it as this program's.
///
/// The same entry whichever event it is registered under: the wrapper is told
/// nothing about which event it is running for, because the payload it forwards
/// already says.
fn entry(env: &Environment) -> Result<Map<String, Value>, Error> {
    let mut entry = Map::new();
    entry.insert("type".to_owned(), Value::from("command"));
    entry.insert(
        "command".to_owned(),
        Value::from(command::encoded_hook_command(
            Agent::Mastracode,
            env.platform(),
            &wrapper(env),
            None,
        )?),
    );
    entry.insert("timeout".to_owned(), Value::from(TIMEOUT_MILLISECONDS));
    entry.insert("description".to_owned(), Value::from(DESCRIPTION));
    Ok(entry)
}

/// What the wrapper should hold, with the binary written into it.
fn generated(platform: Platform, binary: &Path) -> Result<String, Error> {
    let named = match platform {
        Platform::Unix => command::in_shell(binary)?,
        Platform::Windows => command::in_powershell(binary)?,
    };
    Ok(assets::MASTRACODE_WRAPPER
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
/// install now, on every event it registers for.
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
    Ok(EVENTS.iter().all(|event| {
        let Some(Value::Array(entries)) = document.get(event) else {
            return false;
        };
        let mut marked = entries.iter().filter(|entry| sentinel::is_marked(entry));
        marked.next() == Some(&ours) && marked.next().is_none()
    }))
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
    const AGENT: Agent = Agent::Mastracode;

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
        Mastracode
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

    /// Uninstalls, carrying on from the record an install left.
    fn uninstall(env: &Environment, state: &mut State) {
        let changes = Mastracode
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

    /// This program's own entries under `event`.
    fn ours(document: &Value, event: &str) -> Vec<Value> {
        let Some(Value::Array(entries)) = document.get(event) else {
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
        is_well_formed(AGENT, &assets::MASTRACODE_WRAPPER);
    }

    #[test]
    fn a_fresh_install_writes_the_wrapper_and_an_entry_that_runs_it_on_every_event() {
        let (_home, env) = machine_with_agent();

        install(&env);

        let written = fs::read_to_string(wrapper(&env)).expect("no wrapper");
        assert!(sentinel::is_generated(&written));
        assert!(
            written.contains("/opt/bin/agentbus"),
            "the wrapper does not name the binary"
        );

        let document = hooks_document(&env);
        for event in EVENTS {
            let entries = ours(&document, event);
            assert_eq!(entries.len(), 1, "{event}");
            assert!(
                entries[0]["command"]
                    .as_str()
                    .expect("a command")
                    .contains("agentbus.sh"),
                "the entry under {event} does not run the wrapper: {:?}",
                entries[0],
            );
        }
    }

    #[test]
    fn the_event_names_are_the_top_of_the_file_rather_than_something_hanging_from_it() {
        let (_home, env) = machine_with_agent();

        install(&env);

        let document = hooks_document(&env);
        let named: Vec<&str> = document
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(named, EVENTS, "{document:?}");
    }

    #[test]
    fn the_entry_says_how_long_it_may_take_in_the_unit_the_agent_reads() {
        let (_home, env) = machine_with_agent();

        install(&env);

        let entries = ours(&hooks_document(&env), SESSION_START);
        assert_eq!(entries[0]["type"], Value::from("command"));
        assert_eq!(entries[0]["timeout"], Value::from(5_000));
        assert_eq!(entries[0]["description"], Value::from(DESCRIPTION));
    }

    #[test]
    fn the_command_survives_a_machine_that_reads_it_twice() {
        let (_home, env) = machine_with_agent();
        let env = env.with_platform(Platform::Windows);

        let entry = entry(&env).expect("an entry");
        let command = entry["command"].as_str().expect("a command").to_owned();

        let encoded = command
            .strip_prefix("powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand ")
            .unwrap_or_else(|| panic!("{command}"));
        assert_eq!(
            decoded(encoded),
            format!("& '{}'", wrapper(&env).display()),
            "the encoded command is not the invocation it stands for",
        );
    }

    /// What `encoded` was made from, read back the way the machine reading it
    /// would: base64 of the command in little-endian UTF-16.
    fn decoded(encoded: &str) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

        let mut bits = Vec::new();
        for character in encoded.bytes().filter(|byte| *byte != b'=') {
            let index = ALPHABET
                .iter()
                .position(|letter| *letter == character)
                .expect("not one of the letters the encoding uses");
            for shift in (0..6).rev() {
                bits.push((index >> shift) & 1 == 1);
            }
        }
        let bytes: Vec<u8> = bits
            .chunks_exact(8)
            .map(|byte| {
                byte.iter()
                    .fold(0_u8, |whole, bit| (whole << 1) | u8::from(*bit))
            })
            .collect();
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units).expect("not what was encoded")
    }

    #[test]
    fn an_agent_that_is_not_on_the_machine_is_refused_and_nothing_is_written() {
        let (_home, env) = machine();

        let refusal = Mastracode
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
    fn foreign_entries_under_the_same_events_survive_an_install_and_an_uninstall() {
        let (_home, env) = machine_with_agent();
        let theirs = "{\n  \"SessionStart\": [\n    {\n      \"type\": \"command\",\n      \
                      \"command\": \"their-own-script\"\n    }\n  ],\n  \"Notification\": [\n \
                      {\n      \"type\": \"command\",\n      \"command\": \"theirs\"\n    }\n  \
                      ]\n}\n";
        fs::write(hooks_file(&env), theirs).unwrap();

        let mut state = install(&env);
        let document = hooks_document(&env);
        assert_eq!(
            document[SESSION_START].as_array().expect("an array").len(),
            2,
        );

        uninstall(&env, &mut state);

        let after = hooks_document(&env);
        assert_eq!(
            after[SESSION_START][0]["command"],
            Value::from("their-own-script")
        );
        assert_eq!(after["Notification"][0]["command"], Value::from("theirs"));
        for event in EVENTS {
            assert!(ours(&after, event).is_empty(), "{event}");
        }
    }

    #[test]
    fn an_event_name_this_program_introduced_goes_when_the_last_of_its_entries_does() {
        let (_home, env) = machine_with_agent();
        let theirs = "{\n  \"SessionStart\": [\n    {\n      \"type\": \"command\",\n      \
                      \"command\": \"their-own-script\"\n    }\n  ]\n}\n";
        fs::write(hooks_file(&env), theirs).unwrap();
        let mut state = install(&env);

        uninstall(&env, &mut state);

        let after = hooks_document(&env);
        let named: Vec<&str> = after
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            named,
            [SESSION_START],
            "an event key this program introduced was left standing empty",
        );
    }

    #[test]
    fn an_event_holding_something_that_is_not_a_list_of_entries_is_refused() {
        let (_home, env) = machine_with_agent();
        let held = "{\n  \"Stop\": {\n    \"theirs\": true\n  }\n}\n";
        fs::write(hooks_file(&env), held).unwrap();

        let refusal = Mastracode
            .plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"))
            .expect_err("an event with nowhere to put an entry has to stop the plan");

        assert!(matches!(refusal, Error::Conflict { .. }), "{refusal:?}");
        assert_eq!(
            hooks_text(&env).as_deref(),
            Some(held),
            "the hooks file was changed anyway",
        );
        assert!(!wrapper(&env).exists(), "the wrapper was written anyway");
    }

    #[test]
    fn a_hooks_file_that_cannot_be_rewritten_safely_is_refused() {
        for held in ["{not json at all}", "{\"Stop\": [], \"Stop\": []}"] {
            let (_home, env) = machine_with_agent();
            fs::write(hooks_file(&env), held).unwrap();

            let refusal = Mastracode
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
    fn a_wrapper_somebody_else_wrote_stops_the_plan() {
        let (_home, env) = machine_with_agent();
        fs::create_dir_all(hooks_dir(&env)).unwrap();
        fs::write(wrapper(&env), "# something a user wrote\n").unwrap();

        let refusal = Mastracode
            .plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"))
            .expect_err("somebody else's file has to stop the plan");

        assert!(matches!(refusal, Error::NotOurs { .. }), "{refusal:?}");
    }

    #[test]
    fn installing_twice_changes_nothing_the_second_time() {
        let (_home, env) = machine_with_agent();
        let state = install(&env);
        let before = hooks_text(&env);

        let again = Mastracode
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
        assert!(!hooks_file(&env).exists());

        let carried_out = plan(&env);
        assert_eq!(described, carried_out);

        let mut state = State::default();
        apply(&carried_out, &mut state);
        assert!(wrapper(&env).exists());
        assert!(hooks_file(&env).exists());
    }

    #[test]
    fn uninstalling_leaves_nothing_of_its_own_behind() {
        let (_home, env) = machine_with_agent();
        let mut state = install(&env);

        uninstall(&env, &mut state);

        assert!(!wrapper(&env).exists());
        assert!(
            !hooks_dir(&env).exists(),
            "a directory this program made was left standing",
        );
        assert!(
            !hooks_file(&env).exists(),
            "a hooks file this program created was kept for the sake of the \
             entries it wrote in it",
        );
        assert!(
            config_dir(&env).is_dir(),
            "a directory that was never this program's was removed",
        );
    }

    #[test]
    fn what_is_installed_is_what_the_machine_is_asked_about() {
        let (_home, env) = machine_with_agent();
        assert_eq!(Mastracode.status(&env).unwrap(), HookStatus::NotInstalled);

        let mut state = install(&env);
        let expected = crate::version::expected_version(AGENT);
        assert_eq!(
            Mastracode.status(&env).unwrap(),
            HookStatus::Current(expected),
        );

        // One event's worth of the installation taken away leaves a wrapper
        // that runs for everything else and is silent about that one.
        let mut document = hooks_document(&env);
        document
            .as_object_mut()
            .expect("an object")
            .remove("PermissionResult");
        fs::write(
            hooks_file(&env),
            json::render(&document, json::DEFAULT_INDENT),
        )
        .unwrap();
        assert_eq!(
            Mastracode.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected),
        );

        uninstall(&env, &mut state);
        assert_eq!(Mastracode.status(&env).unwrap(), HookStatus::NotInstalled);
    }

    #[test]
    fn an_upgrade_replaces_what_an_earlier_build_wrote_rather_than_adding_to_it() {
        let (_home, env) = machine_with_agent();
        install(&env);

        // What an older build would have left: the same kind of entry, marked,
        // naming a wrapper somewhere this build no longer writes one.
        let mut document = hooks_document(&env);
        let mut stale = Map::new();
        stale.insert("type".to_owned(), Value::from("command"));
        stale.insert(
            "command".to_owned(),
            Value::from("bash '/gone/agentbus.sh'"),
        );
        sentinel::mark(&mut stale);
        document[SESSION_START]
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

        assert_eq!(
            ours(&hooks_document(&env), SESSION_START).len(),
            1,
            "entries accumulated",
        );
    }
}
