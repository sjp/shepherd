//! The socket subscribers read the stream from.
//!
//! The exchange has no request in it. A client connects and reads; the daemon
//! writes a snapshot, then every line it takes in or produces afterwards — an
//! event as it is ingested, a claim an observer made about a correlated slot, a
//! change in what is running in front of a correlated shell — and a heartbeat
//! whenever the stream would otherwise be silent for too long. Nothing a client
//! writes is ever read as a message — it is drained and discarded — because a
//! socket with no request protocol on it cannot be wedged by a client that
//! misunderstands one.
//!
//! # Why the snapshot comes first
//!
//! A subscriber that has just connected has to learn *current state*, not merely
//! future events, or a session that went quiet an hour ago would be invisible
//! until it did something. Sending the snapshot before anything else also makes
//! reconnection the whole of a subscriber's recovery story: drop the connection,
//! open another, and what arrives is correct by construction.
//!
//! # Why a slow subscriber is dropped rather than waited for
//!
//! Every subscriber has a bounded queue of [`SUBSCRIBER_QUEUE`] lines and a task
//! of its own that drains it into the socket. A line that arrives for a full
//! queue does not wait: the subscriber is disconnected on the spot. Nothing on
//! the publishing side ever blocks, so a subscriber that has stopped reading —
//! a GUI stalled behind a modal dialog, a pipe nobody is draining — cannot slow
//! the ingest of the next event, and cannot slow any other subscriber either.
//!
//! That policy is what keeps the guarantee an emitter needs. An emitter is a
//! hook inside somebody's coding agent, with a few milliseconds to spend and no
//! way to report a problem; an emit that blocked on a subscriber would be an
//! agent that hangs because something watching it is slow. The cost of the
//! policy is a subscriber that occasionally has to reconnect, which costs it one
//! snapshot.

use std::fs::Permissions;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agentbus_protocol::Heartbeat;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use crate::bus::{Bus, Published};
use crate::{ACCEPT_RETRY_DELAY, clock};
use agentbus_paths::SOCKET_MODE;

/// How many lines may be waiting to be written to one subscriber.
///
/// Enough to absorb the burst a busy machine produces while a subscriber is
/// briefly busy elsewhere, and small enough that a subscriber which has genuinely
/// stopped reading is found out in short order rather than accumulating a backlog
/// nobody will ever want.
pub const SUBSCRIBER_QUEUE: usize = 1024;

/// How often a heartbeat is written when nothing else is happening.
///
/// The heartbeat is what distinguishes a dead stream from a quiet one, so it goes
/// out on a fixed schedule rather than only when the stream has been idle: a
/// subscriber can then say it has seen nothing for three intervals and reconnect
/// without having to model what the daemon considers activity.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(10);

/// The listening half of the subscribe socket.
#[derive(Debug)]
pub struct SubscribeListener {
    listener: UnixListener,
    path: PathBuf,
}

impl SubscribeListener {
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

    /// Accepts subscribers and serves each of them until it goes away, until the
    /// task running this is dropped.
    pub async fn serve(self, bus: Arc<Bus>, heartbeat: Duration) {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    let bus = Arc::clone(&bus);
                    tokio::spawn(publish(stream, bus, heartbeat));
                }
                Err(error) => {
                    // Accept failures are the daemon's problem, not any one
                    // subscriber's: the file descriptor table filling up is the
                    // usual cause, and it clears on its own.
                    warn!(%error, "cannot accept a connection on the subscribe socket");
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                }
            }
        }
    }
}

/// Serves one subscriber: a snapshot, then the stream, until either end stops.
///
/// The two halves of the connection are handled apart from each other on
/// purpose. Deciding what to send is the job of this task, which never blocks;
/// putting it on the wire is the job of another, which blocks whenever the
/// subscriber is slow. Ending the subscription therefore means abandoning the
/// writer wherever it got to, rather than waiting for a backlog to drain into a
/// socket nobody is reading.
async fn publish(stream: UnixStream, bus: Arc<Bus>, heartbeat: Duration) {
    let (mut incoming, outgoing) = stream.into_split();
    let (queue, backlog) = mpsc::channel::<String>(SUBSCRIBER_QUEUE);
    let (snapshot, events) = bus.subscribe();
    let seq = snapshot.seq;

    let Some(first) = line(&snapshot) else {
        return;
    };
    // The queue is empty and its bound is not one, so this cannot be refused.
    let _ = queue.try_send(first);
    let writing = tokio::spawn(write(outgoing, backlog));

    let ended = tokio::select! {
        () = discard(&mut incoming) => Ended::Disconnected,
        ended = feed(bus, events, seq, &queue, heartbeat) => ended,
    };
    // Whatever it was still writing is no longer wanted: a subscriber that has
    // gone, or one being dropped for falling behind, is not owed the backlog.
    writing.abort();

    match ended {
        Ended::Disconnected => debug!("a subscriber disconnected"),
        Ended::Overflowed => {
            warn!(
                queue = SUBSCRIBER_QUEUE,
                "dropped a subscriber that was not keeping up"
            );
        }
    }
}

