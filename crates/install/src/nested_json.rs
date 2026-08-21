//! Installing for the agents configured by a nested `hooks` object in a JSON
//! settings file.
//!
//! Four agents here are configured the same way, and the shape is the one Claude
//! Code made common: a settings file whose `hooks` key holds an object of event
//! names, each naming an array of entries, each entry an optional `matcher` and
//! a list of `{type, command, timeout}` invocations. Writing four installers for
//! that would mean four copies of the same care about backups, marks, refusals
//! and reversal, drifting apart one fix at a time. So there is one installer
//! here and four descriptions of an agent, and a fifth agent that turns up
//! configured this way should be a fifth description rather than a fifth module.
//!
//! What actually differs between them is small enough to list: where the
//! settings file is, which events are registered, whether the entries carry a
//! matcher, and — for exactly one of them — whether the timeout is counted in
//! seconds or in milliseconds. Everything else is shared, and being shared is
//! the point.
//!
//! # The settings file is rewritten rather than edited line by line
//!
//! These files are read and written by the agents themselves, which rewrite them
//! wholesale whenever a user changes a setting from inside the agent. There is
//! no hand-kept formatting to preserve that the agent itself would not discard
//! at the next opportunity, so an entry goes in through the ordinary merge in
//! [`crate::merge`]: the document is read, this program's own entries are
//! replaced, and it is written back with the indentation it arrived with. A
//! document that cannot be read back out unchanged is refused, which is the same
//! bargain everywhere else in this crate.
//!
//! # A missing configuration directory is a refusal
//!
//! Unlike the agents that keep their configuration where this program is happy
//! to create it, none of these directories is made here. A `~/.factory` that is
//! not there means Droid has never run on this machine, and an installer that
//! made one would be creating another program's home on the strength of a guess
//! about its layout — leaving a directory behind that the agent may never read
//! and that an uninstall of *this* program is then responsible for. Saying so
//! and stopping is both the honest answer and the useful one.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::assets::Asset;
use crate::change::Change;
use crate::command;
use crate::paths::{Environment, Platform};
use crate::state::{Ownership, State};
use crate::status::HookStatus;
use crate::{Error, Installer, Placement, assets, file, json, merge, sentinel};

/// The directory inside an agent's configuration that the wrapper is dropped
/// into.
///
/// The same name for all four, and not because any of them requires it — every
/// entry names the wrapper by an absolute path. It is where a person looking for
/// an installed hook would look, and keeping a generated file out of the top of
/// a directory somebody reads is worth the one extra directory.
const HOOKS_DIR: &str = "hooks";

/// The key in the settings file that the event names hang from.
const HOOKS_KEY: &str = "hooks";

/// The event that says a session has begun.
///
/// Spelled the same way by all four, which is what being one idiom means.
const SESSION_START: &str = "SessionStart";

/// How long an agent is asked to allow the wrapper.
///
/// Far longer than the wrapper can take, and there so that a machine which has
/// somehow made it slow cannot make somebody's session slow with it.
const TIMEOUT_SECONDS: u64 = 5;

/// What the wrapper says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// What unit an agent reads the `timeout` field of a hook entry in.
///
/// The one difference between these four that is not a path or a list, and the
/// reason this is a parameter rather than a constant. A number is only ever as
/// good as the unit it is read in: five, meant as seconds and read as
/// milliseconds, is a hook killed before it has started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    /// The field counts seconds.
    Seconds,
    /// The field counts milliseconds.
    Milliseconds,
}

impl Timeout {
    /// The allowance above, written in this unit.
    fn field(self) -> u64 {
        match self {
            Self::Seconds => TIMEOUT_SECONDS,
            Self::Milliseconds => TIMEOUT_SECONDS * 1_000,
        }
    }
}

/// One agent configured by a nested `hooks` object, described by what makes it
/// different from the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NestedJson {
    /// The agent this installs for.
    agent: Agent,
    /// The file inside its configuration directory the entries go into.
    settings: &'static str,
    /// The events the wrapper is registered for.
    events: &'static [&'static str],
    /// What the entries match, where the agent expects to be told.
    matcher: Option<&'static str>,
    /// The unit the agent reads a hook's timeout in.
    timeout: Timeout,
    /// The wrapper written for it.
    wrapper: &'static Asset,
}

