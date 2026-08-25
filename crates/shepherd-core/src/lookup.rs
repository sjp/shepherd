//! Finding a command among the directories a machine looks for commands in.
//!
//! Two things here need an external command — the tool that runs a development
//! container, and the bus — and both of them want the same thing from it: the
//! path a command was actually found at, decided once, rather than a name
//! handed to something that will resolve it again later against whatever `PATH`
//! says by then. A machine that does not have the command has to be told apart
//! from one that does, before anything is started, and that is a question about
//! files rather than about either command.

use std::ffi::OsString;
use std::fs::Metadata;
use std::path::{Path, PathBuf};

/// The variable naming the directories a command is looked for in.
pub(crate) const PATH_VAR: &str = "PATH";

/// Splits a `PATH` into the directories it names.
///
/// An empty entry means the working directory by long convention, and is
/// dropped rather than honoured: a command found beside whatever directory this
/// application happened to be started from is not the machine's copy of
/// anything.
pub(crate) fn search_path(path: Option<OsString>) -> Vec<PathBuf> {
    path.map(|path| {
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// Where `command` is, if it is in any of `directories`.
pub(crate) fn look_up(directories: &[PathBuf], command: &str) -> Option<PathBuf> {
    directories.iter().find_map(|dir| {
        candidates(dir, command)
            .into_iter()
            .find(|path| runnable(path))
    })
}

/// Every file a command could be, in one directory.
///
/// A machine that decides what runs a file from its extension has several
/// spellings of one command, depending on how it was installed, and all of them
/// are tried. Everywhere else there is exactly one name.
fn candidates(dir: &Path, command: &str) -> Vec<PathBuf> {
    /// The extensions such a machine runs a command installed as a script or a
    /// program under.
    const SHIMS: [&str; 3] = [".cmd", ".exe", ".ps1"];

    let named = dir.join(command);
    if !cfg!(windows) {
        return vec![named];
    }
    std::iter::once(named)
        .chain(SHIMS.map(|shim| dir.join(format!("{command}{shim}"))))
        .collect()
}

/// Whether `path` is a file this machine would run.
fn runnable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && executable(&meta))
}

/// Whether a file's permissions let anybody run it.
#[cfg(unix)]
fn executable(meta: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    meta.permissions().mode() & 0o111 != 0
}

/// Whether a file's permissions let anybody run it, where the question has no
/// such answer: the extension it was found under is the whole of what makes a
/// file a command there.
#[cfg(not(unix))]
fn executable(_meta: &Metadata) -> bool {
    true
}
