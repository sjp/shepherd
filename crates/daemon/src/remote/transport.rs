//! What reaching another endpoint takes, and nothing about how any particular
//! one is reached.
//!
//! A transport is four capabilities and two facts. It can run a command over
//! there and hand back its output as it arrives; it can put a file over there;
//! it can say what kind of machine is over there; and it can say how long to
//! wait before trying again after it breaks. The two facts are a name for people
//! and an identity for programs, and they are deliberately separate: several
//! names may point at one machine, and a name is the wrong thing to deduplicate
//! two views of the same endpoint by.
//!
//! Everything here is written against a machine this process cannot see. That is
//! why a far end's paths are text rather than [`std::path::Path`]s: they are
//! resolved by a filesystem this side has no access to, and treating them as
//! local paths would invite code that tries to look at them.
//!
//! # Why the handle is a stream
//!
//! The same call runs a one-shot probe and a subscription that lasts for days,
//! so what comes back cannot be "the output" — it is the output *so far*, and a
//! caller reads it line by line for as long as it wants to. A caller that only
//! wanted a word waits for the exit status immediately; a caller that is merging
//! somebody else's event stream never does.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

/// The command every transport is expected to be able to run, because the answer
/// decides which binary the far end can execute.
const UNAME: &str = "uname";

/// Why something a transport was asked to do did not happen.
///
/// Every variant names the endpoint it happened at, because the caller is
/// holding several and a message that does not say which one is no use to
/// whoever reads it.
#[derive(Debug, Error)]
pub enum Error {
    /// The command could not be started at all.
    #[error("cannot run {command} at {label}")]
    Run {
        /// The endpoint, as a person would name it.
        label: String,
        /// The command that was to be run.
        command: String,
        /// What went wrong locally.
        #[source]
        source: io::Error,
    },
    /// The file could not be put there.
    #[error("cannot copy {} to {remote} at {label}", local.display())]
    Copy {
        /// The endpoint, as a person would name it.
        label: String,
        /// The file on this machine.
        local: PathBuf,
        /// Where it was to go.
        remote: String,
        /// What went wrong.
        #[source]
        source: io::Error,
    },
    /// The connection failed while what a command said was being read.
    #[error("cannot read what {command} said at {label}")]
    Read {
        /// The endpoint, as a person would name it.
        label: String,
        /// The command that was being read from.
        command: String,
        /// What went wrong.
        #[source]
        source: io::Error,
    },
    /// The command ran and failed.
    #[error("{command} failed at {label}: {status}{}", trailing(said))]
    Failed {
        /// The endpoint, as a person would name it.
        label: String,
        /// The command that failed.
        command: String,
        /// How it ended.
        status: ExitStatus,
        /// Whatever it wrote to stderr.
        said: String,
    },
    /// `uname` answered with something this cannot read.
    #[error("cannot tell what {label} is from {printed:?}")]
    Unrecognized {
        /// The endpoint, as a person would name it.
        label: String,
        /// What was printed instead of an operating system and an architecture.
        printed: String,
    },
}

/// A trailing clause carrying what a failed command complained about, or nothing
/// when it complained about nothing.
pub(crate) fn trailing(text: &str) -> String {
    match text.trim() {
        "" => String::new(),
        said => format!(": {said}"),
    }
}

/// What a machine is, in the terms `uname` reports it in.
///
/// Kept as the two words that were printed rather than as an enumeration of the
/// machines this program knows about, so that an endpoint nobody has thought of
/// is reported by name instead of being flattened into "unknown".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Platform {
    /// What `uname -s` printed: `Linux`, `Darwin`, …
    pub os: String,
    /// What `uname -m` printed: `x86_64`, `aarch64`, `arm64`, …
    pub arch: String,
}

/// The operating system and architecture pairs a release is built for, and the
/// target triple each of them runs.
///
/// A pair that is not here is an error rather than a guess: sending a binary a
/// machine cannot execute wastes a round trip and leaves a file behind, and the
/// pair itself is the only useful thing to say about the failure.
const TRIPLES: [(&str, &str, &str); 4] = [
    ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
    ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
    ("Darwin", "arm64", "aarch64-apple-darwin"),
    ("Darwin", "x86_64", "x86_64-apple-darwin"),
];

impl Platform {
    /// A platform from what `uname` printed.
    pub fn new(os: impl Into<String>, arch: impl Into<String>) -> Self {
        Self {
            os: os.into(),
            arch: arch.into(),
        }
    }

