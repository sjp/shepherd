//! One connection to the subscribe socket, kept up.
//!
//! The socket has no request protocol on it: connect, and read lines until
//! something stops. So everything here is about the "until something stops"
//! half — noticing that a stream is no longer one, and opening another.
//!
//! # What counts as no longer trustworthy
//!
//! Four things, and all four are answered by reconnecting:
//!
//! - **The connection ended.** The daemon exited, or dropped this subscriber for
//!   falling behind — the bus disconnects a slow reader rather than blocking the
//!   agent whose hook is trying to emit.
//! - **Nothing arrived for [`DEFAULT_SILENCE`].** A heartbeat is written every
//!   ten seconds whatever else is happening, so silence for three of those is a
//!   dead stream rather than a quiet one. Nothing else can tell the two apart.
//! - **A sequence number was skipped.** Every line the daemon publishes is
//!   numbered from one counter, so the next line's number is knowable, and one
//!   that is higher means something in between was not received. That is what
//!   the counter is for; a timestamp cannot say it.
//! - **The stream did not begin with a snapshot.** The first line always is one.
//!   A stream that begins with anything else is not one this can read, because
//!   there is no current state to read the rest as changes to.
//!
//! Only lines that were allotted a number take part in the third of those.
//! A heartbeat carries the counter *as of now* rather than a number of its own,
//! so it can legitimately name a line that is still on its way here, and
//! reconnecting on that would mean reconnecting at random.

use std::io::{self, BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use agentbus_paths::SocketPaths;
use agentbus_protocol::{
    Event, ForegroundChange, Heartbeat, Snapshot, StampedAssertion, StreamLine,
};
use tracing::{debug, trace, warn};

/// How long a stream may say nothing at all before it is presumed dead.
///
/// Three heartbeats' worth. Two would be a reconnection every time one was late;
/// much more than three would be minutes of a sidebar showing what used to be
/// true.
pub const DEFAULT_SILENCE: Duration = Duration::from_secs(30);

/// How long to wait before the first attempt to reconnect.
///
/// Short enough that a daemon restarting under a running window is not noticed
/// as an outage, long enough that a socket which refuses every connection is not
/// hammered.
pub const DEFAULT_FIRST_RETRY: Duration = Duration::from_millis(250);

/// The longest wait between attempts to reconnect.
///
/// Where the wait ends up when there is no daemon at all, which is an ordinary
/// state rather than a failure: nothing is missed by asking every few seconds,
/// because a daemon that starts has nothing to say about the time before it did.
pub const DEFAULT_MAX_RETRY: Duration = Duration::from_secs(5);

/// How many updates may be waiting for the caller.
///
/// The same order as the queue the daemon keeps per subscriber, for the same
/// reason: enough to absorb the burst a busy machine produces while the caller
/// is briefly busy elsewhere. What happens when it fills is deliberate and is
/// described on [`Subscriber::spawn`].
pub const DEFAULT_QUEUE: usize = 1024;

/// How often the reading thread stops to look at the clock and at whether it has
/// been asked to stop.
///
/// The socket's read timeout, so an idle stream costs one syscall ten times a
/// second and a shutdown is never waiting on the next line to arrive.
const POLL: Duration = Duration::from_millis(100);

/// What a subscriber tells its caller.
///
/// The lines that carry no information a caller could act on — a `kind` this
/// build does not recognize — are not among these: ignoring what it does not
/// understand is the reader's job, done once here rather than at every place
/// that matches on one of these.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// Everything the bus knows, as of the moment this connection opened.
    ///
    /// **Supersedes** everything said before it. Whatever a caller derived from
    /// earlier updates is to be discarded rather than merged with this: after a
    /// reconnection the bus's account is the whole account, and a session it no
    /// longer lists is a session that is no longer there.
    Reset(Snapshot),
    /// A normalized lifecycle event.
    Event(Event),
    /// A change in what is running in front of a correlated shell.
    Foreground(ForegroundChange),
    /// A claim something watching from outside made about a correlated slot.
    Assertion(StampedAssertion),
    /// Proof the stream is alive rather than merely quiet.
    ///
    /// Nothing has to be done with it — the reader already treats silence as its
    /// own problem — but it is passed on so that anything showing whether the
    /// bus is reachable can say when it was last heard from.
    Heartbeat(Heartbeat),
    /// The connection went away, and nothing more will arrive until the next
    /// [`Update::Reset`].
    ///
    /// What was known is not thereby untrue: sessions carry on whether or not
    /// this can see them. It says only that what is on screen is now a
    /// recollection rather than an observation.
    Disconnected,
}

/// A subscription to the bus, before it is started.
#[derive(Debug, Clone)]
pub struct Subscriber {
    paths: SocketPaths,
    silence: Duration,
    first_retry: Duration,
    max_retry: Duration,
    queue: usize,
}

