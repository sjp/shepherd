//! Shells that run inside a workspace's development container rather than on
//! this machine.
//!
//! A project that describes a development container describes the machine its
//! work is meant to be done on: the toolchain, the services, the versions of
//! everything. A terminal opened for that project on the host is a terminal on
//! the wrong machine. So a workspace can be set to run its shells inside the
//! container instead, and this is what that setting does.
//!
//! # It is chosen, never assumed
//!
//! [`described`] answers whether a folder describes a container at all, and
//! that is *all* it is for: it says whether the choice is worth offering. What
//! turns the behaviour on is the workspace's own setting — see
//! [`WorkspaceSettings::devcontainer`] — because a folder having a container in
//! it is not the same as somebody wanting their terminals inside it. A
//! workspace set to use one is honoured whether or not a description was found,
//! and the command below says clearly what it could not find.
//!
//! # One command, and nothing underneath it
//!
//! Everything here goes through the `devcontainer` command: bringing a
//! container up, checking one is still up, and running a shell in it. Nothing
//! here talks to a container runtime directly. That is a real constraint — it
//! costs a process for questions a runtime would answer instantly — and it is
//! worth paying for, because the command is the thing that knows how to read a
//! project's own description of its container, and reimplementing that
//! understanding against a runtime's API would mean maintaining a second, worse
//! copy of it that drifts.
//!
//! # Crossing the boundary
//!
//! A process inside a container inherits nothing from the host, so the
//! environment a shell needs has to be handed over explicitly, as arguments:
//!
//! ```text
//! devcontainer exec --workspace-folder <folder> --remote-env NAME=value -- <shell>
//! ```
//!
//! Every variable [`ShellOptions::environment`] says a shell should have makes
//! that crossing, which is what keeps a shell in a container the same shell as
//! one on the host. The one that matters most is the correlation: without it,
//! an agent started in that shell is a session the application can see but
//! cannot place, and the whole point of a shell knowing its own name outside
//! this process is lost precisely where a person is least able to work out
//! where something is running.
//!
//! # It blocks, and it can block for minutes
//!
//! Bringing a container up may build an image. Nothing here is asynchronous and
//! nothing here has a timeout of its own — a build that is going to take four
//! minutes should take four minutes rather than be abandoned at some number
//! somebody picked — so a caller with a window to keep drawing must not call
//! any of it on the thread that draws.
//!
//! [`WorkspaceSettings::devcontainer`]: crate::workspace::WorkspaceSettings::devcontainer

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process;

use thiserror::Error;
use tracing::debug;

use crate::correlation::correlation_for;
use crate::ids::ShellAddress;
use crate::lookup::{PATH_VAR, look_up, search_path};
use crate::terminal::{Program, Shell, ShellOptions, SpawnError};
use crate::workspace::Workspace;

#[cfg(test)]
mod tests;

/// The command a container is brought up and run in through.
pub const COMMAND: &str = "devcontainer";

/// The directory a folder describes its development container in.
pub const CONFIG_DIR: &str = ".devcontainer";

/// The file a folder describes one in when it has no such directory.
pub const CONFIG_FILE: &str = ".devcontainer.json";

/// What a shell inside a container runs when nothing has said otherwise.
///
/// Not the login shell of whoever is at the keyboard: that is a fact about this
/// machine, and the container is a different machine which very likely does not
/// have it. This is the one shell a unix is required to have, which is the only
/// thing that can be assumed about a container nobody has described further.
pub const DEFAULT_PROGRAM: &str = "/bin/sh";

/// Bring the container up.
const UP: &str = "up";

/// Run something in it.
const EXEC: &str = "exec";

/// Which folder's container is meant.
const WORKSPACE_FOLDER: &str = "--workspace-folder";

/// One variable to give the process inside the container.
const REMOTE_ENV: &str = "--remote-env";

/// Everything after this is the command to run, not more options.
const END_OF_OPTIONS: &str = "--";

/// What is run in the container to find out whether there is one running.
///
/// The shell every unix has, asked to do nothing at all. It is the same
/// assumption [`DEFAULT_PROGRAM`] makes and no more: a container that cannot
/// answer this is a container that could not have hosted a shell either, so a
/// check that passes where a shell would fail would be a check worth nothing.
const NOTHING: [&str; 2] = ["-c", ":"];

