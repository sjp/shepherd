//! Putting the event bus inside the containers this application's shells run
//! in.
//!
//! A shell started inside a development container is a shell on another
//! machine, and that machine has none of what makes an agent visible here: no
//! copy of the bus's command, and no hooks wired into whatever coding agents
//! are installed in it. An agent started in such a shell runs perfectly well
//! and is reported by nobody. So when a container is brought up to run shells
//! in, the bus is put inside it.
//!
//! # It is the command anybody could type
//!
//! Everything here runs `agentbus install docker <container>` and reads what it
//! said. Nothing here knows how a binary gets into a container, which agents
//! are in there, or what a hook file looks like — all of that is the bus's, and
//! the one thing handed over the boundary is the name of a container, which was
//! already in hand from bringing it up. Nothing was added to the bus for this:
//! the same line typed into a shell does the same thing.
//!
//! # Once per container, and again when it is a different container
//!
//! The command is idempotent, so running it twice costs time and changes
//! nothing. [`Provisioning`] is what spends that time only once, and it counts
//! by *container* rather than by workspace or by folder — which is what makes a
//! container that has been restarted, and is therefore a new container with a
//! new name, provisioned again without anything having to notice the restart.
//!
//! # Failing is not fatal
//!
//! A container the bus could not be put into is still a container shells run
//! in, and those shells still work. What is lost is that agents started in
//! there will not be reported, and that is worth saying out loud — see
//! [`Standing::Unreported`] — rather than worth refusing to open a workspace
//! over.
//!
//! # It blocks
//!
//! The command copies a binary into a container and edits files in it. Like
//! everything else that crosses that boundary, it takes as long as it takes and
//! must not be run on a thread that is drawing a window.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use thiserror::Error;
use tracing::debug;

use crate::daemon::COMMAND;
use crate::ids::WorkspaceId;
use crate::lookup::{PATH_VAR, look_up, search_path};

#[cfg(test)]
mod tests;

/// Put this program somewhere that is not this machine.
const INSTALL: &str = "install";

/// And the somewhere: a container on this machine, named.
const DOCKER: &str = "docker";

/// What is reported for a command that failed without saying why.
const UNEXPLAINED: &str = "it gave no reason";

/// What became of one attempt to put the bus into a container.
///
/// A refusal is carried rather than paraphrased, for the same reason the
/// container command's is: the command that ran knows why it would not, and
/// nothing here could say it better.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provisioned {
    /// The bus is in there.
    Installed,
    /// It ran, and said no.
    Refused {
        /// What it exited with, where the platform said.
        status: Option<i32>,
        /// What it said about why.
        said: String,
    },
}

impl Provisioned {
    /// Whether the bus went in.
    pub fn installed(&self) -> bool {
        matches!(self, Self::Installed)
    }
}

/// Why the bus could not be put into a container at all.
#[derive(Debug, Error)]
pub enum ProvisionError {
    /// The command is not on this machine.
    #[error("`{COMMAND}` is not installed on this machine")]
    NotInstalled,
    /// The command is there and would not run.
    #[error("cannot run `{}`: {source}", path.display())]
    CannotRun {
        /// What was being run.
        path: PathBuf,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
}

/// Something that can put the bus into a container.
///
/// [`Bus`] is the implementation that runs the command this machine has, and is
/// what everything outside tests uses. The trait exists for the same reason the
/// container command has one: what is behind it is a slow process that is
/// absent on some machines, and the question worth asking of the code around it
/// is *which containers it is run for, and how often*, which should not need a
/// container runtime to answer.
pub trait Provisions {
    /// Puts the bus into `container`, and says what became of it.
    ///
    /// The error is for a command that could not be run at all. A command that
    /// ran and refused is a [`Provisioned`], because that is an answer.
    fn provision(&self, container: &str) -> Result<Provisioned, ProvisionError>;
}

/// The bus's command as this machine has it, asked to install itself elsewhere.
///
/// It looks the command up itself rather than trusting a name to a shell, so
/// that a machine without one says so rather than reporting whatever a shell
/// makes of a command that is not there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bus {
    path: Vec<PathBuf>,
}

