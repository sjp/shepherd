//! Reaching a container on this machine.
//!
//! Everything here is one program driven from the outside. The daemon does not
//! link a Docker client and does not speak to the socket: it runs `docker`, the
//! same command a person at a shell would run, and reads what it prints. That
//! is a deliberate trade of a little speed for a great deal of compatibility —
//! the API version negotiation, the socket's location, the credentials, the
//! rootless variants and whatever a replacement for `docker` does differently
//! are all somebody else's problem as long as the command line is the one
//! everybody already implements.
//!
//! # Why a container is cheap to be wrong about
//!
//! A container's filesystem is thrown away and rebuilt, its `/tmp` is nobody's
//! long-term storage, and a person who wanted what is inside it kept would not
//! have put it in a container. So the policy here is the opposite of the one a
//! transport reaching somebody's actual machine has to take: a copy of this
//! program is written into `/tmp` without asking, and the hooks that make the
//! agents in there report are installed without asking, because both are undone
//! by the container going away and neither can damage anything a person would
//! miss.
//!
//! # What is named and what is identity
//!
//! A container has a name, which is what people use and what Docker will
//! cheerfully give to a different container tomorrow, and an id, which is what
//! it *is*. Commands are sent to whichever of the two somebody named, because
//! that is what they asked for and Docker resolves both; what gets written down
//! as the identity of the far end is always the full id, asked for once and
//! remembered, because that is the only one two views of one container are
//! guaranteed to agree on.

mod containers;
mod listing;
mod project;

#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use agentbus_protocol::OriginHop;
use tracing::{debug, info, warn};

use super::bootstrap;
use super::transport::{Backoff, Error, Running, Transport};

pub use containers::Containers;
pub use listing::{Listed, listed};
pub use project::root;

/// The environment variable that names the command to drive containers with.
///
/// An affordance for the things that answer to the same command line without
/// being Docker — `podman`, `nerdctl` — which is the whole of what supporting
/// them takes as long as they keep doing that.
pub const BIN_VAR: &str = "AGENTBUS_DOCKER_BIN";

/// The command driven when nothing says otherwise.
pub const DEFAULT_BIN: &str = "docker";

/// The word a declaration uses for one of these endpoints.
pub const NAME: &str = super::targets::DOCKER;

/// The kind of boundary this transport crosses, as it is stamped on everything
/// relayed through it. The protocol names it, because it is a word subscribers
/// read rather than one this module is free to choose.
pub const KIND: &str = OriginHop::CONTAINER;

/// Where a copy of this program is written inside a container.
///
/// A directory under `/tmp` because a container that was restarted needs a
/// daemon restarted in it anyway, so there is nothing to gain by putting the
/// copy anywhere that survives. The version in the name means a stale copy is
/// never mistaken for a current one, and it makes the write atomic for nothing:
/// nothing refers to that path until the file is whole, so a hook cannot exec
/// half of one. Which directory exactly is the container's answer rather than
/// this program's — see [`Container::landing`].
///
/// The name kept here is where copies used to go, flat and shared between
/// whoever was inside: an image is often several users, and a container built
/// from one that a release provisioned still has them lying about.
const LEGACY_DIR: &str = "/tmp";

/// The prefix every copy this program writes into a container shares.
const INSTALL_PREFIX: &str = "agentbus-";

/// How long to wait before reaching a container again after the last attempt
/// broke.
///
/// Short, because there is nothing between here and there: no network, no
/// authentication, no round trip worth the name. What is usually being waited
/// for is a container that is coming back up, and the cost of asking again too
/// soon is one process that exits immediately.
const BACKOFF: Backoff = Backoff {
    initial: Duration::from_secs(1),
    max: Duration::from_secs(10),
    multiplier: 2.0,
    jitter: 0.2,
};

/// Who puts the hooks into the agents inside a container.
///
/// The two answers are for the two kinds of caller. Something that found a
/// container by looking has nobody in front of it and nothing else that will
/// ever do this, so it does it itself and logs how it went. Something a person
/// ran has both, and doing it underneath them would be the same work twice with
/// the half nobody can see being the half that goes wrong quietly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wiring {
    /// This transport does it, as soon as there is a daemon in there for the
    /// agents to report to.
    Itself,
    /// Whoever asked for the connection does it, and says what happened.
    Caller,
}

/// The `docker` command line, as this daemon drives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Docker {
    binary: PathBuf,
}

