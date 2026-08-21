//! The parts of a machine that installation depends on.
//!
//! Where a coding agent keeps its configuration, and which commands can be run
//! by name, are questions about one particular machine — normally the one this
//! process is running on, but not necessarily, and not at all while these rules
//! are being tested. So the answers are carried in an [`Environment`] that is
//! built from the process's environment in exactly one place, and passed down
//! from there. A test describes a machine instead of having to be run on one,
//! and nothing below this module reads a variable.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs::Metadata;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::agent::OVERRIDE_VARS;

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

/// The variable naming the directory a Windows machine keeps a user's own
/// application data in.
pub const LOCAL_APP_DATA_VAR: &str = "LOCALAPPDATA";

/// The variable naming the profile directory a Windows machine gives a user.
pub const USER_PROFILE_VAR: &str = "USERPROFILE";

/// The variables that describe the machine rather than any agent.
///
/// Read alongside the agents' own, and named here rather than beside them,
/// because they belong to Windows: an agent whose default location is written
/// in terms of them is reading what the machine says about itself, not
/// something it documents for its users to set.
const MACHINE_VARS: [&str; 2] = [LOCAL_APP_DATA_VAR, USER_PROFILE_VAR];

/// The name of this program's own directory, inside whichever base directory it
/// sits under.
const DIR_NAME: &str = "agentbus";

/// The file recording what has been installed, inside the state directory.
const STATE_FILE_NAME: &str = "installed.json";

/// Where the state directory sits when `XDG_STATE_HOME` does not say.
const DEFAULT_STATE_HOME: &str = ".local/state";

/// Where the data directory sits when `XDG_DATA_HOME` does not say.
const DEFAULT_DATA_HOME: &str = ".local/share";

/// What a machine is, as far as running a command by name goes.
///
/// The two families disagree about the one question this crate asks of a search
/// path — what a command is called and what makes it runnable — and they
/// disagree about it in a way no amount of care with paths papers over. Naming
/// the family, rather than compiling one answer in, is also what lets a machine
/// of either kind be described by a test running on the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// A machine whose commands are files with the executable bit set.
    Unix,
    /// A machine whose commands are files whose extension says how to run them.
    Windows,
}

/// The extensions a bare command name can turn out to carry on Windows: a
/// compiled program, and the shims an installer writes so that a script can be
/// run by name.
const SHIMS: [&str; 4] = [".exe", ".cmd", ".bat", ".ps1"];

impl Platform {
    /// The kind of machine this program is running on.
    pub const fn host() -> Self {
        match cfg!(windows) {
            true => Self::Windows,
            false => Self::Unix,
        }
    }
}

/// One machine, as far as installing hooks is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    home: PathBuf,
    path: Vec<PathBuf>,
    state_dir: PathBuf,
    data_dir: PathBuf,
    vars: BTreeMap<&'static str, PathBuf>,
    platform: Platform,
}

impl Environment {
    /// Reads the machine this process is running on.
    ///
    /// A home directory is the one thing there is no sensible fallback for:
    /// every path an agent's configuration lives at is relative to it, and
    /// guessing would mean writing hooks into somebody else's dot-files. What
    /// there is instead is a second variable that names one — see [`home`].
    pub fn from_env() -> Result<Self, Error> {
        let home = home().ok_or(Error::NoHome)?;
        let state_dir = below(&home, env::var_os(STATE_HOME_VAR), DEFAULT_STATE_HOME);
        let data_dir = below(&home, env::var_os(DATA_HOME_VAR), DEFAULT_DATA_HOME);
        Ok(Self {
            path: search_path(env::var_os(PATH_VAR)),
            home,
            state_dir,
            data_dir,
            vars: overrides(),
            platform: Platform::host(),
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
            vars: BTreeMap::new(),
            platform: Platform::host(),
        }
    }

    /// The same machine, with `dirs` as the directories commands are looked up
    /// in.
    #[must_use]
    pub fn with_path(mut self, dirs: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        self.path = dirs.into_iter().map(Into::into).collect();
        self
    }

    /// The same machine, with `name` set to `value`.
    ///
    /// Only the variables the agents document as saying where their
    /// configuration lives are ever asked for, so only those are worth setting;
    /// an empty value is what a machine that does not set one looks like, and
    /// is stored as such.
    #[must_use]
    pub fn with_var(mut self, name: &'static str, value: impl Into<OsString>) -> Self {
        let value = value.into();
        match value.is_empty() {
            true => self.vars.remove(name),
            false => self.vars.insert(name, PathBuf::from(value)),
        };
        self
    }

    /// The same machine, taken to be one of `platform`.
    #[must_use]
    pub fn with_platform(mut self, platform: Platform) -> Self {
        self.platform = platform;
        self
    }

    /// The user's home directory.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// What kind of machine this is.
    pub fn platform(&self) -> Platform {
        self.platform
    }

    /// The directory `name` names, if the machine sets it to one.
    ///
    /// A leading `~` is expanded here rather than taken literally. A variable
    /// set in a shell arrives expanded already, but one set in a container
    /// image, a service manager or an agent's own configuration file does not,
    /// and honouring it as written would put an agent's hooks in a directory
    /// actually called `~`.
    pub fn var(&self, name: &str) -> Option<PathBuf> {
        self.vars.get(name).map(|value| expand(&self.home, value))
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
            candidates(dir, command, self.platform)
                .into_iter()
                .find(|candidate| self.runnable(candidate))
        })
    }

    /// Whether `path` is a file this machine would run.
    ///
    /// Asked directly, rather than through [`look_up`](Self::look_up), where an
    /// agent's own installer leaves its program inside the agent's directory
    /// instead of anywhere a command is normally run from.
    pub fn runnable(&self, path: &Path) -> bool {
        path.metadata().is_ok_and(|meta| {
            meta.is_file()
                && match self.platform {
                    // Nothing here is the equivalent of the executable bit: an
                    // extension the machine knows how to run is the whole of
                    // what makes a file a command, and the name it was found
                    // under already carried one.
                    Platform::Windows => true,
                    Platform::Unix => executable(&meta),
                }
        })
    }
}

