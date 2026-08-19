//! The two small JSON files this daemon keeps beside the bus.
//!
//! One says which endpoints somebody wants attached, the other says what is
//! actually happening to them. They are different enough in meaning that they
//! live in different modules and different directories, and identical in
//! everything that can go wrong with a file — it may not be there, it may hold
//! something a later version wrote, it may be halfway through being replaced —
//! so reading, writing and complaining about them is done once, here.
//!
//! Two rules apply to both. A write is a whole file renamed over the target, so
//! that a reader never sees half of one and a process killed partway through
//! leaves the previous version rather than a truncated one. And a version is
//! checked before the rest is read: a file written by a build that knows more
//! than this one is left exactly as it was found, because overwriting it would
//! discard what its writer meant by it.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tempfile::NamedTempFile;
use thiserror::Error;

/// The key carrying the shape a file is written in.
const VERSION_KEY: &str = "v";

/// The mode a directory made for one of these files is kept at.
pub const DIR_MODE: u32 = 0o700;

/// The mode the files themselves are kept at. Between them they say which
/// machines somebody is working on and what their agents are doing, which is
/// nobody else's business.
pub const FILE_MODE: u32 = 0o600;

/// Why one of these files could not be used.
///
/// Every variant names the file, because a message that says only what went
/// wrong sends whoever reads it looking in the wrong directory.
#[derive(Debug, Error)]
pub enum Error {
    /// It is there and could not be read.
    #[error("cannot read {}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
    /// It could not be replaced.
    #[error("cannot write {}", path.display())]
    Write {
        /// The file.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
    /// It is written in a shape this build does not know, and has been left
    /// alone.
    #[error("{} is written in v{found}, and this is a build that knows v{known}", path.display())]
    Version {
        /// The file.
        path: PathBuf,
        /// The version it says it is.
        found: u64,
        /// The version this build writes and reads.
        known: u32,
    },
    /// It is the right version and is not what that version looks like.
    #[error("cannot make sense of {}", path.display())]
    Malformed {
        /// The file.
        path: PathBuf,
        /// What could not be read.
        #[source]
        source: serde_json::Error,
    },
}

/// Reads `path` as a document of version `known`, or nothing at all when there
/// is no file there.
///
/// An absent file is the ordinary state of both of these — nothing has been
/// declared yet, no daemon is running — so it is an answer rather than a
/// failure, and every caller has something sensible to do with it.
pub fn read<T: DeserializeOwned>(path: &Path, known: u32) -> Result<Option<T>, Error> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    // A file that exists and holds nothing is what an interrupted write of an
    // earlier version could leave, and it says the same as no file at all.
    if text.trim().is_empty() {
        return Ok(None);
    }
    let document: Value = serde_json::from_str(&text).map_err(|source| Error::Malformed {
        path: path.to_owned(),
        source,
    })?;
    // The version is read before the shape, so that a document from a later
    // build is reported as what it is rather than as nonsense.
    match document.get(VERSION_KEY).and_then(Value::as_u64) {
        Some(found) if found == u64::from(known) => {}
        found => {
            return Err(Error::Version {
                path: path.to_owned(),
                found: found.unwrap_or_default(),
                known,
            });
        }
    }
    serde_json::from_value(document)
        .map(Some)
        .map_err(|source| Error::Malformed {
            path: path.to_owned(),
            source,
        })
}

/// Puts `document` at `path`, in one step as far as any reader is concerned.
///
/// The temporary file is made in the target's own directory, because a rename is
/// only atomic within one filesystem and anywhere else would be a guess about
/// how the machine is partitioned. The directory is created if it is not there,
/// and is left private either way.
pub fn write<T: Serialize>(path: &Path, document: &T) -> Result<(), Error> {
    let failed = |source| Error::Write {
        path: path.to_owned(),
        source,
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    make_dir(dir).map_err(failed)?;
    let mut text = serde_json::to_string(document)
        .map_err(io::Error::other)
        .map_err(failed)?;
    text.push('\n');
    let mut file = NamedTempFile::new_in(dir).map_err(failed)?;
    file.write_all(text.as_bytes()).map_err(failed)?;
    fs::set_permissions(file.path(), fs::Permissions::from_mode(FILE_MODE)).map_err(failed)?;
    file.as_file().sync_all().map_err(failed)?;
    file.persist(path)
        .map_err(|error| error.error)
        .map_err(failed)?;
    Ok(())
}

/// Takes `path` away, saying nothing if it was not there.
pub fn remove(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Write {
            path: path.to_owned(),
            source,
        }),
    }
}

/// When `path` was last written, or nothing when there is no file there or the
/// filesystem will not say.
///
/// Nothing is also the honest answer to a filesystem that keeps no modification
/// time: a caller watching for changes then reads the file every time it looks,
/// which is slower and never wrong.
pub fn changed_at(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// Creates `dir` if it is absent, and puts it at [`DIR_MODE`] either way.
fn make_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))
}
