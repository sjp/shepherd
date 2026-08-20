//! Reading another daemon's stream and merging it into this one's.
//!
//! What is at the far end is a whole daemon, folding its own events and holding
//! its own state, and this is a subscriber to it. So attaching is not a way of
//! reaching across a boundary to collect events: it is a way of *relaying* an
//! account that was already complete when it arrived. That distinction is what
//! makes the hard case work. A session that said it was blocked and then went
//! quiet is still blocked over there, whatever happened to the connection in
//! between, and reconnecting asks the daemon that knows and is told so again.
//!
//! # What a hop means
//!
//! Every session, observation and event that arrives this way gets one hop put
//! at the *front* of whatever chain it already carried. The chain is ordered
//! outermost-first from the point of view of whoever is reading this daemon, so
//! a subscriber here sees the way to a thing, in the order it would be reached:
//! a session two levels away, on a host that runs containers, is `[host,
//! container]`. Prepending is the whole of the rule, and it composes without
//! anything having to know how deep it is.
//!
//! # What is not done here
//!
//! Nothing is bound to a process and nothing is reaped. Pids at the far end are
//! numbers in another process table, and a session over there ends when the
//! daemon that folds it says so. This end's only claim of its own is that a
//! daemon it can no longer reach is a daemon whose sessions it can no longer
//! speak for.
//!
//! # Threads rather than tasks
//!
//! A transport hands back a process and a blocking stream, so one thread reads
//! it and hands whole lines to another that supervises. Two, rather than one,
//! because the thread that has to be able to cut a connection cannot be the
//! thread that is blocked reading it. Neither of them is a runtime worker: the
//! daemon's event loop is never inside either, which is what keeps a far end
//! that stalls from being anybody else's problem.

use std::io::{BufRead, BufReader};
use std::process::ChildStderr;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agentbus_protocol::{ForegroundEntry, OriginHop, SessionEntry, Snapshot, StreamLine};
use serde_json::{Map, Value};
use thiserror::Error;
use tracing::{debug, info, warn};

use super::bootstrap::{self, Bootstrap};
use super::transport::{Running, Transport};
use crate::bus::{AttachmentId, Bus};
use crate::clock;

/// The command run at the far end.
///
/// `--ensure-daemon` is what makes the far end a place with a bus on it rather
/// than a place that happens to have one: nobody over there is going to start
/// it by hand, and a daemon that is already running is left exactly as it is.
pub const FAR_END: [&str; 2] = ["subscribe", "--ensure-daemon"];

/// How long a stream may say nothing at all before it is treated as dead.
///
/// The far end sends a heartbeat every ten seconds whatever else is happening,
/// so silence for three of them is evidence rather than patience. Reconnecting
/// costs one round trip and a snapshot, and is always correct, so the wrong call
/// here is cheap in one direction and expensive in the other.
pub const DEFAULT_LIVENESS: Duration = Duration::from_secs(30);

/// How long a stream has to stay up before the failures behind it stop
/// counting.
///
/// Without it, an endpoint that breaks once an hour would eventually be waiting
/// the maximum delay before every attempt, because nothing would ever have told
/// the schedule that the trouble was over.
pub const DEFAULT_STABLE: Duration = Duration::from_secs(60);

/// The key an event's sequence number at the far end is kept under once it has
/// been renumbered here.
const REMOTE_SEQ: &str = "remote_seq";

/// The key a payload that was not an object is kept under when room has to be
/// made beside it.
const VALUE: &str = "value";

/// How many lines may be waiting to be merged.
///
/// The far end is the one thing that has to be slowed down when this end falls
/// behind, because the alternative is dropping events that nobody can ask for
/// again. Local subscribers are protected by their own bounded queues and are
/// not affected either way.
const ARRIVING: usize = 1024;

/// What an attachment is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Reaching the far end and getting a daemon running there.
    Connecting,
    /// Reading its stream.
    Attached,
    /// The stream broke, and this is which attempt at getting it back is next.
    Reconnecting {
        /// How many attempts have already failed.
        attempt: u32,
    },
    /// Something is wrong that trying again will not fix, and a person has to
    /// do something about it.
    NeedsAttention {
        /// What went wrong, as the transport described it.
        said: String,
    },
    /// Stopped, on purpose.
    Detached,
}

