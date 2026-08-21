//! Changing a file on disk without being able to damage it.
//!
//! Everything this program writes to is a file somebody depends on, edited by
//! hand and not under version control. Two rules follow, and they are applied to
//! every write rather than to the ones that look risky. A copy is taken first,
//! so that whatever this program got wrong can be undone by moving one file
//! back. And the write itself is a rename over the target, so that a process
//! killed halfway through leaves the old file rather than half of a new one.
//!
//! Backups accumulate, so only the newest few are kept. They are stamped with
//! the time rather than numbered, because numbering means renaming the ones
//! already there, and a rename of a backup is one more way to lose the thing
//! being protected.

use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tempfile::NamedTempFile;

/// What comes between a file's name and the stamp on a backup of it.
pub const BACKUP_INFIX: &str = ".agentbus-backup-";

/// How many backups of one file are kept.
///
/// Enough to survive a couple of bad runs in a row, few enough that a user's
/// configuration directory does not fill up with copies of itself.
pub const BACKUPS_KEPT: usize = 3;

/// Copies `path` next to itself and drops all but the newest few copies.
///
/// Answers with where the copy went, or with nothing when there was no file to
/// copy.
pub fn back_up(path: &Path) -> io::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    // The clock only has to break the tie between backups taken in the same
    // millisecond; what actually decides the order is that each stamp is
    // greater than every stamp already there. Counting from the clock alone
    // would let two copies taken in one millisecond collide, and a copy that
    // reused the stamp of one just rotated away would be rotated away itself
    // for looking like the oldest.
    let existing = backups_of(path)?;
    let after = existing.first().map_or(0, |(newest, _)| newest + 1);
    let backup = backup_path(path, stamp().max(after));
    fs::copy(path, &backup)?;
    for (_, old) in existing.into_iter().skip(BACKUPS_KEPT - 1) {
        fs::remove_file(old)?;
    }
    Ok(Some(backup))
}

/// Puts `contents` at `path`, in one step as far as any reader is concerned.
///
/// The temporary file is made in the same directory as the target, because a
/// rename is only atomic within one filesystem and anywhere else is a guess
/// about how the machine is partitioned. Missing parent directories are created:
/// an agent's configuration directory may be the very thing that does not exist
/// yet.
///
/// A file that already exists keeps its permissions. A file created here is
/// readable only by its owner, which is what the temporary file it is renamed
/// from was made as: nothing written here has any reason to be legible to
/// another account.
pub fn write(path: &Path, contents: &str) -> io::Result<()> {
    put(path, contents, false)
}

/// Puts `contents` at `path` as a file the machine will run.
///
/// The same write, and then the permissions that make a script a command, which
/// is what an agent asked to run one needs of it. They are set rather than kept:
/// a wrapper that lost its executable bit somewhere — restored from an archive,
/// copied off a filesystem that has no such thing — is an installation that
/// looks complete and does nothing, and installing again is what a user does
/// about that.
pub fn write_runnable(path: &Path, contents: &str) -> io::Result<()> {
    put(path, contents, true)
}

/// Puts `contents` at `path`, optionally as something runnable.
fn put(path: &Path, contents: &str, runnable: bool) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let mut file = NamedTempFile::new_in(dir)?;
    file.write_all(contents.as_bytes())?;
    file.as_file().sync_all()?;
    if let Ok(existing) = fs::metadata(path) {
        fs::set_permissions(file.path(), existing.permissions())?;
    }
    if runnable {
        make_runnable(file.path())?;
    }
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Gives `path` the permissions of a command.
///
/// Readable and runnable by anybody, writable by its owner: the permissions a
/// program's own files are installed with, and nothing about a wrapper is
/// private.
#[cfg(unix)]
fn make_runnable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

