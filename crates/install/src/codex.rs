//! Installing for Codex CLI.
//!
//! Codex reads its hooks from `hooks.json` in its configuration directory, a
//! file separate from the `config.toml` a user keeps their settings in. That
//! separation is what makes this the simplest installation of the three: on a
//! machine where the file does not exist yet there is nothing to merge with and
//! nothing that can be damaged, and on one where it does, the entries added are
//! marked so that upgrading and uninstalling can find exactly this program's own
//! work again.
//!
//! Codex documents a `[hooks]` table in `config.toml` as an alternative. It is
//! deliberately not used: that file is where a user's own settings live, hand-
//! edited and in a format whose comments no rewriter preserves, and there is a
//! drop-in next to it that costs them nothing.
//!
//! The entries name this program's binary by an absolute path rather than by the
//! command `agentbus`, for the reason every agent's installation does: the
//! directory a user installed it into is not guaranteed to be on the `PATH`
//! their agent runs hooks with, and a hook that cannot find its command fails
//! silently, in the one place nobody is looking.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::change::Change;
use crate::paths::Environment;
use crate::state::State;
use crate::{Error, Installer, Placement, merge};

/// The file Codex reads hooks from, inside its configuration directory.
const HOOKS_FILE: &str = "hooks.json";

/// The key in that file that everything below is hung from.
const HOOKS_KEY: &str = "hooks";

/// The events this program asks Codex for, which are exactly the ones the
/// adapter has something to say about.
///
/// Asking for more would run this program's binary on every occurrence of an
/// event whose payload it would then throw away — a cost to somebody's session
/// in exchange for nothing.
const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "Stop",
    "SessionEnd",
];

/// The events whose entries are selected by a matcher, and so need one that
/// selects everything.
///
/// These are the events about a particular tool call. An omitted matcher is
/// documented as matching everything, but saying so is free and leaves nothing
/// to a default that could change.
const MATCHED: &[&str] = &["PreToolUse", "PermissionRequest", "PostToolUse"];

/// The matcher that means "every tool".
const EVERYTHING: &str = "*";

/// The subcommand of this program's own binary that a hook runs.
///
/// Written out here because this crate installs that binary without linking
/// against it; the two agree by this one word, and a change to it is a change
/// to every installation already on a machine.
const EMIT: &str = "emit";

/// How long Codex waits for one of these hooks before giving up on it.
///
/// Far longer than the client's own budget, which is what actually bounds the
/// work. This is the backstop for the case the budget cannot cover — a machine
/// so loaded that the process does not get to run its own deadline — and it is
/// small enough that hitting it is still invisible to somebody typing.
const TIMEOUT: u64 = 5;

/// Codex CLI's hooks, as entries in its own drop-in file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codex;

impl Installer for Codex {
    fn agent(&self) -> Agent {
        Agent::Codex
    }

    fn plan_install(&self, env: &Environment, binary: &Path) -> Result<Vec<Change>, Error> {
        let command = command(binary)?;
        let placements: Vec<Placement> = EVENTS
            .iter()
            .map(|event| placement(event, &command))
            .collect();
        Ok(vec![merge::plan_install(&hooks(env), &placements)?])
    }

    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        Ok(vec![merge::plan_uninstall(&hooks(env), state)?])
    }
}

/// The file the entries go in.
fn hooks(env: &Environment) -> PathBuf {
    Agent::Codex.config_dir(env.home()).join(HOOKS_FILE)
}

/// What one event's entry is, and where it belongs.
fn placement(event: &str, command: &str) -> Placement {
    let mut hook = Map::new();
    hook.insert("type".to_owned(), Value::from("command"));
    hook.insert("command".to_owned(), Value::from(command));
    // Nothing the agent does may wait on this. The client is quick and bounds
    // itself, but an agent that waits for it at all has been slowed down by
    // something installed behind its user's back.
    hook.insert("async".to_owned(), Value::Bool(true));
    hook.insert("timeout".to_owned(), Value::from(TIMEOUT));

    let mut entry = Map::new();
    if MATCHED.contains(&event) {
        entry.insert("matcher".to_owned(), Value::from(EVERYTHING));
    }
    entry.insert("hooks".to_owned(), Value::Array(vec![Value::Object(hook)]));

    Placement::new([HOOKS_KEY, event], entry)
}

