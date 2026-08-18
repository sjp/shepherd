//! One daemon per socket directory.
//!
//! Two daemons sharing a directory would fight over the same socket paths and
//! keep two disagreeing session tables, so starting a second one has to be
//! refused rather than allowed to half-work. The mechanism is an exclusive
//! `flock` on a file beside the sockets, chosen over the sockets themselves
//! because the kernel drops it when the holder dies however it dies. That is
//! what makes recovery unambiguous: whoever holds this lock knows that no other
//! daemon is alive in the directory, and can therefore treat any socket file
//! still lying there as debris from a previous run rather than as somebody
//! else's listener.
//!
//! The file's contents are for people, not for this code. Nothing reads the pid
//! back to make a decision — a pid read from a file is a guess about a process
//! that may already have been replaced — it is there so that whoever is looking
//! at a directory that refuses them has somewhere to start.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::debug;

/// The mode the lock file is kept at, matching the rest of the directory: its
/// contents say which process on this machine is holding somebody's bus.
pub const LOCK_MODE: u32 = 0o600;

/// Why the lock could not be taken.
#[derive(Debug, Error)]
pub enum LockError {
    /// Another process holds the lock, which is to say another daemon is live
    /// in this directory.
    #[error("the lock is held by another process")]
    Held,
    /// The lock file could not be opened, locked or written.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// An exclusive claim on one socket directory, held until it is dropped.
#[derive(Debug)]
pub struct InstanceLock {
    /// Held open for as long as the claim lasts: the lock belongs to this open
    /// file, not to the name it was opened by.
    file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// Claims the directory by locking `path`, creating the file if it is not
    /// there.
    ///
    /// The file is only truncated and rewritten once the lock is held, so a
    /// daemon that is turned away never disturbs the pid of the one that turned
    /// it away.
    pub fn acquire(path: impl Into<PathBuf>) -> Result<Self, LockError> {
        let path = path.into();
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(LOCK_MODE)
            .open(&path)?;
        lock_exclusive(&file)?;

        let pid = std::process::id();
        file.set_len(0)?;
        writeln!(&file, "{pid}")?;
        debug!(path = %path.display(), pid, "took the instance lock");
        Ok(Self { file, path })
    }

    /// The file the lock is held on.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    /// Removes the lock file while the lock on it is still held.
    ///
    /// Unlinking first and releasing second means there is no instant in which
    /// the file is unlocked and still present, so a daemon starting at exactly
    /// this moment either locks the old file and is turned away, or creates a
    /// fresh one and succeeds. It never inherits a stale file that looks free.
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            debug!(path = %self.path.display(), %error, "cannot remove the lock file");
        }
        // Safe by construction, as in `lock_exclusive`. Closing the file would
        // release the lock on its own; doing it here says so, and keeps the
        // release from depending on when the descriptor happens to be dropped.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Takes an exclusive lock on `file` without waiting for one.
fn lock_exclusive(file: &File) -> Result<(), LockError> {
    // Safe by construction: `flock` is given a descriptor this process owns for
    // as long as the call lasts, and an operation flag; it reports every
    // failure through `errno` and touches nothing else.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if taken == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Err(LockError::Held),
        _ => Err(LockError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_lock_file_records_the_holders_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");

        let lock = InstanceLock::acquire(&path).unwrap();

        assert_eq!(lock.path(), path);
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written.trim().parse::<u32>().unwrap(), std::process::id());
    }

    #[test]
    fn the_lock_file_is_private_to_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");

        let _lock = InstanceLock::acquire(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, LOCK_MODE);
    }

    #[test]
    fn dropping_the_lock_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");

        drop(InstanceLock::acquire(&path).unwrap());

        assert!(!path.exists());
    }

    #[test]
    fn a_second_claim_on_a_held_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        let _held = InstanceLock::acquire(&path).unwrap();

        let error = InstanceLock::acquire(&path).unwrap_err();

        assert!(matches!(error, LockError::Held), "{error:?}");
    }

    #[test]
    fn a_lock_file_left_behind_by_a_dead_holder_can_be_taken_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        std::fs::write(&path, "4242\n").unwrap();

        let lock = InstanceLock::acquire(&path).unwrap();

        let written = std::fs::read_to_string(lock.path()).unwrap();
        assert_eq!(written.trim().parse::<u32>().unwrap(), std::process::id());
    }

    #[test]
    fn a_lock_that_cannot_be_opened_reports_the_filesystems_reason() {
        let dir = tempfile::tempdir().unwrap();

        let error = InstanceLock::acquire(dir.path().join("nowhere/daemon.lock")).unwrap_err();

        assert!(matches!(error, LockError::Io(_)), "{error:?}");
    }
}