/// Devin's hooks.
///
/// The one of the four whose settings live in a file called `config.json`, and
/// the one registered for more than a single event: it reports the identity of
/// its session on each of six, so an installation that only took the first would
/// be one that missed every session already running when it was made.
pub static DEVIN: NestedJson = NestedJson {
    agent: Agent::Devin,
    settings: "config.json",
    events: &[
        SESSION_START,
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "PermissionRequest",
        "Stop",
    ],
    // The events carry no tool name to match on, and inventing a pattern for
    // something with nothing to match would be saying more than is meant.
    matcher: None,
    timeout: Timeout::Seconds,
    wrapper: &assets::DEVIN_WRAPPER,
};

/// Droid's hooks.
///
/// Nothing is cleaned out of any file but the one named here. Droid has read its
/// hooks from more than one place over its life, but this program has only ever
/// written to the settings file, so there is nothing of its own anywhere else to
/// find.
pub static DROID: NestedJson = NestedJson {
    agent: Agent::Droid,
    settings: "settings.json",
    events: &[SESSION_START],
    matcher: None,
    timeout: Timeout::Seconds,
    wrapper: &assets::DROID_WRAPPER,
};

/// Qoder's hooks.
pub static QODERCLI: NestedJson = NestedJson {
    agent: Agent::QoderCli,
    settings: "settings.json",
    events: &[SESSION_START],
    // Qoder expects a matcher, so the entry carries the pattern that says
    // "everything" rather than an omission a reader would have to interpret.
    matcher: Some("*"),
    timeout: Timeout::Seconds,
    wrapper: &assets::QODERCLI_WRAPPER,
};

/// Qwen's hooks.
pub static QWEN: NestedJson = NestedJson {
    agent: Agent::Qwen,
    settings: "settings.json",
    events: &[SESSION_START],
    matcher: Some("*"),
    // The one of the four that counts the allowance in milliseconds.
    timeout: Timeout::Milliseconds,
    wrapper: &assets::QWEN_WRAPPER,
};

impl Installer for NestedJson {
    fn agent(&self) -> Agent {
        self.agent
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
        let home = self.config_dir(env);
        if !home.is_dir() {
            return Err(Error::Absent {
                agent: self.agent,
                path: home,
            });
        }
        let mut changes = Vec::new();
        // Writing a file makes the directory above it anyway; this is here so
        // that the making is recorded, which is the one thing the uninstall
        // cannot work out for itself later.
        let hooks_dir = self.hooks_dir(env);
        if !hooks_dir.is_dir() {
            changes.push(Change::Make { path: hooks_dir });
        }
        changes.push(self.plan_wrapper(env, binary)?);
        changes.push(merge::plan_install(
            &self.settings(env),
            &self.placements(env)?,
        )?);
        Ok(changes)
    }

    /// Takes the entries out, then the wrapper.
    ///
    /// In that order for the same reason reversed: the entries are what run the
    /// file, so they go first and nothing is ever pointed at something that is
    /// no longer there.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        let mut changes = vec![merge::plan_uninstall(&self.settings(env), state)?];

        let path = self.wrapper(env);
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
        let hooks_dir = self.hooks_dir(env);
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
    /// case worth telling somebody about — and so is an entry left over from an
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
        let status = HookStatus::of_text(self.agent, &text);
        Ok(status.confirmed(self.is_pointed_at(env)?))
    }

    /// The wrapper this program drops in, which is the file the mark is in.
    fn asset(&self, env: &Environment) -> PathBuf {
        self.wrapper(env)
    }
}

impl NestedJson {
    /// Where the agent keeps its configuration.
    fn config_dir(&self, env: &Environment) -> PathBuf {
        self.agent.config_dir(env)
    }

    /// The directory the wrapper is dropped into.
    fn hooks_dir(&self, env: &Environment) -> PathBuf {
        self.config_dir(env).join(HOOKS_DIR)
    }

    /// The wrapper this program drops in there.
    fn wrapper(&self, env: &Environment) -> PathBuf {
        self.hooks_dir(env).join(match env.platform() {
            Platform::Unix => "agentbus.sh",
            Platform::Windows => "agentbus.ps1",
        })
    }

    /// The settings file the entries go into.
    fn settings(&self, env: &Environment) -> PathBuf {
        self.config_dir(env).join(self.settings)
    }

