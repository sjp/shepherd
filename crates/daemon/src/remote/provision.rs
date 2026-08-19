//! Putting this program on a machine somebody keeps, and taking it off again.
//!
//! This is the other half of getting a copy to a far end, and the opposite
//! policy from the one an attachment uses. An attachment writes a throwaway
//! copy under a versioned name and never asks, which is right for an endpoint
//! that is rebuilt from an image and wrong for a machine a person logs in to:
//! re-sending megabytes down a slow link every time something reconnects is
//! waste, and a directory filling with one copy per release is somebody else's
//! disk quota. So a copy put here is *installed* — one permanent name, written
//! over only when this program is what wrote it last.
//!
//! # What that costs, and what pays for it
//!
//! A permanent name is a name something may exec at any moment, so the write
//! has to be atomic: the copy arrives beside the name and is renamed onto it,
//! after it has answered for itself. A versioned name gave that away free, this
//! one has to arrange it, and the far end is where the arranging happens: the
//! copy is made runnable, asked what it is, and renamed onto the name only if
//! it answered correctly.
//!
//! A permanent name also has to be found again by something that does not know
//! this ran — a hook, a later attachment — which is why the path is one the
//! shared bootstrap already looks in, and why hooks name it absolutely rather
//! than trusting it to be on a `PATH`.
//!
//! # What is never written over
//!
//! One path is ever written, and the machine it is on says which: ordinarily
//! `~/.local/bin/agentbus`, or wherever that machine's own `AGENTBUS_REMOTE_BINARY`
//! or `XDG_BIN_HOME` says instead. Somebody's own installation, wherever a
//! package manager put it, is looked at and left — reported by name and version
//! so they can see what was passed over, and never replaced, because replacing
//! it would be taking over a machine to save a few megabytes.
//!
//! Even that one path is not taken by force. A binary there that this program
//! has no record of putting there is somebody else's, and provisioning stops
//! rather than overwrite it. The record is a file this writes when it installs
//! and reads when it is asked to uninstall, and it is the whole of what makes
//! removing anything safe: no record means nothing here is this program's to
//! remove.
//!
//! # Why the hooks are not part of it
//!
//! Writing a hook into a container's configuration is undone by the container
//! going away. Writing one into `~/.claude` on a machine somebody shares with
//! colleagues is a change to a real user's configuration on a machine they may
//! not solely own. So the two operations are separate here, and wiring up the
//! agents at the far end happens when it has been asked for in so many words
//! and never because something inferred it.

#[cfg(test)]
mod tests;

use std::io::{self, Write};
use std::process::ExitStatus;

use thiserror::Error;

use super::bootstrap::{self, Bootstrap, NOTHING_USABLE};
use super::transport::{self, Platform, Running, Transport};

/// The script that says what an earlier installation left at the far end.
///
/// Each of the three carries the same fragment in front of it, which is what
/// works out where an installation lives on the machine it is about to run on.
/// The answer is the far end's to give — the variables that move these paths
/// are that machine's, readable only by a shell running there — which is why
/// nothing here composes a path out of a home directory it was told about.
const FIND: &str = concat!(
    include_str!("../../assets/where.sh"),
    include_str!("../../assets/find-installation.sh")
);

/// The script that makes the copy that has just been sent the one that runs.
const PLACE: &str = concat!(
    include_str!("../../assets/where.sh"),
    include_str!("../../assets/put-in-place.sh")
);

/// The script that takes an installation away again.
const AWAY: &str = concat!(
    include_str!("../../assets/where.sh"),
    include_str!("../../assets/take-away.sh")
);

/// What the scripts are called when saying which of them went wrong.
const FINDING: &str = "looking for an installation";
const SEARCHING: &str = "looking for an agentbus";
const PLACING: &str = "installing";
const REMOVING: &str = "uninstalling";

/// What an installed copy is asked to do about the coding agents around it.
const INSTALL: &str = "install";
const UNINSTALL: &str = "uninstall";

/// Whether the coding agents at the far end are part of what is being done.
///
/// A separate decision from the binary in both directions, and never inferred:
/// the far end's agent configuration belongs to whoever uses that machine, and
/// this program touches it only when somebody has said to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hooks {
    /// Wire the agents there up to the copy that is now installed, or take that
    /// wiring back out.
    Included,
    /// Leave them exactly as they are.
    Untouched,
}

