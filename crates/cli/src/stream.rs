//! Reading a daemon's stream, for the commands that consume it.
//!
//! The subscribe socket has no request protocol on it: a client connects and
//! reads, and the first thing it reads is always a snapshot. So there is nothing
//! here but a connection and a line reader, and both commands that use it are
//! built out of those two things.
//!
//! Blocking sockets, deliberately. Following a stream is one connection and one
//! read at a time, which an asynchronous runtime would make no faster and would
//! make slower to start; and the whole point of `subscribe` is to be a pipe that
//! anything can put in front of `jq`.

use std::io::{self, BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Instant;

use agentbus_daemon::SocketPaths;
use agentbus_protocol::{Snapshot, StreamLine};
use thiserror::Error;

/// Why a stream could not be read.
#[derive(Debug, Error)]
pub enum Error {
    /// Nothing is listening on the socket, or there is no socket at all.
    #[error("no daemon running in {}", dir.display())]
    NoDaemon {
        /// The directory that was looked in.
        dir: PathBuf,
    },
    /// The socket is there and something is listening, but it could not be
    /// connected to.
    #[error("cannot connect to {}", path.display())]
    Connect {
        /// The socket that could not be connected to.
        path: PathBuf,
        /// What the system said.
        #[source]
        source: io::Error,
    },
    /// The connection failed while it was being read.
    #[error("cannot read the stream")]
    Read(#[source] io::Error),
    /// The daemon closed the connection before saying anything.
    #[error("the daemon closed the connection without sending a snapshot")]
    Closed,
    /// The first line was not a snapshot, so this is not a stream this build
    /// knows how to read.
    #[error("the stream did not begin with a snapshot")]
    NotASnapshot,
}

/// A connection to a daemon's subscribe socket.
#[derive(Debug)]
pub struct Stream {
    lines: BufReader<UnixStream>,
}

/// Connects to the daemon serving `paths`.
///
/// A socket that is absent and a socket nothing is listening on are the same
/// news to whoever ran the command — there is no bus here — and are reported as
/// one thing. Anything else is reported as itself, because a socket that exists
/// and cannot be used is a different problem and saying "no daemon" about it
/// would send someone looking in the wrong place.
pub fn connect(paths: &SocketPaths) -> Result<Stream, Error> {
    match UnixStream::connect(paths.sub()) {
        Ok(socket) => Ok(Stream {
            lines: BufReader::new(socket),
        }),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(Error::NoDaemon {
                dir: paths.dir().to_owned(),
            })
        }
        Err(source) => Err(Error::Connect {
            path: paths.sub().to_owned(),
            source,
        }),
    }
}

impl Stream {
    /// The next line, newline included, or nothing once the daemon has closed
    /// the connection.
    ///
    /// The bytes are handed back as they arrived. This is what `subscribe`
    /// copies to its stdout, and a stream that is a public interface has to be
    /// passed on verbatim rather than through anything that might reformat it.
    pub fn line(&mut self) -> Result<Option<Vec<u8>>, Error> {
        let mut line = Vec::new();
        match self.lines.read_until(b'\n', &mut line) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(line)),
            Err(error) => Err(Error::Read(error)),
        }
    }

    /// The next line, or nothing if none arrives before `deadline`.
    ///
    /// Waiting for a bounded time is the only way to stop reading a stream that
    /// is behaving perfectly: the daemon has no more to say at the moment and
    /// will not close the connection to prove it.
    pub fn line_before(&mut self, deadline: Instant) -> Result<Option<Vec<u8>>, Error> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        self.lines
            .get_ref()
            .set_read_timeout(Some(remaining))
            .map_err(Error::Read)?;
        match self.line() {
            Ok(line) => Ok(line),
            // A read that stopped because the time was up is the ordinary way
            // out of a tail, not a failure to report.
            Err(Error::Read(error)) if timed_out(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The snapshot every stream begins with, as it arrived and as it parses.
    ///
    /// Both, because the two commands want different halves of it and neither
    /// should have to reconstruct the other's: printing the line a daemon sent
    /// is not the same as printing a line built from what was understood of it.
    pub fn snapshot(&mut self) -> Result<(Vec<u8>, Snapshot), Error> {
        let line = self.line()?.ok_or(Error::Closed)?;
        match serde_json::from_slice(&line) {
            Ok(StreamLine::Snapshot(snapshot)) => Ok((line, snapshot)),
            _ => Err(Error::NotASnapshot),
        }
    }
}

/// Whether a read failed because its timeout expired.
///
/// Which of the two kinds a socket read timeout produces is the platform's
/// choice, so both count.
fn timed_out(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Whether a line is the daemon's keepalive rather than something that happened.
///
/// A tail is showing someone what their agents are doing; a heartbeat says only
/// that the connection is alive, which they can see from the fact that they are
/// looking at it.
pub fn is_heartbeat(line: &[u8]) -> bool {
    matches!(
        serde_json::from_slice::<StreamLine>(line),
        Ok(StreamLine::Heartbeat(_))
    )
}