    /// Reads the two words `uname -s -m` prints, in that order.
    pub fn parse(printed: &str) -> Option<Self> {
        let mut words = printed.split_whitespace();
        let os = words.next()?;
        let arch = words.next()?;
        match words.next() {
            None => Some(Self::new(os, arch)),
            Some(_) => None,
        }
    }

    /// The target triple of the release binary this machine runs, or nothing for
    /// a machine no release is built for.
    pub fn triple(&self) -> Option<&'static str> {
        TRIPLES
            .iter()
            .find(|(os, arch, _)| *os == self.os && *arch == self.arch)
            .map(|(_, _, triple)| *triple)
    }

    /// Whether a binary built for `triple` is one this machine could run.
    ///
    /// Operating system and architecture, and nothing else: a build's libc does
    /// not appear in the answer because a machine that reports `Linux x86_64`
    /// says nothing about which one it has. So this is the weaker of the two
    /// questions a provisioner asks — the strong one is whether the copy that
    /// arrived answers to the expected version, which is asked over there, of
    /// the file itself, and is the one that decides.
    pub fn runs(&self, triple: &str) -> bool {
        let (os, arch) = match triple.split_once('-') {
            Some((arch, rest)) => (rest, arch),
            None => return false,
        };
        architecture(&self.arch).is_some_and(|wanted| architecture(arch) == Some(wanted))
            && system(&self.os).is_some_and(|wanted| triple_system(os) == Some(wanted))
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.arch)
    }
}

/// One name for an architecture that has two, so that the word `uname` uses and
/// the word a target triple uses compare equal.
fn architecture(name: &str) -> Option<&'static str> {
    match name {
        "x86_64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        _ => None,
    }
}

/// The operating system `uname -s` named.
fn system(name: &str) -> Option<&'static str> {
    match name {
        "Linux" => Some("linux"),
        "Darwin" => Some("darwin"),
        _ => None,
    }
}

/// The operating system a target triple's trailing components name.
fn triple_system(rest: &str) -> Option<&'static str> {
    if rest.starts_with("linux-") || rest.contains("-linux-") || rest.ends_with("-linux") {
        return Some("linux");
    }
    rest.ends_with("apple-darwin").then_some("darwin")
}

/// How long to wait before trying a broken connection again.
///
/// A policy rather than a schedule: the delays are derived from it, so a caller
/// that has been failing for an hour and one that has just started share no
/// state beyond the number of attempts.
///
/// The jitter is a fraction of each delay, and the fraction of the band to use
/// is supplied by the caller rather than drawn here. Two reasons: this stays a
/// pure function, which is the only kind of backoff a test can assert anything
/// about, and a crate that has no need of random numbers does not acquire a
/// dependency on them for four lines of arithmetic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Backoff {
    /// How long to wait before the first retry.
    pub initial: Duration,
    /// The longest wait, however many attempts have failed.
    pub max: Duration,
    /// What each delay is multiplied by to get the next one.
    pub multiplier: f64,
    /// How far either side of a delay it may be spread, as a fraction of it.
    pub jitter: f64,
}

impl Backoff {
    /// The delay before attempt `attempt`, counting the first retry as zero,
    /// before any spreading.
    pub fn base(&self, attempt: u32) -> Duration {
        let grown = self.initial.as_secs_f64() * self.multiplier.max(1.0).powi(attempt as i32);
        match grown.is_finite() && grown < self.max.as_secs_f64() {
            true => Duration::from_secs_f64(grown),
            false => self.max,
        }
    }

    /// The same delay, spread across its jitter band by `fraction`, which is a
    /// sample in `0..=1`. A fraction of one half is the delay itself.
    pub fn delay(&self, attempt: u32, fraction: f64) -> Duration {
        let jitter = self.jitter.clamp(0.0, 1.0);
        let spread = 1.0 - jitter + 2.0 * jitter * fraction.clamp(0.0, 1.0);
        self.base(attempt).mul_f64(spread)
    }
}

/// A command running at the far end, and its output as it arrives.
///
/// Holding one of these means the far end may still be working, so dropping it
/// without saying anything leaves that process running: what a caller wanted the
/// handle for decides whether that is right, and neither answer belongs here.
pub struct Running {
    child: Child,
    stdout: Box<dyn BufRead + Send>,
    stderr: Option<ChildStderr>,
}

impl fmt::Debug for Running {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Running")
            .field("pid", &self.child.id())
            .finish()
    }
}