/// Why an installation could not be made or unmade.
#[derive(Debug, Error)]
pub enum Error {
    /// Something the transport was asked to do did not happen.
    #[error(transparent)]
    Transport(#[from] transport::Error),
    /// A copy of this program for the far end could not be got hold of.
    #[error(transparent)]
    Bootstrap(#[from] bootstrap::Error),
    /// The connection failed while a script's answer was being read.
    #[error("cannot read what {label} said while {doing}")]
    Read {
        /// The endpoint, as a person would name it.
        label: String,
        /// What was going on.
        doing: &'static str,
        /// What went wrong.
        #[source]
        source: io::Error,
    },
    /// A script ran at the far end and failed.
    #[error("{doing} failed at {label}: {status}{}", transport::trailing(said))]
    Script {
        /// The endpoint, as a person would name it.
        label: String,
        /// What was going on.
        doing: &'static str,
        /// How it ended.
        status: ExitStatus,
        /// Whatever it wrote to stderr.
        said: String,
    },
    /// The far end ran the script that says where an installation goes and did
    /// not say.
    ///
    /// Not the same as a machine that has nowhere to put one: that is refused
    /// over there, in a sentence naming the variables that would answer it, and
    /// arrives here as a script that failed. This is the far end answering
    /// something neither of them agreed on, which nothing can be done about
    /// from this side.
    #[error("cannot tell where an agentbus goes at {label}")]
    Nowhere {
        /// The endpoint, as a person would name it.
        label: String,
    },
    /// There is an agentbus at the one path this installs to, and no record
    /// that this program is what put it there.
    #[error(
        "{path} at {label} is {said}, and there is no record at {record} of this \
         having installed it, so it is left alone. Remove it there, or set \
         AGENTBUS_REMOTE_BINARY on that machine to name the copy you want used"
    )]
    Occupied {
        /// The endpoint, as a person would name it.
        label: String,
        /// The path something else is at.
        path: String,
        /// What that something answered when asked what it is.
        said: String,
        /// Where the record that would have made it writable would be.
        record: String,
    },
    /// Whatever the account of all this was being written to would not take it.
    #[error("cannot say what happened")]
    Unsayable(#[source] io::Error),
}

/// A permanent installation of a version of this program, ready to be made at
/// as many endpoints as wanted.
#[derive(Debug)]
pub struct Provision<'a> {
    bootstrap: &'a Bootstrap,
}

impl<'a> Provision<'a> {
    /// Installs whatever `bootstrap` provisions, permanently.
    pub fn new(bootstrap: &'a Bootstrap) -> Self {
        Self { bootstrap }
    }

    /// Makes sure an agentbus of this version is installed at the far end, and
    /// says what it did on the way.
    ///
    /// The search comes first and decides everything: a far end that already
    /// has a copy of this version — this program's, somebody else's, anywhere
    /// the bootstrap looks — is a far end nothing is written to, which is what
    /// makes running this twice cost one round trip and no writes.
    pub fn install(
        &self,
        transport: &dyn Transport,
        hooks: Hooks,
        out: &mut dyn Write,
    ) -> Result<(), Error> {
        let version = self.bootstrap.version();
        let path = match self.search(transport)? {
            Search::Found(path) => {
                say(out, &format!("agentbus {version} is already at {path}"))?;
                path
            }
            Search::Needs { platform, others } => {
                for other in &others {
                    say(out, &other.passed_over(version))?;
                }
                self.put(transport, &platform, out)?
            }
        };
        match hooks {
            Hooks::Included => self.wire(transport, &path, INSTALL, out),
            Hooks::Untouched => Ok(()),
        }
    }

    /// Takes this program's installation off the far end, and says what it did.
    ///
    /// In the order the far end needs it: the agents there stop being wired up
    /// to a binary while there is still a binary to run the unwiring, then the
    /// daemon that is holding the sockets is asked to stop, and only then does
    /// anything get removed.
    pub fn uninstall(
        &self,
        transport: &dyn Transport,
        hooks: Hooks,
        out: &mut dyn Write,
    ) -> Result<(), Error> {
        let there = self.installation(transport)?;
        if hooks == Hooks::Included {
            match self.usable(transport, &there)? {
                Some(path) => self.wire(transport, &path, UNINSTALL, out)?,
                None => say(
                    out,
                    "there is no agentbus there to take the hooks back out with",
                )?,
            }
        }
        let done = self.script(transport, AWAY, &[], REMOVING)?;
        for line in done.printed.lines() {
            if let Some(line) = recounted(line) {
                say(out, &line)?;
            }
        }
        Ok(())
    }

