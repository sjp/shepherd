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
//! immediately rather than from a client's silence later. It listens for the
//! signals that stop a daemon before any of that, so that one which can be
//! connected to can also be asked to stop. [`Daemon::run`] then serves until a
//! termination signal arrives, and takes the directory back down to nothing on
//! its way out.
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
//! # Endpoints somebody asked for
//!
//! Which other endpoints to attach to is not a property of a running daemon but
//! of what somebody wants, so it is kept in a file that outlives every daemon
//! and is watched rather than sent. A daemon reads it as it starts, looks again
//! on a couple of seconds' cadence, and looks immediately when it is sent a
//! `SIGHUP`; and it writes down what came of each declaration beside its
//! sockets, where the file is as ephemeral as the daemon is. See [`remote`].
//!
//! There is one code path, wherever this runs. Nothing in this crate asks what
//! kind of machine, session or container it is in, or what started it; the
//! socket directory rules in [`paths`] cover those differences by construction.

#![warn(missing_docs)]

pub mod binding;
pub mod bus;
pub mod clock;
pub mod emit;
pub mod foreground;
pub mod lock;
pub mod paths;
pub mod procfs;
pub mod remote;
pub mod subscribe;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agentbus_protocol::{DEFAULT_DONE_RETENTION, DEFAULT_STALE_AFTER, Fold, SessionTable};
use bus::Published;
use foreground::Monitor;
use procfs::ProcFs;
use remote::discover::Discovery;
use remote::reconcile::{self, Plan, Reconciling, Wake};
use remote::{Attachments, Bootstrap, Registry, Targets, attach};
use subscribe::DEFAULT_HEARTBEAT;
use thiserror::Error;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::broadcast::error::TryRecvError;
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

/// How often the process table is looked at.
///
/// Steady state is a couple of reads per correlated shell, so looking this often
/// costs nothing measurable, and it is fast enough that a command someone has
/// just started is on the stream while they are still looking at the terminal
/// they started it in.
pub const FOREGROUND_INTERVAL: Duration = Duration::from_millis(750);

/// The version this daemon reports when it starts.
///
/// Also what a copy of this program at another endpoint has to answer to before
/// it is trusted, which is why it is one constant and not a string written out
/// wherever it is wanted.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long to wait after a failed `accept` before trying again, so that a
/// listener in a state that cannot be accepted on spins slowly rather than
/// burning a core.
pub(crate) const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// What a daemon can be started with.
///
/// The timings are properties of the bus rather than of any client, which is why
/// they are settled once here and not negotiated per connection: every
/// subscriber has to be told the same thing about the same session, and a
/// subscriber deciding for itself how often it wants to hear from the daemon
/// would make one slow client's preference everyone else's cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// How long a session may go without activity before it is reported stale.
    pub stale_after: Duration,
    /// How long a session that has finished is kept before it is forgotten.
    pub done_retention: Duration,
    /// How often each subscriber is sent a heartbeat.
    pub heartbeat: Duration,
    /// How often the declared endpoints are looked at.
    ///
    /// Settable for the same reason the process table's root is: what a test
    /// about the control path needs is for nothing to happen except when it
    /// asks, and an interval it cannot lengthen would mean every such test was
    /// really a measurement of how long a pass takes to come round.
    pub reconcile_every: Duration,
    /// Where the process table is read from.
    ///
    /// Every machine this runs on answers `/proc`, which is the default. It is
    /// settable because a test can then write a process table as files and put a
    /// daemon in front of it, which is the only way to hold the table still long
    /// enough to assert anything about it.
    pub proc_root: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            stale_after: DEFAULT_STALE_AFTER,
            done_retention: DEFAULT_DONE_RETENTION,
            heartbeat: DEFAULT_HEARTBEAT,
            reconcile_every: reconcile::INTERVAL,
            proc_root: PathBuf::from(procfs::DEFAULT_ROOT),
        }
    }
}

