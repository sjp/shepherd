//! Making sure there is a bus to read.
//!
//! The stream this application draws from is published by a daemon, and a
//! machine where nobody has started one is the ordinary case rather than a
//! misconfigured one: somebody installs the bus, installs its hooks in their
//! coding agent, and expects the thing that reads it to work. So this starts
//! one when there is none, and notices when the one it started has gone.
//!
//! ```no_run
//! use std::time::Instant;
//!
//! use shepherd_core::bus::{SocketPaths, Subscriber};
//! use shepherd_core::daemon::{Host, Lifecycle, Presence};
//!
//! // One directory, read by the subscriber and served by the daemon: the
//! // lifecycle is of whichever bus the thing reading it is looking for.
//! let paths = SocketPaths::resolve();
//! let bus = Subscriber::at(paths.clone()).spawn();
//! let mut lifecycle = Lifecycle::new(paths);
//! let mut host = Host::from_env();
//!
//! // Whatever loop the application already has: everything the bus said, and
//! // a look at the clock whether or not it said anything.
//! while let Ok(update) = bus.updates().recv() {
//!     lifecycle.heard(&update, Instant::now());
//!     lifecycle.tick(&mut host, Instant::now());
//!     if let Presence::Unavailable(why) = lifecycle.presence() {
//!         eprintln!("no bus: {why}");
//!     }
//! }
//! ```
//!
//! # The stream is the only test of whether a bus is there
//!
//! Nothing here connects to a socket, looks for one on disk, or asks the
//! operating system about processes. The subscriber is already connecting, and
//! what it reports — a snapshot, then silence, then another snapshot — is a
//! better answer than any of those: a socket file outlives the daemon that
//! bound it, and a process with the right name in it may be serving some other
//! directory entirely. So this is fed [`Update`]s, and every decision it makes
//! is made from them and from the clock.
//!
//! That is also why it is driven rather than driving. A [`tick`] is where a
//! daemon gets started and where one that never served is given up on, and the
//! caller chooses when those happen, on the loop it already runs.
//!
//! # What gets started is a daemon, not this application's daemon
//!
//! `agentbus daemon --dir <directory>`, and nothing else: no flag, no argument
//! and no variable that somebody starting one by hand would not also use. The
//! daemon has no way of telling that this started it and must not be given one
//! — a bus that behaved differently under this application would be a bus whose
//! behaviour could not be reasoned about from its own documentation, and the
//! same daemon has to serve the shell scripts and the command line that are
//! reading it alongside this.
//!
//! Hook installation is not here and is not anywhere else in this application
//! either. Putting hooks into somebody's coding agent edits files they own, and
//! that is a decision they make with the bus's own command. An agent appears
//! here only because its user has already made it.
//!
//! # Giving up is a state, not an exception
//!
//! A machine with no bus installed on it will never grow one because this asked
//! again, so asking forever would be a loop that costs a process each time and
//! tells nobody anything. After [`DEFAULT_ATTEMPTS`] the answer becomes
//! [`Presence::Unavailable`], which says what went wrong in a form something
//! drawing a window can put in front of a person — and [`Lifecycle::retry`] is
//! there for when they have done something about it.
//!
//! [`tick`]: Lifecycle::tick

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agentbus_paths::SocketPaths;
use thiserror::Error;
use tracing::{debug, warn};

use crate::bus::Update;
use crate::lookup::{PATH_VAR, look_up, search_path};

#[cfg(test)]
mod tests;

/// The command the bus is run through.
pub const COMMAND: &str = "agentbus";

/// The subcommand that runs the bus in the foreground until it is stopped.
const DAEMON: &str = "daemon";

/// The flag naming the directory whose sockets a daemon should serve.
const DIR: &str = "--dir";

/// What a daemon exits with when another one already holds the directory.
///
/// The bus allows one daemon per directory and settles it with a lock, so a
/// second one exits at once and says so with a status of its own — which is
/// exactly what something whose goal is "a daemon is running here" wants to
/// hear, because it means one is.
const ALREADY_RUNNING: i32 = 3;

/// How long a stream that has not arrived is waited for before a daemon is
/// started.
///
/// A subscriber connects to a daemon that is already there in the time it takes
/// to open a socket, so this is long enough to hear about one and short enough
/// that a machine with no bus is not left waiting for what is not coming. What
/// it buys is one process not started: starting a second daemon where one is
/// running is harmless, but it is still a fork, an exec and a lock lost.
pub const DEFAULT_GRACE: Duration = Duration::from_millis(500);