    /// Puts a copy where an installation goes, and says where that was.
    fn put(
        &self,
        transport: &dyn Transport,
        platform: &Platform,
        out: &mut dyn Write,
    ) -> Result<String, Error> {
        let label = transport.label();
        let version = self.bootstrap.version();
        let there = self.installation(transport)?;
        if let Some(said) = there.occupier() {
            return Err(Error::Occupied {
                label,
                path: there.binary(),
                said: said.to_owned(),
                record: there.record(),
            });
        }

        let local = self.bootstrap.supply(&label, platform)?;
        say(out, &format!("sending agentbus {version} to {label}"))?;
        transport.copy_in(&local, &there.partial())?;
        let done = self.script(transport, PLACE, &[version], PLACING)?;
        let path = done
            .printed
            .lines()
            .find_map(|line| line.strip_prefix(INSTALLED))
            .map_or_else(|| there.binary(), str::to_owned);
        say(out, &format!("installed agentbus {version} at {path}"))?;
        Ok(path)
    }

    /// Runs the copy at `path` as an installer of hooks, and relays what it
    /// said.
    ///
    /// Through the absolute path deliberately. `~/.local/bin` is on most
    /// machines' `PATH` and guaranteed on none, and what this is starting is
    /// the program that writes hooks naming the binary it was itself run as —
    /// so a bare name here would put a bare name in somebody's hooks, to be
    /// resolved later against a `PATH` that is not this one.
    fn wire(
        &self,
        transport: &dyn Transport,
        path: &str,
        verb: &'static str,
        out: &mut dyn Write,
    ) -> Result<(), Error> {
        let label = transport.label();
        let mut running = transport.run(path, &[verb], None)?;
        let printed = read(&mut running, &label, verb)?;
        let said = running.complaint();
        let status = running.wait().map_err(|source| Error::Read {
            label: label.clone(),
            doing: verb,
            source,
        })?;
        for line in printed.lines() {
            say(out, &format!("  {line}"))?;
        }
        match status.success() {
            true => Ok(()),
            false => Err(Error::Script {
                label,
                doing: verb,
                status,
                said,
            }),
        }
    }

    /// Asks the far end whether it has an agentbus of this version anywhere the
    /// bootstrap looks, and where.
    ///
    /// The shared script with nothing to run, so that the question asked here is
    /// the same question an attachment asks — the same candidates in the same
    /// order, and the same byte-for-byte version check — rather than a second
    /// implementation of it that could drift.
    fn search(&self, transport: &dyn Transport) -> Result<Search, Error> {
        let version = self.bootstrap.version();
        let done = self.ran(transport, bootstrap::SCRIPT, &[version], SEARCHING)?;
        if done.status.code() == Some(NOTHING_USABLE) {
            let machine = done
                .printed
                .lines()
                .find_map(|line| line.strip_prefix(NEEDS))
                .unwrap_or_default();
            let (os, arch) = machine.split_once('/').unwrap_or((machine, ""));
            return Ok(Search::Needs {
                platform: Platform::new(os, arch),
                others: done.said.lines().filter_map(Other::read).collect(),
            });
        }
        let done = done.succeeded(transport, SEARCHING)?;
        match done
            .printed
            .lines()
            .find_map(|line| line.strip_prefix(FOUND))
        {
            Some(path) => Ok(Search::Found(path.to_owned())),
            // It exited zero, said nothing this understands, and did not exec
            // anything: the copy it accepted is not one this can name, which is
            // the same thing as not having found one.
            None => Err(Error::Script {
                label: transport.label(),
                doing: SEARCHING,
                status: done.status,
                said: done.said,
            }),
        }
    }

    /// What an earlier installation left at the far end.
    fn installation(&self, transport: &dyn Transport) -> Result<Installation, Error> {
        let done = self.script(transport, FIND, &[], FINDING)?;
        Installation::read(&done.printed).ok_or_else(|| Error::Nowhere {
            label: transport.label(),
        })
    }