/// Why a subscription ended.
enum Ended {
    /// The subscriber closed the connection, or the connection failed. Ordinary.
    Disconnected,
    /// The subscriber fell far enough behind to fill its queue.
    Overflowed,
}

/// Queues every line this subscriber should see, until it cannot keep up or has
/// gone away.
///
/// What the snapshot already reflects is not queued again. A line is published
/// to subscribers a moment after it enters the state the snapshot is built
/// from, so a subscription that begins in that moment sees it in its snapshot
/// *and* on the stream; comparing against the snapshot's sequence number is
/// what makes the stream a continuation of the snapshot rather than an overlap
/// with it, and it works for an observation exactly as it works for an event
/// because both are numbered from the one counter.
async fn feed(
    bus: Arc<Bus>,
    mut events: tokio::sync::broadcast::Receiver<Published>,
    snapshot_seq: u64,
    queue: &mpsc::Sender<String>,
    heartbeat: Duration,
) -> Ended {
    // Starting an interval one period from now, rather than letting it fire
    // immediately, keeps the first heartbeat a heartbeat rather than a second
    // opening line nobody asked for.
    let heartbeat = heartbeat.max(Duration::from_millis(1));
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + heartbeat, heartbeat);
    loop {
        let line = tokio::select! {
            published = events.recv() => match published {
                Ok(published) if published.seq() <= snapshot_seq => continue,
                Ok(Published::Event(event)) => line(&event),
                Ok(Published::Foreground(change)) => line(&change),
                Ok(Published::Assertion(assertion)) => line(&assertion),
                // Lagging means lines were published faster than this
                // subscriber's own queue could be filled, which is the same
                // failure as an overflow and is treated the same way.
                Err(RecvError::Lagged(lines)) => {
                    debug!(lines, "a subscriber fell behind the published stream");
                    return Ended::Overflowed;
                }
                // The bus is gone, which only happens as the daemon exits.
                Err(RecvError::Closed) => return Ended::Disconnected,
            },
            _ = beat.tick() => line(&Heartbeat::new(bus.last_seq(), clock::now())),
        };
        let Some(line) = line else {
            continue;
        };
        match queue.try_send(line) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Ended::Overflowed,
            Err(mpsc::error::TrySendError::Closed(_)) => return Ended::Disconnected,
        }
    }
}

/// Writes queued lines to the subscriber, until the queue closes or the socket
/// will not take any more.
async fn write(
    mut outgoing: tokio::net::unix::OwnedWriteHalf,
    mut backlog: mpsc::Receiver<String>,
) {
    while let Some(line) = backlog.recv().await {
        if let Err(error) = outgoing.write_all(line.as_bytes()).await {
            debug!(%error, "cannot write to a subscriber");
            return;
        }
    }
}

/// Reads and throws away anything the subscriber sends, and resolves only if the
/// connection itself fails.
///
/// There is nothing a subscriber can say on this socket, so what it sends is
/// read to be discarded rather than to be understood — but it is read, because a
/// client whose writes were never consumed would eventually block on one.
///
/// A subscriber that stops writing has not gone anywhere. Closing the writing
/// half is a perfectly ordinary thing for a client with nothing to say to do —
/// anything piping `/dev/null` into this socket does it on the way in — so the
/// end of what it has to say is not the end of what it is owed. A subscription
/// ends when writing to it fails, which is what a subscriber that has really
/// gone away causes at the next line or, failing that, at the next heartbeat.
async fn discard(incoming: &mut tokio::net::unix::OwnedReadHalf) {
    let mut chunk = [0_u8; 1024];
    loop {
        match incoming.read(&mut chunk).await {
            Ok(0) => return std::future::pending().await,
            Err(_) => return,
            Ok(read) => trace!(bytes = read, "discarded what a subscriber wrote"),
        }
    }
}

/// One stream line, serialized and newline-terminated.
///
/// A value that cannot be serialized is not a stream this daemon can carry, and
/// there is nobody to tell: the subscriber is reading JSON and a half-written
/// line would be worse than a missing one. This cannot happen for the types on
/// this stream, all of which are plain structs.
fn line<T: Serialize>(value: &T) -> Option<String> {
    match serde_json::to_string(value) {
        Ok(mut line) => {
            line.push('\n');
            Some(line)
        }
        Err(error) => {
            warn!(%error, "cannot serialize a line for subscribers");
            None
        }
    }
}