/// How long a daemon that has been started is given to begin serving.
///
/// Binding two sockets is quick even on a loaded machine; what is being allowed
/// for is the process getting as far as trying, and the subscriber's own wait
/// before it next attempts a connection.
pub const DEFAULT_PATIENCE: Duration = Duration::from_secs(5);

/// How long to wait after a daemon failed to start before starting another.
///
/// Doubling with each failure, so that a machine where this is never going to
/// work spends its way to the answer rather than looping at it.
pub const DEFAULT_BACKOFF: Duration = Duration::from_secs(2);

/// How many times a daemon is started before the bus is called unavailable.
///
/// More than one because the first failure is sometimes a race — two of these
/// starting at once, a daemon shutting down as this one arrives — and few
/// because the failures that are not races are permanent.
pub const DEFAULT_ATTEMPTS: u32 = 3;

/// What is known about the bus.
///
/// The thing a window puts in front of somebody: whether what it is showing is
/// live, stale, or nothing at all, and in the last case why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    /// No stream yet, and it has not been given up on. What a run of this
    /// application starts in, and where it stays while a daemon is being
    /// started.
    Waiting,
    /// The stream is live.
    Running,
    /// There was a stream and it stopped. Whatever is on screen is a
    /// recollection until another one arrives.
    Lost,
    /// There is no bus and this could not make one.
    Unavailable(Unavailable),
}

impl Presence {
    /// Whether the stream is live.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// Why there is no bus.
///
/// Structured rather than a sentence because these are different situations for
/// whoever is reading them: one is answered by installing the bus, one by
/// looking at why its daemon is exiting, and one by looking at a machine that
/// is refusing to run a program it has.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Unavailable {
    /// `agentbus` is not on this machine.
    #[error(
        "`{COMMAND}` is not installed on this machine, so there is no bus to read; \
         install it and its hooks to see what your agents are doing"
    )]
    NotInstalled,
    /// It is on this machine and would not run.
    #[error("cannot start `{COMMAND} {DAEMON}`: {said}")]
    WouldNotStart {
        /// What the operating system said, as it was said.
        said: String,
    },
    /// It ran and stopped without serving.
    #[error("`{COMMAND} {DAEMON}` stopped without serving the bus{}", described(*status))]
    Stopped {
        /// What it exited with, where the platform said.
        status: Option<i32>,
    },
    /// It was started, it did not stop, and no stream arrived from it.
    #[error("`{COMMAND} {DAEMON}` was started and the bus never began serving")]
    NeverServed,
}

/// How an exit status is worked into a sentence, where there is one.
fn described(status: Option<i32>) -> String {
    match status {
        Some(status) => format!(" (exit status {status})"),
        None => String::new(),
    }
}

/// What became of a daemon that was started here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// It found another daemon already holding the directory and stood down.
    /// There is a bus; this is simply not the process serving it.
    AlreadyRunning,
    /// It stopped for any other reason.
    Stopped {
        /// What it exited with, where the platform said.
        status: Option<i32>,
    },
}

impl Ended {
    /// What an exit status means.
    fn from_status(status: Option<i32>) -> Self {
        match status {
            Some(ALREADY_RUNNING) => Self::AlreadyRunning,
            status => Self::Stopped { status },
        }
    }
}

/// Why a daemon could not be started at all.
#[derive(Debug, Error)]
pub enum DaemonError {
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

/// Something that can start a daemon and say what became of it.
///
/// [`Host`] is the implementation that runs the command this machine has, and
/// is what everything outside tests uses. The trait exists because what is
/// behind it is a process: absent on some machines, slow to fail on others, and
/// impossible to ask about the interesting cases on purpose. The question worth
/// asking of [`Lifecycle`] is *when it starts one and when it stops trying*,
/// and that question should not need a bus installed to answer.
pub trait Daemons {
    /// Starts a daemon serving `dir`, replacing any this has started before.
    ///
    /// Returns as soon as the process exists. Whether it went on to serve is
    /// not knowable from here — that is what the stream is for.
    fn start(&mut self, dir: &Path) -> Result<(), DaemonError>;

