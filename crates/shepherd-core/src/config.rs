//! The file the arrangement is kept in between runs.
//!
//! One file, per user, holding what is open: the workspaces, what each is
//! called and what has been chosen about it, and the tabs and split
//! arrangements inside it. Launching reads it and rebuilds the model from
//! [`crate::workspace`]; changing anything writes it again.
//!
//! # What is not in it
//!
//! Shells. A saved arrangement says a tab holds two of them side by side; it
//! says nothing about the processes that were in them, what those processes had
//! printed, or how far anybody had scrolled back through it. A restored shell
//! is a fresh process started in the same workspace, in the position the one
//! before it occupied — this application holds no terminals open across a
//! restart and has no daemon that could. That is a deliberate choice about what
//! restoring means, and it is why the types this module serializes are its own
//! rather than the model's: a field carrying something about a running process
//! would have to be added here, on purpose, and there is nowhere for one to
//! arrive by accident.
//!
//! # Nothing is written into anybody's project
//!
//! The path a workspace is for is a key in this file, not a place to keep
//! anything. What somebody has chosen about a folder belongs to them rather
//! than to the folder, which is very often a repository they share with other
//! people, and a tool that drops its own file in one has made a decision on
//! their behalf that it was not asked to make.
//!
//! # Saving
//!
//! [`Config::save`] writes the whole file, as a complete file renamed over the
//! old one, and the caller is expected to call it after anything that changes
//! the arrangement — opening a workspace, opening or closing a tab, splitting.
//! Those are things a person does a few times a minute at most, so there is
//! nothing here worth batching: a debounce would be a timer to drive and would
//! still lose whatever happened in its last interval, and saving on the way out
//! loses everything if the process is killed rather than asked to stop. A save
//! that would write what is already there writes nothing at all, so calling it
//! after an action that changed nothing costs a comparison.
//!
//! # A file that cannot be read is not a file to write over
//!
//! Launching against a file this build cannot make sense of is reported, and
//! the file is then held: [`Config::save`] refuses until [`Config::overwrite`]
//! says somebody has been told. The alternative is the quiet one — start empty,
//! carry on, and replace a description of nine workspaces the first time
//! somebody opens a tab.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ids::{ShellId, TabId, WorkspaceId};
use crate::workspace::{Layout, MalformedLayout, Tab, Workspace, WorkspaceSettings, default_name};

mod tree;

#[cfg(test)]
mod tests;

pub use tree::TreeError;

/// The shape of the file this build writes and understands.
///
/// It is the first thing read and the first thing written, so a file from a
/// build that keeps its configuration differently is recognised as one before
/// any attempt is made to make sense of it. A field added within a version has
/// to be one an older build can lose: it will not understand it, and the next
/// time it saves it will write the file without it.
pub const VERSION: u32 = 1;

/// The variable naming the configuration file outright, for running a second
/// copy of this application against configuration of its own.
pub const CONFIG_VAR: &str = "SHEPHERD_CONFIG";

/// The variable naming the user's home directory.
const HOME_VAR: &str = "HOME";

/// The variable naming the base directory for a user's own configuration.
const CONFIG_HOME_VAR: &str = "XDG_CONFIG_HOME";

/// The variable Windows names a user's roaming application data with.
const APPDATA_VAR: &str = "APPDATA";

/// What this application's directory is called where the convention is a
/// lower-case one.
const DIR: &str = "shepherd";

/// What it is called where the convention is the application's own name.
const NAMED_DIR: &str = "Shepherd";

/// The file, inside whichever of those it turns out to be.
const FILE: &str = "config.toml";

/// Where a desktop application keeps a user's files, which is not the same
/// question on all three platforms.
///
/// This is one value rather than a `cfg` at the point where a path is built, so
/// that the rule for every platform can be read in one place and asserted from
/// a test running on any of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Convention {
    /// `~/Library/Application Support/Shepherd`.
    ApplicationSupport,
    /// `%APPDATA%\Shepherd`.
    RoamingAppData,
    /// `$XDG_CONFIG_HOME/shepherd`, or `~/.config/shepherd`.
    XdgConfigHome,
}

impl Convention {
    /// What this build's platform does.
    const HOST: Self = if cfg!(target_os = "macos") {
        Self::ApplicationSupport
    } else if cfg!(windows) {
        Self::RoamingAppData
    } else {
        Self::XdgConfigHome
    };
}

/// One user's configuration file.
///
/// It remembers what it last read or wrote, which is what lets a save that
/// would change nothing do nothing, and whether what it read was something it
/// could understand, which is what stops a save from replacing a file nobody
/// has looked at yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    path: PathBuf,
    written: Option<String>,
    held: bool,
}

