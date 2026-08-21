//! One step of an installation, worked out before any of it is carried out.
//!
//! Everything an installer does is decided first and done second, so that
//! `--dry-run` and a real run take the same path through the same code and
//! cannot disagree about what a run would have done. A dry run is not a
//! description of the real one: it is the real one, stopped before the last
//! step.
//!
//! That means a step has to be able to stand for anything an installation does,
//! and installations do two quite different things. Most of it is files — write
//! this, remove that, leave the other alone. But an agent whose plugins are
//! registered by its own command line cannot be installed for by writing files
//! alone, and pretending otherwise would mean either editing something this
//! program has no business editing or leaving half the work undone. So running
//! somebody else's tool is a step like any other, planned alongside the writes
//! and reported alongside them.

use std::path::{Path, PathBuf};

use crate::agent::Agent;
use crate::command::Invocation;
use crate::state::{Ownership, State};
use crate::{Error, file};

/// Something that would happen as part of an installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// There is no directory to write a file into, and this would make one.
    ///
    /// Writing a file makes the directories above it anyway; this exists so that
    /// the making is *recorded*, which is the one thing an uninstall could not
    /// otherwise work out. A directory the user already had is theirs and stays,
    /// however empty this program leaves it.
    Make { path: PathBuf },
    /// There is no file, and this would write one.
    Create {
        path: PathBuf,
        contents: String,
        /// Whether the file is one the machine has to be able to run, which is
        /// the case for a script an agent is told to execute and for nothing
        /// else this program writes.
        executable: bool,
    },
    /// There is a file, and this would write it back with different contents.
    Rewrite {
        path: PathBuf,
        contents: String,
        /// Whether the file is one the machine has to be able to run.
        executable: bool,
    },
    /// There is a file this program created and no longer needs.
    Delete { path: PathBuf },
    /// The file is already as it should be.
    Keep { path: PathBuf },
    /// A directory this program made, with nothing of its left in it.
    Clear { path: PathBuf },
    /// The agent's own tool has to be run, because what it does is not
    /// something this program may do by writing a file.
    Run { command: Invocation },
    /// The agent's own tool already reports what running it would achieve.
    Ran { command: Invocation },
}

impl Change {
    /// The file or directory this is about, for the steps that are about one.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Make { path }
            | Self::Create { path, .. }
            | Self::Rewrite { path, .. }
            | Self::Delete { path }
            | Self::Keep { path }
            | Self::Clear { path } => Some(path),
            Self::Run { .. } | Self::Ran { .. } => None,
        }
    }

    /// The command this is about, for the steps that are about one.
    pub fn command(&self) -> Option<&Invocation> {
        match self {
            Self::Run { command } | Self::Ran { command } => Some(command),
            _ => None,
        }
    }

    /// Whether carrying this out would change anything.
    pub fn is_change(&self) -> bool {
        !matches!(self, Self::Keep { .. } | Self::Ran { .. })
    }

    /// Carries it out, recording what became of a file in `state`.
    ///
    /// A file that existed is copied first, whichever direction this is going
    /// in: an uninstall rewrites a file that holds the user's own entries too,
    /// and that is exactly as worth protecting as anything an install touches.
    pub fn apply(&self, agent: Agent, state: &mut State) -> Result<(), Error> {
        match self {
            Self::Make { path } => {
                std::fs::create_dir_all(path).map_err(|source| Error::Write {
                    path: path.to_owned(),
                    source,
                })?;
                state.record(path, agent, Ownership::Created);
            }
            Self::Create {
                path,
                contents,
                executable,
            } => {
                write(path, contents, *executable)?;
                state.record(path, agent, Ownership::Created);
            }
            Self::Rewrite {
                path,
                contents,
                executable,
            } => {
                file::back_up(path).map_err(|source| Error::Write {
                    path: path.to_owned(),
                    source,
                })?;
                write(path, contents, *executable)?;
                state.record(path, agent, Ownership::Merged);
            }
            Self::Delete { path } => {
                file::remove_with_backups(path).map_err(|source| Error::Write {
                    path: path.to_owned(),
                    source,
                })?;
                state.forget(path);
            }
            Self::Clear { path } => {
                file::remove_empty_dirs(path).map_err(|source| Error::Write {
                    path: path.to_owned(),
                    source,
                })?;
                // Only forgotten if it actually went. A directory left standing
                // because somebody else's file is in it is still this program's
                // to try again for.
                if !path.exists() {
                    state.forget(path);
                }
            }
            Self::Run { command } => command.run()?,
            Self::Keep { .. } | Self::Ran { .. } => {}
        }
        Ok(())
    }
}

