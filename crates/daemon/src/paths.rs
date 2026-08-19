//! Where the bus lives on the filesystem.
//!
//! Every component that talks to the bus — the daemon that binds the sockets and
//! every client that connects to one — has to agree on the same directory, so
//! resolution lives here and nowhere else.
//!
//! The rules are, in order: an explicit `AGENTBUS_DIR`, then a subdirectory of
//! the session's `XDG_RUNTIME_DIR`, then a per-user directory under `/tmp`. The
//! last rule is what makes this work unchanged in places that have no session
//! manager and therefore no `XDG_RUNTIME_DIR`; nothing here detects, or needs to
//! detect, what kind of place it is running in. Every path is per-user so that
//! two people on one machine never reach for the same socket.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The environment variable that names the bus directory outright.
pub const DIR_VAR: &str = "AGENTBUS_DIR";

/// The environment variable holding the session's runtime directory.
pub const RUNTIME_DIR_VAR: &str = "XDG_RUNTIME_DIR";

/// The name of the bus's own directory inside the runtime directory.
const DIR_NAME: &str = "agentbus";

/// The file name of the socket hooks send events to.
pub const EMIT_SOCKET: &str = "emit.sock";

/// The file name of the socket subscribers read the stream from.
pub const SUB_SOCKET: &str = "sub.sock";

/// The file name of the lock that grants a daemon the directory.
pub const LOCK_FILE: &str = "daemon.lock";

/// The mode the bus directory is kept at: nobody but its owner has any business
/// listing it, and its contents are enough to drive someone's coding agent.
pub const DIR_MODE: u32 = 0o700;

/// The mode both sockets are kept at, for the same reason.
pub const SOCKET_MODE: u32 = 0o600;

/// The directory the bus uses and the files inside it: the two sockets, the
/// lock a daemon holds while it is serving them, and what it says about the
/// other endpoints it has attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPaths {
    dir: PathBuf,
    emit: PathBuf,
    sub: PathBuf,
    lock: PathBuf,
    attachments: PathBuf,
}

impl SocketPaths {
    /// Resolves the paths from the environment.
    pub fn resolve() -> Self {
        Self::in_dir(resolve_dir(
            std::env::var_os(DIR_VAR),
            std::env::var_os(RUNTIME_DIR_VAR),
            current_uid(),
        ))
    }

    /// The paths inside a directory chosen by the caller.
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            emit: dir.join(EMIT_SOCKET),
            sub: dir.join(SUB_SOCKET),
            lock: dir.join(LOCK_FILE),
            attachments: dir.join(crate::remote::attachments::FILE_NAME),
            dir,
        }
    }

    /// The directory holding the sockets.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The socket hooks send events to.
    pub fn emit(&self) -> &Path {
        &self.emit
    }

    /// The socket subscribers read the stream from.
    pub fn sub(&self) -> &Path {
        &self.sub
    }

    /// The lock file whose holder is the daemon serving this directory.
    pub fn lock(&self) -> &Path {
        &self.lock
    }

    /// The file saying which other endpoints this daemon is attached to.
    pub fn attachments(&self) -> &Path {
        &self.attachments
    }

    /// Both sockets, in no particular order.
    ///
    /// Every operation that treats the sockets as a set rather than as two
    /// named things — clearing what an earlier run left behind, taking them
    /// away on the way out — walks this, so adding a socket to the bus later
    /// cannot leave one of those half-done.
    pub fn sockets(&self) -> [&Path; 2] {
        [&self.emit, &self.sub]
    }

    /// Everything in the directory that belongs to whichever daemon is serving
    /// it, and to no other.
    ///
    /// These are the files a daemon makes as it starts and takes away as it
    /// stops, so one found in a directory a daemon has just claimed is the
    /// remains of a run that did not get to stop, and is cleared. The lock is
    /// not among them: it is what says the directory has been claimed, and it
    /// is released by the kernel rather than by unlinking it.
    pub fn ephemeral(&self) -> [&Path; 3] {
        [&self.emit, &self.sub, &self.attachments]
    }

    /// Creates the directory if it is absent, and puts it at [`DIR_MODE`] either
    /// way.
    ///
    /// The mode is applied to a directory that already existed as well as to one
    /// this call creates: the guarantee worth having is about what the directory
    /// *is*, not about who made it.
    pub fn create_dir(&self) -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(&self.dir)?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(DIR_MODE))
    }
}