/// Every file a bare command name could be, in one directory.
///
/// A Windows command is run by a name whose extension says what runs it, and
/// which extension that is depends on how the thing was installed — so all of
/// them are tried. A name that already carries an extension is the file itself
/// and is left alone.
fn candidates(dir: &Path, command: &str, platform: Platform) -> Vec<PathBuf> {
    let named = dir.join(command);
    if platform == Platform::Unix || Path::new(command).extension().is_some() {
        return vec![named];
    }
    std::iter::once(named)
        .chain(SHIMS.map(|shim| dir.join(format!("{command}{shim}"))))
        .collect()
}

/// Whether a file's permissions let anybody run it.
#[cfg(unix)]
fn executable(meta: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    meta.permissions().mode() & 0o111 != 0
}

/// Whether a file's permissions let anybody run it, where they cannot say.
///
/// A machine of one family being read by a program built for the other can only
/// answer the part of the question it can see, which is that the file is there.
#[cfg(not(unix))]
fn executable(_meta: &Metadata) -> bool {
    true
}

/// The user's home directory, as this machine says where it is.
fn home() -> Option<PathBuf> {
    chosen_home(named(HOME_VAR), named(USER_PROFILE_VAR), Platform::host())
}

/// Which of the two directories that could be a user's home is, given what a
/// machine of `platform` says each of them is.
///
/// The unix variable first, on either kind of machine. A machine that runs its
/// scripts by extension gives every user a profile directory and names it in a
/// variable of its own, but a user of such a machine may also have a home
/// directory in the unix sense — set by a shell that brings one with it — and
/// somebody whose shell says where their home is has said where it is. So the
/// profile directory is the answer only when nothing else is.
///
/// The second is only read on the machine it belongs to. Where it is set on the
/// other kind it is somebody's own, and a program that quietly preferred it to
/// no home directory at all would be writing hooks somewhere nobody asked for
/// them.
fn chosen_home(
    home: Option<PathBuf>,
    profile: Option<PathBuf>,
    platform: Platform,
) -> Option<PathBuf> {
    home.or(match platform {
        Platform::Windows => profile,
        Platform::Unix => None,
    })
}

/// The variables a home directory is read from on this machine, in the order
/// they are read, as a person would say them.
///
/// Named for the message that has to be written when none of them says
/// anything: somebody on a machine with no home directory has to be told which
/// variables would have given it one, and being told about a variable their
/// machine has never heard of would not help them.
pub fn home_vars() -> &'static str {
    match Platform::host() {
        Platform::Windows => "HOME or USERPROFILE",
        Platform::Unix => HOME_VAR,
    }
}

/// What `var` holds, where it holds a path at all.
fn named(var: &str) -> Option<PathBuf> {
    env::var_os(var)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The directories the agents' own variables point their configuration at, and
/// the ones the machine names for itself.
///
/// Read once, with the rest of the machine, so that no rule below this module
/// depends on the environment of the process it happens to be running in.
fn overrides() -> BTreeMap<&'static str, PathBuf> {
    OVERRIDE_VARS
        .iter()
        .chain(MACHINE_VARS.iter())
        .filter_map(|name| {
            let value = env::var_os(name).filter(|value| !value.is_empty())?;
            Some((*name, PathBuf::from(value)))
        })
        .collect()
}

