//! Keeping what is attached in step with what has been declared.
//!
//! The daemon and whoever declares a target never speak to each other. One
//! writes a file and the other reads it, and the reading is a poll of its
//! modification time on a couple of seconds' cadence, brought forward by a
//! `SIGHUP` for anything that wants its declaration acted on now. That is the
//! whole of the control path, and it is deliberately not a socket: the bus's two
//! sockets carry events, an ingest and a stream, and neither of them is a place
//! to ask a daemon to do something. A file also outlives the daemon, which is
//! what a declaration is for — the machine somebody wants attached is still
//! wanted after a reboot.
//!
//! # What a pass does
//!
//! Compare the declarations with the attachments, and make the second look like
//! the first: start what is declared and not attached, stop what is attached and
//! no longer declared, leave everything else alone. Nothing here is incremental
//! and nothing depends on having seen the previous state, so a pass may be run
//! at any time, twice in a row, or after any amount of the file having changed
//! behind its back.
//!
//! Attachments that were found by looking rather than by being declared are not
//! this file's business, and a pass does not touch them: whichever transport
//! discovered one is the thing that knows whether it is still there.
//!
//! # Why a thread
//!
//! A pass reads a file, may start an attachment, and may stop one — and stopping
//! one waits for the thread reading that endpoint to finish. None of that
//! belongs on a runtime worker, where the cost of being wrong is a hook waiting
//! on a connection nobody is accepting. So this is an ordinary thread that
//! sleeps between passes and can be woken, which is also the smallest thing that
//! a `SIGHUP` can poke.

use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use agentbus_protocol::Timestamp;
use tracing::{debug, info, warn};

use super::attach::{self, Attachment};
use super::attachments::{Attachments, Entry, State};
use super::bootstrap::Bootstrap;
use super::targets::{Target, Targets};
use super::transport::Registry;
use crate::bus::Bus;
use crate::clock;

/// How often the declarations are looked at.
///
/// Fast enough that somebody who has just declared a target has it being
/// reached before they have finished reading what they typed, and slow enough
/// that the cost of looking — one `stat` of one file — is not worth measuring.
pub const INTERVAL: Duration = Duration::from_secs(2);

/// Everything reconciling needs: where the two files are, how to build a
/// transport out of a declaration, and what to attach it to.
#[derive(Debug)]
pub struct Plan {
    /// Where declarations are read from.
    pub targets: Targets,
    /// Where what came of them is written.
    pub attachments: Attachments,
    /// How a declaration is turned into a way of reaching an endpoint.
    pub transports: Registry,
    /// The bus everything reached this way is merged into.
    pub bus: Arc<Bus>,
    /// The version to establish at each far end.
    pub bootstrap: Bootstrap,
    /// The timings each attachment runs to.
    pub attach: attach::Settings,
    /// How often to look at the declarations.
    pub every: Duration,
}

/// A running reconciler.
///
/// Dropping it stops the loop and detaches everything it started, so a daemon
/// that lets go of one has let go of every endpoint it was attached to.
#[derive(Debug)]
pub struct Reconciling {
    wake: Arc<Wake>,
    thread: Option<JoinHandle<()>>,
}

impl Reconciling {
    /// Starts reconciling, beginning with a pass right away.
    pub fn start(plan: Plan) -> Self {
        let wake = Arc::new(Wake::new());
        let thread = std::thread::spawn({
            let wake = Arc::clone(&wake);
            move || run(&wake, Reconciler::new(plan))
        });
        Self {
            wake,
            thread: Some(thread),
        }
    }

    /// How to ask for a pass now rather than at the next look.
    pub fn wake(&self) -> Arc<Wake> {
        Arc::clone(&self.wake)
    }

