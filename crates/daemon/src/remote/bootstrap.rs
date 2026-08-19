//! Getting a known-good copy of this program running at the far end.
//!
//! The far end may have a copy already, may have the wrong one, or may have
//! nothing at all, and this side cannot tell which without asking. So it asks
//! with a shell script that does the looking over there — one round trip, in
//! which either the right binary is found and takes over the connection, or the
//! machine says what it is so that the right binary can be sent to it.
//!
//! # What makes a copy the right one
//!
//! Exactly one thing: that it answers `--version` with the line this version
//! implies, byte for byte. A truncated copy, a copy built for another
//! architecture and a copy of an older release all fail that question, and none
//! of them is ever executed. It is also what makes provisioning idempotent — a
//! far end that is already current costs one round trip and no writes at all.
//!
//! # Telling the transport it worked
//!
//! Whichever way it went, the transport is told the version that is now running
//! over there ([`Transport::established`]) before the handle is returned. Some
//! far ends need more than a binary to be worth having — a container needs the
//! agents inside it wired up to the daemon that has just been started there —
//! and this is the moment that is known to be true, whether it was true already
//! or has just been made true.
//!
//! # What is never written over
//!
//! Only [`Transport::install_path`], ever. Somebody who installed `agentbus`
//! themselves, on their `PATH` or through a package manager, has an installation
//! this program does not own; when theirs is the wrong version the script skips
//! it and a copy is put alongside instead. Replacing it would be taking over a
//! machine to save a few megabytes.
//!
//! # Why a mismatched machine is fetched here rather than fetched there
//!
//! When the far end cannot run what this build is, [`Release::binary`] pulls
//! the matching one to this machine before [`Transport::copy_in`] sends it on
//! — paying for the same bytes twice on a link that is usually asymmetric,
//! since this machine typically reaches a release far faster than it reaches
//! an arbitrary remote box. The other order is real and was considered: have
//! the far end fetch its own copy directly, which is one transfer instead of
//! two. It is not what happens, on purpose. That would mean depending on
//! `curl` or `wget` being present over there, on that machine having its own
//! egress to fetch from, and on a URL this daemon did not choose being reached
//! from a network it does not control — one more thing to get right,
//! unattended, on somebody else's machine, for a saving that only shows up on
//! the one path this design already treats as the exception rather than the
//! rule ([`Platform::runs`] is true for most attachments; a release is
//! fetched at all only when it is not). Paying twice for the bytes buys asking
//! nothing new of the far end beyond what it already has to provide.

use std::path::PathBuf;
use std::process::ExitStatus;

use thiserror::Error;
use tracing::{debug, info};

use super::release::{self, Release};
use super::transport::{self, Platform, Running, Transport};

/// The script that finds a usable copy at the far end or says what the far end
/// is.
///
/// Embedded rather than read from disk because the executable that runs it is
/// routinely a copy that was pushed onto a machine with no checkout of anything.
pub const SCRIPT: &str = include_str!("../../assets/bootstrap.sh");

/// The status the script exits with when nothing over there is usable.
///
/// A value no shell and no ordinary program uses for anything, so that "nothing
/// suitable is installed" is never confused with a command that failed.
pub const NOTHING_USABLE: i32 = 42;

/// The prefix of the line on which the script names the machine.
const NEEDS: &str = "need=";

/// The target triple this build was made for.
///
/// Cargo tells a build script and nothing else, so the build script passes it
/// on. It is the only trustworthy answer to "could the far end run this
/// executable", which is what decides whether a copy can be pushed from here.
pub const TARGET: &str = env!("AGENTBUS_TARGET");

