//! Finding the containers on this machine that are worth attaching to.
//!
//! Docker keeps an authoritative list of what is running here, so there is
//! nothing to declare and nothing to guess: the containers that matter are the
//! ones that are up and carry the label the devcontainer tooling puts on
//! everything it builds. Asking costs one short-lived process every quarter of
//! a minute, which is cheap enough that the sweep can be the only mechanism —
//! a container that appears is attached on the next look and one that goes away
//! is let go of on the one after, with nothing watching for events in between.
//!
//! # Compose, and why every running container is attached
//!
//! Several containers may name one project: a Compose stack builds an app
//! container and a database container from the same directory, and both carry
//! the same label. Each is a machine of its own with its own filesystem, so
//! each may have an agent running in it, and there is no sense in which one of
//! them is the project's container. So all of the running ones are attached and
//! the stopped ones are not, which needs no rule of its own beyond that.
//!
//! # What happens when there is no Docker
//!
//! Nothing, loudly once and quietly thereafter. A machine without Docker
//! installed, a Docker whose daemon is not running, and a user who is not in
//! the group that may talk to it are all ordinary and none of them is this
//! program's business to fix. The transport says so once and then looks every
//! minute instead of every fifteen seconds, so that installing Docker later
//! costs a minute rather than a restart.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tracing::{debug, info};

use super::super::discover::{Context, Discovery, Found};
use super::super::targets::HOME_VAR;
use super::super::transport::Transport;
use super::listing::{self, Listed};
use super::{Docker, NAME, project};

/// How often the containers on this machine are looked at.
///
/// Slow, because nothing depends on the latency: an agent that starts inside a
/// container is reported by the daemon in there the moment it starts, and this
/// interval only decides how long it takes for a *container* that has just
/// appeared to be noticed at all.
const SWEEP: Duration = Duration::from_secs(15);

/// How often Docker is tried again once it has turned out not to be there.
const RETRY: Duration = Duration::from_secs(60);

/// The fewest characters of an id somebody has to have typed for a declaration
/// to be read as naming a container by it rather than by name.
const ID_PREFIX: usize = 4;

/// The containers on this machine, as somewhere to find endpoints.
#[derive(Debug)]
pub struct Containers {
    docker: Docker,
    home: Option<PathBuf>,
    sweep: Duration,
    retry: Duration,
    /// Whether the last look worked, so that a Docker which is not there is
    /// complained about when it stops answering and not on every sweep.
    answering: Mutex<bool>,
}

impl Containers {
    /// The containers reachable through whatever this machine's environment
    /// says to drive them with.
    pub fn resolve() -> Self {
        Self::through(Docker::resolve())
    }

    /// The containers reachable through a particular command.
    pub fn through(docker: Docker) -> Self {
        Self {
            docker,
            home: std::env::var_os(HOME_VAR)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            sweep: SWEEP,
            retry: RETRY,
            answering: Mutex::new(true),
        }
    }

    /// The same, looked at on a cadence of the caller's choosing: how often to
    /// look when there is a Docker to ask, and how often to try again when
    /// there is not.
    #[must_use]
    pub fn looking_every(mut self, sweep: Duration, retry: Duration) -> Self {
        self.sweep = sweep;
        self.retry = retry;
        self
    }

    /// The same, with the walk to a project's root bounded somewhere the caller
    /// chose rather than at this machine's home directory.
    #[must_use]
    pub fn under(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// What `docker ps` printed, or nothing when it could not be asked.
    fn ask(&self) -> Option<String> {
        let printed = match self.docker.output(&listing::command()) {
            Ok(output) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).into_owned()
            }
            Ok(output) => {
                self.unavailable(&format!(
                    "{} exited with {}: {}",
                    self.docker.binary().display(),
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
                return None;
            }
            Err(error) => {
                self.unavailable(&format!(
                    "cannot run {}: {error}",
                    self.docker.binary().display()
                ));
                return None;
            }
        };
        let mut answering = self
            .answering
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !*answering {
            info!(
                command = %self.docker.binary().display(),
                "there is a Docker here after all; watching for containers again"
            );
            *answering = true;
        }
        Some(printed)
    }

    /// Says once that there is no Docker to ask, and says nothing about it
    /// again until there is.
    fn unavailable(&self, said: &str) {
        let mut answering = self
            .answering
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *answering {
            info!(
                said,
                "not watching for containers on this machine; \
                 everything else about this bus is unaffected"
            );
            *answering = false;
        }
    }

    /// The project directories this daemon's own sessions are working in,
    /// nearest thing first and each of them once.
    fn projects(&self, working: &[String]) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        for cwd in working {
            if let Some(root) = project::root(PathBuf::from(cwd).as_path(), self.home.as_deref())
                && !roots.contains(&root)
            {
                roots.push(root);
            }
        }
        roots
    }
}

impl Discovery for Containers {
    fn transport(&self) -> &'static str {
        NAME
    }

    fn every(&self) -> Duration {
        match *self
            .answering
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            true => self.sweep,
            false => self.retry,
        }
    }

    fn sweep(&self, context: &Context<'_>) -> Option<Vec<Found>> {
        let printed = self.ask()?;
        let all = listing::listed(&printed);
        let mine: Vec<Listed> = all
            .into_iter()
            .filter(Listed::running)
            .filter(|listed| !declared(listed, context.declared))
            .collect();

        // Nothing here decides *whether* a container is attached to. What the
        // projects settle is the order, so that on a machine with a dozen
        // containers the one somebody is working in is reached first.
        let projects = self.projects(context.working);
        let wanted: BTreeSet<&str> = projects.iter().filter_map(|root| root.to_str()).collect();
        let (theirs, others): (Vec<Listed>, Vec<Listed>) = mine
            .into_iter()
            .partition(|listed| wanted.contains(listed.folder.as_str()));

        let found: Vec<Found> = theirs
            .into_iter()
            .chain(others)
            .map(|listed| {
                debug!(
                    container = listed.name,
                    id = listed.id,
                    project = listed.folder,
                    "there is a container here"
                );
                Found {
                    args: vec![listed.name.clone()],
                    // Addressed by id and called by name: the id is what it is,
                    // and the name is what a person recognizes.
                    transport: Arc::new(self.docker.calling(listed.id, listed.name))
                        as Arc<dyn Transport>,
                }
            })
            .collect();
        if found.is_empty() {
            debug!("no containers here are worth attaching to");
        }
        Some(found)
    }
}

/// Whether any of `declared` names this container.
///
/// A container somebody asked for by name is attached because they asked, and
/// offering it again as something found would mean two attachments to one
/// place. Both of the ways Docker lets a container be named are compared,
/// because a declaration was written by whoever was looking at whichever of
/// them Docker had just printed.
fn declared(listed: &Listed, declared: &[Vec<String>]) -> bool {
    declared.iter().any(|args| match args.as_slice() {
        [word] => word == &listed.name || same_id(word, &listed.id),
        _ => false,
    })
}

/// Whether a word is this container's id, however much of it was written down.
///
/// Ids are hexadecimal and are quoted at whatever length the thing that printed
/// them chose, so either may be a prefix of the other. The length floor is what
/// keeps a container *named* `ab` from being taken for one whose id starts with
/// those letters.
fn same_id(word: &str, id: &str) -> bool {
    word.len() >= ID_PREFIX
        && word.chars().all(|letter| letter.is_ascii_hexdigit())
        && (id.starts_with(word) || word.starts_with(id))
}