impl Config {
    /// The file this machine's conventions put it at.
    ///
    /// `SHEPHERD_CONFIG` names it outright and wins; otherwise it is
    /// `~/Library/Application Support/Shepherd/config.toml` on macOS,
    /// `%APPDATA%\Shepherd\config.toml` on Windows, and
    /// `$XDG_CONFIG_HOME/shepherd/config.toml` — or `~/.config/shepherd` where
    /// that variable does not name an absolute path — everywhere else.
    ///
    /// A machine that names none of the directories its own convention is
    /// built on has nowhere to keep configuration, and is told so rather than
    /// given a relative path that would put a file wherever the application
    /// happened to be started from.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::resolve(Convention::HOST, |name| std::env::var_os(name))
            .map(Self::at)
            .ok_or(ConfigError::Nowhere)
    }

    /// The file at a path the caller has chosen.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            written: None,
            held: false,
        }
    }

    /// Where this configuration is kept.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What is in the file, or an empty layout when there is no file.
    ///
    /// A first launch has nothing to read and is not an error: nobody has
    /// opened anything yet, which is exactly what an empty layout says. Every
    /// other way of not being able to read it is reported, and holds the file
    /// against being written over.
    pub fn load(&mut self) -> Result<Layout, ConfigError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(problem) if problem.kind() == io::ErrorKind::NotFound => {
                self.written = None;
                self.held = false;
                return Ok(Layout::new());
            }
            Err(source) => {
                return Err(self.hold(ConfigError::Read {
                    path: self.path.clone(),
                    source,
                }));
            }
        };

        match self.read(&text) {
            Ok(layout) => {
                self.written = Some(text);
                self.held = false;
                Ok(layout)
            }
            Err(problem) => Err(self.hold(problem)),
        }
    }

    /// Writes `layout`, unless the file already says exactly that.
    ///
    /// The file is written whole, to a temporary file in its own directory and
    /// renamed over the old one, so that a process that stops halfway through
    /// leaves the previous configuration intact rather than half of a new one.
    pub fn save(&mut self, layout: &Layout) -> Result<(), ConfigError> {
        if self.held {
            return Err(ConfigError::Held {
                path: self.path.clone(),
            });
        }

        let text = toml::to_string_pretty(&SavedFile::of(layout))
            .map_err(|source| ConfigError::Encode { source })?;
        if self.written.as_deref() == Some(text.as_str()) {
            return Ok(());
        }

        self.write(&text).map_err(|source| ConfigError::Write {
            path: self.path.clone(),
            source,
        })?;
        self.written = Some(text);
        Ok(())
    }

    /// Allows a save over a file that could not be read.
    ///
    /// This is what somebody says once they have been shown what was wrong with
    /// it and have decided to carry on regardless. Whatever was in the file is
    /// gone at the next save, so it is deliberately not something that happens
    /// on its own.
    pub fn overwrite(&mut self) {
        self.held = false;
        self.written = None;
    }

    /// Whether a save would be refused, the file having been read and not
    /// understood.
    pub fn is_held(&self) -> bool {
        self.held
    }

    /// The layout `text` describes.
    fn read(&self, text: &str) -> Result<Layout, ConfigError> {
        let versioned: Versioned = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: self.path.clone(),
            source: Box::new(source),
        })?;
        match versioned.version {
            None => {
                return Err(ConfigError::Unversioned {
                    path: self.path.clone(),
                });
            }
            Some(version) if version != VERSION => {
                return Err(ConfigError::Version {
                    path: self.path.clone(),
                    version,
                });
            }
            Some(_) => {}
        }

        let file: SavedFile = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: self.path.clone(),
            source: Box::new(source),
        })?;
        self.rebuild(file)
    }

    /// The model a description of one comes back as.
    fn rebuild(&self, file: SavedFile) -> Result<Layout, ConfigError> {
        let mut workspaces = Vec::with_capacity(file.workspaces.len());
        for saved in file.workspaces {
            let id = WorkspaceId::from_raw(saved.id);
            let name = saved.name.unwrap_or_else(|| default_name(&saved.path));
            let at = format!("workspace {id} {name:?}");

            let mut tabs = Vec::with_capacity(saved.tabs.len());
            for saved in saved.tabs {
                let id = TabId::from_raw(saved.id);
                let name = saved.name.unwrap_or_default();
                let at = format!("{at}, tab {id} {name:?}");
                let tree =
                    tree::parse(&saved.layout).map_err(|source| ConfigError::Arrangement {
                        path: self.path.clone(),
                        at: at.clone(),
                        source,
                    })?;
                tabs.push(
                    Tab::restore(id, name, tree, saved.focused.map(ShellId::from_raw)).map_err(
                        |source| ConfigError::Describes {
                            path: self.path.clone(),
                            at,
                            source,
                        },
                    )?,
                );
            }

            let settings = WorkspaceSettings {
                devcontainer: saved.devcontainer,
            };
            workspaces.push(
                Workspace::restore(id, saved.path, name, settings, tabs).map_err(|source| {
                    ConfigError::Describes {
                        path: self.path.clone(),
                        at,
                        source,
                    }
                })?,
            );
        }

        Layout::restore(workspaces).map_err(|source| ConfigError::Describes {
            path: self.path.clone(),
            at: "the list of workspaces".to_owned(),
            source,
        })
    }

    /// Puts `text` where this configuration lives, whole.
    fn write(&self, text: &str) -> io::Result<()> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir)?;
        // The temporary file goes in the target's own directory because a
        // rename is only atomic within one filesystem, and anywhere else would
        // be a guess about how this machine is partitioned.
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(text.as_bytes())?;
        file.flush()?;
        file.persist(&self.path).map_err(io::Error::from)?;
        Ok(())
    }

    /// Records that the file could not be read, and hands the reason back.
    fn hold(&mut self, problem: ConfigError) -> ConfigError {
        self.held = true;
        self.written = None;
        problem
    }

    /// The file the conventions of `convention` put this application's
    /// configuration at, given a way to read the environment.
    ///
    /// A variable that is present but empty names nothing, and a base directory
    /// that is not absolute is ignored as its own specification requires.
    fn resolve(convention: Convention, var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
        let named = |name: &str| {
            var(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };

        if let Some(file) = named(CONFIG_VAR) {
            return Some(file);
        }

        let home = || named(HOME_VAR);
        let dir = match convention {
            Convention::ApplicationSupport => home()?
                .join("Library")
                .join("Application Support")
                .join(NAMED_DIR),
            Convention::RoamingAppData => named(APPDATA_VAR)?.join(NAMED_DIR),
            Convention::XdgConfigHome => {
                match named(CONFIG_HOME_VAR).filter(|base| base.is_absolute()) {
                    Some(base) => base.join(DIR),
                    None => home()?.join(".config").join(DIR),
                }
            }
        };
        Some(dir.join(FILE))
    }
}

