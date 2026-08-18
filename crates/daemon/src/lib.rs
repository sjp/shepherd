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
//! use agentbus_daemon::{Daemon, SocketPaths};
//!
//! Daemon::bind(SocketPaths::resolve())?.run().await;
//! # Ok(())
//! # }
//! ```
//!
//! [`Daemon::bind`] does everything that can fail — creating the directory and
//! binding the sockets — so that a daemon which starts at all is one that is
//! listening, and a caller finds out about a socket it cannot bind immediately
//! rather than from a client's silence later. [`Daemon::run`] then serves until
//! the future is dropped.
//!
//! There is one code path, wherever this runs. Nothing in this crate asks what
//! kind of machine, session or container it is in; the socket directory rules in
//! [`paths`] cover those differences by construction.

#![warn(missing_docs)]

pub mod bus;
pub mod clock;
pub mod emit;
pub mod paths;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tracing::info;

pub use bus::Bus;
pub use emit::EmitListener;
pub use paths::SocketPaths;

/// How often every session's clock is moved forward.
///
/// A session becomes stale, and a finished one is forgotten, on the strength of
/// a tick, so this is the granularity of both. One second is far finer than
/// either timeout needs and costs one pass over a handful of sessions.
pub const TICK_INTERVAL: Duration = Duration::from_secs(1);

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
    /// A socket could not be bound. Usually another daemon is already running.
    #[error("cannot bind {}", path.display())]
    Bind {
        /// The socket that was being bound.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
}

/// A running bus: the sockets it listens on and the state behind them.
#[derive(Debug)]
pub struct Daemon {
    paths: SocketPaths,
    bus: Arc<Bus>,
    emit: EmitListener,
}

impl Daemon {
    /// Prepares the socket directory and binds every socket.
    pub fn bind(paths: SocketPaths) -> Result<Self, Error> {
        paths.create_dir().map_err(|source| Error::Directory {
            path: paths.dir().to_owned(),
            source,
        })?;
        let emit = EmitListener::bind(paths.emit()).map_err(|source| Error::Bind {
            path: paths.emit().to_owned(),
            source,
        })?;
        Ok(Self {
            paths,
            bus: Arc::new(Bus::new()),
            emit,
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

    /// Serves until the future running this is dropped.
    pub async fn run(self) {
        info!(socket = %self.emit.path().display(), "listening for events");
        let ticking = tick(Arc::clone(&self.bus));
        let serving = self.emit.serve(Arc::clone(&self.bus));
        tokio::join!(ticking, serving);
    }
}

/// Moves every session's clock forward, forever.
async fn tick(bus: Arc<Bus>) {
    let mut interval = tokio::time::interval(TICK_INTERVAL);
    loop {
        interval.tick().await;
        bus.tick(&clock::now());
    }
}
