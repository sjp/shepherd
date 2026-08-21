//! Installing for Kimi.
//!
//! Two things go on the machine. A wrapper script is dropped into a `hooks`
//! directory inside Kimi's configuration directory, and a block of `[[hooks]]`
//! tables is written into the `config.toml` beside it telling Kimi to run the
//! wrapper. The wrapper is where the generation mark lives, so a machine can be
//! asked what it is carrying; the tables are what make the wrapper ever run.
//!
//! # A block between marker lines
//!
//! `config.toml` is the file a user keeps their own settings in, comments and
//! all, and there is nowhere in a TOML table to hang the key that marks this
//! program's entries in a JSON document. So the claim is made by position
//! instead: everything between the two marker comments is this program's to
//! replace and to remove, and every byte outside them is somebody else's and is
//! copied through untouched. The editor in [`crate::toml_text`] is what enforces
//! that, and it refuses a file it cannot read a line at a time rather than
//! guessing at one.
//!
//! # The table
//!
//! Twelve rows go in, and three of them carry a `matcher` — Kimi's own regular
//! expression, tested against the name of the tool an event is about. Two of
//! those three are the two halves of one event: the tool that puts a question to
//! the person at the keyboard, and every other tool. Together they are the whole
//! of `PreToolUse`, written as two rows because that is the shape of the surface
//! being registered. The distinction is one Kimi draws, and a table that erased
//! it would be this program deciding at install time that a distinction the
//! agent makes is not worth carrying.
//!
//! Every row runs the same wrapper, because what an event means is read from the
//! payload afterwards and never here.
//!
//! No pattern above is ever read by this program. They are Kimi configuration,
//! written into Kimi's file for Kimi to evaluate, and nothing on the path an
//! event takes from the wrapper to the bus matches anything: the mappings that
//! path is driven by are tables of names, by decision, and an expression
//! arriving out of somebody's configuration file is exactly what that decision
//! keeps away from it.
//!
//! # The version floor
//!
//! Kimi is the one agent here whose hooks arrived in a known release, and
//! installing into an older one writes files that are read by nothing. So the
//! agent is asked what it is while the plan is being worked out, and there are
//! three answers. Old enough and the plan is a refusal naming both versions,
//! before anything is written. New enough and nothing is said. And a question
//! that could not be answered at all — no command on the search path, a command
//! that failed, output nothing can be read out of — is not a refusal: it is a
//! remark carried through the plan, because a user whose agent this program
//! could not interrogate is better served by hooks and a sentence about them
//! than by neither.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::agent::Agent;
use crate::change::Change;
use crate::command::{self, Invocation};
use crate::paths::{Environment, Platform};
use crate::state::{Ownership, State};
use crate::status::HookStatus;
use crate::{Error, Installer, assets, file, sentinel, toml_text};

/// The directory inside Kimi's configuration that the wrapper is dropped into.
const HOOKS_DIR: &str = "hooks";

/// The file Kimi reads its settings, and its hooks, out of.
const CONFIG_FILE: &str = "config.toml";

/// How long Kimi is asked to allow the wrapper, in seconds.
///
/// Far longer than the wrapper can take, and there so that a machine which has
/// somehow made it slow cannot make somebody's session slow with it.
const TIMEOUT: u64 = 5;

/// What the wrapper says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// What the agent is asked in order to find out which release it is.
const VERSION_QUESTION: &str = "--version";

/// The oldest Kimi that runs hooks at all.
const MINIMUM: Version = Version {
    major: 0,
    minor: 14,
    patch: 0,
};

/// A pattern matching only the tool that puts a question to the person at the
/// keyboard.
const ASKING: &str = "^AskUserQuestion$";

/// A pattern matching every tool except that one.
const NOT_ASKING: &str = "^(?!AskUserQuestion$).*$";

/// Every event the wrapper is registered for, with the tool it is narrowed to
/// where it is narrowed to one.
///
/// The whole of what this installation asks Kimi for. What any of them means is
/// decided from the payload the wrapper forwards, so an event registered here
/// and not yet understood costs a run of the wrapper and nothing else — and one
/// that is understood later needs no second visit to anybody's machine.
const EVENTS: [(&str, Option<&str>); 12] = [
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    ("PreToolUse", Some(NOT_ASKING)),
    ("PreToolUse", Some(ASKING)),
    ("PostToolUse", Some(ASKING)),
    ("PostToolUseFailure", Some(ASKING)),
    ("SubagentStart", None),
    ("PreCompact", None),
    ("PermissionRequest", None),
    ("PermissionResult", None),
    ("Stop", None),
    ("Interrupt", None),
];