/// Why a far end could not be got ready.
#[derive(Debug, Error)]
pub enum Error {
    /// Something the transport was asked to do did not happen.
    #[error(transparent)]
    Transport(#[from] transport::Error),
    /// The connection failed while the script's answer was being read.
    #[error("cannot read what the bootstrap said at {label}")]
    Read {
        /// The endpoint, as a person would name it.
        label: String,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
    /// The script itself did not run.
    #[error(
        "the bootstrap failed at {label}: {status}{}",
        transport::trailing(said)
    )]
    Script {
        /// The endpoint, as a person would name it.
        label: String,
        /// How the script ended.
        status: ExitStatus,
        /// Whatever it wrote to stderr.
        said: String,
    },
    /// No release is built for the kind of machine the far end turned out to be.
    #[error("{label} is {platform}, which no build of agentbus is made for")]
    UnknownPlatform {
        /// The endpoint, as a person would name it.
        label: String,
        /// What it said it was.
        platform: Platform,
    },
    /// The far end needs a binary this machine is not, and the release it would
    /// have come from could not supply one.
    #[error(
        "{label} needs an agentbus for {triple} and this one was built for {target}, \
         so it had to be fetched; if {label} already has an agentbus {version} for \
         {triple}, set AGENTBUS_REMOTE_BINARY there to point at it"
    )]
    NoBinaryFor {
        /// The endpoint, as a person would name it.
        label: String,
        /// The triple the far end needs.
        triple: String,
        /// What this build is for.
        target: String,
        /// The version that is wanted at the far end.
        version: String,
        /// What went wrong fetching it.
        ///
        /// Boxed because it is much the largest thing any of these variants
        /// carries, and every call in this module returns this type.
        #[source]
        source: Box<release::Error>,
    },
    /// This executable could not be found, so there was nothing to send.
    #[error("cannot find this executable to send to {label}")]
    NoLocalBinary {
        /// The endpoint, as a person would name it.
        label: String,
        /// What the system said.
        #[source]
        source: std::io::Error,
    },
    /// A copy was sent and the far end still would not run it.
    #[error("the copy sent to {path} at {label} does not answer to agentbus {version}")]
    NotVerified {
        /// The endpoint, as a person would name it.
        label: String,
        /// Where the copy was put.
        path: String,
        /// The version it was supposed to be.
        version: String,
    },
}

/// What one run of the script came to.
enum Attempt {
    /// A usable copy was found and is now running the command.
    Started(Running),
    /// Nothing usable was there; this is what the machine says it is.
    Needs(Platform),
}

/// A version of this program, and where a copy of it can be got from, ready to
/// be established at as many endpoints as wanted.
#[derive(Debug, Clone)]
pub struct Bootstrap {
    version: String,
    local: Option<PathBuf>,
    target: String,
    release: Release,
}

impl Bootstrap {
    /// Provisions `version`, sending this running executable to every far end
    /// that could run it and fetching from the published release for the rest.
    pub fn new(version: impl Into<String>) -> Self {
        let version = version.into();
        Self {
            release: Release::published(&version),
            version,
            local: None,
            target: TARGET.to_owned(),
        }
    }

    /// Sends `path` instead of this running executable.
    ///
    /// For a caller that has fetched the binary the far end needs from somewhere
    /// else, and for tests, which need a copy whose answers they chose.
    pub fn sending(mut self, path: impl Into<PathBuf>, target: impl Into<String>) -> Self {
        self.local = Some(path.into());
        self.target = target.into();
        self
    }

    /// Fetches from `release` rather than from where this version was
    /// published.
    ///
    /// For a caller pointed at a mirror, and for tests, which have a release of
    /// their own in a directory.
    pub fn fetching(mut self, release: Release) -> Self {
        self.release = release;
        self
    }

    /// The version a far end has to be running for this to leave it alone.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Runs `command` at the far end through an `agentbus` of this version,
    /// putting one there first if there is not one already.
    ///
    /// What comes back is the far-end process itself, with its output unread:
    /// whether it was found or had to be sent makes no difference to the caller,
    /// which is the whole point of this being one call.
    pub fn run(&self, transport: &dyn Transport, command: &[&str]) -> Result<Running, Error> {
        let label = transport.label();
        let platform = match self.attempt(transport, command)? {
            Attempt::Started(running) => {
                transport.established(&self.version);
                return Ok(running);
            }
            Attempt::Needs(platform) => platform,
        };

        let local = self.supply(&label, &platform)?;
        let remote = transport.install_path(&self.version);
        info!(
            endpoint = label,
            version = self.version,
            path = remote,
            "no agentbus of this version is installed there; putting one alongside"
        );
        transport.copy_in(&local, &remote)?;
        self.make_executable(transport, &remote)?;

        match self.attempt(transport, command)? {
            Attempt::Started(running) => {
                transport.established(&self.version);
                Ok(running)
            }
            // One retry and no more. A copy that was written and still does not
            // answer to its own version is not going to answer to it on the
            // third ask, and looping here would mean rewriting the same file at
            // whatever rate the caller retries.
            Attempt::Needs(_) => Err(Error::NotVerified {
                label,
                path: remote,
                version: self.version.clone(),
            }),
        }
    }

