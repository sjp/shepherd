//! The bus itself: the per-user socket directory, ingest of emitted events,
//! the session table and the ring buffer of recent events, fan-out to
//! subscribers, monitoring of the foreground process behind each correlation,
//! and attachment to daemons running on other endpoints so their events merge
//! into the local stream. This crate owns all the state and all the I/O; the
//! command-line front end only starts it and talks to it over the sockets.
//!
//! # Running one
//!
//! ```no_run
//! # async fn run() -> Result<(), agentbus_daemon::Error> {
//! use agentbus_daemon::{Daemon, Settings, SocketPaths};
//!
//! let stopped = Daemon::bind(SocketPaths::resolve(), Settings::default())?
//!     .run()
//!     .await;
//! println!("stopped on {stopped}");
//! # Ok(())
//! # }
//! ```
//!
//! [`Daemon::bind`] does everything that can fail — claiming the directory,
//! creating it and binding the sockets — so that a daemon which starts at all is
//! one that is listening, and a caller finds out about a socket it cannot bind
//! immediately rather than from a client's silence later. [`Daemon::run`] then
//! serves until a termination signal arrives, and takes the directory back down
//! to nothing on its way out.
//!
//! # One at a time, and no debris
//!
//! Exactly one daemon may own a socket directory, which is settled by an
//! exclusive lock rather than by whoever bound a socket first; see [`lock`].
//! Holding that lock is also what licenses the cleanup: a socket file present in
//! a directory this daemon has just claimed cannot belong to a live daemon, so
//! it is removed and rebound. That is the whole of the recovery story for a
//! daemon that was killed outright, and it means a machine never needs a human
//! to clear a stale socket by hand.
//!
//! There is one code path, wherever this runs. Nothing in this crate asks what
//! kind of machine, session or container it is in, or what started it; the
//! socket directory rules in [`paths`] cover those differences by construction.

#![warn(missing_docs)]

pub mod bus;
pub mod clock;
pub mod emit;
pub mod lock;
pub mod paths;
pub mod subscribe;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agentbus_protocol::{DEFAULT_DONE_RETENTION, DEFAULT_STALE_AFTER, Fold, SessionTable};
use subscribe::DEFAULT_HEARTBEAT;
use thiserror::Error;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{debug, error, info};

pub use bus::Bus;
pub use emit::EmitListener;
pub use lock::InstanceLock;
pub use paths::SocketPaths;
pub use subscribe::SubscribeListener;

/// How often every session's clock is moved forward.
///
/// A session becomes stale, and a finished one is forgotten, on the strength of
/// a tick, so this is the granularity of both. One second is far finer than
/// either timeout needs and costs one pass over a handful of sessions.
pub const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// The version this daemon reports when it starts.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long to wait after a failed `accept` before trying again, so that a
/// listener in a state that cannot be accepted on spins slowly rather than
/// burning a core.
pub(crate) const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// The timings a daemon can be started with.
///
/// All of them are properties of the bus rather than of any client, which is why
/// they are settled once here and not negotiated per connection: every
/// subscriber has to be told the same thing about the same session, and a
/// subscriber deciding for itself how often it wants to hear from the daemon
/// would make one slow client's preference everyone else's cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// How long a session may go without activity before it is reported stale.
    pub stale_after: Duration,
    /// How long a session that has finished is kept before it is forgotten.
    pub done_retention: Duration,
    /// How often each subscriber is sent a heartbeat.
    pub heartbeat: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            stale_after: DEFAULT_STALE_AFTER,
            done_retention: DEFAULT_DONE_RETENTION,
            heartbeat: DEFAULT_HEARTBEAT,
        }
    }
}

impl Settings {
    /// A session table configured the way these settings ask for.
    fn table(self) -> SessionTable {
        SessionTable::new()
            .with_fold(Fold::with_stale_after(self.stale_after))
            .with_done_retention(self.done_retention)
    }
}

/// Why a daemon could not start.
#[derive(Debug, Error)]
pub enum Error {
    /// The socket directory could not be created, or could not be made private.
    #[error("cannot prepare the bus directory {}", path.display())]
    Directory {
        /// The directory that was being prepared.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
    /// Another daemon holds this directory. Nothing was touched.
    #[error("another daemon is already running in {}", dir.display())]
    AlreadyRunning {
        /// The directory that is already served.
        dir: PathBuf,
    },
    /// The lock that decides who owns the directory could not be taken, for a
    /// reason other than somebody else holding it.
    #[error("cannot lock {}", path.display())]
    Lock {
        /// The lock file.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
    /// A socket left behind by an earlier run could not be cleared away.
    #[error("cannot remove the stale socket {}", path.display())]
    Stale {
        /// The socket that could not be removed.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
    /// A socket could not be bound.
    #[error("cannot bind {}", path.display())]
    Bind {
        /// The socket that was being bound.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
}

/// Why a daemon stopped serving.
///
/// A daemon that starts serving at all only stops because it was asked to, so
/// this says which signal did the asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// `SIGTERM`, from a supervisor or from whoever wanted the bus gone.
    Terminated,
    /// `SIGINT`, from the keyboard of whoever is running it in a terminal.
    Interrupted,
}

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Terminated => "SIGTERM",
            Self::Interrupted => "SIGINT",
        })
    }
}

/// A running bus: the sockets it listens on and the state behind them.
#[derive(Debug)]
pub struct Daemon {
    paths: SocketPaths,
    settings: Settings,
    bus: Arc<Bus>,
    emit: EmitListener,
    subscribe: SubscribeListener,
    lock: InstanceLock,
}