    /// The path of an agentbus at the far end that could take hooks back out:
    /// the installed one if it is there, and otherwise whatever the search
    /// turns up.
    fn usable(
        &self,
        transport: &dyn Transport,
        there: &Installation,
    ) -> Result<Option<String>, Error> {
        if there.binary_said.is_some() {
            return Ok(Some(there.binary()));
        }
        match self.search(transport)? {
            Search::Found(path) => Ok(Some(path)),
            Search::Needs { .. } => Ok(None),
        }
    }

    /// Runs one script at the far end, having insisted that it succeeded.
    fn script(
        &self,
        transport: &dyn Transport,
        script: &str,
        args: &[&str],
        doing: &'static str,
    ) -> Result<Outcome, Error> {
        self.ran(transport, script, args, doing)?
            .succeeded(transport, doing)
    }

    /// The same, without an opinion about how it ended.
    ///
    /// For the one script whose failure is an answer rather than a failure: the
    /// shared bootstrap reports "nothing here is usable" by exiting non-zero.
    fn ran(
        &self,
        transport: &dyn Transport,
        script: &str,
        args: &[&str],
        doing: &'static str,
    ) -> Result<Outcome, Error> {
        let label = transport.label();
        let mut words = vec!["-s", "--"];
        words.extend_from_slice(args);
        let mut running = transport.run("sh", &words, Some(script))?;
        let printed = read(&mut running, &label, doing)?;
        let said = running.complaint();
        let status = running.wait().map_err(|source| Error::Read {
            label,
            doing,
            source,
        })?;
        Ok(Outcome {
            printed,
            said,
            status,
        })
    }
}

/// What one run of a script at the far end came to.
struct Outcome {
    /// What it printed.
    printed: String,
    /// What it complained.
    said: String,
    /// How it ended.
    status: ExitStatus,
}

impl Outcome {
    /// The same run, if it succeeded, and otherwise the failure it was.
    fn succeeded(self, transport: &dyn Transport, doing: &'static str) -> Result<Self, Error> {
        match self.status.success() {
            true => Ok(self),
            false => Err(Error::Script {
                label: transport.label(),
                doing,
                status: self.status,
                said: self.said,
            }),
        }
    }
}

/// What the far end has that is worth having.
enum Search {
    /// A copy of the wanted version is there, at this path.
    Found(String),
    /// Nothing usable is: what kind of machine it is, and what was passed over
    /// on the way to finding that out.
    Needs {
        /// What the machine says it is.
        platform: Platform,
        /// Every installation that is there and is not this version.
        others: Vec<Other>,
    },
}

/// Somebody else's installation, or an older one of this program's.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Other {
    /// Where it is.
    path: String,
    /// What it answered when asked what it is.
    said: String,
}

impl Other {
    /// One read out of what the search said it passed over.
    fn read(line: &str) -> Option<Self> {
        let (path, said) = line.strip_prefix(OTHER)?.split_once('\t')?;
        Some(Self {
            path: path.to_owned(),
            said: said.to_owned(),
        })
    }

    /// How it is reported to somebody who is about to have a copy installed
    /// beside it.
    fn passed_over(&self, version: &str) -> String {
        let said = match self.said.trim() {
            "" => "something that does not say what it is".to_owned(),
            said => format!("\"{said}\""),
        };
        format!(
            "found {said} at {}; expected agentbus {version}; leaving it alone",
            self.path
        )
    }
}

/// Where an installation of this program goes at the far end, and what is
/// already there.
///
/// Every path in here was worked out by the machine it names and read back,
/// never composed on this side. That is not fastidiousness: which paths those
/// are depends on variables only a shell over there can read, and a second
/// answer computed here would be free to disagree with the one the scripts
/// that do the writing will use.
///
/// String work rather than [`std::path::Path`] work, and deliberately: these
/// are paths on a machine whose filesystem this process cannot see, and
/// treating one as a local path is how code that tries to look at it gets
/// written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Installation {
    /// Where an installed copy goes.
    binary: String,
    /// Where a copy is written before it is moved onto that name.
    partial: String,
    /// Where the record of what was installed is kept.
    record: String,
    /// What the file at the installed path answered, if anything is there.
    binary_said: Option<String>,
    /// The version the record says was installed, if there is a record.
    recorded_version: Option<String>,
    /// The path the record says it was installed at.
    recorded_path: Option<String>,
}

