//! What this program has written, and which of it was its own to begin with.
//!
//! Uninstalling has to answer a question that cannot be answered from the file
//! being uninstalled: once this program's entries are taken out of a config file
//! and nothing is left, should the empty file be removed or kept? An empty file
//! this program created is litter. An empty file the user created — and then
//! asked this program to add to — is theirs, and deleting it would be an
//! installer removing something it never installed.
//!
//! Beside that, and for the same kind of reason, is what each agent's
//! installation is made of: which files were written for it and which generation
//! of the hooks they were when they were written. That is bookkeeping and not an
//! answer — asked which generation an agent is carrying *now*, this program
//! reads the file, because the file is what the agent runs and a user may have
//! changed it since. What the record is for is the questions the files cannot
//! answer: what an older build wrote, and where, once the build that wrote it
//! has been replaced.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::agent::Agent;
use crate::file;

/// The shape of the record this build writes.
///
/// A record written by an earlier build is read as it stands and written back in
/// this shape, so that nothing an earlier installation left behind is lost by a
/// later one turning up. A record from a *later* build is refused instead of
/// half-understood: guessing at a shape this build has never seen would risk
/// forgetting files an uninstall is the only thing that will ever remove.
const VERSION: u32 = 2;

/// How a file came to hold this program's entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ownership {
    /// There was no file, and this program made one.
    Created,
    /// There was a file already, and this program added to it.
    Merged,
}

/// What was installed for one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Install {
    /// Every file written for it, in the order they were written.
    pub assets: Vec<PathBuf>,
    /// Which generation of that agent's hooks those files were.
    pub version: u32,
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
    /// What was installed for each agent, by the agent's name.
    ///
    /// Absent from a record an earlier build wrote, and empty is exactly what
    /// that means: files that were installed before this program kept track of
    /// which generation they were.
    #[serde(default)]
    agents: BTreeMap<String, Install>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: VERSION,
            files: BTreeMap::new(),
            agents: BTreeMap::new(),
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
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(Error::Read {
                    path: path.to_owned(),
                    source: error,
                });
            }
        };
        let mut state: Self = serde_json::from_str(&text).map_err(|error| Error::State {
            path: path.to_owned(),
            reason: error.to_string(),
        })?;
        if state.version > VERSION {
            return Err(Error::State {
                path: path.to_owned(),
                reason: format!(
                    "it was written by a later version of this program, which keeps records in a shape {VERSION} does not know"
                ),
            });
        }
        // Everything an earlier shape held is held by this one too, so what was
        // read is complete and is simply this shape now. Nothing is written
        // until something else changes.
        state.version = VERSION;
        Ok(state)
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

    /// Every file the record knows about that lies below `root`.
    ///
    /// What this answers is the question an installation left by an earlier
    /// build poses: a file it wrote is one only the record still names, because
    /// the code that knew where to look for it has since been replaced. Asked
    /// about the directory that build generated into, this hands back what is
    /// left to take away — including the entries whose files somebody has
    /// already deleted by hand, which are the ones nothing else would find.
    pub fn recorded_below(&self, root: &Path) -> Vec<PathBuf> {
        self.files
            .keys()
            .filter(|path| path.starts_with(root))
            .cloned()
            .collect()
    }

    /// What was installed for `agent`, if anything was.
    pub fn installation(&self, agent: Agent) -> Option<&Install> {
        self.agents.get(agent.name())
    }

    /// Remembers that `agent` now has `assets` installed, at generation
    /// `version`, and says whether that changed the record.
    ///
    /// The last answer stands, unlike the per-file record above: this says what
    /// is installed now, and an installation that has just replaced an earlier
    /// one has replaced what was true about it as well.
    pub fn installed(&mut self, agent: Agent, assets: Vec<PathBuf>, version: u32) -> bool {
        let install = Install { assets, version };
        self.agents.insert(agent.name().to_owned(), install.clone()) != Some(install)
    }

    /// Forgets what was installed for `agent`, and says whether there was
    /// anything to forget.
    pub fn uninstalled(&mut self, agent: Agent) -> bool {
        self.agents.remove(agent.name()).is_some()
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
    fn what_an_earlier_build_wrote_is_read_whole_and_written_back_in_this_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installed.json");
        // Written out as an earlier build wrote it, byte for byte, rather than
        // generated: a record on somebody's machine is bytes, and the thing
        // being tested is that these ones are still understood.
        let earlier = r#"{
  "version": 1,
  "files": {
    "/home/u/.local/share/agentbus/claude-marketplace/agentbus/hooks/hooks.json": {
      "agent": "claude",
      "ownership": "created"
    },
    "/home/u/.codex/hooks.json": {
      "agent": "codex",
      "ownership": "merged"
    }
  }
}
"#;
        fs::write(&path, earlier).unwrap();

        let state = State::load(&path).unwrap();

        assert_eq!(
            state.ownership(Path::new(
                "/home/u/.local/share/agentbus/claude-marketplace/agentbus/hooks/hooks.json"
            )),
            Some(Ownership::Created)
        );
        assert_eq!(
            state.ownership(Path::new("/home/u/.codex/hooks.json")),
            Some(Ownership::Merged)
        );
        assert_eq!(state.installation(Agent::Claude), None);

        state.save(&path).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();

        assert_eq!(written["version"], serde_json::Value::from(VERSION));
        assert_eq!(State::load(&path).unwrap(), state, "and nothing was lost");
    }

    #[test]
    fn a_record_from_a_later_build_is_refused_rather_than_half_understood() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installed.json");
        fs::write(
            &path,
            format!("{{ \"version\": {}, \"files\": {{}} }}", VERSION + 1),
        )
        .unwrap();

        assert!(matches!(State::load(&path), Err(Error::State { .. })));
    }

    #[test]
    fn what_was_installed_for_an_agent_is_remembered_and_replaced_and_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installed.json");
        let mut state = State::default();
        let wrapper = PathBuf::from("/home/u/.local/share/agentbus/codex/agentbus-hook.sh");

        assert!(state.installed(Agent::Codex, vec![wrapper.clone()], 1));
        assert!(
            !state.installed(Agent::Codex, vec![wrapper.clone()], 1),
            "installing the same thing again changes nothing"
        );
        assert!(state.installed(Agent::Codex, vec![wrapper.clone()], 2));

        state.save(&path).unwrap();
        let read = State::load(&path).unwrap();

        let install = read.installation(Agent::Codex).expect("nothing recorded");
        assert_eq!(install.assets, vec![wrapper]);
        assert_eq!(install.version, 2);
        assert_eq!(read.installation(Agent::Claude), None);

        let mut read = read;
        assert!(read.uninstalled(Agent::Codex));
        assert!(!read.uninstalled(Agent::Codex));
        assert_eq!(read.installation(Agent::Codex), None);
    }

    #[test]
    fn a_record_that_cannot_be_read_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installed.json");
        fs::write(&path, "not a record").unwrap();

        assert!(matches!(State::load(&path), Err(Error::State { .. })));
    }
}