impl Daemon {
    /// Claims the socket directory, clears what an earlier run left in it, and
    /// binds every socket.
    ///
    /// The order is the point. The lock comes first, so that a second daemon is
    /// turned away before it has removed anything; the cleanup comes next,
    /// because holding the lock is what proves the files found there are
    /// nobody's; binding comes last.
    pub fn bind(paths: SocketPaths, settings: Settings) -> Result<Self, Error> {
        paths.create_dir().map_err(|source| Error::Directory {
            path: paths.dir().to_owned(),
            source,
        })?;
        let lock = InstanceLock::acquire(paths.lock()).map_err(|error| match error {
            lock::LockError::Held => Error::AlreadyRunning {
                dir: paths.dir().to_owned(),
            },
            lock::LockError::Io(source) => Error::Lock {
                path: paths.lock().to_owned(),
                source,
            },
        })?;
        clear_stale_sockets(&paths)?;
        let emit = EmitListener::bind(paths.emit()).map_err(|source| Error::Bind {
            path: paths.emit().to_owned(),
            source,
        })?;
        let subscribe = SubscribeListener::bind(paths.sub()).map_err(|source| Error::Bind {
            path: paths.sub().to_owned(),
            source,
        })?;
        Ok(Self {
            paths,
            settings,
            bus: Arc::new(Bus::with_table(settings.table())),
            emit,
            subscribe,
            lock,
        })
    }

    /// The state behind the sockets.
    pub fn bus(&self) -> &Arc<Bus> {
        &self.bus
    }

    /// Where this daemon is listening.
    pub fn paths(&self) -> &SocketPaths {
        &self.paths
    }

    /// The timings this daemon was started with.
    pub fn settings(&self) -> Settings {
        self.settings
    }

    /// Serves until a termination signal arrives, then leaves the directory as
    /// it found it.
    ///
    /// Returning means the sockets and the lock file are gone: the listeners are
    /// dropped, so nothing more is accepted, before the files naming them are
    /// unlinked. A client that connects in between finds a socket nobody is
    /// listening on, which is exactly what it would find a moment later anyway,
    /// and its own connect fails immediately rather than hanging.
    pub async fn run(self) -> Stopped {
        let Self {
            paths,
            settings,
            bus,
            emit,
            subscribe,
            lock,
        } = self;
        info!(socket = %emit.path().display(), "listening for events");
        info!(socket = %subscribe.path().display(), "publishing the stream");

        let stopped = tokio::select! {
            stopped = terminated() => stopped,
            // Serving never finishes on its own; it is here to be run, and to be
            // dropped the moment the signal arrives.
            () = serve(bus, emit, subscribe, settings) => {
                unreachable!("the bus stops only when it is told to")
            }
        };

        for socket in paths.sockets() {
            remove_if_present(socket);
        }
        drop(lock);
        stopped
    }
}

/// Accepts events, publishes them, and moves the clock forward, forever.
async fn serve(
    bus: Arc<Bus>,
    emit: EmitListener,
    subscribe: SubscribeListener,
    settings: Settings,
) {
    let ticking = tick(Arc::clone(&bus));
    let receiving = emit.serve(Arc::clone(&bus));
    let publishing = subscribe.serve(bus, settings.heartbeat);
    tokio::join!(ticking, receiving, publishing);
}

/// Moves every session's clock forward, forever.
async fn tick(bus: Arc<Bus>) {
    let mut interval = tokio::time::interval(TICK_INTERVAL);
    loop {
        interval.tick().await;
        bus.tick(&clock::now());
    }
}

/// Resolves when this process is asked to stop.
///
/// If the handlers cannot be installed the daemon carries on serving and the
/// signals keep their default disposition, which ends the process abruptly. That
/// is a worse exit than the orderly one, not a broken one: what it leaves behind
/// is exactly what a `SIGKILL` leaves behind, and the next start clears it.
async fn terminated() -> Stopped {
    let (mut term, mut interrupt) = match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(term), Ok(interrupt)) => (term, interrupt),
        (Err(error), _) | (_, Err(error)) => {
            error!(%error, "cannot listen for termination signals; shutdown will not be orderly");
            return std::future::pending().await;
        }
    };
    tokio::select! {
        _ = term.recv() => Stopped::Terminated,
        _ = interrupt.recv() => Stopped::Interrupted,
    }
}

/// Removes any socket file left in a directory this daemon has just claimed.
///
/// Holding the lock is what makes this safe: no live daemon owns anything here,
/// so a socket file is the remains of one that died without cleaning up, and
/// binding over it is impossible while it exists.
fn clear_stale_sockets(paths: &SocketPaths) -> Result<(), Error> {
    for socket in paths.sockets() {
        match std::fs::remove_file(socket) {
            Ok(()) => info!(path = %socket.display(), "removed a socket left by an earlier run"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Stale {
                    path: socket.to_owned(),
                    source,
                });
            }
        }
    }
    Ok(())
}

/// Removes `path`, saying so at most in the log.
///
/// This runs on the way out, where there is nobody left to report a failure to
/// and nothing useful to do about one: the next daemon to start here treats
/// whatever survives as debris and removes it again.
fn remove_if_present(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => debug!(path = %path.display(), "removed"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => debug!(path = %path.display(), %error, "cannot remove"),
    }
}