/// Writes a file, naming it if that fails.
fn write(path: &Path, contents: &str, executable: bool) -> Result<(), Error> {
    let written = match executable {
        true => file::write_runnable(path, contents),
        false => file::write(path, contents),
    };
    written.map_err(|source| Error::Write {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn only_the_steps_that_do_nothing_are_no_steps_at_all() {
        let path = PathBuf::from("/home/u/.claude/plugin.json");
        let command = Invocation::new("/usr/bin/claude", ["plugin", "list"]);

        assert!(Change::Make { path: path.clone() }.is_change());
        assert!(
            Change::Create {
                path: path.clone(),
                contents: String::new(),
                executable: false,
            }
            .is_change()
        );
        assert!(Change::Clear { path: path.clone() }.is_change());
        assert!(
            Change::Run {
                command: command.clone()
            }
            .is_change()
        );
        assert!(!Change::Keep { path }.is_change());
        assert!(!Change::Ran { command }.is_change());
    }

    #[test]
    fn a_step_is_about_a_file_or_about_a_command_and_never_both() {
        let file = Change::Delete {
            path: PathBuf::from("/home/u/.claude/plugin.json"),
        };
        let ran = Change::Ran {
            command: Invocation::new("/usr/bin/claude", ["plugin", "list"]),
        };

        assert!(file.path().is_some() && file.command().is_none());
        assert!(ran.command().is_some() && ran.path().is_none());
    }

    #[test]
    fn a_directory_this_program_makes_is_remembered_as_its_own_and_forgotten_again() {
        let root = tempfile::tempdir().unwrap();
        let made = root.path().join("plugin");
        let mut state = State::default();

        Change::Make { path: made.clone() }
            .apply(Agent::OpenCode, &mut state)
            .unwrap();

        assert!(made.is_dir(), "the directory was not made");
        assert_eq!(state.ownership(&made), Some(Ownership::Created));

        Change::Clear { path: made.clone() }
            .apply(Agent::OpenCode, &mut state)
            .unwrap();

        assert!(!made.exists(), "the directory was left behind");
        assert_eq!(state.ownership(&made), None);
    }

    #[test]
    fn a_file_an_agent_has_to_run_lands_as_something_this_machine_would_run() {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("hooks/agentbus-hook.sh");
        let mut state = State::default();

        Change::Create {
            path: script.clone(),
            contents: String::from("exit 0\n"),
            executable: true,
        }
        .apply(Agent::Claude, &mut state)
        .unwrap();

        assert_eq!(mode(&script), 0o755);

        Change::Rewrite {
            path: script.clone(),
            contents: String::from("exit 0 # again\n"),
            executable: true,
        }
        .apply(Agent::Claude, &mut state)
        .unwrap();

        assert_eq!(
            mode(&script),
            0o755,
            "a rewrite of a script has to leave it runnable too"
        );
    }

    #[test]
    fn a_file_nobody_runs_is_left_as_something_nobody_runs() {
        let root = tempfile::tempdir().unwrap();
        let document = root.path().join("hooks.json");
        let mut state = State::default();

        Change::Create {
            path: document.clone(),
            contents: String::from("{}\n"),
            executable: false,
        }
        .apply(Agent::Codex, &mut state)
        .unwrap();

        assert_eq!(mode(&document) & 0o111, 0);
    }

    /// What a file's permissions are, as the number they are written as.
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn clearing_takes_the_directories_this_program_made_and_stops_at_anything_else() {
        let root = tempfile::tempdir().unwrap();
        let ours = root.path().join("marketplace");
        fs::create_dir_all(ours.join("plugin/hooks")).unwrap();
        let mut state = State::default();

        Change::Clear { path: ours.clone() }
            .apply(Agent::Claude, &mut state)
            .unwrap();

        assert!(!ours.exists(), "an emptied directory was left behind");
        assert!(
            root.path().exists(),
            "clearing went above what it was given"
        );
    }
}