impl Settings {
    /// A session table configured the way these settings ask for.
    fn table(&self) -> SessionTable {
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
    /// A file left behind by an earlier run could not be cleared away.
    #[error("cannot remove {}, left by an earlier run", path.display())]
    Stale {
        /// The file that could not be removed.
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
    monitor: Option<Monitor>,
    lock: InstanceLock,
    signals: Option<Signals>,
    targets: Targets,
    transports: Registry,
    discoveries: Vec<Arc<dyn Discovery>>,
}

impl Daemon {
    /// Starts listening for the signals that stop a daemon, claims the socket
    /// directory, clears what an earlier run left in it, and binds every socket.
    ///
    /// Must be called from inside a Tokio runtime, which is where the signal
    /// listeners register themselves.
    ///
    /// The order is the point. Listening for signals comes first, so that a
    /// daemon anything can reach is a daemon that can be stopped politely: a
    /// socket accepts connections from the moment it is bound, and a supervisor
    /// that starts a daemon and immediately changes its mind would otherwise
    /// find it dying by default disposition and leaving its directory behind.
    /// The lock comes next, so that a second daemon is turned away before it has
    /// removed anything; the cleanup after that, because holding the lock is
    /// what proves the files found there are nobody's; binding comes last.
    ///
    /// Whether there is a process table to read is settled here too, once, by
    /// trying to read it. A daemon that cannot — another operating system, a
    /// root that is not there, a table it may not list — serves everything else
    /// exactly as it would have done and says nothing whatever about the
    /// foreground, which is different from saying there is nothing in it.
    pub fn bind(paths: SocketPaths, settings: Settings) -> Result<Self, Error> {
        let signals = Signals::install();
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
        clear_stale_files(&paths)?;
        let emit = EmitListener::bind(paths.emit()).map_err(|source| Error::Bind {
            path: paths.emit().to_owned(),
            source,
        })?;
        let subscribe = SubscribeListener::bind(paths.sub()).map_err(|source| Error::Bind {
            path: paths.sub().to_owned(),
            source,
        })?;
        let monitor = Monitor::new(ProcFs::new(&settings.proc_root));
        let mut bus = Bus::with_table(settings.table());
        if monitor.available() {
            bus = bus.observing();
        } else {
            info!(
                root = %settings.proc_root.display(),
                "there is no process table here; not reporting foreground processes"
            );
        }
        Ok(Self {
            paths,
            bus: Arc::new(bus),
            emit,
            subscribe,
            monitor: monitor.available().then_some(monitor),
            lock,
            signals,
            settings,
            targets: Targets::resolve(),
            transports: Registry::standard(),
            discoveries: remote::discover::standard(),
        })
    }

    /// The same daemon, reading its declared endpoints from `targets`.
    #[must_use]
    pub fn declaring(mut self, targets: Targets) -> Self {
        self.targets = targets;
        self
    }

    /// The same daemon, able to reach an endpoint by any transport in
    /// `transports` and by no other.
    #[must_use]
    pub fn reaching(mut self, transports: Registry) -> Self {
        self.transports = transports;
        self
    }

    /// The same daemon, looking for endpoints of its own accord in `discoveries`
    /// and nowhere else.
    ///
    /// An empty list is a daemon that attaches to exactly what has been declared
    /// and to nothing else, which is what a test wanting no surprises from the
    /// machine it happens to be running on asks for.
    #[must_use]
    pub fn discovering(mut self, discoveries: Vec<Arc<dyn Discovery>>) -> Self {
        self.discoveries = discoveries;
        self
    }

    /// The state behind the sockets.
    pub fn bus(&self) -> &Arc<Bus> {
        &self.bus
    }

    /// Where this daemon is listening.
    pub fn paths(&self) -> &SocketPaths {
        &self.paths
    }

    /// Where this daemon reads its declared endpoints from.
    pub fn targets(&self) -> &Targets {
        &self.targets
    }

    /// What this daemon was started with.
    pub fn settings(&self) -> &Settings {
        &self.settings
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
            monitor,
            lock,
            signals,
            targets,
            transports,
            discoveries,
        } = self;
        info!(socket = %emit.path().display(), "listening for events");
        info!(socket = %subscribe.path().display(), "publishing the stream");
        info!(declarations = %targets.path().display(), "watching for declared endpoints");

        let reconciling = Reconciling::start(Plan {
            targets,
            attachments: Attachments::in_dir(paths.dir()),
            transports,
            discoveries,
            bus: Arc::clone(&bus),
            bootstrap: Bootstrap::new(VERSION),
            attach: attach::Settings::default(),
            every: settings.reconcile_every,
        });
        let (stops, hangups) = split(signals);

        let stopped = tokio::select! {
            stopped = terminated(stops) => stopped,
            // Serving never finishes on its own; it is here to be run, and to be
            // dropped the moment the signal arrives.
            () = serve(bus, emit, subscribe, monitor, settings.heartbeat, hangups, reconciling.wake()) => {
                unreachable!("the bus stops only when it is told to")
            }
        };

        // Before the files are removed, because detaching is what withdraws the
        // sessions the far ends were speaking for, and because it is what takes
        // the record of them away.
        drop(reconciling);
        for file in paths.ephemeral() {
            remove_if_present(file);
        }
        drop(lock);
        stopped
    }
}

/// Accepts events, publishes them, watches the process table, and moves the
/// clock forward, forever.
async fn serve(
    bus: Arc<Bus>,
    emit: EmitListener,
    subscribe: SubscribeListener,
    monitor: Option<Monitor>,
    heartbeat: Duration,
    hangups: Option<Signal>,
    wake: Arc<Wake>,
) {
    let ticking = tick(Arc::clone(&bus));
    let watching = watch(Arc::clone(&bus), monitor);
    let receiving = emit.serve(Arc::clone(&bus));
    let publishing = subscribe.serve(bus, heartbeat);
    let reloading = reload(hangups, wake);
    tokio::join!(ticking, watching, receiving, publishing, reloading);
}

/// Asks for the declared endpoints to be looked at again every time this
/// process is told to reload, forever.
///
/// What `SIGHUP` means to a daemon by long convention is "read your
/// configuration again", and that is exactly what it is taken to mean here.
/// Anything that has just written a declaration and does not want to wait out
/// the ordinary interval sends one; nothing else about the daemon is affected,
/// because there is nothing else it reads from a file.
async fn reload(hangups: Option<Signal>, wake: Arc<Wake>) {
    let Some(mut hangups) = hangups else {
        return std::future::pending().await;
    };
    while hangups.recv().await.is_some() {
        debug!("asked to look at the declared endpoints again");
        wake.now();
    }
    std::future::pending().await
}

/// Moves every session's clock forward, forever.
async fn tick(bus: Arc<Bus>) {
    let mut interval = tokio::time::interval(TICK_INTERVAL);
    loop {
        interval.tick().await;
        bus.tick(&clock::now());
    }
}

/// Looks at the process table on a fixed cadence and publishes what changed,
/// forever, or waits forever where there is no process table to look at.
///
/// The looking is done on a blocking thread rather than on a runtime worker. In
/// the steady state it is a couple of reads per correlated shell and would not
/// be worth the hand-off, but every few seconds it is one read of `environ` per
/// process on the machine, and a worker parked in that is a worker not accepting
/// the connection a hook is waiting on.
async fn watch(bus: Arc<Bus>, monitor: Option<Monitor>) {
    let Some(mut monitor) = monitor else {
        return std::future::pending().await;
    };
    let mut published = bus.events();
    let mut interval = tokio::time::interval(FOREGROUND_INTERVAL);
    loop {
        interval.tick().await;
        if !wanted(&mut published, &mut monitor) {
            return;
        }
        let now = clock::now();
        let looked = tokio::task::spawn_blocking(move || {
            let transitions = monitor.tick(&now);
            (monitor, now, transitions)
        })
        .await;
        let (returned, now, transitions) = match looked {
            Ok(looked) => looked,
            // The only way here is a panic in the monitor, which has already
            // been reported by the panic hook. The rest of the daemon is
            // untouched by it and carries on; what is lost is the foreground.
            Err(error) => {
                error!(%error, "the foreground monitor stopped; no more will be reported");
                return;
            }
        };
        monitor = returned;
        for pid in bus.observed(&transitions, &now) {
            debug!(pid, "a process that was being followed has ended");
        }
    }
}

/// Names to the monitor every correlation seen on an event since the last look,
/// and says whether there is any point looking again.
///
/// This is what a correlation nobody has seen before buys: the monitor brings
/// its next sweep forward and finds the shell at once instead of within its own
/// interval. Nothing is filtered by it — a correlation the process table has and
/// no event ever mentioned is reported just the same — so falling behind the
/// published stream costs latency and nothing else.
fn wanted(
    published: &mut tokio::sync::broadcast::Receiver<Published>,
    monitor: &mut Monitor,
) -> bool {
    loop {
        match published.try_recv() {
            Ok(Published::Event(event)) => {
                if let Some(correlation) = event
                    .correlation
                    .as_deref()
                    .filter(|correlation| !correlation.is_empty())
                {
                    monitor.want(correlation);
                }
            }
            Ok(Published::Foreground(_)) => {}
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Lagged(lines)) => {
                debug!(
                    lines,
                    "the foreground monitor fell behind the published stream"
                );
            }
            // Unreachable while this holds the bus the channel belongs to, and
            // the honest answer to it anyway: a bus nobody can publish to has
            // no use for anything this would observe.
            Err(TryRecvError::Closed) => return false,
        }
    }
}