    /// Ends the loop and waits for it, so that everything started here has been
    /// stopped by the time this returns.
    fn stop(&mut self) {
        self.wake.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Reconciling {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The handle a reconciler is woken or stopped through.
#[derive(Debug)]
pub struct Wake {
    asked: Mutex<Asked>,
    woken: Condvar,
}

/// What has been asked of a sleeping reconciler.
#[derive(Debug, Default)]
struct Asked {
    stopping: bool,
    now: bool,
}

impl Wake {
    fn new() -> Self {
        Self {
            asked: Mutex::new(Asked::default()),
            woken: Condvar::new(),
        }
    }

    /// Asks for a pass immediately.
    ///
    /// A pass that is already running is not interrupted; one runs directly
    /// after it, which is the same answer arrived at a moment later and is what
    /// makes this safe to call as often as anything likes.
    pub fn now(&self) {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .now = true;
        self.woken.notify_all();
    }

    /// Asks for the loop to end.
    fn stop(&self) {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stopping = true;
        self.woken.notify_all();
    }

    /// Waits for `how_long`, for a poke, or for the end, whichever comes first.
    /// Says whether there is any point carrying on.
    fn wait(&self, how_long: Duration) -> bool {
        let asked = self.asked.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut asked, _) = self
            .woken
            .wait_timeout_while(asked, how_long, |asked| !asked.stopping && !asked.now)
            .unwrap_or_else(PoisonError::into_inner);
        asked.now = false;
        !asked.stopping
    }
}

/// Reconciles until it is told to stop, then detaches everything.
fn run(wake: &Wake, mut reconciler: Reconciler) {
    let every = reconciler.every;
    loop {
        reconciler.pass();
        if !wake.wait(every) {
            break;
        }
    }
    reconciler.stop();
}

/// One endpoint this reconciler has done something about.
///
/// Either it is attached, or the transport that would have reached it could not
/// be built from what was declared — an address that resolves to nothing, an
/// argument the tool would refuse. The second is a state to report and not a
/// failure to retry, because nothing about it will be different in two seconds:
/// what changes it is somebody changing the declaration, which is when it is
/// tried again.
#[derive(Debug)]
struct Live {
    transport: String,
    args: Vec<String>,
    label: String,
    attachment: Option<Attachment>,
    refused: Option<String>,
    state: State,
    attempt: u32,
    since: Timestamp,
}

impl Live {
    /// What this endpoint looks like from outside.
    fn entry(&self) -> Entry {
        Entry {
            transport: self.transport.clone(),
            args: self.args.clone(),
            identity: self.attachment.as_ref().and_then(Attachment::identity),
            // One for now. A transport that discovers two sets of words reach
            // one endpoint adds the other here rather than by rewriting
            // anybody's declaration.
            aliases: vec![self.args.clone()],
            label: self.label.clone(),
            state: self.state,
            attempt: self.attempt,
            last_error: self.refused.clone(),
            since: self.since.clone(),
            auto: false,
        }
    }

    /// Whether this is what `target` declared.
    fn is(&self, target: &Target) -> bool {
        self.transport == target.transport && self.args == target.args
    }
}

/// The state one reconciler carries between passes.
#[derive(Debug)]
struct Reconciler {
    targets: Targets,
    attachments: Attachments,
    transports: Registry,
    bus: Arc<Bus>,
    bootstrap: Bootstrap,
    settings: attach::Settings,
    every: Duration,
    live: Vec<Live>,
    /// When the declarations were last read, and whether they ever have been.
    read: Option<Option<SystemTime>>,
    /// The names complained about already, so that a declaration nobody can act
    /// on is mentioned once rather than on every pass.
    unknown: BTreeSet<String>,
    /// What was last written, so that a file nothing has changed is not
    /// rewritten.
    published: Option<Vec<Entry>>,
}

impl Reconciler {
    fn new(plan: Plan) -> Self {
        let Plan {
            targets,
            attachments,
            transports,
            bus,
            bootstrap,
            attach,
            every,
        } = plan;
        Self {
            targets,
            attachments,
            transports,
            bus,
            bootstrap,
            settings: attach,
            every,
            live: Vec::new(),
            read: None,
            unknown: BTreeSet::new(),
            published: None,
        }
    }

    /// Reads the declarations if they have changed, looks at what every
    /// attachment is doing, and writes it down if any of it is news.
    fn pass(&mut self) {
        self.declared();
        self.observe();
        self.publish();
    }

    /// Brings the attachments into line with the declarations, if those have
    /// changed since the last look.
    fn declared(&mut self) {
        let changed = self.targets.changed_at();
        if self.read == Some(changed) {
            return;
        }
        self.read = Some(changed);
        match self.targets.list() {
            Ok(targets) => self.reconcile(&targets),
            // Left exactly as they are. A file somebody is halfway through
            // editing, or one a later build wrote, is not a reason to tear down
            // attachments that are working.
            Err(error) => {
                warn!(%error, "cannot read what has been declared; leaving everything attached as it is")
            }
        }
    }

    /// Starts what is declared and not attached, stops what is attached and no
    /// longer declared.
    fn reconcile(&mut self, declared: &[Target]) {
        let (kept, gone): (Vec<Live>, Vec<Live>) = std::mem::take(&mut self.live)
            .into_iter()
            .partition(|live| declared.iter().any(|target| live.is(target)));
        self.live = kept;
        for live in gone {
            info!(
                transport = live.transport,
                endpoint = live.label,
                "nobody wants this attached any more"
            );
            // Dropping it is what stops the far end and withdraws everything it
            // reported; it is done here rather than left to the end of the pass
            // so that the state written afterwards is the state after it.
            drop(live);
        }
        for target in declared {
            if self.live.iter().any(|live| live.is(target)) {
                continue;
            }
            if let Some(live) = self.begin(target) {
                self.live.push(live);
            }
        }
    }

    /// Starts reaching one endpoint, or says why nothing was started.
    fn begin(&mut self, target: &Target) -> Option<Live> {
        let Some(made) = self.transports.make(&target.transport, &target.args) else {
            if self.unknown.insert(target.transport.clone()) {
                warn!(
                    transport = target.transport,
                    known = %self.known(),
                    "a target is declared for a transport this build has never heard of; ignoring it"
                );
            }
            return None;
        };
        let now = clock::now();
        match made {
            Ok(transport) => {
                let label = transport.label();
                info!(
                    transport = target.transport,
                    endpoint = label,
                    "attaching to a declared endpoint"
                );
                let attachment = Attachment::start(
                    transport,
                    self.bootstrap.clone(),
                    Arc::clone(&self.bus),
                    self.settings,
                );
                Some(Live {
                    transport: target.transport.clone(),
                    args: target.args.clone(),
                    label,
                    attachment: Some(attachment),
                    refused: None,
                    state: State::Connecting,
                    attempt: 0,
                    since: now,
                })
            }
            Err(said) => {
                warn!(
                    transport = target.transport,
                    said, "cannot reach what this target declares"
                );
                Some(Live {
                    transport: target.transport.clone(),
                    label: target.args.join(" "),
                    args: target.args.clone(),
                    attachment: None,
                    refused: Some(said),
                    state: State::NeedsAttention,
                    attempt: 0,
                    since: now,
                })
            }
        }
    }

    /// The transports this daemon does know of, for a message about one it does
    /// not.
    fn known(&self) -> String {
        match self.transports.names().collect::<Vec<&str>>() {
            names if names.is_empty() => "none".to_owned(),
            names => names.join(", "),
        }
    }

    /// Asks every attachment what it is doing, and stamps the ones that have
    /// changed with the moment they changed.
    fn observe(&mut self) {
        for live in &mut self.live {
            let Some(attachment) = live.attachment.as_ref() else {
                continue;
            };
            let (state, attempt, said) = reported(&attachment.state());
            if state == live.state && attempt == live.attempt && said == live.refused {
                continue;
            }
            debug!(endpoint = live.label, %state, "an attachment changed");
            live.state = state;
            live.attempt = attempt;
            live.refused = said;
            live.since = clock::now();
        }
    }

    /// Writes what every attachment is doing, if it is not what was written
    /// last time.
    fn publish(&mut self) {
        let entries: Vec<Entry> = self.live.iter().map(Live::entry).collect();
        if self.published.as_ref() == Some(&entries) {
            return;
        }
        if let Err(error) = self.attachments.write(&entries) {
            warn!(%error, "cannot write what is attached; nothing else is affected");
        }
        // Recorded whether or not the write worked, so that a directory that
        // cannot be written to is complained about when something changes
        // rather than on every pass.
        self.published = Some(entries);
    }

    /// Detaches everything and takes the file away.
    fn stop(&mut self) {
        self.live.clear();
        if let Err(error) = self.attachments.remove() {
            debug!(%error, "cannot remove what is attached");
        }
    }
}

/// One attachment's state, as it is written down.
///
/// An attachment whose supervisor has finished reads as on its way out rather
/// than as gone: it is still in the picture until the pass that removes it, and
/// the only way it gets there is somebody having stopped it.
fn reported(state: &attach::State) -> (State, u32, Option<String>) {
    match state {
        attach::State::Connecting => (State::Connecting, 0, None),
        attach::State::Attached => (State::Attached, 0, None),
        attach::State::Reconnecting { attempt } => (State::Reconnecting, *attempt, None),
        attach::State::NeedsAttention { said } => (State::NeedsAttention, 0, Some(said.clone())),
        attach::State::Detached => (State::Detaching, 0, None),
    }
}

#[cfg(test)]
mod tests;