/// Gives `path` the permissions of a command, where a machine has no such
/// thing.
///
/// What makes a file runnable there is the extension it was written under,
/// which the name it is being written to already carries.
#[cfg(not(unix))]
fn make_runnable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Reads a file that may not be there.
///
/// A file that is absent is not a failure anywhere in this crate: it is the
/// ordinary state of a config file before anything has been installed, and of
/// one after everything has been uninstalled.
pub fn read(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Removes `root` and every directory below it that is empty, deepest first.
///
/// A directory holding anything at all is left where it is, and so is
/// everything above it. This runs at the end of an uninstall, over a tree this
/// program made and filled entirely by itself, so anything found in it is
/// something somebody else put there — and an uninstaller that deleted it would
/// be removing what it never installed. A `root` that is not there at all has
/// nothing to do.
pub fn remove_empty_dirs(root: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        entries => entries?,
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_empty_dirs(&entry.path())?;
        }
    }
    match fs::remove_dir(root) {
        // Left behind because something that is not this program's is in it,
        // which is the whole point of removing them one at a time.
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
        result => result,
    }
}

/// Removes `path` and every backup of it this program made.
///
/// Only ever called for a file this program created, where the backups are
/// copies of its own work and not of anybody's data. Removing a file and
/// leaving copies of it behind would not be an uninstall.
pub fn remove_with_backups(path: &Path) -> io::Result<()> {
    for (_, backup) in backups_of(path)? {
        fs::remove_file(backup)?;
    }
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Every backup of `path`, newest first.
///
/// A name that carries the infix but not a stamp this program could have
/// written is not one of ours, and is left where it is.
pub fn backups_of(path: &Path) -> io::Result<Vec<(u128, PathBuf)>> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(Vec::new());
    };
    let mut prefix = name.as_bytes().to_vec();
    prefix.extend_from_slice(BACKUP_INFIX.as_bytes());

    let mut found = Vec::new();
    let entries = match fs::read_dir(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(found),
        entries => entries?,
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(rest) = name.as_bytes().strip_prefix(&prefix[..]) else {
            continue;
        };
        let Some(stamp) = std::str::from_utf8(rest)
            .ok()
            .and_then(|stamp| stamp.parse::<u128>().ok())
        else {
            continue;
        };
        found.push((stamp, entry.path()));
    }
    found.sort_by_key(|(stamp, _)| std::cmp::Reverse(*stamp));
    Ok(found)
}

/// Where a backup of `path` taken at `stamp` goes.
fn backup_path(path: &Path, stamp: u128) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!("{BACKUP_INFIX}{stamp}"));
    path.with_file_name(name)
}

/// What a backup taken now is stamped with: milliseconds since the epoch.
///
/// An integer rather than a date, because the only thing anything does with it
/// is put the backups in order, and a plain count needs no calendar to produce
/// and none to compare.
fn stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn writing_creates_the_directories_it_needs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep/deeper/hooks.json");

        write(&path, "{}\n").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "{}\n");
    }

    #[test]
    fn a_file_that_was_there_keeps_its_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        write(&path, "new").unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn there_is_nothing_to_back_up_when_there_is_no_file() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(back_up(&dir.path().join("absent.json")).unwrap(), None);
    }

    #[test]
    fn a_backup_holds_what_the_file_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        fs::write(&path, "before").unwrap();

        let backup = back_up(&path).unwrap().unwrap();
        write(&path, "after").unwrap();

        assert_eq!(fs::read_to_string(backup).unwrap(), "before");
        assert_eq!(fs::read_to_string(&path).unwrap(), "after");
    }

    #[test]
    fn only_the_newest_backups_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");

        for round in 0..BACKUPS_KEPT + 3 {
            fs::write(&path, round.to_string()).unwrap();
            back_up(&path).unwrap();
        }

        let backups = backups_of(&path).unwrap();
        assert_eq!(backups.len(), BACKUPS_KEPT);
        let newest: Vec<String> = backups
            .iter()
            .map(|(_, path)| fs::read_to_string(path).unwrap())
            .collect();
        assert_eq!(newest, ["5", "4", "3"]);
    }

    #[test]
    fn a_file_next_door_that_is_not_a_backup_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        fs::write(&path, "ours").unwrap();
        let theirs = dir.path().join("hooks.json.agentbus-backup-by-hand");
        fs::write(&theirs, "theirs").unwrap();

        back_up(&path).unwrap();
        remove_with_backups(&path).unwrap();

        assert!(theirs.exists());
        assert!(!path.exists());
        assert!(backups_of(&path).unwrap().is_empty());
    }
}
