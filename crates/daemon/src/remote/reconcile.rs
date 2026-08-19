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
//! # What was found rather than declared
//!
//! A pass also asks every transport that can find its own endpoints what it can
//! see, on whatever cadence that transport asks to be asked on, and keeps those
//! attachments in step with the answer the same way. The two sets never touch:
//! taking a declaration back never stops something that was found, and a list
//! that has stopped mentioning an endpoint never stops one somebody asked for.
//! Where both name one endpoint the declaration wins, because somebody asked
//! for that one and nobody asked for the other.
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
use std::time::{Duration, Instant, SystemTime};

use agentbus_protocol::Timestamp;
use tracing::{debug, info, warn};

use super::attach::{self, Attachment};
use super::attachments::{Attachments, Entry, Sharing, State};
use super::bootstrap::Bootstrap;
use super::discover::{Context, Discovery, Found};
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
    /// The transports that find their own endpoints rather than being told.
    pub discoveries: Vec<Arc<dyn Discovery>>,
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
    /// Whether it was found by looking rather than by being declared.
    auto: bool,
    /// The way in to it, as far as could be told before anything reached it.
    way_in: Option<String>,
    /// What it turned out to be, once something said so. Kept once learned: an
    /// endpoint whose stream has been let go because another set of words
    /// reaches the same daemon is still known to be that daemon.
    identity: Option<String>,
    /// The declaration whose attachment carries this one, where these words
    /// turned out to be another name for something already reached.
    alias_of: Option<Vec<String>>,
}

impl Live {
    /// What this endpoint looks like from outside, given every other set of
    /// words that turned out to reach it.
    fn entry(&self, aliases: &[&Live]) -> Entry {
        Entry {
            transport: self.transport.clone(),
            args: self.args.clone(),
            identity: self.identity.clone(),
            way_in: self.way_in.clone(),
            aliases: std::iter::once(self.args.clone())
                .chain(aliases.iter().map(|alias| alias.args.clone()))
                .collect(),
            sharing: self.sharing(aliases),
            label: self.label.clone(),
            state: self.state,
            attempt: self.attempt,
            last_error: self.refused.clone(),
            since: self.since.clone(),
            auto: self.auto,
        }
    }

    /// Whether every set of words reaching this endpoint reaches it the same
    /// way, and nothing at all where there is only one of them or where the
    /// transport has no way in to compare.
    fn sharing(&self, aliases: &[&Live]) -> Option<Sharing> {
        if aliases.is_empty() || self.way_in.is_none() {
            return None;
        }
        match aliases.iter().all(|alias| alias.way_in == self.way_in) {
            true => Some(Sharing::Shared),
            false => Some(Sharing::Separate),
        }
    }

    /// Whether this is what `target` declared.
    fn is(&self, target: &Target) -> bool {
        self.transport == target.transport && self.args == target.args
    }
}

/// Whether two declarations turn out to name one endpoint.
///
/// Two answers, and which one is being given depends on how much is known. Where
/// both far ends have said what they are, that settles it and nothing else is
/// consulted: an identity is the daemon's own account of itself, and two
/// different accounts are two daemons however alike the addresses look. Until
/// then the way in stands in for it — two declarations ssh resolves to one
/// endpoint are one endpoint, near enough to report as one — which is a guess,
/// and one the paragraph above overturns the moment there is anything better.
fn one(earlier: &Live, later: &Live) -> bool {
    if earlier.transport != later.transport {
        return false;
    }
    match (&earlier.identity, &later.identity) {
        (Some(earlier), Some(later)) => earlier == later,
        _ => earlier.way_in.is_some() && earlier.way_in == later.way_in,
    }
}