    /// A copy of this program that a machine like `platform` can run, ready to
    /// be sent to one.
    ///
    /// Sending what is already here needs no network and no release to have
    /// been published, so it is preferred wherever the far end could run it;
    /// fetching is for the far ends this machine simply does not contain.
    ///
    /// Exposed because where a copy is *put* is a policy of whoever is doing
    /// the putting — a throwaway path for an endpoint that will be rebuilt, a
    /// permanent one for a machine somebody keeps — while which copy to send is
    /// the same question every time.
    pub fn supply(&self, label: &str, platform: &Platform) -> Result<PathBuf, Error> {
        let triple = platform.triple().ok_or_else(|| Error::UnknownPlatform {
            label: label.to_owned(),
            platform: platform.clone(),
        })?;
        match platform.runs(&self.target) {
            true => self.here(label),
            false => self
                .release
                .binary(triple)
                .map_err(|source| Error::NoBinaryFor {
                    label: label.to_owned(),
                    triple: triple.to_owned(),
                    target: self.target.clone(),
                    version: self.version.clone(),
                    source: Box::new(source),
                }),
        }
    }

    /// The copy of this program on this machine, which is the one that was
    /// named or the one that is running.
    fn here(&self, label: &str) -> Result<PathBuf, Error> {
        match &self.local {
            Some(path) => Ok(path.clone()),
            None => std::env::current_exe().map_err(|source| Error::NoLocalBinary {
                label: label.to_owned(),
                source,
            }),
        }
    }

    /// Runs the script once and says which way it went.
    fn attempt(&self, transport: &dyn Transport, command: &[&str]) -> Result<Attempt, Error> {
        let label = transport.label();
        let mut args = vec!["-s", "--", self.version.as_str()];
        args.extend_from_slice(command);
        let mut running = transport.run("sh", &args, Some(SCRIPT))?;

        // The script's own output is one line and it comes before it exits, so
        // one line is all it takes to tell the two outcomes apart — and reading
        // exactly one means a stream that belongs to the far-end command can be
        // handed on with that line put back at the front of it.
        let mut first = String::new();
        let read = running
            .stdout()
            .read_line(&mut first)
            .map_err(|source| Error::Read {
                label: label.clone(),
                source,
            })?;

        if let Some(machine) = first.trim_end().strip_prefix(NEEDS) {
            let status = self.ended(&mut running, &label)?;
            if status.code() != Some(NOTHING_USABLE) {
                return Err(Error::Script {
                    label,
                    status,
                    said: running.complaint(),
                });
            }
            let (os, arch) = machine.split_once('/').unwrap_or((machine, ""));
            let platform = Platform::new(os, arch);
            debug!(endpoint = label, %platform, "nothing usable is installed there");
            return Ok(Attempt::Needs(platform));
        }

        if read == 0 {
            // Nothing on stdout at all. Either the command took over and had
            // nothing to say, which is its business, or the script never ran —
            // and those are told apart by how it ended.
            let status = self.ended(&mut running, &label)?;
            if !status.success() {
                return Err(Error::Script {
                    label,
                    status,
                    said: running.complaint(),
                });
            }
            return Ok(Attempt::Started(running));
        }

        running.unread(first);
        Ok(Attempt::Started(running))
    }

    /// Waits for the script to finish.
    fn ended(&self, running: &mut Running, label: &str) -> Result<ExitStatus, Error> {
        running.wait().map_err(|source| Error::Read {
            label: label.to_owned(),
            source,
        })
    }

    /// Makes the copy that has just been sent runnable.
    ///
    /// Separate from the copy because whether a file arrives with its mode
    /// intact is a property of how it was carried, and only one command has to
    /// be sent to make the answer the same either way.
    fn make_executable(&self, transport: &dyn Transport, remote: &str) -> Result<(), Error> {
        let mut running = transport.run("chmod", &["+x", remote], None)?;
        let status = running.wait().map_err(|source| transport::Error::Read {
            label: transport.label(),
            command: "chmod".to_owned(),
            source,
        })?;
        match status.success() {
            true => Ok(()),
            false => Err(transport::Error::Failed {
                label: transport.label(),
                command: "chmod".to_owned(),
                status,
                said: running.complaint(),
            }
            .into()),
        }
    }
}
