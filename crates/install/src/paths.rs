//! The parts of a machine that installation depends on.
//!
//! Where a coding agent keeps its configuration, and which commands can be run
//! by name, are questions about one particular machine — normally the one this
//! process is running on, but not necessarily, and not at all while these rules
//! are being tested. So the answers are carried in an [`Environment`] that is
//! built from the process's environment in exactly one place, and passed down
//! from there. A test describes a machine instead of having to be run on one,
//! and nothing below this module reads a variable.

use std::env;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::Error;

/// The variable naming the user's home directory.
pub const HOME_VAR: &str = "HOME";

/// The variable holding the directories commands are looked up in.
pub const PATH_VAR: &str = "PATH";

/// The variable naming the base directory for state that should survive a
/// reboot but is not configuration.
pub const STATE_HOME_VAR: &str = "XDG_STATE_HOME";

/// The variable naming the base directory for a user's own copies of things a
/// program ships.
pub const DATA_HOME_VAR: &str = "XDG_DATA_HOME";

/// The name of this program's own directory, inside whichever base directory it
/// sits under.
const DIR_NAME: &str = "agentbus";

/// The file recording what has been installed, inside the state directory.
const STATE_FILE_NAME: &str = "installed.json";

/// Where the state directory sits when `XDG_STATE_HOME` does not say.
const DEFAULT_STATE_HOME: &str = ".local/state";

/// Where the data directory sits when `XDG_DATA_HOME` does not say.
const DEFAULT_DATA_HOME: &str = ".local/share";

/// One machine, as far as installing hooks is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    home: PathBuf,
    path: Vec<PathBuf>,
    state_dir: PathBuf,
    data_dir: PathBuf,
}

impl Environment {
    /// Reads the machine this process is running on.
    ///
    /// A home directory is the one thing there is no sensible fallback for:
    /// every path an agent's configuration lives at is relative to it, and
    /// guessing would mean writing hooks into somebody else's dot-files.
    pub fn from_env() -> Result<Self, Error> {
        let home = env::var_os(HOME_VAR)
            .filter(|home| !home.is_empty())
            .ok_or(Error::NoHome)?;
        let home = PathBuf::from(home);
        let state_dir = below(&home, env::var_os(STATE_HOME_VAR), DEFAULT_STATE_HOME);
        let data_dir = below(&home, env::var_os(DATA_HOME_VAR), DEFAULT_DATA_HOME);
        Ok(Self {
            path: search_path(env::var_os(PATH_VAR)),
            home,
            state_dir,
            data_dir,
        })
    }

    /// A machine whose home directory is `home`, with nothing on its `PATH`.
    pub fn rooted(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            state_dir: below(&home, None, DEFAULT_STATE_HOME),
            data_dir: below(&home, None, DEFAULT_DATA_HOME),
            path: Vec::new(),
            home,
        }
    }

    /// The same machine, with `dirs` as the directories commands are looked up
    /// in.
    #[must_use]
    pub fn with_path(mut self, dirs: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        self.path = dirs.into_iter().map(Into::into).collect();
        self
    }

    /// The user's home directory.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The file recording which paths this program has written and which of
    /// them it created.
    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join(STATE_FILE_NAME)
    }

    /// Where this program keeps the files it generates for the agents to read.
    ///
    /// Separate from the state directory because these are not bookkeeping:
    /// they are the installation itself, they are read by another program long
    /// after the installer has exited, and a user looking for what was put on
    /// their machine should find them where a user's own copy of a program's
    /// data belongs.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Where `command` would be found by running it by name, if anywhere.
    ///
    /// The executable bit is part of the question: a directory on the `PATH`
    /// holding a file of the right name that nobody can run says nothing about
    /// whether the agent is installed.
    pub fn look_up(&self, command: &str) -> Option<PathBuf> {
        self.path.iter().find_map(|dir| {
            let candidate = dir.join(command);
            let executable = candidate
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
            executable.then_some(candidate)
        })
    }
}

/// One of this program's own directories, under the base directory the user
/// named if they named one and under the home directory otherwise.
fn below(home: &Path, base: Option<OsString>, default: &str) -> PathBuf {
    match base.filter(|value| !value.is_empty()) {
        Some(base) => PathBuf::from(base).join(DIR_NAME),
        None => home.join(default).join(DIR_NAME),
    }
}

/// Splits a `PATH` into the directories it names.
///
/// An empty entry means the working directory by long convention, and is
/// dropped rather than honoured: a command found next to whatever directory
/// this happened to be run from is not evidence that an agent is installed.
fn search_path(path: Option<OsString>) -> Vec<PathBuf> {
    path.map(|path| {
        env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_follows_the_base_directory_when_there_is_one() {
        assert_eq!(
            below(
                Path::new("/home/u"),
                Some(OsString::from("/var/state")),
                DEFAULT_STATE_HOME
            ),
            PathBuf::from("/var/state/agentbus")
        );
    }

    #[test]
    fn a_directory_otherwise_sits_under_the_home_directory() {
        assert_eq!(
            below(Path::new("/home/u"), None, DEFAULT_DATA_HOME),
            PathBuf::from("/home/u/.local/share/agentbus")
        );
    }

    #[test]
    fn an_empty_base_directory_counts_as_unset() {
        assert_eq!(
            below(
                Path::new("/home/u"),
                Some(OsString::new()),
                DEFAULT_STATE_HOME
            ),
            PathBuf::from("/home/u/.local/state/agentbus")
        );
    }

    #[test]
    fn the_record_of_what_is_installed_sits_in_the_state_directory() {
        assert_eq!(
            Environment::rooted("/home/u").state_file(),
            PathBuf::from("/home/u/.local/state/agentbus/installed.json")
        );
    }

    #[test]
    fn what_is_generated_for_the_agents_sits_in_the_data_directory() {
        assert_eq!(
            Environment::rooted("/home/u").data_dir(),
            Path::new("/home/u/.local/share/agentbus")
        );
    }

    #[test]
    fn an_empty_search_path_entry_is_dropped() {
        assert_eq!(
            search_path(Some(OsString::from("/usr/bin::/bin"))),
            vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
        );
        assert!(search_path(None).is_empty());
    }

    #[test]
    fn a_command_is_only_found_where_it_can_be_run() {
        let dir = tempfile::tempdir().unwrap();
        let env = Environment::rooted(dir.path()).with_path([dir.path()]);
        std::fs::write(dir.path().join("codex"), "").unwrap();

        assert_eq!(env.look_up("codex"), None);

        std::fs::set_permissions(
            dir.path().join("codex"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        assert_eq!(env.look_up("codex"), Some(dir.path().join("codex")));
        assert_eq!(env.look_up("claude"), None);
    }

    #[test]
    fn a_directory_of_the_right_name_is_not_a_command() {
        let dir = tempfile::tempdir().unwrap();
        let env = Environment::rooted(dir.path()).with_path([dir.path()]);
        std::fs::create_dir(dir.path().join("claude")).unwrap();

        assert_eq!(env.look_up("claude"), None);
    }
}