/// The timings an attachment runs to.
///
/// Both are properties of this end rather than of any endpoint — how long to
/// believe in silence, and how long counts as having worked — so they are
/// settled once. What is transport-specific is the backoff, and that comes from
/// the transport itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// How long a stream may say nothing before it is treated as dead.
    pub liveness: Duration,
    /// How long a stream has to last before the schedule of delays is reset.
    pub stable: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            liveness: DEFAULT_LIVENESS,
            stable: DEFAULT_STABLE,
        }
    }
}

/// One far end whose stream is being merged into this daemon's.
///
/// Dropping it detaches: the far-end process is stopped, the thread reading it
/// is joined, and everything it reported is withdrawn. The daemon over there is
/// untouched and carries on, which is the point of having put it there.
#[derive(Debug)]
pub struct Attachment {
    transport: Arc<dyn Transport>,
    shared: Arc<Shared>,
    supervisor: Option<JoinHandle<()>>,
}

impl Attachment {
    /// Starts reaching the far end, and keeps reaching it until it is detached.
    ///
    /// Returns immediately: the first connection has not been made yet, and an
    /// endpoint that is not there yet is a state to report rather than a failure
    /// to hand back. Whether it is up is [`Attachment::state`].
    pub fn start(
        transport: Arc<dyn Transport>,
        bootstrap: Bootstrap,
        bus: Arc<Bus>,
        settings: Settings,
    ) -> Self {
        let id = bus.attach();
        let shared = Arc::new(Shared::new());
        let supervisor = std::thread::spawn({
            let transport = Arc::clone(&transport);
            let shared = Arc::clone(&shared);
            move || supervise(&shared, &*transport, &bootstrap, &bus, id, settings)
        });
        Self {
            transport,
            shared,
            supervisor: Some(supervisor),
        }
    }

    /// What this attachment is doing right now.
    pub fn state(&self) -> State {
        self.shared.state()
    }