/// Whether both far ends have said what they are and said the same thing, which
/// is the point at which one of the two streams stops being worth reading.
fn settled(earlier: &Live, later: &Live) -> bool {
    earlier.identity.is_some() && earlier.identity == later.identity
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
    discoveries: Vec<Sweeping>,
    /// The endpoints somebody declared.
    wanted: Vec<Live>,
    /// The endpoints a transport found for itself.
    found: Vec<Live>,
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
            discoveries,
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
            // Due at once, so that the first pass is a whole pass rather than
            // one that has looked at only half of what is reachable.
            discoveries: discoveries.into_iter().map(Sweeping::new).collect(),
            wanted: Vec::new(),
            found: Vec::new(),
            read: None,
            unknown: BTreeSet::new(),
            published: None,
        }
    }

    /// Reads the declarations if they have changed, looks at what every
    /// attachment is doing, and writes it down if any of it is news.
    fn pass(&mut self) {
        self.declared();
        self.discovered();
        self.observe();
        self.regroup();
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
        let (kept, gone): (Vec<Live>, Vec<Live>) = std::mem::take(&mut self.wanted)
            .into_iter()
            .partition(|live| declared.iter().any(|target| live.is(target)));
        self.wanted = kept;
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
            if self.wanted.iter().any(|live| live.is(target)) {
                continue;
            }
            if let Some(live) = self.begin(&target.transport, &target.args) {
                self.wanted.push(live);
            }
        }
    }

    /// Asks every transport that is due what it can find, and keeps what is
    /// attached in step with the answer.
    fn discovered(&mut self) {
        // Something that has since been declared is attached because somebody
        // asked for it. Letting go of the one that was found is done here
        // rather than left to the next sweep so that the two never overlap.
        let claimed: Vec<(String, Vec<String>)> = self
            .wanted
            .iter()
            .map(|live| (live.transport.clone(), live.args.clone()))
            .collect();
        self.found.retain(|found| {
            !claimed
                .iter()
                .any(|(transport, args)| *transport == found.transport && *args == found.args)
        });

        for index in 0..self.discoveries.len() {
            if !self.discoveries[index].due() {
                continue;
            }
            let discovery = Arc::clone(&self.discoveries[index].discovery);
            let name = discovery.transport();
            let working = self.working();
            let declared: Vec<Vec<String>> = self
                .wanted
                .iter()
                .filter(|live| live.transport == name)
                .map(|live| live.args.clone())
                .collect();
            let swept = discovery.sweep(&Context {
                working: &working,
                declared: &declared,
            });
            // Set from when the answer came back rather than from when the
            // question was asked, so that a discovery which takes a while to
            // answer is not asked again the moment it has.
            self.discoveries[index].again(discovery.every());
            // Nothing means the question could not be asked, which is not the
            // same as an answer of nothing and must not be treated like one.
            if let Some(swept) = swept {
                self.settle(name, swept);
            }
        }
    }

    /// Makes what is attached through `transport` exactly what it just found.
    fn settle(&mut self, transport: &str, swept: Vec<Found>) {
        let (kept, gone): (Vec<Live>, Vec<Live>) = std::mem::take(&mut self.found)
            .into_iter()
            .partition(|live| {
                live.transport != transport || swept.iter().any(|found| found.args == live.args)
            });
        self.found = kept;
        for live in gone {
            info!(
                transport,
                endpoint = live.label,
                "this is not there any more"
            );
            drop(live);
        }
        for found in swept {
            if self
                .found
                .iter()
                .any(|live| live.transport == transport && live.args == found.args)
            {
                continue;
            }
            let label = found.transport.label();
            info!(
                transport,
                endpoint = label,
                "attaching to an endpoint that was found here"
            );
            let attachment = Attachment::start(
                found.transport,
                self.bootstrap.clone(),
                Arc::clone(&self.bus),
                self.settings,
            );
            self.found.push(Live {
                transport: transport.to_owned(),
                args: found.args,
                label,
                way_in: attachment.way_in(),
                attachment: Some(attachment),
                refused: None,
                state: State::Connecting,
                attempt: 0,
                since: clock::now(),
                auto: true,
                identity: None,
                alias_of: None,
            });
        }
    }

    /// Where this daemon's own sessions said they were working, newest first
    /// and each place once.
    ///
    /// Only this daemon's own: a session relayed from somewhere else is working
    /// in a directory on that machine, and looking for it here would match the
    /// wrong thing or nothing at all.
    fn working(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for entry in self.bus.sessions() {
            if !entry.origin.is_empty() {
                continue;
            }
            if let Some(cwd) = entry.cwd.filter(|cwd| !cwd.is_empty())
                && !seen.contains(&cwd)
            {
                seen.push(cwd);
            }
        }
        seen
    }

    /// Starts reaching one endpoint, or says why nothing was started.
    fn begin(&mut self, name: &str, args: &[String]) -> Option<Live> {
        let Some(made) = self.transports.make(name, args) else {
            if self.unknown.insert(name.to_owned()) {
                warn!(
                    transport = name,
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
                    transport = name,
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
                    transport: name.to_owned(),
                    args: args.to_vec(),
                    label,
                    way_in: attachment.way_in(),
                    attachment: Some(attachment),
                    refused: None,
                    state: State::Connecting,
                    attempt: 0,
                    since: now,
                    auto: false,
                    identity: None,
                    alias_of: None,
                })
            }
            Err(said) => {
                warn!(
                    transport = name,
                    said, "cannot reach what this target declares"
                );
                Some(Live {
                    transport: name.to_owned(),
                    label: args.join(" "),
                    args: args.to_vec(),
                    attachment: None,
                    refused: Some(said),
                    state: State::NeedsAttention,
                    attempt: 0,
                    since: now,
                    auto: false,
                    way_in: None,
                    identity: None,
                    alias_of: None,
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
        for live in self.wanted.iter_mut().chain(self.found.iter_mut()) {
            let Some(attachment) = live.attachment.as_ref() else {
                continue;
            };
            // Kept rather than replaced: an attachment that is between
            // connections has not stopped being the endpoint it reached.
            if let Some(identity) = attachment.identity() {
                live.identity = Some(identity);
            }
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

    /// Works out which declarations turn out to name one endpoint, and lets go
    /// of the streams that are reading a daemon another stream is already
    /// reading.
    ///
    /// Only the declared ones. What a transport finds for itself it finds by
    /// reading a list of what is there, and a list does not mention one endpoint
    /// twice; what is declared is whatever somebody typed, which is exactly
    /// where the same machine gets named three ways.
    ///
    /// The order is the declarations' own, and the earliest of a group carries
    /// it: it is the one that was attached first, so keeping that one is the
    /// choice that lets go of the connection made most recently and leaves the
    /// one that has been working alone.
    fn regroup(&mut self) {
        let mut leaders: Vec<Option<usize>> = Vec::with_capacity(self.wanted.len());
        for later in 0..self.wanted.len() {
            let leader = (0..later).find(|earlier| {
                leaders[*earlier].is_none() && one(&self.wanted[*earlier], &self.wanted[later])
            });
            leaders.push(leader);
        }
        for (later, leader) in leaders.into_iter().enumerate() {
            match leader {
                Some(earlier) => {
                    self.wanted[later].alias_of = Some(self.wanted[earlier].args.clone());
                    if settled(&self.wanted[earlier], &self.wanted[later]) {
                        self.collapse(earlier, later);
                    }
                }
                // Either it never was another name for anything, or whatever it
                // was a name for is no longer declared, in which case this is
                // now the one that has to be reaching the endpoint.
                None => {
                    self.wanted[later].alias_of = None;
                    self.resume(later);
                }
            }
        }
    }

    /// Stops reading one far end because the declaration at `earlier` is
    /// reading the same daemon.
    ///
    /// What was reported through it stays: it is the other attachment's account
    /// as much as this one's, and withdrawing it here would take away rows that
    /// are still true. Whether the way in is closed depends on whether anything
    /// else is still reaching through it — for ssh, two declarations it resolves
    /// alike share one connection, and closing that would cut a stream this is
    /// trying not to disturb.
    fn collapse(&mut self, earlier: usize, later: usize) {
        let shared = self.shares_the_way_in(later);
        let Some(attachment) = self.wanted[later].attachment.take() else {
            return;
        };
        info!(
            endpoint = self.wanted[later].label,
            also = self.wanted[earlier].label,
            identity = self.wanted[later].identity,
            "these turn out to be one endpoint; reading it through the first of them"
        );
        attachment.superseded(shared);
    }

    /// Whether anything else this daemon is reading reaches its far end the same
    /// way as the declaration at `index` reaches its own.
    ///
    /// Which is to say: whether letting go of that one would take something
    /// else's connection with it.
    fn shares_the_way_in(&self, index: usize) -> bool {
        let mine = &self.wanted[index].way_in;
        mine.is_some()
            && self
                .wanted
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .map(|(_, live)| live)
                .chain(self.found.iter())
                .any(|live| live.attachment.is_some() && &live.way_in == mine)
    }

    /// Starts reaching an endpoint again after whatever was reaching it for it
    /// stopped being declared.
    ///
    /// Nothing happens to anything that is already attached, that is being
    /// retried, or that could not be made into a transport at all: the last is a
    /// state a person has to do something about, and trying it again every two
    /// seconds would neither fix it nor be mentioned.
    fn resume(&mut self, index: usize) {
        let live = &self.wanted[index];
        if live.attachment.is_some() || live.refused.is_some() {
            return;
        }
        let (name, args) = (live.transport.clone(), live.args.clone());
        info!(
            transport = name,
            endpoint = live.label,
            "this is the only name left for an endpoint that was reached under another"
        );
        if let Some(live) = self.begin(&name, &args) {
            self.wanted[index] = live;
        }
    }

    /// Every other declaration reaching the endpoint `live` is attached to, in
    /// the order they were declared in.
    fn aliases_of(&self, live: &Live) -> Vec<&Live> {
        self.wanted
            .iter()
            .filter(|alias| {
                alias.transport == live.transport && alias.alias_of.as_ref() == Some(&live.args)
            })
            .collect()
    }

    /// Writes what every attachment is doing, if it is not what was written
    /// last time.
    ///
    /// One entry per endpoint rather than per declaration: several sets of words
    /// that reach one place are one thing that is attached, listing all of them.
    fn publish(&mut self) {
        let entries: Vec<Entry> = self
            .wanted
            .iter()
            .filter(|live| live.alias_of.is_none())
            .map(|live| live.entry(&self.aliases_of(live)))
            .chain(self.found.iter().map(|live| live.entry(&[])))
            .collect();
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
        self.wanted.clear();
        self.found.clear();
        if let Err(error) = self.attachments.remove() {
            debug!(%error, "cannot remove what is attached");
        }
    }
}

/// One discovery, and when it is next to be asked.
///
/// The cadence is the discovery's own and is asked for after every sweep rather
/// than once, so that one which has just found the thing it looks with missing
/// can slow itself down without anything here knowing why.
#[derive(Debug)]
struct Sweeping {
    discovery: Arc<dyn Discovery>,
    due: Instant,
}

impl Sweeping {
    fn new(discovery: Arc<dyn Discovery>) -> Self {
        Self {
            discovery,
            due: Instant::now(),
        }
    }

    /// Whether it is time to ask again.
    fn due(&self) -> bool {
        Instant::now() >= self.due
    }

    /// Not before `how_long` from now.
    fn again(&mut self, how_long: Duration) {
        self.due = Instant::now() + how_long;
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
