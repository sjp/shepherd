//! What this program has written, and which of it was its own to begin with.
//!
//! Uninstalling has to answer a question that cannot be answered from the file
//! being uninstalled: once this program's entries are taken out of a config file
//! and nothing is left, should the empty file be removed or kept? An empty file
//! this program created is litter. An empty file the user created — and then
//! asked this program to add to — is theirs, and deleting it would be an
//! installer removing something it never installed.
//!
//! Nothing else is remembered here. This is not a manifest of what is installed:
//! the files themselves are that, they carry the mark that says who wrote them,
//! and a record that disagreed with them would be worse than no record at all.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::agent::Agent;
use crate::file;

/// The shape of the record this build writes.
const VERSION: u32 = 1;

/// How a file came to hold this program's entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ownership {
    /// There was no file, and this program made one.
    Created,
    /// There was a file already, and this program added to it.
    Merged,
}

/// One file this program has written to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The agent the file configures.
    pub agent: String,
    /// Whether the file itself is this program's.
    pub ownership: Ownership,
}

/// Every file this program has written to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// What this record was written by, so a later build can read an earlier
    /// one.
    version: u32,
    /// The files, by absolute path.
    files: BTreeMap<PathBuf, Record>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: VERSION,
            files: BTreeMap::new(),
        }
    }
}

impl State {
    /// Reads the record at `path`, or an empty one if there is none there.
    ///
    /// A record that cannot be read is a failure rather than an empty record:
    /// carrying on as though nothing had ever been installed would make the
    /// next uninstall leave this program's own files behind, and the user can
    /// see this one for themselves and delete it.
    pub fn load(path: &Path) -> Result<Self, Error> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| Error::State {
                path: path.to_owned(),
                reason: error.to_string(),
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(Error::Read {
                path: path.to_owned(),
                source: error,
            }),
        }
    }

    /// Writes the record to `path`.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let mut contents = serde_json::to_string_pretty(self).map_err(|error| Error::State {
            path: path.to_owned(),
            reason: error.to_string(),
        })?;
        contents.push('\n');
        file::write(path, &contents).map_err(|source| Error::Write {
            path: path.to_owned(),
            source,
        })
    }

    /// How `path` came to hold this program's entries, if it does.
    pub fn ownership(&self, path: &Path) -> Option<Ownership> {
        self.files.get(path).map(|record| record.ownership)
    }

    /// Remembers that `path` now holds entries for `agent`.
    ///
    /// The first answer stands. A file this program created keeps being this
    /// program's however many times it is written to afterwards, and a later
    /// write is not evidence that it was somebody else's all along.
    pub fn record(&mut self, path: &Path, agent: Agent, ownership: Ownership) {
        self.files.entry(path.to_owned()).or_insert(Record {
            agent: agent.name().to_owned(),
            ownership,
        });
    }

    /// Forgets `path`, which is no longer written to.
    pub fn forget(&mut self, path: &Path) {
        self.files.remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_that_is_not_there_yet_is_empty() {
        let dir = tempfile::tempdir().unwrap();

        let state = State::load(&dir.path().join("installed.json")).unwrap();

        assert_eq!(state, State::default());
    }

    #[test]
    fn what_was_written_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/installed.json");
        let mut state = State::default();
        state.record(
            Path::new("/home/u/.codex/hooks.json"),
            Agent::Codex,
            Ownership::Created,
        );

        state.save(&path).unwrap();

        assert_eq!(State::load(&path).unwrap(), state);
        assert!(fs::read_to_string(&path).unwrap().ends_with("}\n"));
    }

    #[test]
    fn a_file_this_program_made_stays_its_own() {
        let mut state = State::default();
        let path = Path::new("/home/u/.codex/hooks.json");

        state.record(path, Agent::Codex, Ownership::Created);
        state.record(path, Agent::Codex, Ownership::Merged);

        assert_eq!(state.ownership(path), Some(Ownership::Created));
    }

    #[test]
    fn a_file_nobody_has_written_to_is_owned_by_nobody() {
        let mut state = State::default();
        let path = Path::new("/home/u/.codex/hooks.json");
        assert_eq!(state.ownership(path), None);

        state.record(path, Agent::Codex, Ownership::Merged);
        state.forget(path);

        assert_eq!(state.ownership(path), None);
    }

    #[test]
    fn a_record_that_cannot_be_read_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installed.json");
        fs::write(&path, "not a record").unwrap();

        assert!(matches!(State::load(&path), Err(Error::State { .. })));
    }
}