    /// What the wrapper should hold, with the binary written into it.
    fn generated(&self, platform: Platform, binary: &Path) -> Result<String, Error> {
        let named = match platform {
            Platform::Unix => command::in_shell(binary)?,
            Platform::Windows => command::in_powershell(binary)?,
        };
        Ok(self.wrapper.text(platform).replace(BINARY_MARK, &named))
    }

    /// What writing the wrapper would do, given whatever is at its path now.
    ///
    /// A file of that name this program did not write stops the plan. It is the
    /// one case where doing nothing is worse than saying so: silently leaving it
    /// would report a successful installation of hooks that are not installed,
    /// and silently replacing it would delete something a user wrote.
    fn plan_wrapper(&self, env: &Environment, binary: &Path) -> Result<Change, Error> {
        let path = self.wrapper(env);
        let contents = self.generated(env.platform(), binary)?;
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

    /// One entry per event, and where each belongs.
    fn placements(&self, env: &Environment) -> Result<Vec<Placement>, Error> {
        let entry = self.entry(env)?;
        Ok(self
            .events
            .iter()
            .map(|event| Placement::new([HOOKS_KEY, event], entry.clone()))
            .collect())
    }

    /// The entry itself, before the merge marks it as this program's.
    ///
    /// The same entry whichever event it is registered under: the wrapper is
    /// told nothing about which event it is running for, because the payload it
    /// forwards already says.
    fn entry(&self, env: &Environment) -> Result<Map<String, Value>, Error> {
        let mut hook = Map::new();
        hook.insert("type".to_owned(), Value::from("command"));
        hook.insert(
            "command".to_owned(),
            Value::from(command::hook_command(
                self.agent,
                env.platform(),
                &self.wrapper(env),
                None,
            )?),
        );
        hook.insert("timeout".to_owned(), Value::from(self.timeout.field()));

        let mut entry = Map::new();
        if let Some(matcher) = self.matcher {
            entry.insert("matcher".to_owned(), Value::from(matcher));
        }
        entry.insert("hooks".to_owned(), Value::Array(vec![Value::Object(hook)]));
        Ok(entry)
    }

    /// Whether the settings on this machine run the wrapper this program would
    /// install now, on every event it registers for.
    ///
    /// Every one of them, because an installation missing one of its events is
    /// an installation that will be silent about whatever that event was for.
    /// A settings file that cannot be read as this program reads it counts as
    /// not pointing anywhere: it may well be the agent's to understand, but it
    /// is not one an install could add to either, so an installation resting on
    /// it is one that needs a person.
    fn is_pointed_at(&self, env: &Environment) -> Result<bool, Error> {
        let Some(text) = read(&self.settings(env))? else {
            return Ok(false);
        };
        let Ok(document) = json::parse(&text) else {
            return Ok(false);
        };
        let mut ours = self.entry(env)?;
        sentinel::mark(&mut ours);
        let ours = Value::Object(ours);
        Ok(self.events.iter().all(|event| {
            let Some(Value::Array(entries)) = document.get(HOOKS_KEY).and_then(|at| at.get(event))
            else {
                return false;
            };
            let mut marked = entries.iter().filter(|entry| sentinel::is_marked(entry));
            marked.next() == Some(&ours) && marked.next().is_none()
        }))
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
    use crate::assets::tests::is_well_formed;

    /// Every agent this module installs for.
    const ALL: [&NestedJson; 4] = [&DEVIN, &DROID, &QODERCLI, &QWEN];

    /// A machine with a home directory that really exists and holds nothing.
    fn machine() -> (tempfile::TempDir, Environment) {
        let home = tempfile::tempdir().expect("cannot make a temporary directory");
        let env = Environment::rooted(home.path());
        (home, env)
    }

    /// The same machine, with `installer`'s agent already on it.
    fn machine_with(installer: &NestedJson) -> (tempfile::TempDir, Environment) {
        let (home, env) = machine();
        fs::create_dir_all(installer.config_dir(&env)).expect("cannot make the agent's directory");
        (home, env)
    }

    /// What installing on `env` would do, with nothing installed before.
    fn plan(installer: &NestedJson, env: &Environment) -> Vec<Change> {
        installer
            .plan_install(env, &State::default(), Path::new("/opt/bin/agentbus"))
            .expect("planning failed")
    }

    /// Carries `changes` out, so that the next plan sees the machine they left.
    fn apply(installer: &NestedJson, changes: &[Change], state: &mut State) {
        for change in changes {
            change
                .apply(installer.agent, state)
                .expect("carrying out a step failed");
        }
    }

    /// Installs for real, and answers with the record it left.
    fn install(installer: &NestedJson, env: &Environment) -> State {
        let mut state = State::default();
        let changes = plan(installer, env);
        apply(installer, &changes, &mut state);
        state
    }

    /// The settings file as it stands.
    fn settings_text(installer: &NestedJson, env: &Environment) -> Option<String> {
        fs::read_to_string(installer.settings(env)).ok()
    }

    /// The settings file, read as a document.
    fn settings_document(installer: &NestedJson, env: &Environment) -> Value {
        serde_json::from_str(&settings_text(installer, env).expect("no settings file"))
            .expect("the settings file is not JSON")
    }

    /// This program's own entries under `event`.
    fn ours(document: &Value, event: &str) -> Vec<Value> {
        let Some(Value::Array(entries)) = document.get(HOOKS_KEY).and_then(|at| at.get(event))
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
    fn every_agent_here_is_described_once_and_installs_its_own_wrapper() {
        let mut agents: Vec<Agent> = ALL.iter().map(|installer| installer.agent).collect();
        let described = agents.len();
        agents.sort_unstable();
        agents.dedup();

        assert_eq!(agents.len(), described, "two descriptions, one agent");
        for installer in ALL {
            assert!(
                !installer.events.is_empty(),
                "{} hooks nothing",
                installer.agent
            );
            is_well_formed(installer.agent, installer.wrapper);
        }
    }

    #[test]
    fn a_fresh_install_writes_the_wrapper_and_registers_every_event() {
        for installer in ALL {
            let (_home, env) = machine_with(installer);

            install(installer, &env);

            let wrapper = fs::read_to_string(installer.wrapper(&env)).expect("no wrapper");
            assert!(sentinel::is_generated(&wrapper), "{}", installer.agent);
            assert!(
                wrapper.contains("/opt/bin/agentbus"),
                "{} does not name the binary",
                installer.agent
            );

            let document = settings_document(installer, &env);
            for event in installer.events {
                let entries = ours(&document, event);
                assert_eq!(entries.len(), 1, "{} on {event}", installer.agent);
                let hook = &entries[0]["hooks"][0];
                assert_eq!(hook["type"], Value::from("command"));
                assert!(
                    hook["command"]
                        .as_str()
                        .expect("a command")
                        .contains("agentbus.sh"),
                    "{} does not run its wrapper",
                    installer.agent
                );
            }
        }
    }

    #[test]
    fn one_agent_counts_the_allowance_in_milliseconds_and_the_rest_in_seconds() {
        let expected = [
            (&DEVIN, TIMEOUT_SECONDS),
            (&DROID, TIMEOUT_SECONDS),
            (&QODERCLI, TIMEOUT_SECONDS),
            (&QWEN, TIMEOUT_SECONDS * 1_000),
        ];

        for (installer, timeout) in expected {
            let (_home, env) = machine_with(installer);
            install(installer, &env);

            let document = settings_document(installer, &env);
            let entry = &ours(&document, SESSION_START)[0];
            assert_eq!(
                entry["hooks"][0]["timeout"],
                Value::from(timeout),
                "{}",
                installer.agent
            );
        }
    }

    #[test]
    fn an_agent_that_expects_a_matcher_is_given_one_and_the_others_are_not() {
        let expected = [
            (&DEVIN, None),
            (&DROID, None),
            (&QODERCLI, Some("*")),
            (&QWEN, Some("*")),
        ];

        for (installer, matcher) in expected {
            let (_home, env) = machine_with(installer);
            install(installer, &env);

            let document = settings_document(installer, &env);
            let entry = &ours(&document, SESSION_START)[0];
            assert_eq!(
                entry.get("matcher").and_then(Value::as_str),
                matcher,
                "{}",
                installer.agent
            );
        }
    }

    #[test]
    fn an_agent_that_is_not_on_the_machine_is_refused_and_nothing_is_written() {
        for installer in ALL {
            let (_home, env) = machine();

            let refusal = installer
                .plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"))
                .expect_err("a missing configuration directory has to stop the plan");

            let said = refusal.to_string();
            let home = installer.config_dir(&env);
            assert!(
                said.contains(&home.display().to_string()) && said.contains(installer.agent.name()),
                "{said:?} says neither where nor which agent",
            );
            assert!(
                !home.exists(),
                "{} had its home made for it",
                installer.agent
            );
        }
    }

    #[test]
    fn foreign_entries_survive_an_install_and_an_uninstall() {
        for installer in ALL {
            let (_home, env) = machine_with(installer);
            let theirs = format!(
                "{{\n  \"theirs\": true,\n  \"hooks\": {{\n    \"{SESSION_START}\": [\n      \
                 {{\n        \"hooks\": [\n          {{\n            \"type\": \"command\",\n     \
                 \"command\": \"their-own-script\"\n          }}\n        ]\n      }}\n    ]\n  \
                 }}\n}}\n"
            );
            fs::write(installer.settings(&env), &theirs).unwrap();

            let mut state = install(installer, &env);
            let document = settings_document(installer, &env);
            assert_eq!(document["theirs"], Value::from(true), "{}", installer.agent);
            assert_eq!(
                document[HOOKS_KEY][SESSION_START]
                    .as_array()
                    .expect("an array")
                    .len(),
                2,
                "{}",
                installer.agent
            );

            let changes = installer.plan_uninstall(&env, &state).expect("planning");
            apply(installer, &changes, &mut state);

            let after = settings_document(installer, &env);
            assert_eq!(after["theirs"], Value::from(true), "{}", installer.agent);
            assert!(
                ours(&after, SESSION_START).is_empty(),
                "{}",
                installer.agent
            );
            assert_eq!(
                after[HOOKS_KEY][SESSION_START][0]["hooks"][0]["command"],
                Value::from("their-own-script"),
                "{}",
                installer.agent
            );
        }
    }

    #[test]
    fn a_settings_file_that_cannot_be_rewritten_safely_is_refused() {
        for (installer, held) in [
            (&DEVIN, "{not json at all}"),
            (&DROID, "{\"hooks\": {}, \"hooks\": {}}"),
            (&QODERCLI, "{not json at all}"),
            (&QWEN, "{\"a\": 1, \"a\": 2}"),
        ] {
            let (_home, env) = machine_with(installer);
            fs::write(installer.settings(&env), held).unwrap();

            let refusal = installer
                .plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"))
                .expect_err("a file that cannot be rewritten has to stop the plan");

            assert!(
                matches!(refusal, Error::NotRewritable { .. }),
                "{}: {refusal:?}",
                installer.agent
            );
            assert_eq!(
                settings_text(installer, &env).as_deref(),
                Some(held),
                "{} had its settings changed anyway",
                installer.agent
            );
        }
    }

    #[test]
    fn a_wrapper_somebody_else_wrote_stops_the_plan() {
        for installer in ALL {
            let (_home, env) = machine_with(installer);
            let path = installer.wrapper(&env);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "# something a user wrote\n").unwrap();

            let refusal = installer
                .plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"))
                .expect_err("somebody else's file has to stop the plan");

            assert!(
                matches!(refusal, Error::NotOurs { .. }),
                "{}: {refusal:?}",
                installer.agent
            );
        }
    }

    #[test]
    fn installing_twice_changes_nothing_the_second_time() {
        for installer in ALL {
            let (_home, env) = machine_with(installer);
            let state = install(installer, &env);
            let before = settings_text(installer, &env);

            let again = installer
                .plan_install(&env, &state, Path::new("/opt/bin/agentbus"))
                .expect("planning failed");

            assert!(
                again
                    .iter()
                    .all(|change| matches!(change, Change::Keep { .. })),
                "{}: {again:?}",
                installer.agent
            );
            assert_eq!(
                settings_text(installer, &env),
                before,
                "{}",
                installer.agent
            );
        }
    }

    #[test]
    fn a_dry_run_says_exactly_what_a_real_one_would_do() {
        for installer in ALL {
            let (_home, env) = machine_with(installer);
            let described = plan(installer, &env);

            assert!(!installer.wrapper(&env).exists(), "{}", installer.agent);

            let carried_out = plan(installer, &env);
            assert_eq!(described, carried_out, "{}", installer.agent);

            let mut state = State::default();
            apply(installer, &carried_out, &mut state);
            assert!(installer.wrapper(&env).exists(), "{}", installer.agent);
        }
    }

    #[test]
    fn uninstalling_leaves_nothing_of_its_own_behind() {
        for installer in ALL {
            let (_home, env) = machine_with(installer);
            let mut state = install(installer, &env);

            let changes = installer.plan_uninstall(&env, &state).expect("planning");
            apply(installer, &changes, &mut state);

            assert!(!installer.wrapper(&env).exists(), "{}", installer.agent);
            assert!(
                !installer.settings(&env).exists(),
                "{} kept a settings file it created",
                installer.agent
            );
            assert!(
                !installer.hooks_dir(&env).exists(),
                "{} kept a directory it made",
                installer.agent
            );
            assert!(
                installer.config_dir(&env).is_dir(),
                "{} removed a directory that was never its own",
                installer.agent
            );
        }
    }

    #[test]
    fn a_directory_the_user_already_had_survives_an_uninstall() {
        for installer in ALL {
            let (_home, env) = machine_with(installer);
            fs::create_dir_all(installer.hooks_dir(&env)).unwrap();
            let mut state = install(installer, &env);

            let changes = installer.plan_uninstall(&env, &state).expect("planning");
            apply(installer, &changes, &mut state);

            assert!(
                installer.hooks_dir(&env).is_dir(),
                "{} removed a directory that was already there",
                installer.agent
            );
        }
    }

    #[test]
    fn what_is_installed_is_what_the_machine_is_asked_about() {
        for installer in ALL {
            let (_home, env) = machine_with(installer);
            assert_eq!(
                installer.status(&env).unwrap(),
                HookStatus::NotInstalled,
                "{}",
                installer.agent
            );

            let mut state = install(installer, &env);
            assert_eq!(
                installer.status(&env).unwrap(),
                HookStatus::Current(crate::version::expected_version(installer.agent)),
                "{}",
                installer.agent
            );

            // A settings file emptied of this program's entries leaves the
            // wrapper standing with nothing to run it.
            fs::write(installer.settings(&env), "{}\n").unwrap();
            assert_eq!(
                installer.status(&env).unwrap(),
                HookStatus::NeedsRepair(crate::version::expected_version(installer.agent)),
                "{}",
                installer.agent
            );

            let changes = installer.plan_uninstall(&env, &state).expect("planning");
            apply(installer, &changes, &mut state);
            assert_eq!(
                installer.status(&env).unwrap(),
                HookStatus::NotInstalled,
                "{}",
                installer.agent
            );
        }
    }

    #[test]
    fn an_installation_missing_one_of_its_events_needs_repairing() {
        let installer = &DEVIN;
        let (_home, env) = machine_with(installer);
        install(installer, &env);

        let mut document = settings_document(installer, &env);
        let dropped = installer.events.last().expect("six events");
        document[HOOKS_KEY]
            .as_object_mut()
            .expect("an object")
            .remove(*dropped);
        fs::write(
            installer.settings(&env),
            json::render(&document, json::DEFAULT_INDENT),
        )
        .unwrap();

        assert_eq!(
            installer.status(&env).unwrap(),
            HookStatus::NeedsRepair(crate::version::expected_version(installer.agent)),
        );
    }

    #[test]
    fn an_upgrade_replaces_what_an_earlier_build_wrote_rather_than_adding_to_it() {
        for installer in ALL {
            let (_home, env) = machine_with(installer);
            install(installer, &env);

            // What an older build would have left: the same entry, marked,
            // naming a wrapper somewhere this build no longer writes one.
            let mut document = settings_document(installer, &env);
            let mut stale = Map::new();
            stale.insert(
                "hooks".to_owned(),
                serde_json::json!([{"type": "command", "command": "/gone/agentbus.sh"}]),
            );
            sentinel::mark(&mut stale);
            document[HOOKS_KEY][SESSION_START]
                .as_array_mut()
                .expect("an array")
                .push(Value::Object(stale));
            fs::write(
                installer.settings(&env),
                json::render(&document, json::DEFAULT_INDENT),
            )
            .unwrap();

            let mut state = State::default();
            let changes = plan(installer, &env);
            apply(installer, &changes, &mut state);

            let after = settings_document(installer, &env);
            assert_eq!(
                ours(&after, SESSION_START).len(),
                1,
                "{} accumulated entries",
                installer.agent
            );
        }
    }
}