/// A path as a variable held it, with a leading `~` replaced by `home`.
fn expand(home: &Path, value: &Path) -> PathBuf {
    let Some(text) = value.to_str() else {
        return value.to_owned();
    };
    match text {
        "~" => home.to_owned(),
        _ => match text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
            Some(rest) => home.join(rest),
            None => value.to_owned(),
        },
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
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A directory with `name` in it, that anybody may run.
    fn runnable(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), "").unwrap();
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

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
    fn a_machine_that_names_a_home_directory_twice_is_read_by_the_first_name() {
        let home = || Some(PathBuf::from("/home/u"));
        let profile = || Some(PathBuf::from("/c/Users/u"));

        assert_eq!(
            chosen_home(home(), profile(), Platform::Windows),
            home(),
            "a shell that brought a home directory with it has said where it is"
        );
        assert_eq!(chosen_home(None, profile(), Platform::Windows), profile());
        assert_eq!(chosen_home(home(), None, Platform::Windows), home());
        assert_eq!(chosen_home(None, None, Platform::Windows), None);
    }

    #[test]
    fn the_profile_directory_is_not_a_home_directory_on_a_unix_machine() {
        let profile = Some(PathBuf::from("/c/Users/u"));

        assert_eq!(chosen_home(None, profile, Platform::Unix), None);
    }

    #[test]
    fn the_variables_a_machine_is_asked_for_are_the_ones_it_would_answer() {
        assert!(home_vars().contains(HOME_VAR));
        assert_eq!(
            home_vars().contains(USER_PROFILE_VAR),
            Platform::host() == Platform::Windows,
            "a variable this machine never reads is not one to name in a refusal"
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

    #[test]
    fn a_command_on_a_windows_machine_is_found_under_the_extension_it_was_installed_with() {
        let dir = tempfile::tempdir().unwrap();
        let env = Environment::rooted(dir.path())
            .with_path([dir.path()])
            .with_platform(Platform::Windows);
        // Written without the executable bit, which is what a file on a machine
        // that has no such thing looks like from here.
        std::fs::write(dir.path().join("agy.cmd"), "").unwrap();

        assert_eq!(env.look_up("agy"), Some(dir.path().join("agy.cmd")));
        assert_eq!(
            env.look_up("agy.exe"),
            None,
            "a name that carries an extension is the file itself"
        );
    }

    #[test]
    fn a_windows_shim_is_not_a_command_on_a_unix_machine() {
        let dir = tempfile::tempdir().unwrap();
        let env = Environment::rooted(dir.path())
            .with_path([dir.path()])
            .with_platform(Platform::Unix);
        runnable(dir.path(), "agy.cmd");

        assert_eq!(env.look_up("agy"), None);
    }

    #[test]
    fn the_command_itself_comes_before_any_shim_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let env = Environment::rooted(dir.path())
            .with_path([dir.path()])
            .with_platform(Platform::Windows);
        std::fs::write(dir.path().join("agy"), "").unwrap();
        std::fs::write(dir.path().join("agy.exe"), "").unwrap();

        assert_eq!(env.look_up("agy"), Some(dir.path().join("agy")));
    }

    #[test]
    fn a_directory_of_the_right_name_is_not_a_command_on_a_windows_machine() {
        let dir = tempfile::tempdir().unwrap();
        let env = Environment::rooted(dir.path())
            .with_path([dir.path()])
            .with_platform(Platform::Windows);
        std::fs::create_dir(dir.path().join("agy.exe")).unwrap();

        assert_eq!(env.look_up("agy"), None);
    }

    #[test]
    fn a_search_path_is_read_in_the_order_it_was_given() {
        let root = tempfile::tempdir().unwrap();
        let (first, second) = (root.path().join("first"), root.path().join("second"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        runnable(&first, "codex");
        runnable(&second, "codex");

        let env = Environment::rooted(root.path()).with_path([&second, &first]);

        assert_eq!(env.look_up("codex"), Some(second.join("codex")));
    }

    #[test]
    fn a_variable_is_read_back_as_the_directory_it_names() {
        let env = Environment::rooted("/home/u").with_var(crate::agent::CODEX_HOME_VAR, "/srv/c");

        assert_eq!(
            env.var(crate::agent::CODEX_HOME_VAR),
            Some(PathBuf::from("/srv/c"))
        );
        assert_eq!(env.var(crate::agent::QWEN_HOME_VAR), None);
    }

    #[test]
    fn a_variable_set_to_nothing_is_one_that_was_not_set() {
        let env = Environment::rooted("/home/u")
            .with_var(crate::agent::CODEX_HOME_VAR, "/srv/c")
            .with_var(crate::agent::CODEX_HOME_VAR, "");

        assert_eq!(env.var(crate::agent::CODEX_HOME_VAR), None);
    }

    #[test]
    fn a_home_directory_a_variable_only_gestured_at_is_the_one_this_machine_has() {
        let home = Path::new("/home/u");

        assert_eq!(expand(home, Path::new("~")), home);
        assert_eq!(expand(home, Path::new("~/agents")), home.join("agents"));
        assert_eq!(expand(home, Path::new("~\\agents")), home.join("agents"));
        assert_eq!(
            expand(home, Path::new("/srv/~/agents")),
            Path::new("/srv/~/agents"),
            "a tilde that is not at the front is part of the path"
        );
    }
}