impl Bus {
    /// This machine, with commands looked for where it says they are.
    pub fn from_env() -> Self {
        Self::searching(search_path(std::env::var_os(PATH_VAR)))
    }

    /// A machine that looks for the command in `directories` and nowhere else.
    pub fn searching(directories: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self {
            path: directories.into_iter().map(Into::into).collect(),
        }
    }

    /// Where the bus's command is, if it is anywhere this machine looks.
    pub fn command(&self) -> Result<PathBuf, ProvisionError> {
        look_up(&self.path, COMMAND).ok_or(ProvisionError::NotInstalled)
    }
}

impl Provisions for Bus {
    fn provision(&self, container: &str) -> Result<Provisioned, ProvisionError> {
        let path = self.command()?;
        debug!(container, "putting the bus into a development container");

        // Captured rather than printed, because this runs behind a window:
        // whatever it has to say has to reach a person through what the window
        // shows or through this application's own diagnostics, and a message
        // written to a stream nobody is reading is a message lost.
        let output = Command::new(&path)
            .arg(INSTALL)
            .arg(DOCKER)
            .arg(container)
            .stdin(Stdio::null())
            .output()
            .map_err(|source| ProvisionError::CannotRun {
                path: path.clone(),
                source,
            })?;

        Ok(match output.status.success() {
            true => Provisioned::Installed,
            false => Provisioned::Refused {
                status: output.status.code(),
                said: said(&output),
            },
        })
    }
}

/// What a workspace's row has to say about the container its shells run in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// The bus is being put into it, and nothing is known yet.
    UnderWay,
    /// The bus is in there: an agent started in that container will be
    /// reported like any other.
    Ready,
    /// It could not be put in, so agents started in that container will not be
    /// reported. The shells still work; nothing here can see what runs in them.
    Unreported,
}

/// Which container each workspace's shells run in, and what became of putting
/// the bus into each of those containers.
///
/// Keyed by container rather than by workspace, because the container is what
/// the bus goes into: two workspaces sharing one are one installation, and one
/// workspace whose container has been restarted is two.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provisioning {
    done: BTreeMap<String, Standing>,
    using: BTreeMap<WorkspaceId, String>,
}

impl Provisioning {
    /// Nothing provisioned yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `workspace`'s shells run in `container`, and answers
    /// whether the bus still has to be put into it.
    ///
    /// False for a container this has already been asked about, including one
    /// it is still being put into: the point of asking is to do the work once,
    /// and a second caller arriving while the first is still running is exactly
    /// the case a count of finished attempts would get wrong.
    pub fn using(&mut self, workspace: WorkspaceId, container: &str) -> bool {
        self.using.insert(workspace, container.to_owned());
        let known = self.done.contains_key(container);
        if !known {
            self.done.insert(container.to_owned(), Standing::UnderWay);
        }
        !known
    }

    /// Records what became of putting the bus into `container`.
    pub fn provisioned(&mut self, container: &str, installed: bool) {
        let standing = match installed {
            true => Standing::Ready,
            false => Standing::Unreported,
        };
        self.done.insert(container.to_owned(), standing);
    }

    /// What is to be said about `workspace`'s container, if it has one.
    pub fn of(&self, workspace: WorkspaceId) -> Option<Standing> {
        self.done.get(self.using.get(&workspace)?).copied()
    }

    /// Forgets which container a workspace that has been closed was using.
    ///
    /// What became of that container is kept: it is a fact about a container
    /// that is still running, and a workspace opened on it again should not
    /// pay for the installation a second time.
    pub fn forget(&mut self, workspace: WorkspaceId) {
        self.using.remove(&workspace);
    }
}

/// What a command that refused said about why.
///
/// Its diagnostics first and its ordinary output second: a command that failed
/// says why on the former, and one that says it on the latter is still saying
/// it.
fn said(output: &std::process::Output) -> String {
    for stream in [&output.stderr, &output.stdout] {
        let text = String::from_utf8_lossy(stream);
        let text = text.trim();
        if !text.is_empty() {
            return text.to_owned();
        }
    }
    UNEXPLAINED.to_owned()
}
