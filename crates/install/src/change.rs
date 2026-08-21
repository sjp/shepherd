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
//!
//! Two of the steps change nothing on the machine at all. Where an
//! installation depends on a setting in somebody else's file being switched on,
//! whether this program was the one to switch it on is a fact that exists only
//! while the installation is being worked out — the line it wrote is
//! indistinguishable from the line a user wrote — and the step that carries it
//! is what puts it into the record before it is lost. And where working the
//! plan out turned up something the user has to be told, the telling is a step
//! too: a plan is reported step by step, so a remark that travelled any other
//! way would either be printed out of order with the work it is about or not
//! printed at all on the dry run, which is the run it matters most on.

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
    /// Who a setting in somebody else's file belongs to.
    ///
    /// Not a change to anything: the file it names is written by the step
    /// beside this one, or was already right. What this carries is the one fact
    /// about such a setting that cannot be read back off the disk afterwards —
    /// whether this program was the one that switched it on — because a line
    /// switching something on looks the same whoever wrote it, and an uninstall
    /// that guessed would either leave it on for ever or switch off something
    /// the user switched on themselves.
    Setting {
        path: PathBuf,
        /// The setting, named the way the file names it.
        setting: String,
        /// Whether it is this program's to switch off again.
        ours: bool,
    },
    /// Something the plan turned up that the user has to be told, and that no
    /// other step will say for itself.
    ///
    /// Not a change to anything, and not a failure either: it is what an
    /// installer says when it went ahead without being able to check something
    /// it would rather have checked. A plan that could neither refuse nor
    /// reassure has to hand that on, because the alternative is an
    /// installation that looks exactly like one nothing was in doubt about.
    Note {
        /// What to tell them, as a sentence they will read among the files.
        message: String,
    },
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
            // The setting is one line inside a file another step is already
            // about, and a report that named the file twice would be saying
            // that two things had happened to it. A remark is about the plan
            // rather than about any one file in it.
            Self::Run { .. } | Self::Ran { .. } | Self::Setting { .. } | Self::Note { .. } => None,
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
        !matches!(
            self,
            Self::Keep { .. } | Self::Ran { .. } | Self::Setting { .. } | Self::Note { .. }
        )
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
            Self::Setting {
                path,
                setting,
                ours,
            } => match ours {
                true => state.claim(path, setting),
                false => state.release(path, setting),
            },
            Self::Keep { .. } | Self::Ran { .. } | Self::Note { .. } => {}
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
    fn a_setting_is_remembered_as_this_programs_and_given_back_again() {
        let path = PathBuf::from("/home/u/.codex/config.toml");
        let mut state = State::default();
        let claim = |ours| Change::Setting {
            path: path.clone(),
            setting: "features.hooks".to_owned(),
            ours,
        };

        claim(true).apply(Agent::Codex, &mut state).unwrap();
        assert!(state.claimed(&path, "features.hooks"));

        claim(false).apply(Agent::Codex, &mut state).unwrap();
        assert!(!state.claimed(&path, "features.hooks"));
    }

    #[test]
    fn remembering_who_a_setting_belongs_to_is_not_a_change_to_anything() {
        let step = Change::Setting {
            path: PathBuf::from("/home/u/.codex/config.toml"),
            setting: "features.hooks".to_owned(),
            ours: true,
        };

        assert!(!step.is_change());
        assert_eq!(step.path(), None, "the file has a step of its own");
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