/// Applies the precedence rules to values already read from the environment.
///
/// An empty value counts as unset. A variable that is present but empty names no
/// directory, and treating it as the answer would put the sockets at a relative
/// path in whatever the process's working directory happened to be.
fn resolve_dir(explicit: Option<OsString>, runtime_dir: Option<OsString>, uid: u32) -> PathBuf {
    if let Some(dir) = explicit.filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(runtime_dir) = runtime_dir.filter(|value| !value.is_empty()) {
        return PathBuf::from(runtime_dir).join(DIR_NAME);
    }
    per_user(uid)
}

/// The directory this program falls back to on a machine that names nowhere
/// else to put its files.
pub(crate) fn per_user_dir() -> PathBuf {
    per_user(current_uid())
}

/// The per-user directory under `/tmp` for `uid`.
fn per_user(uid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/{DIR_NAME}-{uid}"))
}

/// The effective user id of this process.
fn current_uid() -> u32 {
    // Safe by construction: `geteuid` reads one field of the calling process and
    // cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(explicit: Option<&str>, runtime_dir: Option<&str>) -> PathBuf {
        resolve_dir(
            explicit.map(OsString::from),
            runtime_dir.map(OsString::from),
            1000,
        )
    }

    #[test]
    fn an_explicit_directory_is_used_verbatim() {
        assert_eq!(
            resolved(Some("/somewhere/else"), Some("/run/user/1000")),
            PathBuf::from("/somewhere/else")
        );
    }

    #[test]
    fn the_runtime_directory_is_used_when_there_is_no_explicit_one() {
        assert_eq!(
            resolved(None, Some("/run/user/1000")),
            PathBuf::from("/run/user/1000/agentbus")
        );
    }

    #[test]
    fn without_a_runtime_directory_the_fallback_is_per_user_under_tmp() {
        assert_eq!(resolved(None, None), PathBuf::from("/tmp/agentbus-1000"));
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        assert_eq!(
            resolved(Some(""), Some("/run/user/1000")),
            PathBuf::from("/run/user/1000/agentbus")
        );
        assert_eq!(
            resolved(Some(""), Some("")),
            PathBuf::from("/tmp/agentbus-1000")
        );
    }

    #[test]
    fn the_fallback_is_per_user() {
        assert_ne!(resolve_dir(None, None, 1000), resolve_dir(None, None, 1001));
    }

    #[test]
    fn both_sockets_sit_in_the_resolved_directory() {
        let paths = SocketPaths::in_dir("/run/user/1000/agentbus");
        assert_eq!(paths.emit(), Path::new("/run/user/1000/agentbus/emit.sock"));
        assert_eq!(paths.sub(), Path::new("/run/user/1000/agentbus/sub.sock"));
        assert_eq!(
            paths.lock(),
            Path::new("/run/user/1000/agentbus/daemon.lock")
        );
        assert_eq!(
            paths.attachments(),
            Path::new("/run/user/1000/agentbus/attachments.json")
        );
        assert_eq!(paths.dir(), Path::new("/run/user/1000/agentbus"));
        assert_eq!(paths.sockets(), [paths.emit(), paths.sub()]);
        assert_eq!(
            paths.ephemeral(),
            [paths.emit(), paths.sub(), paths.attachments()]
        );
    }

    #[test]
    fn creating_the_directory_is_idempotent_and_leaves_it_private() {
        let parent = tempfile::tempdir().unwrap();
        let paths = SocketPaths::in_dir(parent.path().join("nested/agentbus"));

        paths.create_dir().unwrap();
        paths.create_dir().unwrap();

        let mode = fs::metadata(paths.dir()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, DIR_MODE);
    }

    #[test]
    fn creating_the_directory_tightens_one_that_was_left_open() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let paths = SocketPaths::in_dir(dir.path());

        paths.create_dir().unwrap();

        let mode = fs::metadata(paths.dir()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, DIR_MODE);
    }
}