impl Running {
    /// Starts `command` as a child of this process, with `stdin` written to it
    /// and both of its output streams captured.
    ///
    /// This is how every transport built out of a local command — `docker exec`,
    /// `ssh`, a shell — produces its handle, so that they differ in the command
    /// line they build and in nothing else.
    ///
    /// Writing stdin here rather than on a thread is safe for what this is for:
    /// what gets written is a script of a few hundred bytes, which the pipe
    /// buffer takes whole. A far end that has already stopped reading is not a
    /// failure to report — its exit status is the news, and the caller is about
    /// to look at it.
    pub fn spawn(command: &mut Command, stdin: Option<&str>) -> io::Result<Self> {
        command
            .stdin(match stdin {
                Some(_) => Stdio::piped(),
                None => Stdio::null(),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        if let Some(text) = stdin {
            let mut pipe = child.stdin.take().expect("stdin was asked for");
            match pipe.write_all(text.as_bytes()) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
                Err(error) => return Err(error),
            }
        }
        let stdout = child.stdout.take().expect("stdout was asked for");
        let stderr = child.stderr.take();
        Ok(Self {
            child,
            stdout: Box::new(BufReader::new(stdout)),
            stderr,
        })
    }

    /// What the far end has said so far, and will say next.
    pub fn stdout(&mut self) -> &mut dyn BufRead {
        &mut self.stdout
    }

    /// Takes the stream away, leaving the handle able to wait for the far end
    /// and to stop it, and able to say nothing more about what it said.
    ///
    /// Anything reading a stream that lasts does it on a thread of its own, and
    /// that thread cannot also be the one that has to be able to cut the
    /// connection — it is blocked on a read only the cutting will finish.
    /// Separating the two is what lets one thread do each.
    pub fn take_stdout(&mut self) -> Box<dyn BufRead + Send> {
        std::mem::replace(&mut self.stdout, Box::new(io::empty()))
    }

    /// Puts bytes already taken from stdout back in front of it.
    ///
    /// A caller sometimes has to read a line to find out what it is holding, and
    /// then wants to hand the stream on with that line still in it. Reading it
    /// out and putting it back is the only honest way to do that, because
    /// deciding without reading would mean guessing.
    pub fn unread(&mut self, bytes: impl Into<Vec<u8>>) {
        let rest = std::mem::replace(&mut self.stdout, Box::new(io::empty()));
        self.stdout = Box::new(BufReader::new(io::Cursor::new(bytes.into()).chain(rest)));
    }

    /// Whatever the far end complained about, and nothing at all if it did not
    /// complain or if the complaint could not be read.
    ///
    /// Used to put the far end's own words into a failure this side reports. It
    /// consumes the stream, so it is for the end of a command's life.
    pub fn complaint(&mut self) -> String {
        let Some(mut stderr) = self.stderr.take() else {
            return String::new();
        };
        let mut said = Vec::new();
        let _ = stderr.read_to_end(&mut said);
        String::from_utf8_lossy(&said).into_owned()
    }

    /// The stream the far end complains on, for a caller that will be holding
    /// this handle for a long time.
    ///
    /// Anything that keeps a handle alive has to drain this, or the far end
    /// eventually blocks on a pipe nobody is emptying.
    pub fn stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    /// Waits for the far end to finish, and says how it went.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    /// How it went, if it has finished, and nothing if it is still running.
    pub fn finished(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Stops the far end.
    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
}

/// A way of reaching another endpoint and running things on it.
///
/// Object-safe on purpose: the things that use a transport hold several at once,
/// of kinds settled at run time, and must not be written against any one of
/// them.
pub trait Transport: fmt::Debug + Send + Sync {
    /// The kind of boundary this transport crosses, as it is stamped on the
    /// events that come back through it.
    fn kind(&self) -> &'static str;

    /// What to call this endpoint when telling somebody about it. Not an
    /// identity: two of these may name one machine.
    fn label(&self) -> String;

    /// What this endpoint *is*, for deduplication and for the trail an event
    /// carries.
    ///
    /// Nothing until the endpoint has been reached, for a transport that cannot
    /// know before then.
    fn identity(&self) -> Option<String>;

    /// Where a copy of this program is put at the far end when one has to be
    /// sent.
    ///
    /// This is the only path anything ever writes to over there, so it has to be
    /// somewhere the next run will look; a transport whose far end is thrown
    /// away and rebuilt says so by naming the version in it, which also makes
    /// the write atomic for nothing, since no earlier copy is ever at that name.
    fn install_path(&self, version: &str) -> String;

    /// Runs a command at the far end, writing `stdin` to it, and hands back its
    /// output as it arrives.
    fn run(&self, command: &str, args: &[&str], stdin: Option<&str>) -> Result<Running, Error>;

    /// Copies a local file to `remote` at the far end, creating whatever the
    /// path needs.
    fn copy_in(&self, local: &Path, remote: &str) -> Result<(), Error>;

    /// Whether a failure is worth trying again.
    ///
    /// Almost everything is: an endpoint that has not started yet, a container
    /// being rebuilt, a network that is down. The exception is a failure that
    /// will keep happening until a person does something about it — a
    /// credential that is refused is the one that matters — and saying so is
    /// what stops something from retrying forever in a state nobody is being
    /// told about.
    fn recoverable(&self, _error: &Error) -> bool {
        true
    }

    /// How long to wait before trying again after this transport breaks.
    ///
    /// Transport-appropriate, not shared: reconnecting to a container on this
    /// machine costs nothing, and reconnecting across a network may cost a
    /// round trip and an authentication.
    fn backoff(&self) -> Backoff;

    /// What kind of machine the far end is.
    ///
    /// The default asks it, which is what every transport that can run a command
    /// can do. One that already knows — because whatever it is talking to told
    /// it — should say so instead and save the round trip.
    fn probe(&self) -> Result<Platform, Error> {
        let running = self.run(UNAME, &["-s", "-m"], None)?;
        platform(running, &self.label())
    }
}

/// Reads a platform out of what `uname -s -m` printed.
///
/// Exposed because a transport that overrides [`Transport::probe`] for some
/// endpoints and not others still wants the ordinary answer for the rest.
pub fn platform(mut running: Running, label: &str) -> Result<Platform, Error> {
    let mut printed = String::new();
    running
        .stdout()
        .read_line(&mut printed)
        .map_err(|source| Error::Read {
            label: label.to_owned(),
            command: UNAME.to_owned(),
            source,
        })?;
    let status = running.wait().map_err(|source| Error::Read {
        label: label.to_owned(),
        command: UNAME.to_owned(),
        source,
    })?;
    if !status.success() {
        return Err(Error::Failed {
            label: label.to_owned(),
            command: UNAME.to_owned(),
            status,
            said: running.complaint(),
        });
    }
    Platform::parse(&printed).ok_or_else(|| Error::Unrecognized {
        label: label.to_owned(),
        printed: printed.trim().to_owned(),
    })
}

/// What a transport is made out of: the words a declaration gave it, and either
/// a way of reaching the endpoint they name or the reason there is not one.
///
/// The reason is a plain sentence rather than a type, because the things that
/// can be wrong with a declaration are as varied as the transports that read
/// one — an address that resolves to nothing, an argument this version of a
/// tool does not accept, a container that is not there — and everything above
/// this line does exactly one thing with it, which is to show it to whoever
/// declared the target.
pub type Made = Result<Arc<dyn Transport>, String>;

/// How a transport is built from a declaration.
type Maker = Box<dyn Fn(&[String]) -> Made + Send + Sync>;

/// The ways this daemon knows of reaching another endpoint, by the name a
/// declaration calls each of them.
///
/// A name it has never heard of is not an error: a declaration may have been
/// written by a later build, or by a person who guessed, and either way the
/// only sensible response is to leave that one alone and carry on with the
/// rest. So the answer to an unknown name is "nothing", distinct from the
/// answer to a known name that could not be made into a transport.
pub struct Registry {
    makers: BTreeMap<String, Maker>,
}

impl Registry {
    /// A registry that knows of no transports at all.
    pub fn new() -> Self {
        Self {
            makers: BTreeMap::new(),
        }
    }

    /// Every way of reaching an endpoint that this build has.
    ///
    /// There are none yet. A transport is added here as it is written, which is
    /// the whole of what a daemon has to be told about a new one.
    pub fn standard() -> Self {
        Self::new()
    }

    /// The same registry, also able to reach an endpoint declared under `name`.
    #[must_use]
    pub fn with(
        mut self,
        name: impl Into<String>,
        make: impl Fn(&[String]) -> Made + Send + Sync + 'static,
    ) -> Self {
        self.makers.insert(name.into(), Box::new(make));
        self
    }

    /// A transport reaching what `args` names, or nothing when no transport is
    /// registered under `name`.
    pub fn make(&self, name: &str, args: &[String]) -> Option<Made> {
        self.makers.get(name).map(|make| make(args))
    }

    /// The names something may be declared under, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.makers.keys().map(String::as_str)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::standard()
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}