impl Subscriber {
    /// A subscription to whichever bus this environment names.
    pub fn resolve() -> Self {
        Self::at(SocketPaths::resolve())
    }

    /// A subscription to the bus in a directory chosen by the caller.
    pub fn at(paths: SocketPaths) -> Self {
        Self {
            paths,
            silence: DEFAULT_SILENCE,
            first_retry: DEFAULT_FIRST_RETRY,
            max_retry: DEFAULT_MAX_RETRY,
            queue: DEFAULT_QUEUE,
        }
    }

    /// Presumes a stream dead after `silence` rather than [`DEFAULT_SILENCE`].
    #[must_use]
    pub const fn with_silence(mut self, silence: Duration) -> Self {
        self.silence = silence;
        self
    }

    /// Waits `first` before the first attempt to reconnect and at most `max`
    /// between later ones.
    #[must_use]
    pub const fn with_backoff(mut self, first: Duration, max: Duration) -> Self {
        self.first_retry = first;
        self.max_retry = max;
        self
    }

    /// Holds `updates` of them for the caller rather than [`DEFAULT_QUEUE`].
    #[must_use]
    pub const fn with_queue(mut self, updates: usize) -> Self {
        self.queue = updates;
        self
    }

    /// Where this subscription looks for the bus.
    pub fn paths(&self) -> &SocketPaths {
        &self.paths
    }

    /// Starts reading, on a thread of its own.
    ///
    /// Returns immediately, whether or not there is a daemon to connect to: a
    /// bus that is not running yet is an ordinary state, and the loop keeps
    /// trying until there is one or until the handle is dropped.
    ///
    /// A caller that stops draining the queue eventually blocks this thread on a
    /// full one, at which point the daemon stops being read from and drops this
    /// subscriber for falling behind — and the reconnection that follows brings
    /// a fresh snapshot. That is the protocol's own remedy for a slow
    /// subscriber, and letting it happen is what keeps a stalled window from
    /// silently accumulating a backlog of updates nobody will ever look at.
    pub fn spawn(self) -> SubscriberHandle {
        let (sent, updates) = sync_channel(self.queue.max(1));
        let stopping = Stopping::new();
        let reading = {
            let stopping = stopping.clone();
            thread::Builder::new()
                .name("bus-subscriber".to_owned())
                .spawn(move || Reader::new(self, sent, stopping).run())
                .expect("a thread to read the bus on")
        };
        SubscriberHandle {
            updates: Some(updates),
            stopping,
            reading: Some(reading),
        }
    }
}

/// A running subscription.
///
/// Dropping it stops the reading thread and waits for it, so a subscription
/// never outlives what asked for it.
#[derive(Debug)]
pub struct SubscriberHandle {
    /// Held in an [`Option`] only so that dropping the handle can let go of the
    /// receiving end *before* it waits for the thread; see [`Drop`].
    updates: Option<Receiver<Update>>,
    stopping: Stopping,
    reading: Option<JoinHandle<()>>,
}

impl SubscriberHandle {
    /// Everything the subscription has to say, in the order the bus said it.
    pub fn updates(&self) -> &Receiver<Update> {
        self.updates
            .as_ref()
            .expect("the queue is taken only as the handle is dropped")
    }

    /// Stops reading and waits for the thread, which is also what dropping the
    /// handle does.
    pub fn stop(self) {}
}

impl Drop for SubscriberHandle {
    fn drop(&mut self) {
        self.stopping.stop();
        // The receiving end goes first, on purpose. A reader blocked on a queue
        // this caller stopped draining is woken by the far end of it going
        // away; waiting for the thread while still holding it would be waiting
        // for something that is waiting for us.
        drop(self.updates.take());
        if let Some(reading) = self.reading.take() {
            let _ = reading.join();
        }
    }
}

/// The thread's own view of the subscription.
struct Reader {
    subscriber: Subscriber,
    updates: SyncSender<Update>,
    stopping: Stopping,
    /// Whether the caller has been told there is a stream. Ends one
    /// [`Update::Disconnected`] per connection actually lost, rather than one
    /// per attempt to make one.
    connected: bool,
}

/// What to do with the rest of a connection.
enum Verdict {
    /// Read on.
    Continue,
    /// Stop reading this one and open another.
    Reconnect,
}

impl Reader {
    fn new(subscriber: Subscriber, updates: SyncSender<Update>, stopping: Stopping) -> Self {
        Self {
            subscriber,
            updates,
            stopping,
            connected: false,
        }
    }

