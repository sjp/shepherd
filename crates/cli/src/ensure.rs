//! Starting a daemon that outlives whoever wanted it.
//!
//! Subscribing to a bus that is not running is ordinarily a mistake worth
//! reporting — somebody typed a command in a directory where nothing is
//! listening. But there is a case where it is not: something that has just
//! arrived on a machine and wants the bus there needs the daemon started as part
//! of connecting to it, because there is nobody on that machine to start it.
//!
//! # Why the daemon has to survive this process
//!
//! A daemon that died with the connection would take the state with it, and the
//! state is the point. An agent that said it was blocked before a connection
//! dropped never says so again — it is blocked, waiting for the person who is no
//! longer being told about it — so a reconnect that found a fresh daemon would
//! find an empty one, and exactly the situation the bus exists to show would be
//! the one it silently lost. So the daemon is put beyond this process entirely:
//! its own session, its own working directory, its output in a file, and not a
//! child of anything that will go away.
//!
//! What is started is an ordinary daemon. Nothing about it knows it was started
//! by a subscriber, and `agentbus daemon` is unchanged by any of this.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agentbus_daemon::SocketPaths;
use agentbus_paths::{LOG_FILE, LOG_MODE};
use thiserror::Error;
use tracing::debug;

/// How long to wait for a daemon that has just been started to begin serving.
///
/// Long enough for a machine under load to get a process to the point of binding
/// two sockets, short enough that a caller which is never going to get an answer
/// finds out while it still has somewhere to report it.
pub const PATIENCE: Duration = Duration::from_secs(2);

/// How often the socket is tried while waiting.
const POLL: Duration = Duration::from_millis(50);

/// Why no daemon could be got running.
#[derive(Debug, Error)]
pub enum Error {
    /// The directory the sockets would go in could not be prepared.
    #[error("cannot prepare the bus directory {}", dir.display())]
    Directory {
        /// The directory that was being prepared.
        dir: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
    /// The file the daemon's output would go to could not be opened.
    #[error("cannot open {}", path.display())]
    Log {
        /// The log file.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
    /// This program could not find its own executable, so there was nothing to
    /// start.
    #[error("cannot find this executable")]
    NoExecutable(#[source] io::Error),
    /// The daemon process could not be started.
    #[error("cannot start a daemon in {}", dir.display())]
    Start {
        /// The directory it would have served.
        dir: PathBuf,
        /// What the system said.
        #[source]
        source: io::Error,
    },
    /// A daemon was started and nothing was listening by the time the patience
    /// ran out.
    #[error("nothing is serving {} after {PATIENCE:?}; see {LOG_FILE} there", dir.display())]
    NotServing {
        /// The directory that was waited on.
        dir: PathBuf,
    },
}

/// Makes sure a daemon is serving `paths`, starting one if none is.
///
/// Returns once the subscribe socket accepts a connection, so a caller may
/// connect immediately afterwards without racing what it has just started.
///
/// Two callers doing this at once is ordinary and needs no arrangement between
/// them: the loser of the race for the directory's lock exits at once and says
/// so in the log, the winner serves, and both callers are waiting for the same
/// socket to answer.
pub fn daemon(paths: &SocketPaths) -> Result<(), Error> {
    paths.create_dir().map_err(|source| Error::Directory {
        dir: paths.dir().to_owned(),
        source,
    })?;
    let path = paths.log().to_owned();
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(LOG_MODE)
        .open(&path)
        .map_err(|source| Error::Log {
            path: path.clone(),
            source,
        })?;
    let errors = log.try_clone().map_err(|source| Error::Log {
        path: path.clone(),
        source,
    })?;
    let executable = std::env::current_exe().map_err(Error::NoExecutable)?;

    let mut command = Command::new(executable);
    command
        .arg("daemon")
        .arg("--dir")
        .arg(paths.dir())
        // Nothing about the daemon's own work depends on where it was started,
        // and holding a directory open would keep a filesystem busy that
        // whoever started it may want to unmount.
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(errors);
    // Safe by construction: what runs between the fork and the exec is three
    // system calls, each of which is safe to make in a forked child, and no
    // allocation, no lock and nothing of this process's own.
    unsafe {
        command.pre_exec(detach);
    }
    let mut started = command.spawn().map_err(|source| Error::Start {
        dir: paths.dir().to_owned(),
        source,
    })?;
    // The process that was spawned is not the daemon: it forked one and exited.
    // Waiting for it is what keeps it from lingering as a zombie, and it has
    // already gone.
    let _ = started.wait();

    debug!(dir = %paths.dir().display(), "started a daemon");
    serving(paths)
}

/// Between the fork and the exec, in the child: puts the daemon out of this
/// process's reach.
fn detach() -> io::Result<()> {
    // Forking again means the process that goes on to be the daemon is nobody's
    // child: the one that was spawned exits here, is reaped immediately, and
    // what is left is adopted by init. So the daemon survives whatever happens
    // to the process that wanted it, and leaves nothing behind if it does not.
    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error()),
        0 => {}
        _ => unsafe { libc::_exit(0) },
    }
    // A session of its own, so that a signal sent to the terminal — the ^C that
    // stops the program that started this — is not also sent to the daemon.
    match unsafe { libc::setsid() } {
        -1 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}

/// Waits until something accepts a connection on the subscribe socket.
///
/// Connecting is the test, rather than the socket file existing: a file left
/// behind by a daemon that was killed outright exists and answers nothing, and a
/// caller told to go ahead on the strength of it would fail on the very next
/// line.
fn serving(paths: &SocketPaths) -> Result<(), Error> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if UnixStream::connect(paths.sub()).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::NotServing {
                dir: paths.dir().to_owned(),
            });
        }
        std::thread::sleep(POLL);
    }
}