impl Docker {
    /// The command this machine's environment says to use.
    ///
    /// An empty value counts as unset, for the reason every variable in this
    /// program treats one that way: a variable that is present and says nothing
    /// names no command, and taking it at its word would mean trying to run the
    /// empty string.
    pub fn resolve() -> Self {
        Self::named(
            std::env::var_os(BIN_VAR)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| OsString::from(DEFAULT_BIN)),
        )
    }

    /// The command at a path or name the caller chose.
    pub fn named(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    /// What is being run, for saying so.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// The container this transport reaches, named by whichever of its name and
    /// its id somebody used.
    pub fn container(&self, reference: impl Into<String>) -> Container {
        let reference = reference.into();
        Container {
            docker: self.clone(),
            label: reference.clone(),
            reference,
            id: Mutex::new(None),
            installed: Mutex::new(None),
            landing: OnceLock::new(),
            wiring: Wiring::Itself,
        }
    }

    /// The same container, called something other than the word used to reach
    /// it — for one that was found rather than named, where the name is known
    /// alongside whatever is being used to address it.
    fn calling(&self, reference: impl Into<String>, label: impl Into<String>) -> Container {
        Container {
            docker: self.clone(),
            reference: reference.into(),
            label: label.into(),
            id: Mutex::new(None),
            installed: Mutex::new(None),
            landing: OnceLock::new(),
            wiring: Wiring::Itself,
        }
    }

    /// Runs one whole `docker` command and collects everything it said.
    ///
    /// For the commands that answer and finish. The one that does not — the
    /// subscription at the far end, which lasts as long as the attachment — is
    /// started through [`Running`] instead, and is the only reason both exist.
    fn output(&self, args: &[&str]) -> io::Result<Output> {
        Command::new(&self.binary)
            .args(args)
            .stdin(Stdio::null())
            .output()
    }
}

/// One container, and the way in to it.
#[derive(Debug)]
pub struct Container {
    docker: Docker,
    /// What commands are addressed to: a name or an id, whichever was used.
    reference: String,
    /// What it is called when telling somebody about it.
    label: String,
    /// Its full id, once anything has asked.
    id: Mutex<Option<String>>,
    /// The version whose hooks have been put inside it, once any have.
    installed: Mutex<Option<String>>,
    /// Where it said a borrowed copy of this program goes, once it has been
    /// asked.
    landing: OnceLock<String>,
    /// Whose job the hooks inside it are.
    wiring: Wiring,
}

impl Container {
    /// The same container, with the agents inside it left to whoever asked for
    /// this to wire up.
    ///
    /// For a caller that is going to run the installation itself and relay what
    /// it said, which is what anything with a person in front of it should do:
    /// an account of what happened to each agent is worth far more than a line
    /// in a log nobody is reading, and it is the only way a failure in there
    /// becomes a failure of the command they ran.
    #[must_use]
    pub fn wired_by_hand(mut self) -> Self {
        self.wiring = Wiring::Caller;
        self
    }

    /// What every command to this container begins with.
    ///
    /// `-i` always and `-t` never: what is on the other end of this is a pipe
    /// this program reads, not a terminal, and asking for one where there is
    /// none is how a command that works by hand fails under a daemon.
    fn exec(&self, command: &str, args: &[&str]) -> Vec<String> {
        let mut argv = vec![
            "exec".to_owned(),
            "-i".to_owned(),
            self.reference.clone(),
            command.to_owned(),
        ];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        argv
    }

    /// Asks Docker what this container's full id is, and remembers it.
    ///
    /// Asked at most once per attempt to reach the container, rather than
    /// whenever somebody wants the identity, because a container that is not
    /// there answers this as slowly as it answers anything and the question
    /// would otherwise be asked on every pass of a loop that runs all day.
    fn identify(&self) {
        if self
            .id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
        {
            return;
        }
        let Ok(output) = self
            .docker
            .output(&["inspect", "-f", "{{.Id}}", &self.reference])
        else {
            return;
        };
        if !output.status.success() {
            return;
        }
        let said = String::from_utf8_lossy(&output.stdout);
        let id = said.trim();
        if !id.is_empty() {
            debug!(container = self.label, id, "that is what it is");
            *self.id.lock().unwrap_or_else(PoisonError::into_inner) = Some(id.to_owned());
        }
    }

    /// Where a borrowed copy of this program goes inside this container, made
    /// sure of and remembered.
    ///
    /// Asked of the container rather than assumed, for the same reason its id
    /// is: only the far end knows, and the answer does not change while it is
    /// running. It is settled by the same fragment the search script is built
    /// from, so what is written and what the next search looks for cannot come
    /// apart — and the fragment makes the directory as well, which is what
    /// lets `docker cp` be given a path whose parent it would not create.
    ///
    /// A container that will not answer gets the directory copies used to go
    /// in. There is nowhere in the answer to put a failure; a copy that lands
    /// where the search does not look still fails the version check, which is
    /// the only thing that licenses running anything in there.
    fn landing(&self) -> &str {
        self.landing.get_or_init(|| {
            let argv = self.exec("sh", &["-c", bootstrap::PROBE]);
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            match self.docker.output(&argv) {
                Ok(output) if output.status.success() => {
                    let said = String::from_utf8_lossy(&output.stdout);
                    match said.trim() {
                        "" => LEGACY_DIR.to_owned(),
                        landing => landing.to_owned(),
                    }
                }
                Ok(output) => {
                    debug!(
                        container = self.label,
                        status = %output.status,
                        said = %String::from_utf8_lossy(&output.stderr).trim(),
                        "that container would not say where a copy of agentbus should go"
                    );
                    LEGACY_DIR.to_owned()
                }
                Err(error) => {
                    debug!(
                        container = self.label,
                        %error, "cannot ask that container where a copy of agentbus should go"
                    );
                    LEGACY_DIR.to_owned()
                }
            }
        })
    }