    /// Connects, reads, and connects again, until the handle goes away.
    fn run(mut self) {
        let mut wait = self.subscriber.first_retry;
        while self.stopping.running() {
            let streamed = self.serve();
            if self.connected {
                self.connected = false;
                self.send(Update::Disconnected);
            }
            // A connection that got as far as a snapshot is evidence that the
            // bus is there and answering, so the next interruption starts its
            // own backoff from the beginning rather than inheriting the wait
            // from an outage that is over.
            if streamed {
                wait = self.subscriber.first_retry;
            }
            if !self.stopping.nap(wait) {
                return;
            }
            wait = (wait * 2).min(self.subscriber.max_retry);
        }
    }

    /// One connection, from the snapshot that opens it to whatever ends it.
    ///
    /// Returns whether it got as far as that snapshot.
    fn serve(&mut self) -> bool {
        let Some(socket) = self.connect() else {
            return false;
        };
        let mut lines = Lines::new(socket);
        // The number the next numbered line should carry, once a snapshot has
        // said where the stream begins. `None` until then, which is also what
        // says that no line but a snapshot may be read yet.
        let mut expected: Option<u64> = None;
        let mut heard = Instant::now();
        while self.stopping.running() {
            match lines.read() {
                Incoming::Line(line) => {
                    heard = Instant::now();
                    if let Verdict::Reconnect = self.line(&line, &mut expected) {
                        return expected.is_some();
                    }
                }
                Incoming::Quiet => {
                    if heard.elapsed() >= self.subscriber.silence {
                        warn!(
                            silence = ?self.subscriber.silence,
                            "the bus has said nothing for longer than a live stream can"
                        );
                        return expected.is_some();
                    }
                }
                Incoming::Ended => {
                    debug!("the bus closed the connection");
                    return expected.is_some();
                }
            }
        }
        expected.is_some()
    }

    /// Opens a connection, or says why there is not one to open.
    fn connect(&self) -> Option<UnixStream> {
        let socket = match UnixStream::connect(self.subscriber.paths.sub()) {
            Ok(socket) => socket,
            Err(error) => {
                // No socket, or nothing behind one, is what a machine whose bus
                // is not running looks like. It is worth saying, and it is not
                // worth warning about.
                debug!(
                    path = %self.subscriber.paths.sub().display(),
                    %error,
                    "cannot reach the bus"
                );
                return None;
            }
        };
        // Every wait this thread does is this timeout: it is what lets a stream
        // that has gone silent, and a handle that has been dropped, both be
        // noticed by something that is otherwise blocked on a read.
        if let Err(error) = socket.set_read_timeout(Some(POLL)) {
            warn!(%error, "cannot put a timeout on the connection to the bus");
            return None;
        }
        Some(socket)
    }

    /// Reads one line and decides what it means for the rest of the connection.
    fn line(&mut self, line: &[u8], expected: &mut Option<u64>) -> Verdict {
        let parsed = match serde_json::from_slice::<StreamLine>(line) {
            Ok(parsed) => parsed,
            Err(error) => {
                // A line this cannot parse at all is a producer that has broken
                // the envelope, which is not the same as one that has said
                // something new: the second is `StreamLine::Unknown` and is
                // ordinary. Dropping the line keeps a stream readable through
                // one bad value.
                warn!(%error, "the bus sent a line that is not of this protocol");
                return self.before_a_snapshot(expected);
            }
        };
        match parsed {
            StreamLine::Snapshot(snapshot) => {
                *expected = Some(snapshot.seq.saturating_add(1));
                debug!(
                    seq = snapshot.seq,
                    sessions = snapshot.sessions.len(),
                    "the bus sent a snapshot"
                );
                self.connected = true;
                self.send(Update::Reset(snapshot));
                Verdict::Continue
            }
            _ if expected.is_none() => {
                // The first line is always a snapshot. Without one there is no
                // current state for the rest of the stream to be changes to, so
                // there is nothing to do with this connection but replace it.
                warn!("the bus did not begin the stream with a snapshot");
                Verdict::Reconnect
            }
            StreamLine::Heartbeat(heartbeat) => {
                // Numbered with the counter as of now rather than with a number
                // of its own, so it says only that the stream is alive.
                trace!(seq = heartbeat.seq, "the bus is alive");
                self.send(Update::Heartbeat(heartbeat));
                Verdict::Continue
            }
            StreamLine::Event(event) => self.carry(event.seq, expected, || Update::Event(event)),
            StreamLine::ForegroundChange(change) => {
                self.carry(change.seq, expected, || Update::Foreground(change))
            }
            StreamLine::Assertion(assertion) => {
                self.carry(assertion.seq, expected, || Update::Assertion(assertion))
            }
            StreamLine::Unknown => {
                // A daemon that has learned to say something this build does not
                // know. Ignoring it is what lets that daemon be newer than this
                // program without either of them being wrong — but the line still
                // took a number, and ignoring the number too would make the line
                // after it look like a gap and put an older subscriber into a
                // reconnection loop against a newer daemon. `seq` belongs to the
                // envelope rather than to any one kind, so it can be read from a
                // line whose kind cannot.
                trace!("the bus sent a line of a kind this build does not know");
                match numbering(line) {
                    Some(seq) => match step(seq, expected) {
                        Step::Gap => Verdict::Reconnect,
                        Step::Fresh | Step::Seen => Verdict::Continue,
                    },
                    None => Verdict::Continue,
                }
            }
        }
    }