/// What is reported for a command that failed without saying why.
const UNEXPLAINED: &str = "it gave no reason";

/// Whether `folder` describes a development container.
///
/// A directory holding the description, or the description alone at the top of
/// the folder — the two shapes a project may use. This is a signal and not a
/// decision: it says the choice is worth offering for this workspace, and
/// nothing more.
pub fn described(folder: &Path) -> bool {
    folder.join(CONFIG_DIR).is_dir() || folder.join(CONFIG_FILE).is_file()
}

/// What became of one run of the container command.
///
/// Refusing is not a failure of this application: a container that will not
/// start, a description with a mistake in it, a runtime that is not running are
/// all things the command is *supposed* to report, and it says so far better
/// than anything here could paraphrase. So what it said is carried rather than
/// replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It did what it was asked.
    Succeeded,
    /// It ran, and said no.
    Refused {
        /// What it exited with, where the platform said.
        status: Option<i32>,
        /// What it printed about why.
        said: String,
    },
}

impl Outcome {
    /// Whether it did what it was asked.
    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// Something that can run the container command.
///
/// [`Machine`] is the implementation that runs the command this machine has,
/// and is what everything outside tests uses. The trait exists because what is
/// on the other side of it is a process — slow, absent on some machines, and
/// with a great deal to say when it goes wrong — and because the questions
/// worth asking of the code here are about *what it asks for*, which is a
/// question that should not need a container runtime to answer.
pub trait Containers {
    /// Where the container command is on this machine.
    ///
    /// Answered separately from running one because a shell inside a container
    /// is started by this application's own terminal machinery rather than
    /// here: the command has to be named to it, and naming the path it was
    /// found at means what runs is what was found rather than whatever a
    /// `PATH` resolves to a moment later.
    fn command(&self) -> Result<PathBuf, ContainerError>;

    /// Runs the command with `args`, waiting for it to finish.
    ///
    /// The error is for a command that could not be run at all. A command that
    /// ran and refused is an [`Outcome`], because that is an answer.
    fn run(&self, args: &[String]) -> Result<Outcome, ContainerError>;
}

/// This machine, asked to run the container command it has.
///
/// It looks the command up itself rather than trusting a name to a shell, so
/// that a machine without one says so before anything is started rather than
/// through whatever error a terminal produces for a program that is not there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Machine {
    path: Vec<PathBuf>,
}

impl Machine {
    /// This machine, with commands looked for where it says they are.
    pub fn from_env() -> Self {
        Self::searching(search_path(std::env::var_os(PATH_VAR)))
    }

    /// A machine that looks for commands in `directories` and nowhere else.
    pub fn searching(directories: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self {
            path: directories.into_iter().map(Into::into).collect(),
        }
    }

    /// Where the command is, if it is anywhere this machine looks.
    fn look_up(&self) -> Option<PathBuf> {
        look_up(&self.path, COMMAND)
    }
}

impl Containers for Machine {
    fn command(&self) -> Result<PathBuf, ContainerError> {
        self.look_up().ok_or(ContainerError::NotInstalled)
    }

    fn run(&self, args: &[String]) -> Result<Outcome, ContainerError> {
        let command = self.command()?;
        debug!(command = %line(args), "asking about a development container");

        // What it says is captured rather than printed, because this runs
        // behind a window: the only place a person will ever see it is in
        // whatever this application shows them, and a message written to a
        // stream nobody is reading is a message lost.
        let output = process::Command::new(&command)
            .args(args)
            .stdin(process::Stdio::null())
            .output()
            .map_err(|source| ContainerError::CannotRun {
                command: line(args),
                source,
            })?;

        Ok(match output.status.success() {
            true => Outcome::Succeeded,
            false => Outcome::Refused {
                status: output.status.code(),
                said: said(&output),
            },
        })
    }
}