    /// What became of the daemon started here, if it is over.
    ///
    /// Answered once per daemon: the first call after one ends says how, and
    /// later calls say nothing until another is started. `None` while one is
    /// running, and while none has been started.
    fn ended(&mut self) -> Option<Ended>;
}

/// The bus's command as this machine has it, run as a child of this process.
///
/// It looks the command up itself rather than trusting a name to be resolved
/// later, so that a machine without one says so before anything is started.
///
/// The daemon it starts is an ordinary child, which is what makes it
/// supervisable: a process that had been put beyond this one's reach could not
/// afterwards be asked whether it is still there. It is left running when this
/// application exits, because the bus is not this application's — a person's
/// hooks go on emitting into it, and the next thing to read the stream finds
/// the state that accumulated in the meantime rather than an empty daemon.
#[derive(Debug, Default)]
pub struct Host {
    path: Vec<PathBuf>,
    daemon: Option<Child>,
}

impl Host {
    /// This machine, with commands looked for where it says they are.
    pub fn from_env() -> Self {
        Self::searching(search_path(std::env::var_os(PATH_VAR)))
    }

    /// A machine that looks for the command in `directories` and nowhere else.
    pub fn searching(directories: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self {
            path: directories.into_iter().map(Into::into).collect(),
            daemon: None,
        }
    }

    /// Where the bus's command is, if it is anywhere this machine looks.
    pub fn command(&self) -> Result<PathBuf, DaemonError> {
        look_up(&self.path, COMMAND).ok_or(DaemonError::NotInstalled)
    }

    /// The process the daemon started here is, while it is still running.
    ///
    /// The one thing this knows that nothing else can find out: a bus may be
    /// served by a daemon somebody else started, and the difference matters to
    /// anything reporting on what this application is responsible for. It is
    /// not a handle on the daemon — stopping one is nobody's business here, and
    /// a bus is left running when this application exits.
    pub fn started(&self) -> Option<u32> {
        self.daemon.as_ref().map(Child::id)
    }
}

impl Daemons for Host {
    fn start(&mut self, dir: &Path) -> Result<(), DaemonError> {
        let path = self.command()?;
        debug!(dir = %dir.display(), "starting a bus");

        let started = Command::new(&path)
            .arg(DAEMON)
            .arg(DIR)
            .arg(dir)
            // Nothing about the daemon's work depends on where it was started,
            // and holding a directory open would keep a filesystem busy that
            // whoever started this may want to unmount. Not an argument to the
            // daemon and not visible to it: where a process was started is not
            // something it can behave differently because of.
            .current_dir(Path::new("/"))
            // Nothing to read: a daemon started here is not attached to
            // anybody's terminal, and one that inherited this process's input
            // would be a second reader of it.
            .stdin(Stdio::null())
            // What it says goes wherever this application's own diagnostics go,
            // which is the same place they would go had somebody run it in the
            // terminal this was started from. Capturing them into a pipe nobody
            // drains would eventually stop the daemon writing them, and sending
            // them to a file of this application's choosing would be a decision
            // about the bus's own logging that belongs to the bus.
            .spawn()
            .map_err(|source| DaemonError::CannotRun {
                path: path.clone(),
                source,
            })?;

        self.daemon = Some(started);
        Ok(())
    }

    fn ended(&mut self) -> Option<Ended> {
        let daemon = self.daemon.as_mut()?;
        match daemon.try_wait() {
            Ok(None) => None,
            Ok(Some(status)) => {
                self.daemon = None;
                Some(Ended::from_status(status.code()))
            }
            Err(error) => {
                // Nothing can be asked of a child that cannot be waited for, so
                // it is treated as gone: another will be started if the stream
                // does not arrive, and that is the same remedy this would have
                // reached for had the answer been an exit status.
                warn!(%error, "cannot tell whether the bus is still running");
                self.daemon = None;
                Some(Ended::Stopped { status: None })
            }
        }
    }
}

/// What is being done about the bus not being there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    /// Nothing: the stream is live, or the bus has been given up on.
    Settled,
    /// No stream. When the waiting began — `None` until the clock is first
    /// looked at — and how long it lasts before a daemon is started.
    Waiting {
        since: Option<Instant>,
        wait: Duration,
    },
    /// A daemon was started at this moment and has yet to serve.
    Started { at: Instant },
}