impl Installation {
    /// One read out of what the far end printed, or nothing if it did not say
    /// where an installation goes.
    fn read(printed: &str) -> Option<Self> {
        let mut binary = None;
        let mut partial = None;
        let mut record = None;
        let mut binary_said = None;
        let mut recorded_version = None;
        let mut recorded_path = None;
        for line in printed.lines() {
            if let Some(path) = line.strip_prefix(BINARY_AT) {
                binary = Some(path.to_owned());
            } else if let Some(path) = line.strip_prefix(PARTIAL_AT) {
                partial = Some(path.to_owned());
            } else if let Some(path) = line.strip_prefix(RECORD_AT) {
                record = Some(path.to_owned());
            } else if let Some(said) = line.strip_prefix(BINARY_SAID) {
                binary_said = Some(said.to_owned());
            } else if let Some(recorded) = line.strip_prefix(RECORD) {
                match recorded.split_once('=') {
                    Some(("version", version)) => recorded_version = Some(version.to_owned()),
                    Some(("path", path)) => recorded_path = Some(path.to_owned()),
                    _ => {}
                }
            }
        }
        Some(Self {
            binary: binary?,
            partial: partial?,
            record: record?,
            binary_said,
            recorded_version,
            recorded_path,
        })
    }

    /// Where an installed copy goes.
    fn binary(&self) -> String {
        self.binary.clone()
    }

    /// Where a copy is written before it is moved onto that name.
    fn partial(&self) -> String {
        self.partial.clone()
    }

    /// Where the record of what was installed is kept.
    fn record(&self) -> String {
        self.record.clone()
    }

    /// What is at the installed path that this program has no record of putting
    /// there, if anything.
    ///
    /// The record is what the question turns on, not the version: a copy of the
    /// wanted version would have been found by the search long before this, so
    /// anything still standing here is either an installation of this
    /// program's that is out of date — which may be written over, because this
    /// is the one path it owns — or somebody else's, which may not.
    fn occupier(&self) -> Option<&str> {
        let said = self.binary_said.as_deref()?;
        match self.recorded_path.as_deref() == Some(self.binary.as_str()) {
            true => None,
            false => Some(match said.trim() {
                "" => "something that does not say what it is",
                said => said,
            }),
        }
    }
}

/// The prefix the search names the copy it accepted with.
const FOUND: &str = "found=";

/// The prefix the search names the machine with when it accepted none.
const NEEDS: &str = "need=";

/// The prefix the search names an installation it passed over with.
const OTHER: &str = "other=";

/// The prefixes the far end names an installation's three paths with.
const BINARY_AT: &str = "bin=";
const PARTIAL_AT: &str = "partial=";
const RECORD_AT: &str = "record=";

/// The prefix carrying what the file at the installed path answered.
const BINARY_SAID: &str = "binary=";

/// The prefix carrying one line of the record of what was installed.
const RECORD: &str = "marker ";

/// The prefix naming what was installed and where.
const INSTALLED: &str = "installed=";

/// What one line of an account of an uninstall means, said as a sentence.
///
/// Anything unrecognized is dropped rather than relayed: this is a program at
/// the far end talking to this one, and a line neither of them agreed on is not
/// something to put in front of a person.
fn recounted(line: &str) -> Option<String> {
    let (key, value) = line.split_once('=')?;
    Some(match key {
        "stopped" => format!("asked the daemon there to stop; it was process {value}"),
        "removed" => format!("removed {value}"),
        "forgot" => format!("removed the record at {value}"),
        "gone" => format!("{value} is not there"),
        "unrecorded" => {
            format!("nothing at {value} was installed by this, so nothing was removed")
        }
        "elsewhere" => format!("the record names {value}, which is not where this installs to"),
        "kept" => format!("{value} is not the copy that was installed there; leaving it alone"),
        "said" => format!("  it answers \"{value}\""),
        _ => return None,
    })
}

/// Everything a far-end command printed, read to the end.
///
/// To the end because none of these scripts is a stream: each says its piece
/// and stops, and the caller wants all of it before it decides anything.
fn read(running: &mut Running, label: &str, doing: &'static str) -> Result<String, Error> {
    let mut printed = String::new();
    running
        .stdout()
        .read_to_string(&mut printed)
        .map_err(|source| Error::Read {
            label: label.to_owned(),
            doing,
            source,
        })?;
    Ok(printed)
}

/// Tells whoever asked for this what has just happened.
fn say(out: &mut dyn Write, line: &str) -> Result<(), Error> {
    writeln!(out, "{line}").map_err(Error::Unsayable)
}