/// Why a development container could not be used.
#[derive(Debug, Error)]
pub enum ContainerError {
    /// The command is not on this machine.
    ///
    /// Reported rather than worked around. Starting a shell here instead would
    /// produce a working terminal on the wrong machine, and every agent run in
    /// it would be attributed to a shell that is not where the work is
    /// happening — a wrong answer, arrived at silently, which is worse than no
    /// answer.
    #[error(
        "`{COMMAND}` is not installed on this machine, and a workspace whose shells run in a development container needs it"
    )]
    NotInstalled,
    /// The command is there and would not run.
    #[error("cannot run `{command}`: {source}")]
    CannotRun {
        /// What was being run.
        command: String,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
    /// The command ran, and the container did not come up.
    #[error("`{command}` did not bring up the development container in {}: {said}", folder.display())]
    NotUp {
        /// The folder whose container it is.
        folder: PathBuf,
        /// What was being run.
        command: String,
        /// What it exited with, where the platform said.
        status: Option<i32>,
        /// What it said about why.
        said: String,
    },
    /// A path that has to be named to the command is not text.
    ///
    /// Refused rather than approximated: the lossy spelling of a path names a
    /// different folder, or a command that is not there, and either would be
    /// obeyed rather than reported.
    #[error("{} cannot be named to `{COMMAND}`: it is not text", path.display())]
    Unnameable {
        /// The path.
        path: PathBuf,
    },
}

/// Why a shell could not be started.
#[derive(Debug, Error)]
pub enum StartError {
    /// The workspace's development container could not be used.
    #[error(transparent)]
    Container(#[from] ContainerError),
    /// The terminal, or the process on the far side of it, would not start.
    #[error(transparent)]
    Shell(#[from] SpawnError),
}

/// One workspace's development container.
///
/// It remembers whether it has brought the container up, which is what keeps
/// the second shell in a workspace from paying for what the first one already
/// did. That memory is never trusted on its own: a container can be stopped by
/// anything on the machine at any time, so what the memory buys is a cheap
/// question instead of an expensive one, not a question skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Devcontainer {
    folder: PathBuf,
    up: bool,
}

impl Devcontainer {
    /// The container described by `folder`.
    pub fn at(folder: impl Into<PathBuf>) -> Self {
        Self {
            folder: folder.into(),
            up: false,
        }
    }

    /// The folder whose container this is.
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// Whether that folder describes a container. See [`described`].
    pub fn is_described(&self) -> bool {
        described(&self.folder)
    }

    /// Whether the container was up when it was last asked about.
    pub fn is_up(&self) -> bool {
        self.up
    }

    /// Makes sure there is a container running to start shells in.
    ///
    /// The first time, that means bringing one up. Afterwards it means asking
    /// whether the one that was brought up is still there, and bringing it up
    /// again if it is not.
    pub fn ensure_up(&mut self, containers: &impl Containers) -> Result<(), ContainerError> {
        if self.up {
            if containers.run(&self.probe()?)?.succeeded() {
                return Ok(());
            }
            debug!(folder = %self.folder.display(), "the development container has stopped");
            self.up = false;
        }

        let args = self.bring_up()?;
        match containers.run(&args)? {
            Outcome::Succeeded => {
                self.up = true;
                Ok(())
            }
            Outcome::Refused { status, said } => Err(ContainerError::NotUp {
                folder: self.folder.clone(),
                command: line(&args),
                status,
                said,
            }),
        }
    }

    /// How a shell for `address` is started inside the container.
    ///
    /// The options a caller assembled for a shell on this machine, turned into
    /// options that run the same shell in the container: what they asked to run
    /// becomes what the command is told to run, and the environment they asked
    /// for is carried over as arguments, because nothing crosses that boundary
    /// by itself.
    pub fn options(
        &self,
        address: ShellAddress,
        options: &ShellOptions,
        containers: &impl Containers,
    ) -> Result<ShellOptions, ContainerError> {
        let command = containers.command()?;
        let named = text(&command)?.to_owned();
        let correlation = correlation_for(address.workspace, address.shell);
        let inside = options
            .chosen_program()
            .cloned()
            .unwrap_or_else(|| Program::new(DEFAULT_PROGRAM));
        let args = self.exec(&options.environment(&correlation), &inside)?;

        Ok(options.clone().program(Program::new(named).with_args(args)))
    }

