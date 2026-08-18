//! Detection of the coding agents installed on a machine and installation of
//! the hooks that make them emit events onto the bus. Each supported agent has
//! its own configuration surface — a plugin directory, a settings file, a
//! plugin script — so this crate keeps the per-agent knowledge in one place and
//! presents a uniform install/uninstall interface over it. Every operation is
//! idempotent and exactly reversible: installing twice changes nothing the
//! second time, and uninstalling leaves no trace behind.
//!
//! The rules that make that true are not per-agent, and they live here rather
//! than in each installer:
//!
//! - Prefer an agent's own drop-in mechanism to editing a file the user
//!   maintains by hand. Where there is no drop-in, merge into the file rather
//!   than replacing it, and refuse a file that cannot be rewritten without
//!   changing what it means.
//! - Mark every entry written, so that upgrading and uninstalling can find
//!   exactly this program's own work and nothing else. See [`sentinel`].
//! - Copy a file before changing it, and change it by renaming a complete new
//!   one over it. See [`file`].
//! - Work out the whole change before making any of it, so that a run refused
//!   by one agent's config file has not half-written another's. See [`merge`].
//! - Remember which files this program created, because that is the one thing
//!   an uninstall cannot work out from what is on disk. See [`state`].
//!
//! Everything an operation depends on about the machine arrives in an
//! [`Environment`], so that these rules can be tested against a described
//! machine instead of the one the tests happen to run on.

pub mod agent;
pub mod assets;
pub mod change;
pub mod claude;
pub mod command;
pub mod file;
pub mod json;
pub mod merge;
pub mod paths;
pub mod sentinel;
pub mod state;

use std::io;
use std::path::{Path, PathBuf};

pub use agent::{Agent, DetectedAgent, UnknownAgent, detect};
pub use change::Change;
pub use command::Invocation;
pub use merge::Placement;
pub use paths::Environment;
pub use state::State;

/// Why an installation could not be carried out.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// There is no home directory, so nothing can be located.
    #[error(
        "{} is not set, so there is no way to tell where anything is",
        paths::HOME_VAR
    )]
    NoHome,
    /// A file could not be read.
    #[error("cannot read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A file could not be written.
    #[error("cannot write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A file could be read but not safely written back.
    #[error("refusing to change {path}, which was left as it was")]
    NotRewritable {
        path: PathBuf,
        #[source]
        problem: json::Problem,
    },
    /// A file holds something other than what an entry has to go into.
    #[error("{path} holds something unexpected at {at}, where this needs {needed}")]
    Conflict {
        path: PathBuf,
        at: String,
        needed: &'static str,
    },
    /// The record of what has been installed could not be read or written.
    #[error("cannot use the record of what is installed at {path}: {reason}")]
    State { path: PathBuf, reason: String },
    /// This program cannot tell where its own binary is.
    #[error("cannot tell where this program's own binary is")]
    Binary {
        #[source]
        source: io::Error,
    },
    /// A path cannot be written into a configuration file at all.
    #[error("{path} cannot be written into a configuration file, because it is not text")]
    Unwritable { path: PathBuf },
    /// A command one of the agents provides could not be run.
    #[error("cannot run `{command}`; run it by hand to finish the job")]
    CannotRun {
        command: String,
        #[source]
        source: io::Error,
    },
    /// A command one of the agents provides refused to do what it was asked.
    #[error("`{command}` failed{}", match status {
        Some(code) => format!(" and exited {code}"),
        None => String::from(", killed by a signal"),
    })]
    CommandFailed {
        command: String,
        status: Option<i32>,
    },
}

/// Whether an operation is being carried out or only worked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Change the files.
    Apply,
    /// Change nothing, and answer with what would have changed.
    DryRun,
}

/// What an operation did, or would do, to one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The agent whose configuration this is about.
    pub agent: Agent,
    /// Every file involved, whether or not it changed.
    pub changes: Vec<Change>,
}

impl Outcome {
    /// Whether anything about this agent changed, or would.
    pub fn is_change(&self) -> bool {
        self.changes.iter().any(Change::is_change)
    }
}

/// What one agent's hooks are made of.
///
/// An installer says what it would do and lets the caller decide whether to do
/// it, so that `--dry-run` and a real run take the same path through the same
/// code and cannot disagree about what a run would have done.
pub trait Installer {
    /// The agent this installs for.
    fn agent(&self) -> Agent;

    /// What installing would do, with `binary` as the absolute path of the
    /// `agentbus` binary the hooks are to run.
    fn plan_install(&self, env: &Environment, binary: &Path) -> Result<Vec<Change>, Error>;

    /// What uninstalling would do.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error>;
}