/// Kimi's hooks: a wrapper script, and a block of tables in the user's own
/// configuration file that runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kimi;

impl Installer for Kimi {
    fn agent(&self) -> Agent {
        Agent::Kimi
    }

    /// Asks the agent what it is, writes the wrapper, and then points the tables
    /// at it.
    ///
    /// In that order, outwards. The question comes first because its unhappy
    /// answer is a refusal, and a refusal is worth nothing once a file has been
    /// written. The tables come last because they are what makes the wrapper
    /// run, so a run interrupted in the middle leaves a file nothing calls
    /// rather than a call to a file that is not there.
    fn plan_install(
        &self,
        env: &Environment,
        _state: &State,
        binary: &Path,
    ) -> Result<Vec<Change>, Error> {
        let home = config_dir(env);
        // Never made here. A configuration directory that is not there means
        // this agent has never run on the machine, and an installer that made
        // one would be creating another program's home on the strength of a
        // guess about its layout.
        if !home.is_dir() {
            return Err(Error::Absent {
                agent: self.agent(),
                path: home,
            });
        }
        let mut changes = Vec::new();
        changes.extend(examine(env)?);
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

    /// Takes the tables out, and then the wrapper.
    ///
    /// The same order reversed, inwards: nothing is ever left pointing at
    /// something that is no longer there.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        let mut changes = vec![plan_config_removal(env, state)?];

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
        // already had is theirs however empty this leaves it, and it may hold
        // hooks of their own.
        let hooks = hooks_dir(env);
        if state.ownership(&hooks) == Some(Ownership::Created) {
            changes.push(Change::Clear { path: hooks });
        }
        Ok(changes)
    }

    /// Reads the wrapper, and then looks for the block that runs it.
    ///
    /// The wrapper is the file that says which generation this is, because it is
    /// the one whose content is the installation. A block that no longer says
    /// what this build would write — edited, half removed, left over from an
    /// installation whose wrapper has since moved — leaves a perfectly current
    /// file that nothing ever runs, which is the case worth telling somebody
    /// about.
    fn status(&self, env: &Environment) -> Result<HookStatus, Error> {
        let path = self.asset(env);
        let Some(text) = read(&path)? else {
            return Ok(HookStatus::NotInstalled);
        };
        if !sentinel::is_generated(&text) {
            return Ok(HookStatus::NotInstalled);
        }
        let status = HookStatus::of_text(self.agent(), &text);
        Ok(status.confirmed(is_registered(env)?))
    }

    /// The wrapper this program drops in, which is the file the mark is in.
    fn asset(&self, env: &Environment) -> PathBuf {
        wrapper(env)
    }
}

