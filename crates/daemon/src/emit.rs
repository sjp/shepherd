//! The socket emitters send events to.
//!
//! The exchange is one line long and one-directional: a client connects, writes
//! one JSON line, and closes without waiting for anything. That is what the
//! client on the other end needs it to be — it is a hook running inside somebody's
//! coding agent, with a budget of a few milliseconds and no way to report a
//! problem — so this side never writes a byte back, never negotiates, and never
//! gives a client a reason to wait.
//!
//! Which makes the server's whole job defensive. Every connection is handled in
//! its own task, and each one is bounded twice over: a client cannot spend more
//! than [`READ_TIMEOUT`] delivering its line, and cannot make the daemon hold
//! more than [`MAX_LINE`] of it in memory. Neither bound can be reached by an
//! honest emitter, and neither lets a dishonest one — or a hung one — stop the
//! next hook's event from being ingested.

use std::fs::Permissions;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, trace, warn};

use crate::bus::Bus;
use crate::paths::SOCKET_MODE;

/// The most of one line the daemon will hold.
///
/// An event carries the agent's payload verbatim, which can be a whole tool
/// result, so the bound is generous; it exists to put a ceiling on what one
/// connection can cost, not to police the size of a legitimate event.
pub const MAX_LINE: usize = 1024 * 1024;

/// How long a client has to deliver its line once it has connected.
///
/// An emitter writes everything it has immediately and closes. One second is
/// therefore an eternity for the honest case, while still making a connection
/// that arrives and then says nothing something the daemon forgets about
/// promptly rather than a task it holds open indefinitely.
pub const READ_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to wait after a failed `accept` before trying again, so that a
/// listener in a state that cannot be accepted on spins slowly rather than
/// burning a core.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// The listening half of the emit socket.
#[derive(Debug)]
pub struct EmitListener {
    listener: UnixListener,
    path: PathBuf,
}

impl EmitListener {
    /// Binds the socket at `path` and restricts it to its owner.
    ///
    /// The mode is applied after binding because that is the only order the
    /// platform offers; the window is harmless because the directory the socket
    /// sits in is already the owner's alone.
    pub fn bind(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, Permissions::from_mode(SOCKET_MODE))?;
        Ok(Self { listener, path })
    }

    /// The path the socket is bound at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accepts connections and ingests what they deliver, until the task running
    /// this is dropped.
    pub async fn serve(self, bus: Arc<Bus>) {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    let bus = Arc::clone(&bus);
                    tokio::spawn(receive(stream, bus));
                }
                Err(error) => {
                    // Accept failures are the daemon's problem, not any one
                    // client's: the file descriptor table filling up is the
                    // usual cause, and it clears on its own.
                    warn!(%error, "cannot accept a connection on the emit socket");
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                }
            }
        }
    }
}

/// Reads one connection's line and hands it to the bus.
async fn receive(mut stream: UnixStream, bus: Arc<Bus>) {
    match tokio::time::timeout(READ_TIMEOUT, read_line(&mut stream)).await {
        Ok(Ok(line)) => {
            if let Some(event) = bus.ingest(&line) {
                trace!(seq = event.seq, kind = %event.kind, "ingested an event");
            }
        }
        Ok(Err(ReadError::TooLong)) => {
            debug!(limit = MAX_LINE, "dropped an over-long line");
        }
        Ok(Err(ReadError::Io(error))) => {
            debug!(%error, "dropped a connection that could not be read");
        }
        Err(_) => {
            debug!(
                timeout_ms = READ_TIMEOUT.as_millis(),
                "dropped a connection that delivered nothing in time"
            );
        }
    }
}

/// Why a connection produced no line.
enum ReadError {
    /// The client sent more than [`MAX_LINE`] without a newline.
    TooLong,
    /// The connection could not be read.
    Io(io::Error),
}

/// Reads up to the first newline, or to end of input if the client closed
/// without one.
///
/// Reading stops at [`MAX_LINE`], so a client that never sends a newline costs a
/// bounded amount of memory whatever it does. Anything after the first newline
/// is not read: the protocol is one line per connection, and a client with more
/// to say opens another one.
async fn read_line(stream: &mut UnixStream) -> Result<Vec<u8>, ReadError> {
    let mut line = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = stream.read(&mut chunk).await.map_err(ReadError::Io)?;
        if read == 0 {
            return Ok(line);
        }
        let chunk = &chunk[..read];
        if let Some(end) = chunk.iter().position(|byte| *byte == b'\n') {
            line.extend_from_slice(&chunk[..end]);
            return if line.len() > MAX_LINE {
                Err(ReadError::TooLong)
            } else {
                Ok(line)
            };
        }
        line.extend_from_slice(chunk);
        if line.len() > MAX_LINE {
            return Err(ReadError::TooLong);
        }
    }
}