    /// Passes on a line that was allotted a sequence number, unless the number
    /// says a line went missing on the way here.
    fn carry(
        &mut self,
        seq: u64,
        expected: &mut Option<u64>,
        update: impl FnOnce() -> Update,
    ) -> Verdict {
        match step(seq, expected) {
            Step::Fresh => {
                self.send(update());
                Verdict::Continue
            }
            Step::Seen => Verdict::Continue,
            Step::Gap => Verdict::Reconnect,
        }
    }

    /// What a line nobody could read means, which depends only on whether the
    /// stream has produced its snapshot yet.
    fn before_a_snapshot(&self, expected: &Option<u64>) -> Verdict {
        match expected {
            Some(_) => Verdict::Continue,
            None => Verdict::Reconnect,
        }
    }

    /// Hands an update to the caller, waiting for room if there is none.
    ///
    /// A queue nobody is draining ends this subscription rather than growing:
    /// the far end of it going away is how a caller that has dropped its handle
    /// stops a thread that is blocked here.
    fn send(&self, update: Update) {
        if self.updates.send(update).is_err() {
            debug!("nobody is reading the bus any more");
            self.stopping.stop();
        }
    }
}

/// Where a numbered line sits relative to the one that was expected next.
enum Step {
    /// The next line, which is nearly always what arrives.
    Fresh,
    /// A line at or before what the snapshot already accounts for. The daemon
    /// does not send these; applying one twice would be applying it twice.
    Seen,
    /// Something in between was not received.
    Gap,
}

/// Places `seq` against what was expected, and moves the expectation on.
fn step(seq: u64, expected: &mut Option<u64>) -> Step {
    // Only reachable before a snapshot has arrived, which is answered before
    // anything gets here; a numbered line with no stream to place it in is a gap
    // by any reading.
    let Some(next) = *expected else {
        return Step::Gap;
    };
    if seq > next {
        warn!(seq, expected = next, "the bus's stream skipped a line");
        return Step::Gap;
    }
    if seq < next {
        trace!(seq, expected = next, "a line the snapshot already covered");
        return Step::Seen;
    }
    *expected = Some(seq.saturating_add(1));
    Step::Fresh
}

/// The sequence number on a line whose kind this build does not know, where it
/// carries one.
fn numbering(line: &[u8]) -> Option<u64> {
    serde_json::from_slice::<serde_json::Value>(line)
        .ok()?
        .get("seq")?
        .as_u64()
}

/// Whether the thread has been asked to stop, and how it waits.
#[derive(Debug, Clone)]
struct Stopping(Arc<AtomicBool>);

impl Stopping {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Asks the thread to stop at its next opportunity.
    fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether there is still a reason to keep reading.
    fn running(&self) -> bool {
        !self.0.load(Ordering::Relaxed)
    }

    /// Waits for `wait`, in pieces, so that being asked to stop is noticed while
    /// waiting rather than after it. Reports whether there is still a reason to
    /// carry on.
    fn nap(&self, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        while self.running() {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            thread::sleep(left.min(POLL));
        }
        false
    }
}

/// The lines of one connection, read with a bounded wait.
struct Lines {
    reader: BufReader<UnixStream>,
    /// What has arrived of the line being read. Kept across waits: a read that
    /// times out in the middle of a line has still consumed those bytes, and
    /// throwing them away would lose the framing for every line after it.
    pending: Vec<u8>,
}

/// What came of asking for the next line.
enum Incoming {
    /// A whole line, without its newline.
    Line(Vec<u8>),
    /// Nothing arrived before the wait was up.
    Quiet,
    /// The connection is over, cleanly or otherwise.
    Ended,
}

impl Lines {
    fn new(socket: UnixStream) -> Self {
        Self {
            reader: BufReader::new(socket),
            pending: Vec::new(),
        }
    }

    fn read(&mut self) -> Incoming {
        match self.reader.read_until(b'\n', &mut self.pending) {
            Ok(0) => Incoming::Ended,
            Ok(_) if self.pending.last() == Some(&b'\n') => {
                let mut line = std::mem::take(&mut self.pending);
                line.pop();
                Incoming::Line(line)
            }
            // Bytes with no newline after them and nothing more coming: the
            // connection ended in the middle of a line.
            Ok(_) => Incoming::Ended,
            Err(error) if timed_out(&error) => Incoming::Quiet,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Incoming::Quiet,
            Err(error) => {
                debug!(%error, "cannot read from the bus");
                Incoming::Ended
            }
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