/// The command line a hook runs.
///
/// A path is bytes and this has to become a JSON string, so a path that is not
/// text cannot be turned into one. That is refused rather than approximated: the
/// lossy spelling of it would parse, install, and produce hooks that run a
/// command which is not there — a failure that shows up as an agent quietly
/// emitting nothing, which is the hardest kind to attribute to its cause.
fn command(binary: &Path) -> Result<String, Error> {
    let path = binary.to_str().ok_or_else(|| Error::Unwritable {
        path: binary.to_owned(),
    })?;
    Ok(format!("{path} {EMIT} --agent {}", Agent::Codex))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sentinel;

    /// What installing would write to a machine that has no such file yet.
    ///
    /// Planned against a home directory that really exists but really has no
    /// `hooks.json` in it, so that this is the drop-in case rather than a merge
    /// with whatever the machine running the tests happens to have.
    fn written(binary: &str) -> Value {
        let home = tempfile::tempdir().expect("cannot make a temporary directory");
        let env = Environment::rooted(home.path());
        let changes = Codex
            .plan_install(&env, Path::new(binary))
            .expect("planning failed");
        let [Change::Create { path, contents }] = changes.as_slice() else {
            panic!("a file that is not there should be created: {changes:?}");
        };
        assert_eq!(path, &hooks(&env));
        serde_json::from_str(contents).expect("what would be written is not JSON")
    }

    /// The one entry registered for `event`.
    fn entry<'a>(document: &'a Value, event: &str) -> &'a Value {
        let entries = document[HOOKS_KEY][event]
            .as_array()
            .unwrap_or_else(|| panic!("nothing is registered for {event}"));
        match entries.as_slice() {
            [entry] => entry,
            many => panic!("{event} has {} entries", many.len()),
        }
    }

    #[test]
    fn the_entries_go_in_the_agents_own_drop_in_file() {
        assert_eq!(
            hooks(&Environment::rooted("/home/u")),
            PathBuf::from("/home/u/.codex/hooks.json")
        );
    }

    #[test]
    fn there_is_one_entry_for_every_event_the_adapter_reads_and_no_other() {
        let document = written("/opt/bin/agentbus");

        let registered: Vec<&String> = document[HOOKS_KEY]
            .as_object()
            .expect("nothing was registered")
            .keys()
            .collect();
        assert_eq!(registered, EVENTS.to_vec());
    }

    #[test]
    fn every_hook_runs_the_binary_by_the_path_it_was_given_and_is_never_waited_on() {
        let document = written("/opt/bin/agentbus");

        for event in EVENTS {
            let hooks = entry(&document, event)["hooks"]
                .as_array()
                .unwrap_or_else(|| panic!("{event} registered no command"));
            assert_eq!(hooks.len(), 1, "{event}");
            assert_eq!(hooks[0]["type"], Value::from("command"), "{event}");
            assert_eq!(
                hooks[0]["command"],
                Value::from("/opt/bin/agentbus emit --agent codex"),
                "{event}"
            );
            assert_eq!(hooks[0]["async"], Value::Bool(true), "{event}");
            assert_eq!(hooks[0]["timeout"], Value::from(TIMEOUT), "{event}");
        }
    }

    #[test]
    fn only_the_events_about_a_tool_call_are_selected_by_a_matcher() {
        let document = written("/opt/bin/agentbus");

        for event in EVENTS {
            let matcher = entry(&document, event).get("matcher");
            match MATCHED.contains(event) {
                true => assert_eq!(matcher, Some(&Value::from(EVERYTHING)), "{event}"),
                false => assert_eq!(matcher, None, "{event}"),
            }
        }
    }

    #[test]
    fn every_entry_carries_the_mark_that_makes_it_findable_again() {
        let document = written("/opt/bin/agentbus");

        for event in EVENTS {
            assert!(sentinel::is_marked(entry(&document, event)), "{event}");
        }
    }

    #[test]
    fn a_path_with_something_to_escape_survives_being_put_in_json() {
        let document = written("/opt/a \"b\"/agentbus");

        assert_eq!(
            entry(&document, "Stop")["hooks"][0]["command"],
            Value::from("/opt/a \"b\"/agentbus emit --agent codex")
        );
    }

    #[test]
    fn a_path_that_is_not_text_is_refused_rather_than_approximated() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let binary = PathBuf::from(OsStr::from_bytes(b"/opt/\xff/agentbus"));

        assert!(matches!(command(&binary), Err(Error::Unwritable { .. })));
        assert!(matches!(
            Codex.plan_install(&Environment::rooted("/home/u"), &binary),
            Err(Error::Unwritable { .. })
        ));
    }
}