/// The bus's daemon, kept running for as long as this is ticked.
///
/// It holds no connection and starts no thread. What it holds is the answer to
/// "is there a bus", arrived at from what the subscriber reported and the time
/// it was reported at, and the record of what has been done about it.
#[derive(Debug)]
pub struct Lifecycle {
    paths: SocketPaths,
    grace: Duration,
    patience: Duration,
    backoff: Duration,
    allowed: u32,

    presence: Presence,
    attempt: Attempt,
    tried: u32,
}

impl Lifecycle {
    /// The daemon serving `paths`, which is where the subscriber reading this
    /// is looking.
    pub fn new(paths: SocketPaths) -> Self {
        Self {
            paths,
            grace: DEFAULT_GRACE,
            patience: DEFAULT_PATIENCE,
            backoff: DEFAULT_BACKOFF,
            allowed: DEFAULT_ATTEMPTS,
            presence: Presence::Waiting,
            attempt: Attempt::Waiting {
                since: None,
                wait: DEFAULT_GRACE,
            },
            tried: 0,
        }
    }

    /// Waits `grace` for a stream before starting a daemon, rather than
    /// [`DEFAULT_GRACE`].
    #[must_use]
    pub const fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self.attempt = Attempt::Waiting {
            since: None,
            wait: grace,
        };
        self
    }

    /// Gives a daemon `patience` to begin serving rather than
    /// [`DEFAULT_PATIENCE`].
    #[must_use]
    pub const fn with_patience(mut self, patience: Duration) -> Self {
        self.patience = patience;
        self
    }

    /// Waits `backoff` after a failure, doubling, rather than
    /// [`DEFAULT_BACKOFF`].
    #[must_use]
    pub const fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Starts at most `attempts` daemons before giving up, rather than
    /// [`DEFAULT_ATTEMPTS`].
    #[must_use]
    pub const fn with_attempts(mut self, attempts: u32) -> Self {
        self.allowed = attempts;
        self
    }

    /// Which bus this is the lifecycle of.
    pub fn paths(&self) -> &SocketPaths {
        &self.paths
    }

    /// What is known about the bus.
    pub fn presence(&self) -> &Presence {
        &self.presence
    }

    /// How many daemons have been started since the last stream arrived.
    pub fn attempts(&self) -> u32 {
        self.tried
    }

    /// Takes in what the subscriber reported.
    ///
    /// Every update is evidence about whether there is a bus, and only two
    /// kinds change the answer: the snapshot that opens a stream, and the end
    /// of one. The rest arrive on a stream that is already known to be live.
    pub fn heard(&mut self, update: &Update, now: Instant) {
        match update {
            Update::Reset(_) => {
                if !self.presence.is_running() {
                    debug!(dir = %self.paths.dir().display(), "the bus is serving");
                }
                // A stream is a bus, whoever started it and whatever this had
                // concluded a moment ago — including having given up, because
                // giving up is a conclusion about the past.
                self.presence = Presence::Running;
                self.attempt = Attempt::Settled;
                self.tried = 0;
            }
            Update::Disconnected => {
                if self.presence.is_running() {
                    self.presence = Presence::Lost;
                }
                // The subscriber reconnects on its own, and to a daemon that is
                // still there it will succeed long before the grace is up. So
                // this waits before concluding that the daemon is what went
                // away, rather than starting one for every stream that blinked.
                //
                // Nothing is re-armed while a daemon that has just been started
                // is still being waited for: it is already being waited for, on
                // a clock of its own, and the connection that ended was to
                // whatever was there before it.
                if !matches!(self.presence, Presence::Unavailable(_))
                    && !matches!(self.attempt, Attempt::Started { .. })
                {
                    self.attempt = Attempt::Waiting {
                        since: Some(now),
                        wait: self.grace,
                    };
                }
            }
            Update::Event(_) | Update::Foreground(_) | Update::Assertion(_) => {}
            Update::Heartbeat(_) => {}
        }
    }

    /// Does whatever the clock says is now due: starts a daemon, or gives up on
    /// one that never served.
    ///
    /// Meant for a loop that runs anyway — a frame, a timer, the same place the
    /// subscriber's queue is drained. Calling it more often than that costs a
    /// comparison; calling it rarely only makes this slower to act.
    pub fn tick(&mut self, daemons: &mut impl Daemons, now: Instant) {
        if let Some(ended) = daemons.ended() {
            self.ended(ended, now);
        }
        match self.attempt {
            Attempt::Settled => {}
            Attempt::Waiting { since, wait } => {
                // The wait runs from the first look at the clock rather than
                // from whenever this was constructed, so that an application
                // which builds one of these long before it starts ticking does
                // not find its grace already spent.
                let since = since.unwrap_or(now);
                self.attempt = Attempt::Waiting {
                    since: Some(since),
                    wait,
                };
                if now.saturating_duration_since(since) >= wait {
                    self.start(daemons, now);
                }
            }
            Attempt::Started { at } => {
                if now.saturating_duration_since(at) >= self.patience {
                    warn!(
                        dir = %self.paths.dir().display(),
                        patience = ?self.patience,
                        "a bus was started and has not begun serving"
                    );
                    self.failed(Unavailable::NeverServed, now);
                }
            }
        }
    }

    /// Starts trying again after the bus was given up on.
    ///
    /// For when somebody has done something about why it was unavailable. It
    /// starts a daemon at the next [`tick`](Self::tick) rather than waiting out
    /// a grace, because somebody asking for this has already waited.
    pub fn retry(&mut self, now: Instant) {
        if self.presence.is_running() {
            return;
        }
        // What was concluded is no longer the answer: something is being done
        // about the bus again, and saying so is half of what was asked for.
        if matches!(self.presence, Presence::Unavailable(_)) {
            self.presence = Presence::Waiting;
        }
        self.tried = 0;
        self.attempt = Attempt::Waiting {
            since: Some(now),
            wait: Duration::ZERO,
        };
    }

    /// Starts one, and decides what its not starting means.
    fn start(&mut self, daemons: &mut impl Daemons, now: Instant) {
        self.tried = self.tried.saturating_add(1);
        match daemons.start(self.paths.dir()) {
            Ok(()) => self.attempt = Attempt::Started { at: now },
            // Not retried, whatever the count says. A machine without the bus
            // installed will not have grown it by the time the backoff is up,
            // and the person reading the message is the one who can fix it.
            Err(DaemonError::NotInstalled) => self.give_up(Unavailable::NotInstalled),
            Err(DaemonError::CannotRun { path, source }) => {
                warn!(path = %path.display(), %source, "cannot run the bus");
                self.failed(
                    Unavailable::WouldNotStart {
                        said: source.to_string(),
                    },
                    now,
                );
            }
        }
    }

    /// Takes in the end of a daemon this started.
    fn ended(&mut self, ended: Ended, now: Instant) {
        match (ended, &self.attempt) {
            // It stood down because another daemon holds the directory, which
            // means there is one to wait for. The patience already running is
            // how long that wait lasts.
            (Ended::AlreadyRunning, _) => {
                debug!(
                    dir = %self.paths.dir().display(),
                    "another bus is already serving this directory"
                );
            }
            // It stopped before it ever served, so this attempt is over and the
            // exit status is the most that can be said about why.
            (Ended::Stopped { status }, Attempt::Started { .. }) => {
                warn!(status, "the bus stopped without serving");
                self.failed(Unavailable::Stopped { status }, now);
            }
            // It stopped while there was a stream, or while one was being
            // waited for. Neither is this attempt's business: the stream is
            // what says whether there is a bus, and if that one was the bus
            // then its ending arrives as the stream ending.
            (Ended::Stopped { status }, _) => {
                debug!(status, "a bus started here has stopped");
            }
        }
    }

    /// Records an attempt that came to nothing, and either arranges another or
    /// stops.
    fn failed(&mut self, why: Unavailable, now: Instant) {
        if self.tried >= self.allowed {
            return self.give_up(why);
        }
        // Doubling from the moment this failed, so that the attempts a machine
        // with a real problem makes spread out instead of repeating.
        let wait = self
            .backoff
            .saturating_mul(2u32.saturating_pow(self.tried.saturating_sub(1).min(u32::BITS - 1)));
        self.attempt = Attempt::Waiting {
            since: Some(now),
            wait,
        };
    }

    /// Stops trying, and says why.
    fn give_up(&mut self, why: Unavailable) {
        warn!(dir = %self.paths.dir().display(), %why, "there is no bus to read");
        self.presence = Presence::Unavailable(why);
        self.attempt = Attempt::Settled;
    }
}