    /// The kind of boundary this attachment crosses.
    pub fn kind(&self) -> &'static str {
        self.transport.kind()
    }

    /// What to call the far end when telling somebody about it.
    pub fn label(&self) -> String {
        self.transport.label()
    }

    /// What the far end turned out to be.
    ///
    /// Nothing until it has been reached, which is why it is asked each time
    /// rather than remembered from when the attachment was started. Two answers
    /// are possible and they are asked for in the order of how much they are
    /// worth: what the transport knows it reached, and failing that what the
    /// daemon over there says it is. A far end that says nothing about itself
    /// leaves this empty rather than being given a name from this side, because
    /// a name this side made up would compare equal to the one it made up for
    /// somewhere else.
    pub fn identity(&self) -> Option<String> {
        self.transport.identity().or_else(|| self.shared.said())
    }

    /// The way in to the far end, as much of it as could be told without
    /// reaching it.
    pub fn way_in(&self) -> Option<String> {
        self.transport.way_in()
    }

    /// Stops reading the far end because another attachment turned out to be
    /// reading the same daemon, leaving everything it reported in place.
    ///
    /// Detaching would be wrong here in two ways, and this is the same operation
    /// with both of them corrected. What this attachment reported is not over —
    /// the other attachment is reporting the same sessions, from the same daemon
    /// — so nothing is withdrawn. And `shared` says whether the way in belongs
    /// to both of them, in which case it is left open: closing it would take
    /// down the connection the surviving attachment is reading through.
    pub fn superseded(mut self, shared: bool) {
        if shared {
            self.transport.keep_open();
        }
        self.shared.supersede();
        self.stop();
    }

    /// Stops reading the far end and withdraws everything it reported.
    pub fn detach(mut self) {
        self.stop();
    }

    /// Ends the supervisor and waits for it, so that everything this attachment
    /// contributed has been withdrawn by the time this returns.
    fn stop(&mut self) {
        self.shared.stop();
        if let Some(supervisor) = self.supervisor.take() {
            let _ = supervisor.join();
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Why a connection to the far end ended before it began.
#[derive(Debug, Error)]
enum Broken {
    /// The far end could not be reached, or no daemon could be got running
    /// there.
    #[error(transparent)]
    Bootstrap(#[from] bootstrap::Error),
    /// Something answered, and it was not a daemon's stream.
    #[error("{label} did not begin by saying what it knows")]
    NoSnapshot {
        /// The endpoint, as a person would name it.
        label: String,
    },
}

/// What the supervisor and whoever is holding the attachment both touch.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    /// The far-end process while there is one, so that stopping it is not the
    /// exclusive privilege of the thread that is blocked reading it.
    running: Mutex<Option<Running>>,
    /// What the daemon over there said it is, once it has said.
    said: Mutex<Option<String>>,
    stopping: Mutex<bool>,
    /// Whether what this attachment reported is another attachment's too, which
    /// decides whether stopping withdraws any of it.
    superseded: Mutex<bool>,
    woken: Condvar,
}

impl Shared {
    fn new() -> Self {
        Self {
            state: Mutex::new(State::Connecting),
            running: Mutex::new(None),
            said: Mutex::new(None),
            stopping: Mutex::new(false),
            superseded: Mutex::new(false),
            woken: Condvar::new(),
        }
    }

    /// What the daemon at the far end says it is, once one has said.
    fn said(&self) -> Option<String> {
        self.said
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Remembers what the daemon at the far end says it is.
    fn says(&self, id: Option<&str>) {
        if let Some(id) = id {
            *self.said.lock().unwrap_or_else(PoisonError::into_inner) = Some(id.to_owned());
        }
    }

    /// Says that everything this attachment reported is another one's too.
    fn supersede(&self) {
        *self
            .superseded
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
    }

    /// Whether stopping should leave what was reported where it is.
    fn stays(&self) -> bool {
        *self
            .superseded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn state(&self) -> State {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set(&self, state: State) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
    }

    fn stopping(&self) -> bool {
        *self.stopping.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for `how_long`, or until the attachment is told to stop, whichever
    /// comes first. Says whether there is any point carrying on.
    fn pause(&self, how_long: Duration) -> bool {
        let stopping = self.stopping.lock().unwrap_or_else(PoisonError::into_inner);
        let (stopping, _) = self
            .woken
            .wait_timeout_while(stopping, how_long, |stopping| !*stopping)
            .unwrap_or_else(PoisonError::into_inner);
        !*stopping
    }

    /// Waits until the attachment is told to stop, however long that is.
    ///
    /// What an attachment does when trying again would not help: it stays where
    /// it is, saying so, rather than either exiting — which would take its
    /// state with it — or retrying something that is going to fail the same way.
    fn park(&self) {
        let stopping = self.stopping.lock().unwrap_or_else(PoisonError::into_inner);
        let _unused = self
            .woken
            .wait_while(stopping, |stopping| !*stopping)
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// Asks the attachment to stop, and cuts whatever it is reading so that the
    /// asking takes effect now rather than at the end of the next read.
    fn stop(&self) {
        *self.stopping.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.woken.notify_all();
        self.cut();
    }

    /// Stops the far-end process, if one is running.
    fn cut(&self) {
        if let Some(running) = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            let _ = running.kill();
        }
    }

    /// Takes the far-end process back, so that it can be waited for.
    fn taken(&self) -> Option<Running> {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// Keeps one far end attached for as long as anybody wants it attached.
fn supervise(
    shared: &Shared,
    transport: &dyn Transport,
    bootstrap: &Bootstrap,
    bus: &Arc<Bus>,
    id: AttachmentId,
    settings: Settings,
) {
    let label = transport.label();
    let mut attempt = 0;
    while !shared.stopping() {
        shared.set(State::Connecting);
        match connected(shared, transport, bootstrap, bus, id, settings) {
            Ok(lasted) => {
                info!(endpoint = label, ?lasted, "the stream from there ended");
                // A connection that worked for a while is evidence that the
                // endpoint is fine, so the next failure starts again from the
                // shortest delay rather than from wherever the last run of
                // trouble left the schedule.
                if lasted >= settings.stable {
                    attempt = 0;
                }
            }
            Err(error) => {
                // Asked of the transport whichever layer the failure came back
                // through, because the thing that tells a host that is merely
                // down from one that will never let this daemon in is what the
                // tool printed, and that survives being wrapped.
                if !transport.recoverable(&error) {
                    warn!(endpoint = label, %error, "cannot attach, and trying again will not help");
                    shared.set(State::NeedsAttention {
                        said: error.to_string(),
                    });
                    shared.park();
                    break;
                }
                warn!(endpoint = label, %error, "cannot attach");
            }
        }
        if shared.stopping() {
            break;
        }
        let delay = transport.backoff().delay(attempt, clock::scatter());
        debug!(endpoint = label, attempt, ?delay, "trying again");
        shared.set(State::Reconnecting { attempt });
        if !shared.pause(delay) {
            break;
        }
        attempt = attempt.saturating_add(1);
    }
    // What was reported stays where it is when something else is reporting the
    // same daemon; otherwise this end can no longer speak for any of it.
    match shared.stays() {
        true => bus.forget(id),
        false => bus.detach(id, &clock::now()),
    }
    shared.set(State::Detached);
    info!(endpoint = label, "detached");
}

/// Establishes one stream and merges it until it ends, saying how long it
/// lasted.
fn connected(
    shared: &Shared,
    transport: &dyn Transport,
    bootstrap: &Bootstrap,
    bus: &Arc<Bus>,
    id: AttachmentId,
    settings: Settings,
) -> Result<Duration, Broken> {
    let label = transport.label();
    let mut running = bootstrap.run(transport, &FAR_END)?;
    // Something has to empty what the far end complains on, or it eventually
    // blocks writing to a pipe nobody is reading.
    if let Some(stderr) = running.stderr() {
        let label = label.clone();
        std::thread::spawn(move || complaining(&label, stderr));
    }
    let stream = running.take_stdout();
    *shared
        .running
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(running);
    // Detaching between reaching the far end and holding on to it would
    // otherwise leave the process running with nobody to stop it.
    if shared.stopping() {
        shared.cut();
    }

    let (lines, arriving) = sync_channel(ARRIVING);
    let reader = std::thread::spawn(move || read(stream, &lines));

    let merged = merging(shared, &arriving, transport, bus, id, settings);

    // Cutting first is what ends the reader: it is blocked on a read that only
    // the process going away will finish.
    shared.cut();
    drop(arriving);
    let _ = reader.join();
    if let Some(mut ended) = shared.taken() {
        let _ = ended.kill();
        // Waiting is what keeps a far end that has just been stopped from
        // staying in this machine's process table as a zombie.
        let _ = ended.wait();
    }
    merged
}

/// Reads one stream from beginning to end, merging every line of it.
fn merging(
    shared: &Shared,
    arriving: &Receiver<Vec<u8>>,
    transport: &dyn Transport,
    bus: &Arc<Bus>,
    id: AttachmentId,
    settings: Settings,
) -> Result<Duration, Broken> {
    let label = transport.label();
    // The bootstrap has already read the far end's first line to find out what
    // it was holding, so this is waiting rather than pending. What it must be
    // is a snapshot: a daemon's stream begins with one, and anything else means
    // whatever answered is not one.
    let first = arriving
        .recv_timeout(settings.liveness)
        .ok()
        .and_then(|line| parse(&line));
    let Some(StreamLine::Snapshot(snapshot)) = first else {
        return Err(Broken::NoSnapshot { label });
    };

    let hop = hop(transport, &snapshot);
    shared.says(snapshot.daemon.as_ref().map(|daemon| daemon.id.as_str()));
    info!(endpoint = label, id = hop.id, "attached");
    seed(bus, id, &hop, snapshot);
    shared.set(State::Attached);
    let began = Instant::now();

    loop {
        let line = match arriving.recv_timeout(settings.liveness) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                warn!(
                    endpoint = label,
                    silent = ?settings.liveness,
                    "nothing has arrived from there, not even a heartbeat"
                );
                return Ok(began.elapsed());
            }
            // The reader has finished, which means the far end has.
            Err(RecvTimeoutError::Disconnected) => return Ok(began.elapsed()),
        };
        let Some(line) = parse(&line) else {
            continue;
        };
        match line {
            // A daemon sends one snapshot per connection, so this is unusual
            // rather than impossible; taking it as the current account is the
            // same thing that was done with the first one.
            StreamLine::Snapshot(snapshot) => seed(bus, id, &hop, snapshot),
            StreamLine::Event(mut event) => {
                event.origin.insert(0, hop.clone());
                event.raw = remembering(event.raw, event.seq);
                bus.merge(id, event);
            }
            StreamLine::ForegroundChange(mut change) => {
                if let Some(entry) = &mut change.foreground {
                    entry.origin.insert(0, hop.clone());
                }
                bus.merge_foreground(id, change);
            }
            // Consumed, never forwarded. It has already done its whole job by
            // arriving, and this daemon owes its own subscribers heartbeats of
            // its own rather than somebody else's.
            StreamLine::Heartbeat(_) => {}
            // Dropped rather than applied. A claim is about a slot the daemon
            // that made it can see, and it has already had its whole effect
            // over there: whatever it changed is in the sessions that daemon
            // reports, which arrive here as any other session does. Applying it
            // again here would file it against a correlation on *this*
            // machine — an opaque string that means whatever the local shells
            // mean by it — and let something nobody here can see speak for a
            // slot in front of somebody's face.
            StreamLine::Assertion(_) => {}
            StreamLine::Unknown => {}
        }
    }
}

/// Takes the far end's account of itself as this daemon's account of the far
/// end.
fn seed(bus: &Arc<Bus>, id: AttachmentId, hop: &OriginHop, snapshot: Snapshot) {
    let sessions: Vec<SessionEntry> = snapshot
        .sessions
        .into_iter()
        .map(|mut entry| {
            entry.origin.insert(0, hop.clone());
            entry
        })
        .collect();
    let foreground: Option<Vec<ForegroundEntry>> = snapshot.foreground.map(|entries| {
        entries
            .into_iter()
            .map(|mut entry| {
                entry.origin.insert(0, hop.clone());
                entry
            })
            .collect()
    });
    bus.seed(id, &sessions, foreground.as_deref(), &clock::now());
}

/// The hop that reaches this far end, as everything relayed through it will
/// carry.
///
/// The transport's own identity wins where it has one, because it knows what it
/// connected to and the far end is only reporting what it believes about
/// itself. A daemon that names itself is the answer for a transport that cannot
/// tell — an address is not an identity, and two addresses may reach one
/// machine.
fn hop(transport: &dyn Transport, snapshot: &Snapshot) -> OriginHop {
    let said = snapshot.daemon.as_ref().map(|daemon| daemon.id.as_str());
    let id = match (transport.identity(), said) {
        (Some(known), Some(said)) if known != said => {
            warn!(
                endpoint = transport.label(),
                known, said, "the daemon there does not agree about what it is"
            );
            known
        }
        (Some(known), _) => known,
        (None, Some(said)) => said.to_owned(),
        // Nothing has settled what the far end is yet. What a person calls it is
        // the least bad stand-in, and it is stable for as long as this
        // attachment lasts, which is what a chain needs of it.
        (None, None) => transport.label(),
    };
    OriginHop::new(transport.kind(), id, transport.label())
}

/// An event's payload with the far end's own sequence number kept in it.
///
/// The number is replaced on the way through, because a subscriber here is
/// promised this daemon's counter and nobody else's. The original is worth
/// keeping all the same: it is how somebody comparing the two streams tells
/// which line is which, and how a gap at the far end is still visible after the
/// numbering has been redone.
fn remembering(raw: Option<Value>, seq: u64) -> Option<Value> {
    let mut carried = match raw {
        Some(Value::Object(carried)) => carried,
        // Anything that is not an object has nowhere to put a key, so it is
        // carried whole under one instead of being dropped.
        Some(other) => {
            let mut carried = Map::new();
            carried.insert(VALUE.to_owned(), other);
            carried
        }
        None => Map::new(),
    };
    carried.insert(REMOTE_SEQ.to_owned(), Value::from(seq));
    Some(Value::Object(carried))
}

/// One line of the far end's stream, or nothing for a line that is not one.
///
/// A line that does not parse is skipped rather than fatal, for the reason the
/// protocol gives readers generally: a daemon of a later version may say things
/// this one has never heard of, and the rest of what it says is still worth
/// having.
fn parse(line: &[u8]) -> Option<StreamLine> {
    match serde_json::from_slice(line) {
        Ok(line) => Some(line),
        Err(error) => {
            debug!(%error, "skipped a line from the far end that is not one");
            None
        }
    }
}

/// Reads whole lines from the far end until it stops or nobody wants them.
fn read(mut stream: Box<dyn BufRead + Send>, lines: &SyncSender<Vec<u8>>) {
    loop {
        let mut line = Vec::new();
        match stream.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if lines.send(line).is_err() {
                    return;
                }
            }
        }
    }
}

/// Puts whatever the far end complains about into this daemon's log.
fn complaining(label: &str, stderr: ChildStderr) {
    for said in BufReader::new(stderr).lines().map_while(Result::ok) {
        debug!(endpoint = label, said, "the far end said something");
    }
}

#[cfg(test)]
mod tests;