    /// Starts a shell for `address` inside the container, bringing the
    /// container up first if it is not already.
    pub fn spawn(
        &mut self,
        address: ShellAddress,
        options: &ShellOptions,
        containers: &impl Containers,
    ) -> Result<Shell, StartError> {
        self.ensure_up(containers)?;
        let options = self.options(address, options, containers)?;
        Ok(Shell::spawn(address, &options)?)
    }

    /// The arguments that bring the container up.
    fn bring_up(&self) -> Result<Vec<String>, ContainerError> {
        Ok(vec![
            UP.to_owned(),
            WORKSPACE_FOLDER.to_owned(),
            text(&self.folder)?.to_owned(),
        ])
    }

    /// The arguments that ask whether there is a container to run in.
    fn probe(&self) -> Result<Vec<String>, ContainerError> {
        self.exec(
            &BTreeMap::new(),
            &Program::new(DEFAULT_PROGRAM).with_args(NOTHING),
        )
    }

    /// The arguments that run `program` in the container with `env` set.
    fn exec(
        &self,
        env: &BTreeMap<String, String>,
        program: &Program,
    ) -> Result<Vec<String>, ContainerError> {
        let mut args = vec![
            EXEC.to_owned(),
            WORKSPACE_FOLDER.to_owned(),
            text(&self.folder)?.to_owned(),
        ];
        for (name, value) in env {
            args.push(REMOTE_ENV.to_owned());
            args.push(format!("{name}={value}"));
        }
        args.push(END_OF_OPTIONS.to_owned());
        args.push(program.program().to_owned());
        args.extend(program.args().iter().cloned());
        Ok(args)
    }
}

/// Where one workspace's shells are started.
///
/// A workspace answers this once, from what somebody chose about it, and then
/// every shell opened in it is started the same way. It is a type rather than a
/// boolean checked at each call site because the container half carries state —
/// what it already knows about the container being up — and because a call site
/// that has this in its hand cannot forget to bring a container up before
/// starting a shell in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shells {
    /// Started here, in the folder the workspace is for.
    ThisMachine,
    /// Started inside the workspace's development container.
    Container(Devcontainer),
}

impl Shells {
    /// Where `workspace` has been set to run its shells.
    pub fn for_workspace(workspace: &Workspace) -> Self {
        match workspace.settings().devcontainer {
            true => Self::Container(Devcontainer::at(workspace.path())),
            false => Self::ThisMachine,
        }
    }

    /// The container shells are started in, for a workspace that uses one.
    pub fn container(&self) -> Option<&Devcontainer> {
        match self {
            Self::ThisMachine => None,
            Self::Container(container) => Some(container),
        }
    }

    /// Starts a shell for `address`.
    ///
    /// `containers` is untouched for a workspace whose shells run here, which
    /// is most of them: nothing about a machine without the container command
    /// on it affects an ordinary workspace.
    pub fn start(
        &mut self,
        address: ShellAddress,
        options: &ShellOptions,
        containers: &impl Containers,
    ) -> Result<Shell, StartError> {
        match self {
            Self::ThisMachine => Ok(Shell::spawn(address, options)?),
            Self::Container(container) => container.spawn(address, options, containers),
        }
    }
}

/// A path as it is handed to the command, or a refusal if it is not text.
fn text(path: &Path) -> Result<&str, ContainerError> {
    path.to_str().ok_or_else(|| ContainerError::Unnameable {
        path: path.to_owned(),
    })
}

/// How a run of the command would be typed, for a message about one that went
/// wrong.
fn line(args: &[String]) -> String {
    let mut line = COMMAND.to_owned();
    for arg in args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// What a command that refused said about why.
///
/// Its diagnostics first and its ordinary output second, because a command that
/// failed says why on the former — but one that says it on the latter is still
/// saying it, and dropping the message because it arrived on the other stream
/// would leave a person with nothing.
fn said(output: &process::Output) -> String {
    for stream in [&output.stderr, &output.stdout] {
        let text = String::from_utf8_lossy(stream);
        let text = text.trim();
        if !text.is_empty() {
            return text.to_owned();
        }
    }
    UNEXPLAINED.to_owned()
}