/// The signals that ask a daemon to stop, listened for from before it has
/// anything to stop.
///
/// Installing the handlers is what takes the signals away from their default
/// disposition, which is to end the process where it stands, so it has to happen
/// before the daemon is visible to anything that might send one.
#[derive(Debug)]
struct Signals {
    term: Signal,
    interrupt: Signal,
    hangup: Signal,
}

impl Signals {
    /// Starts listening, or explains why this daemon will not stop politely.
    ///
    /// A daemon whose handlers could not be installed carries on serving and the
    /// signals keep their default disposition, which ends the process abruptly.
    /// That is a worse exit than the orderly one, not a broken one: what it
    /// leaves behind is exactly what a `SIGKILL` leaves behind, and the next
    /// start clears it.
    fn install() -> Option<Self> {
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
            signal(SignalKind::hangup()),
        ) {
            (Ok(term), Ok(interrupt), Ok(hangup)) => Some(Self {
                term,
                interrupt,
                hangup,
            }),
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                error!(%error, "cannot listen for termination signals; shutdown will not be orderly");
                None
            }
        }
    }
}

/// The signals that stop a daemon, and the one that asks it to read its
/// declarations again, as two things that are waited for in different places.
fn split(signals: Option<Signals>) -> (Option<(Signal, Signal)>, Option<Signal>) {
    match signals {
        Some(Signals {
            term,
            interrupt,
            hangup,
        }) => (Some((term, interrupt)), Some(hangup)),
        None => (None, None),
    }
}

/// Resolves when this process is asked to stop, or never, for a daemon that
/// could not listen for the asking.
async fn terminated(stops: Option<(Signal, Signal)>) -> Stopped {
    let Some((mut term, mut interrupt)) = stops else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => Stopped::Terminated,
        _ = interrupt.recv() => Stopped::Interrupted,
    }
}

/// Removes anything left in a directory this daemon has just claimed by a run
/// that did not get to clean up after itself.
///
/// Holding the lock is what makes this safe: no live daemon owns anything here,
/// so what is found is the remains of one that died without cleaning up. For the
/// sockets it is also necessary — binding over a file that exists is impossible
/// — and for the rest it is what keeps something reading the directory from
/// being told about a state of the world that ended when that daemon did.
fn clear_stale_files(paths: &SocketPaths) -> Result<(), Error> {
    for file in paths.ephemeral() {
        match std::fs::remove_file(file) {
            Ok(()) => info!(path = %file.display(), "removed a file left by an earlier run"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Stale {
                    path: file.to_owned(),
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