/// Every agent this build can install for.
///
/// The per-agent installers are registered here and nowhere else, so that the
/// command line, the report and the uninstall all agree on the same list
/// without any of them naming an agent.
pub fn installers() -> Vec<&'static dyn Installer> {
    vec![&claude::Claude]
}

/// Every agent this build can install for.
pub fn supported() -> Vec<Agent> {
    installers()
        .iter()
        .map(|installer| installer.agent())
        .collect()
}

/// Installs the hooks for `agents`.
pub fn install(env: &Environment, agents: &[Agent], mode: Mode) -> Result<Vec<Outcome>, Error> {
    let chosen = chosen(agents);
    if chosen.is_empty() {
        return Ok(Vec::new());
    }
    let binary = binary()?;
    carry_out(env, mode, chosen, |installer, _| {
        installer.plan_install(env, &binary)
    })
}

/// Removes the hooks for `agents`.
pub fn uninstall(env: &Environment, agents: &[Agent], mode: Mode) -> Result<Vec<Outcome>, Error> {
    let chosen = chosen(agents);
    if chosen.is_empty() {
        return Ok(Vec::new());
    }
    let outcomes = carry_out(env, mode, chosen, |installer, state| {
        installer.plan_uninstall(env, state)
    })?;
    // The directory the installers generate into is this program's own, and one
    // left standing empty is a trace of something that is supposed to be gone.
    // Not reported as a change, because it is not one: an empty directory held
    // nothing, and removing it is the difference between having uninstalled and
    // looking like it. It goes only when it is empty, so an agent this run was
    // not asked about keeps everything of its own.
    if mode == Mode::Apply {
        let _ = file::remove_empty_dirs(env.data_dir());
    }
    Ok(outcomes)
}

/// Plans every agent's changes, then makes them unless this is a dry run.
///
/// The record of what has been installed is written once, at the end, and only
/// if something actually changed: a run that turns out to be a no-op leaves the
/// machine exactly as it found it, down to the modification time of this
/// program's own bookkeeping.
fn carry_out(
    env: &Environment,
    mode: Mode,
    installers: Vec<&'static dyn Installer>,
    plan: impl Fn(&dyn Installer, &State) -> Result<Vec<Change>, Error>,
) -> Result<Vec<Outcome>, Error> {
    let record = env.state_file();
    let mut state = State::load(&record)?;
    let mut outcomes = Vec::new();
    let mut changed = false;
    let mut stopped = None;
    for installer in installers {
        let changes = match plan(installer, &state) {
            Ok(changes) => changes,
            Err(error) => {
                stopped = Some(error);
                break;
            }
        };
        if mode == Mode::Apply {
            for change in &changes {
                if let Err(error) = change.apply(installer.agent(), &mut state) {
                    stopped = Some(error);
                    break;
                }
                changed |= change.is_change();
            }
        }
        outcomes.push(Outcome {
            agent: installer.agent(),
            changes,
        });
        if stopped.is_some() {
            break;
        }
    }
    // Written before anything is reported, including before a failure is
    // reported. A run that got as far as writing a file and then stopped has
    // changed the machine, and a record that forgot the part that succeeded
    // would leave the next uninstall unable to find it.
    if changed {
        state.save(&record)?;
    }
    match stopped {
        Some(error) => Err(error),
        None => Ok(outcomes),
    }
}

/// The installers for `agents`, in the order they are registered.
fn chosen(agents: &[Agent]) -> Vec<&'static dyn Installer> {
    installers()
        .into_iter()
        .filter(|installer| agents.contains(&installer.agent()))
        .collect()
}

/// Where this program's own binary is.
///
/// Hooks name it by this path rather than by the command `agentbus`, because the
/// directory a user installed it into is not guaranteed to be on the `PATH`
/// their coding agent runs hooks with — and a hook that cannot find its command
/// fails in the one place nobody is looking.
pub fn binary() -> Result<PathBuf, Error> {
    std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|source| Error::Binary { source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_installer_is_only_chosen_when_its_agent_is_asked_for() {
        let all: Vec<Agent> = installers().iter().map(|one| one.agent()).collect();

        assert_eq!(
            chosen(&all).len(),
            all.len(),
            "asking for every agent should choose every installer"
        );
        assert!(chosen(&[]).is_empty());
    }

    #[test]
    fn no_two_installers_claim_the_same_agent() {
        let mut agents: Vec<Agent> = installers().iter().map(|one| one.agent()).collect();
        let registered = agents.len();
        agents.sort_unstable();
        agents.dedup();

        assert_eq!(agents.len(), registered);
    }

    #[test]
    fn the_binary_is_named_by_an_absolute_path() {
        let binary = binary().unwrap();
        assert!(binary.is_absolute(), "{}", binary.display());
    }
}