/// Why a configuration file could not be read, or written.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// This machine names no directory to keep configuration in.
    #[error("this machine names no directory to keep configuration in")]
    Nowhere,
    /// The file is there and could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
    /// The file is not what this build writes.
    #[error("{} is not readable as configuration: {source}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// Where it stopped making sense.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// The file says nothing about which format it is in, so it is not one of
    /// this application's.
    #[error("{} does not say which format it is in", path.display())]
    Unversioned {
        /// The file.
        path: PathBuf,
    },
    /// The file is in a format this build does not know, which is very likely a
    /// later one.
    #[error(
        "{} is in format {version}, and this build understands format {VERSION}",
        path.display()
    )]
    Version {
        /// The file.
        path: PathBuf,
        /// What it says it is.
        version: u32,
    },
    /// A tab's arrangement of shells could not be read.
    #[error("{}: {at}: {source}", path.display())]
    Arrangement {
        /// The file.
        path: PathBuf,
        /// Which part of it.
        at: String,
        /// What is wrong with the line.
        #[source]
        source: TreeError,
    },
    /// The file describes something the model could not be.
    #[error("{}: {at}: {source}", path.display())]
    Describes {
        /// The file.
        path: PathBuf,
        /// Which part of it.
        at: String,
        /// What it says that cannot be true.
        #[source]
        source: MalformedLayout,
    },
    /// The layout could not be turned into a file's worth of text. Nothing an
    /// arrangement can hold does this except a workspace whose path is not
    /// valid Unicode.
    #[error("this layout cannot be written down: {source}")]
    Encode {
        /// What refused it.
        #[source]
        source: toml::ser::Error,
    },
    /// The file could not be written.
    #[error("cannot write {}: {source}", path.display())]
    Write {
        /// The file.
        path: PathBuf,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
    /// A save was refused because the file has not been read successfully.
    #[error("{} was not understood, and has not been written over", path.display())]
    Held {
        /// The file.
        path: PathBuf,
    },
}

/// The version, read on its own before anything is made of the rest.
#[derive(Debug, Deserialize)]
struct Versioned {
    version: Option<u32>,
}

/// A whole configuration file.
///
/// This and the three below are the entire serialized form. Everything in them
/// is structure — which folders, called what, arranged how — and there is
/// deliberately nothing about a process, a terminal or a screen.
#[derive(Debug, Serialize, Deserialize)]
struct SavedFile {
    version: u32,
    #[serde(default, rename = "workspace")]
    workspaces: Vec<SavedWorkspace>,
}

/// One workspace as it is written down.
#[derive(Debug, Serialize, Deserialize)]
struct SavedWorkspace {
    id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    path: PathBuf,
    #[serde(default)]
    devcontainer: bool,
    #[serde(default, rename = "tab")]
    tabs: Vec<SavedTab>,
}

/// One tab as it is written down.
#[derive(Debug, Serialize, Deserialize)]
struct SavedTab {
    id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    focused: Option<u32>,
    layout: String,
}

impl SavedFile {
    /// How `layout` is written down.
    fn of(layout: &Layout) -> Self {
        Self {
            version: VERSION,
            workspaces: layout
                .workspaces()
                .iter()
                .map(|workspace| SavedWorkspace {
                    id: workspace.id().raw(),
                    name: Some(workspace.name().to_owned()),
                    path: workspace.path().to_owned(),
                    devcontainer: workspace.settings().devcontainer,
                    tabs: workspace
                        .tabs()
                        .iter()
                        .map(|tab| SavedTab {
                            id: tab.id().raw(),
                            name: Some(tab.name().to_owned()),
                            focused: Some(tab.focused().raw()),
                            layout: tree::write(tab.tree()),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}