    /// Puts this program's hooks into the agents inside the container, so that
    /// what they do is reported by the daemon now running in there.
    ///
    /// Nothing asks first, and nothing is put back. Writing a hook into
    /// somebody's own machine is a thing to be careful with; writing one into a
    /// container is not, because the file it lands in was made by an image and
    /// goes away with it — and without this an agent started in there is
    /// invisible until somebody thinks to run a command inside a container they
    /// may not know exists.
    fn install(&self, version: &str) {
        let path = self.install_path(version);
        let argv = self.exec(&path, &["install"]);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        match self.docker.output(&argv) {
            Ok(output) if output.status.success() => {
                info!(
                    container = self.label,
                    version, "the agents in there now report to this bus"
                );
                *self
                    .installed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(version.to_owned());
            }
            // Kept, not fatal. What has already been established is a daemon in
            // there whose stream this side is merging, and that is worth having
            // whether or not any agent inside was wired up to it.
            Ok(output) => warn!(
                container = self.label,
                status = %output.status,
                said = %String::from_utf8_lossy(&output.stderr).trim(),
                "cannot put the hooks into that container; attached to it anyway"
            ),
            Err(error) => warn!(
                container = self.label,
                %error, "cannot put the hooks into that container; attached to it anyway"
            ),
        }
    }

    /// Takes this program's hooks back out of the container and removes every
    /// copy of it that was written there.
    ///
    /// The glob is the point of running a shell for this: what has to go is not
    /// the copy of the version running now but every copy any version ever put
    /// there, and only the container knows which those are.
    pub fn uninstall(&self, version: &str) -> Result<(), Error> {
        let path = self.install_path(version);
        let removing = format!(
            "{}\nfor f in \"$landing\"/{INSTALL_PREFIX}* {LEGACY_DIR}/{INSTALL_PREFIX}*; do \
             [ -f \"$f\" ] || continue; rm -f -- \"$f\"; done",
            bootstrap::LANDING
        );
        for argv in [
            self.exec(&path, &["uninstall"]),
            self.exec("sh", &["-c", &removing]),
        ] {
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            let output = self.docker.output(&argv).map_err(|source| Error::Run {
                label: self.label.clone(),
                command: self.docker.binary.display().to_string(),
                source,
            })?;
            if !output.status.success() {
                return Err(Error::Failed {
                    label: self.label.clone(),
                    command: argv.join(" "),
                    status: output.status,
                    said: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
        }
        Ok(())
    }
}

impl Transport for Container {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn identity(&self) -> Option<String> {
        self.id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn install_path(&self, version: &str) -> String {
        format!("{}/{INSTALL_PREFIX}{version}", self.landing())
    }

    fn run(&self, command: &str, args: &[&str], stdin: Option<&str>) -> Result<Running, Error> {
        // Here rather than anywhere else because this is the one call every way
        // of reaching a container goes through, and it runs when something is
        // about to be done rather than when somebody is about to be told.
        self.identify();
        let argv = self.exec(command, args);
        let mut process = Command::new(&self.docker.binary);
        process.args(&argv);
        Running::spawn(&mut process, stdin).map_err(|source| Error::Run {
            label: self.label.clone(),
            command: self.docker.binary.display().to_string(),
            source,
        })
    }

    /// Copies a file in, and leaves making it runnable to whoever asked.
    ///
    /// `docker cp` carries a file's mode across, but a caller cannot rely on
    /// that being true of every transport, so the provisioner sends its own
    /// `chmod` regardless. Sending a second one here would be the same command
    /// twice.
    fn copy_in(&self, local: &Path, remote: &str) -> Result<(), Error> {
        let there = format!("{}:{remote}", self.reference);
        let output = self
            .docker
            .output(&["cp", &local.display().to_string(), &there])
            .map_err(|source| Error::Copy {
                label: self.label.clone(),
                local: local.to_owned(),
                remote: remote.to_owned(),
                source,
            })?;
        match output.status.success() {
            true => Ok(()),
            false => Err(Error::Failed {
                label: self.label.clone(),
                command: format!("{} cp", self.docker.binary.display()),
                status: output.status,
                said: String::from_utf8_lossy(&output.stderr).into_owned(),
            }),
        }
    }

    fn backoff(&self) -> Backoff {
        BACKOFF
    }

    /// Wires up the agents inside as soon as there is a daemon in there for
    /// them to report to, and again whenever the version changes.
    ///
    /// It has to be again: the hooks name the copy by its full path, the path
    /// carries the version, and a hook naming a path nothing is at would report
    /// nothing at all.
    ///
    /// Unless whoever asked for the connection said they would do it — see
    /// [`Container::wired_by_hand`] — in which case doing it here would be the
    /// same installation twice, and the copy of it that nobody can see is the
    /// one that fails without saying so.
    fn established(&self, version: &str) {
        if self.wiring == Wiring::Caller {
            return;
        }
        let done = self
            .installed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_deref()
            == Some(version);
        if !done {
            self.install(version);
        }
    }
}