/// Where the agent keeps its configuration.
fn config_dir(env: &Environment) -> PathBuf {
    Agent::Kimi.config_dir(env)
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

/// The file the tables go in.
fn config(env: &Environment) -> PathBuf {
    config_dir(env).join(CONFIG_FILE)
}

/// A release of the agent, as the three numbers that order one against another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// The version `text` reports, if it reports one.
    ///
    /// What a command prints when asked what it is has no agreed shape: it may
    /// be the number alone, or the number after a name, with or without a `v`
    /// in front of it and with or without something after it saying which build
    /// it is. So every word is tried in turn and the first one that begins like
    /// a version is the answer, and anything trailing the third number is
    /// ignored rather than allowed to spoil it.
    fn read(text: &str) -> Option<Self> {
        text.split_whitespace().find_map(|word| {
            let mut numbers = word.trim_start_matches('v').splitn(3, '.');
            let major = numbers.next()?.parse().ok()?;
            let minor = numbers.next()?.parse().ok()?;
            let patch = numbers
                .next()
                .map(|rest| rest.chars().take_while(char::is_ascii_digit).collect())
                .map_or(Some(0), |digits: String| digits.parse().ok())?;
            Some(Self {
                major,
                minor,
                patch,
            })
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// What the agent on this machine says it is, if it can be asked at all.
///
/// The command is run rather than a file being read because the release is not
/// written down anywhere this program has any business looking. Nothing has
/// happened yet when this is asked, so a command that cannot be run or refuses
/// to answer is simply an unanswered question.
fn installed_version(env: &Environment) -> Option<Version> {
    let command = Agent::Kimi.command(env)?;
    Version::read(&Invocation::new(command, [VERSION_QUESTION]).ask()?)
}

/// What the agent's own version means for the plan.
///
/// Nothing at all where it is new enough, a refusal where it is not, and a
/// remark where the question went unanswered.
fn examine(env: &Environment) -> Result<Vec<Change>, Error> {
    let Some(found) = installed_version(env) else {
        return Ok(vec![Change::Note {
            message: format!(
                "could not confirm {}'s version; its hooks need {MINIMUM} or newer",
                Agent::Kimi
            ),
        }]);
    };
    match found < MINIMUM {
        true => Err(Error::TooOld {
            agent: Agent::Kimi,
            found: found.to_string(),
            needed: MINIMUM.to_string(),
        }),
        false => Ok(Vec::new()),
    }
}

/// What the wrapper should hold, with the binary written into it.
fn generated(platform: Platform, binary: &Path) -> Result<String, Error> {
    let named = match platform {
        Platform::Unix => command::in_shell(binary)?,
        Platform::Windows => command::in_powershell(binary)?,
    };
    Ok(assets::KIMI_WRAPPER
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

/// The tables that go between the marker lines, as the block would read.
fn tables(env: &Environment) -> Result<String, Error> {
    let command = command::hook_command(Agent::Kimi, env.platform(), &wrapper(env), None)?;
    let tables: Vec<String> = EVENTS
        .iter()
        .map(|(event, matcher)| table(event, *matcher, &command))
        .collect();
    // A blank line between one table and the next, because that is how a person
    // writes them and this block is read by people.
    Ok(tables.join("\n\n"))
}

/// One table: which event, which tools of that event, what to run, and how long
/// it may take.
fn table(event: &str, matcher: Option<&str>, command: &str) -> String {
    let mut lines = vec![
        "[[hooks]]".to_owned(),
        format!("event = {}", toml_text::string(event)),
    ];
    lines.extend(matcher.map(|matcher| format!("matcher = {}", toml_text::string(matcher))));
    lines.push(format!("command = {}", toml_text::string(command)));
    lines.push(format!("timeout = {TIMEOUT}"));
    lines.join("\n")
}

/// What writing the block would do, given whatever is in the file now.
///
/// A file that is not there is read as an empty one, so that writing the block
/// into nothing goes through the same editor as writing it into somebody's
/// settings — one spelling of the block, arrived at one way.
fn plan_config(env: &Environment) -> Result<Change, Error> {
    let path = config(env);
    let existing = read(&path)?;
    let before = existing.as_deref().unwrap_or_default();
    let contents = toml_text::set_block(before, &tables(env)?).map_err(problem(&path))?;
    Ok(match existing {
        None => Change::Create {
            path,
            contents,
            executable: false,
        },
        Some(text) if text == contents => Change::Keep { path },
        Some(_) => Change::Rewrite {
            path,
            contents,
            executable: false,
        },
    })
}

/// What taking the block back out would do.
///
/// A file this program created and has just emptied goes: an empty file this
/// program made is litter, and one it merely added a block to is the user's
/// however little of it is left.
fn plan_config_removal(env: &Environment, state: &State) -> Result<Change, Error> {
    let path = config(env);
    let Some(text) = read(&path)? else {
        return Ok(Change::Keep { path });
    };
    let contents = toml_text::remove_block(&text).map_err(problem(&path))?;
    let created = state.ownership(&path) == Some(Ownership::Created);
    Ok(match contents {
        _ if created && contents.trim().is_empty() => Change::Delete { path },
        ref same if *same == text => Change::Keep { path },
        contents => Change::Rewrite {
            path,
            contents,
            executable: false,
        },
    })
}

/// Whether the file on this machine runs the wrapper this program would install
/// now.
///
/// A file that cannot be read a line at a time counts as running nothing. It
/// may well be Kimi's to understand, but it is not one an install could add to
/// either, so an installation resting on it is one that needs a person.
fn is_registered(env: &Environment) -> Result<bool, Error> {
    let Some(text) = read(&config(env))? else {
        return Ok(false);
    };
    let Ok(Some(block)) = toml_text::block(&text) else {
        return Ok(false);
    };
    Ok(block == tables(env)?)
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
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::assets::tests::is_well_formed;

    /// The agent this module installs for.
    const AGENT: Agent = Agent::Kimi;

    /// Where this program's own binary is, as far as these tests are concerned.
    const BINARY: &str = "/opt/bin/agentbus";

    /// A machine with a home directory that really exists and holds nothing, and
    /// a search path with nothing on it.
    fn machine() -> (tempfile::TempDir, Environment) {
        let root = tempfile::tempdir().expect("cannot make a temporary directory");
        let home = root.path().join("home");
        let bin = root.path().join("bin");
        fs::create_dir_all(&home).expect("cannot make the home directory");
        fs::create_dir_all(&bin).expect("cannot make the search path");
        let env = Environment::rooted(home)
            .with_path([bin])
            .with_platform(Platform::Unix);
        (root, env)
    }

    /// The same machine, with the agent's configuration directory on it.
    fn machine_with_agent() -> (tempfile::TempDir, Environment) {
        let (root, env) = machine();
        fs::create_dir_all(config_dir(&env)).expect("cannot make the agent's directory");
        (root, env)
    }

    /// The same machine again, with an agent on its search path that says it is
    /// new enough.
    fn machine_with_current_agent() -> (tempfile::TempDir, Environment) {
        let (root, env) = machine_with_agent();
        answering(&root, &format!("kimi-code {MINIMUM}"));
        (root, env)
    }

    /// Puts a command called `kimi` on the machine's search path that prints
    /// `answer` and reports success.
    fn answering(root: &tempfile::TempDir, answer: &str) {
        speaking(root, &format!("printf '%s\\n' '{answer}'\nexit 0\n"));
    }

    /// Puts a command called `kimi` on the machine's search path whose whole
    /// body is `body`.
    fn speaking(root: &tempfile::TempDir, body: &str) {
        let path = root.path().join("bin").join("kimi");
        fs::write(&path, format!("#!/bin/sh\n{body}")).expect("cannot write the agent's command");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("cannot make the agent's command runnable");
    }

    /// What installing on `env` would do, with nothing installed before.
    fn plan(env: &Environment) -> Vec<Change> {
        Kimi.plan_install(env, &State::default(), Path::new(BINARY))
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
        let changes = Kimi.plan_uninstall(env, state).expect("planning failed");
        apply(&changes, state);
    }

    /// The configuration file as it stands.
    fn config_text(env: &Environment) -> Option<String> {
        fs::read_to_string(config(env)).ok()
    }

    /// What is between the marker lines of the configuration file as it stands.
    fn installed_block(env: &Environment) -> Option<String> {
        toml_text::block(&config_text(env)?).expect("the file cannot be read")
    }

    #[test]
    fn the_wrapper_obeys_the_rules_every_wrapper_obeys() {
        is_well_formed(AGENT, &assets::KIMI_WRAPPER);
    }

    #[test]
    fn a_fresh_install_writes_the_wrapper_and_a_block_of_tables_that_runs_it() {
        let (_root, env) = machine_with_current_agent();

        install(&env);

        let written = fs::read_to_string(wrapper(&env)).expect("no wrapper");
        assert!(sentinel::is_generated(&written));
        assert!(
            written.contains(BINARY),
            "the wrapper does not name the binary"
        );

        let block = installed_block(&env).expect("no block");
        assert_eq!(
            block.matches("[[hooks]]").count(),
            EVENTS.len(),
            "one table per event, and no others:\n{block}"
        );
        for (event, matcher) in EVENTS {
            assert!(
                block.contains(&format!("event = \"{event}\"")),
                "{event} is not registered",
            );
            if let Some(matcher) = matcher {
                assert!(
                    block.contains(&format!("matcher = {}", toml_text::string(matcher))),
                    "{matcher} is not written the way the file writes it",
                );
            }
        }
        assert_eq!(
            block.matches("command = ").count(),
            EVENTS.len(),
            "every table runs something",
        );
        assert!(
            block.contains(&format!("command = \"bash '{}'\"", wrapper(&env).display())),
            "the tables do not run the wrapper:\n{block}",
        );
    }

    #[test]
    fn the_two_halves_of_the_split_event_are_both_registered() {
        let (_root, env) = machine_with_current_agent();

        install(&env);

        let block = installed_block(&env).expect("no block");
        assert_eq!(
            block.matches("event = \"PreToolUse\"").count(),
            2,
            "the event narrowed by tool is not registered as both of its halves",
        );
        assert!(block.contains(&toml_text::string(ASKING)));
        assert!(block.contains(&toml_text::string(NOT_ASKING)));
    }

    #[test]
    fn everything_outside_the_markers_is_given_back_byte_for_byte() {
        let (_root, env) = machine_with_current_agent();
        // Comments, blank lines, a table of their own and a trailing setting:
        // everything an editor working by line could quietly reflow.
        let theirs =
            "# my settings\n\n[model]\nname = \"kimi\"   # the good one\n\n\napproval = 3\n";
        fs::write(config(&env), theirs).unwrap();

        let mut state = install(&env);

        let after = config_text(&env).expect("no configuration file");
        assert!(
            after.starts_with(theirs),
            "the user's own bytes did not survive:\n{after}",
        );
        assert!(installed_block(&env).is_some(), "no block was written");

        uninstall(&env, &mut state);
        assert_eq!(
            config_text(&env).as_deref(),
            Some(theirs),
            "uninstalling did not give back the bytes the install was handed",
        );
    }

    #[test]
    fn a_block_left_by_an_earlier_build_is_replaced_rather_than_joined() {
        let (_root, env) = machine_with_current_agent();
        let stale = format!(
            "[user]\nname = \"u\"\n\n{}\n[[hooks]]\nevent = \"Stop\"\ncommand = \"something-else\"\n{}\n",
            toml_text::BLOCK_BEGIN,
            toml_text::BLOCK_END,
        );
        fs::write(config(&env), &stale).unwrap();

        install(&env);

        let after = config_text(&env).expect("no configuration file");
        assert_eq!(
            after.matches(toml_text::BLOCK_BEGIN).count(),
            1,
            "a second block was added beside the first:\n{after}",
        );
        assert!(
            !after.contains("something-else"),
            "the stale block survived:\n{after}",
        );
        assert!(
            after.contains("[user]"),
            "the user's own settings went with it:\n{after}",
        );
    }

    #[test]
    fn an_agent_that_is_not_on_the_machine_is_refused_and_nothing_is_written() {
        let (_root, env) = machine();

        let refusal = Kimi
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
    fn a_wrapper_somebody_else_wrote_stops_the_plan() {
        let (_root, env) = machine_with_current_agent();
        fs::create_dir_all(hooks_dir(&env)).unwrap();
        fs::write(wrapper(&env), "# something a user wrote\n").unwrap();

        let refusal = Kimi
            .plan_install(&env, &State::default(), Path::new(BINARY))
            .expect_err("somebody else's file has to stop the plan");

        assert!(matches!(refusal, Error::NotOurs { .. }), "{refusal:?}");
    }

    #[test]
    fn a_configuration_file_that_cannot_be_read_a_line_at_a_time_is_refused() {
        let (_root, env) = machine_with_current_agent();
        // A marker line of this program's own, inside a string, where a
        // line-by-line editor cannot say what it means.
        fs::write(
            config(&env),
            format!("banner = \"\"\"\n{}\n\"\"\"\n", toml_text::BLOCK_BEGIN),
        )
        .unwrap();

        let refusal = Kimi
            .plan_install(&env, &State::default(), Path::new(BINARY))
            .expect_err("a file that cannot be read has to stop the plan");

        assert!(matches!(refusal, Error::NotEditable { .. }), "{refusal:?}");
        assert!(!wrapper(&env).exists(), "the wrapper was written anyway");
    }

    #[test]
    fn installing_twice_changes_nothing_the_second_time() {
        let (_root, env) = machine_with_current_agent();
        let state = install(&env);
        let before = config_text(&env);

        let again = Kimi
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
    fn a_dry_run_says_exactly_what_a_real_one_would_do() {
        let (_root, env) = machine_with_current_agent();
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
        let (_root, env) = machine_with_current_agent();
        let mut state = install(&env);

        uninstall(&env, &mut state);

        assert!(!wrapper(&env).exists());
        assert!(
            !config(&env).exists(),
            "a configuration file this program made was left standing empty",
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
    fn an_agent_reporting_a_release_older_than_the_hooks_stops_the_plan() {
        let (root, env) = machine_with_agent();
        answering(&root, "kimi-code 0.13.9");

        let refusal = Kimi
            .plan_install(&env, &State::default(), Path::new(BINARY))
            .expect_err("an agent too old for its hooks has to stop the plan");

        let said = refusal.to_string();
        assert!(
            said.contains("0.13.9") && said.contains(&MINIMUM.to_string()),
            "{said:?} does not name both versions",
        );
        assert!(!wrapper(&env).exists(), "the wrapper was written anyway");
        assert!(!config(&env).exists(), "the tables were written anyway");
    }

    #[test]
    fn an_agent_new_enough_for_its_hooks_is_installed_for_without_remark() {
        let (root, env) = machine_with_agent();
        answering(&root, "kimi-code 1.2.3");

        let changes = plan(&env);

        assert!(
            !changes
                .iter()
                .any(|change| matches!(change, Change::Note { .. })),
            "{changes:?}",
        );
    }

    #[test]
    fn an_agent_that_cannot_be_asked_is_installed_for_and_said_so() {
        // Nothing on the search path at all, a command that refuses to answer,
        // and one that answers with something no version can be read out of.
        let unanswerable: [Option<&str>; 3] = [None, Some("exit 3\n"), Some("echo nothing\n")];

        for body in unanswerable {
            let (root, env) = machine_with_agent();
            if let Some(body) = body {
                speaking(&root, body);
            }

            let changes = plan(&env);

            let Some(Change::Note { message }) = changes
                .iter()
                .find(|change| matches!(change, Change::Note { .. }))
            else {
                panic!("{body:?} left the plan with nothing to say: {changes:?}");
            };
            assert!(
                message.contains(AGENT.name()) && message.contains(&MINIMUM.to_string()),
                "{message:?} says neither which agent nor what it needs",
            );

            let mut state = State::default();
            apply(&changes, &mut state);
            assert!(wrapper(&env).exists(), "{body:?} stopped the installation");
            assert!(installed_block(&env).is_some(), "{body:?} wrote no tables");
        }
    }

    #[test]
    fn what_is_installed_is_what_the_machine_is_asked_about() {
        let (_root, env) = machine_with_current_agent();
        assert_eq!(Kimi.status(&env).unwrap(), HookStatus::NotInstalled);

        let mut state = install(&env);
        let expected = crate::version::expected_version(AGENT);
        assert_eq!(Kimi.status(&env).unwrap(), HookStatus::Current(expected));

        // A block edited by hand leaves the wrapper standing with nothing to
        // run it, and installing again puts it back.
        let text = config_text(&env).expect("no configuration file");
        fs::write(config(&env), text.replace("\"Stop\"", "\"NeverHappens\"")).unwrap();
        assert_eq!(
            Kimi.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected)
        );
        apply(&plan(&env), &mut state);
        assert_eq!(Kimi.status(&env).unwrap(), HookStatus::Current(expected));

        // One with no block at all is no more of a registration than one saying
        // the wrong thing.
        fs::write(config(&env), "[model]\nname = \"kimi\"\n").unwrap();
        assert_eq!(
            Kimi.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected)
        );

        apply(&plan(&env), &mut state);
        uninstall(&env, &mut state);
        assert_eq!(Kimi.status(&env).unwrap(), HookStatus::NotInstalled);
    }

    #[test]
    fn a_wrapper_moved_since_the_tables_were_written_is_an_installation_that_needs_repairing() {
        let (_root, env) = machine_with_current_agent();
        install(&env);
        let text = config_text(&env).expect("no configuration file");
        fs::write(
            config(&env),
            text.replace(&wrapper(&env).display().to_string(), "/gone/agentbus.sh"),
        )
        .unwrap();

        assert_eq!(
            Kimi.status(&env).unwrap(),
            HookStatus::NeedsRepair(crate::version::expected_version(AGENT)),
        );
    }

    #[test]
    fn a_version_is_read_out_of_whatever_shape_a_command_answers_in() {
        let read = |text: &str| Version::read(text).map(|version| version.to_string());

        assert_eq!(read("0.14.0"), Some("0.14.0".to_owned()));
        assert_eq!(read("kimi-code 1.2.3\n"), Some("1.2.3".to_owned()));
        assert_eq!(read("v0.15.1"), Some("0.15.1".to_owned()));
        assert_eq!(read("2.0.0-rc.1"), Some("2.0.0".to_owned()));
        assert_eq!(read("0.14"), Some("0.14.0".to_owned()));
        assert_eq!(read("kimi code version 3.4.5"), Some("3.4.5".to_owned()));
    }

    #[test]
    fn an_answer_holding_no_version_is_no_answer() {
        assert_eq!(Version::read(""), None);
        assert_eq!(Version::read("kimi-code"), None);
        assert_eq!(Version::read("unknown"), None);
        assert_eq!(Version::read("14"), None);
    }

    #[test]
    fn versions_are_ordered_by_each_number_in_turn() {
        let version = |major, minor, patch| Version {
            major,
            minor,
            patch,
        };

        assert!(version(0, 13, 9) < MINIMUM);
        assert!(version(0, 14, 0) == MINIMUM);
        assert!(version(0, 14, 1) > MINIMUM);
        assert!(version(1, 0, 0) > MINIMUM);
    }
}
