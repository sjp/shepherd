//! The state every part of the daemon shares: the sequence counter, the session
//! table, the foreground observations, the recent-event buffer and the
//! publisher.
//!
//! Ingest is where an emitter's line stops being a string and becomes an event
//! this daemon vouches for. Three of the envelope's fields are the daemon's to
//! decide and are never taken from an emitter: `seq`, because a counter that
//! anyone can write is not a counter; `origin`, because an emitter has no idea
//! what it is inside, and a line arriving on this daemon's own socket has by
//! definition crossed no boundary to get here; and `ts` when the emitter did not
//! supply a usable one.
//!
//! The lock is held for stamping, the fold, the buffer write and the publish,
//! and for nothing else. Those four are one step: an event's sequence number is
//! the order subscribers are promised, so the moment it is numbered is the
//! moment it has to be handed to them, or two ingests racing could publish 41
//! after 42. Reading from a socket happens outside the lock, so a client that
//! connects and stalls holds up nothing but itself, and publishing under it
//! costs nothing that could block: a full subscriber is dropped rather than
//! waited for.
//!
//! What the process table says goes through the same door for the same reason.
//! An observation is numbered from the same counter as an event and published
//! from under the same lock, so the stream a subscriber reads has one order in
//! it rather than two that have to be reconciled.
//!
//! Holding both halves is also what lets this say that a session is over. A
//! session that speaks from a terminal something is being watched in is bound to
//! that process, and the end of the process ends the session — definitively,
//! whatever the agent did or did not manage to say on its way out. That is only
//! ever done from a positive observation: a session nothing was ever seen
//! running for is left alone rather than guessed at, because a status guessed
//! wrong here reads exactly like one that was reported.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::{Mutex, PoisonError};

use agentbus_protocol::{
    DaemonIdentity, Event, ForegroundChange, ForegroundEntry, ForegroundState, OriginHop,
    SSH_CONNECTION_DETAIL, SessionEntry, SessionKey, SessionStatus, SessionTable, Snapshot,
    Timestamp, UnstampedEvent,
};
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::binding::{Bind, Bindings};
use crate::clock;
use crate::foreground::{Slot, Transition};
use crate::procfs::Pid;

/// How many recent events the daemon keeps.
///
/// Enough to hold the burst a busy session produces, so that a snapshot and the
/// live stream can be built from one consistent moment; not a replay log, and
/// deliberately not persisted anywhere.
pub const RECENT_EVENTS: usize = 1024;

/// One line the bus publishes to everything watching it.
///
/// Both kinds travel on one channel because they are numbered from one counter
/// and a subscriber is promised that counter's order. Two channels would be free
/// to deliver 42 after 43.
#[derive(Debug, Clone, PartialEq)]
pub enum Published {
    /// An event that was ingested.
    Event(Event),
    /// A change in what is running in front of a correlated shell.
    Foreground(ForegroundChange),
}

impl Published {
    /// The sequence number this line was stamped with.
    pub fn seq(&self) -> u64 {
        match self {
            Self::Event(event) => event.seq,
            Self::Foreground(change) => change.seq,
        }
    }
}

/// One daemon this one has attached to, for as long as it is attached.
///
/// Opaque, and unique only within this process. What it is for is telling one
/// attachment's contributions from another's: everything an attachment reports
/// has to be withdrawable together when it goes away, and nothing else in the
/// state it feeds is keyed in a way that would allow that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AttachmentId(u64);

impl fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What one attached daemon has told this one.
#[derive(Debug, Default)]
struct Attached {
    /// Every session learned through it, so that they can all be ended when it
    /// stops reporting them or goes away.
    sessions: BTreeSet<SessionKey>,
    /// Whether the far end watches a process table at all. "Nobody is looking"
    /// and "nobody is running anything" are different facts at every point in a
    /// chain, so this is carried rather than inferred from an empty table.
    watching: bool,
    /// What it says is in the foreground, keyed by the slot *it* filed the
    /// observation under and the pid in *its own* process namespace. The pid
    /// means nothing on this machine beyond telling two of that daemon's
    /// observations apart.
    ///
    /// Its slot rather than the one an observation ends up with here, because
    /// this table is maintained by the lines that daemon sends and it withdraws
    /// a shell by the name it knows the shell by. A correlation put on an
    /// observation here is carried in the observation, where it belongs, and
    /// changes nothing about which of the far end's shells it is.
    foreground: BTreeMap<(String, u32), ForegroundEntry>,
    /// Which of this daemon's correlations each connection the far end named
    /// turned out to be, once a port matched; see [`State::correlate`].
    ///
    /// Kept per attachment because a port is only unique within one machine's
    /// view of it, and the far end is what makes the pair a pair.
    connections: BTreeMap<String, String>,
}

/// Everything the daemon knows, and the one place events enter it.
#[derive(Debug)]
pub struct Bus {
    state: Mutex<State>,
    events: broadcast::Sender<Published>,
    /// Who this daemon is, for the snapshots it hands out, and nothing at all
    /// for one that has not been told.
    identity: Option<DaemonIdentity>,
}

/// The parts of the bus that only ever move together.
#[derive(Debug)]
struct State {
    table: SessionTable,
    seq: u64,
    recent: VecDeque<Event>,
    /// What is in the foreground of each watched shell, keyed by the slot the
    /// observation is filed under and the shell it was seen through, or `None`
    /// where nothing is watching the process table at all.
    ///
    /// Two shells may be filed under one value, and they are two answers to one
    /// question rather than one answer twice, so the shell is part of the key.
    foreground: Option<BTreeMap<(String, Pid), ForegroundEntry>>,
    /// The process each session was last seen speaking from, for as long as both
    /// of them are still there.
    bindings: Bindings,
    /// How many daemons this one has attached to, ever, which is where the next
    /// attachment's identity comes from.
    attachments: u64,
    /// What each attached daemon has reported, kept apart from what this daemon
    /// knows first-hand so that losing one of them withdraws exactly what it
    /// said and nothing else.
    attached: BTreeMap<AttachmentId, Attached>,
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus {
    /// An empty bus with no sessions and no events.
    pub fn new() -> Self {
        Self::with_table(SessionTable::new())
    }

    /// An empty bus whose fold and retention are configured by the caller.
    pub fn with_table(table: SessionTable) -> Self {
        let (events, _) = broadcast::channel(RECENT_EVENTS);
        Self {
            state: Mutex::new(State {
                table,
                seq: 0,
                recent: VecDeque::with_capacity(RECENT_EVENTS),
                foreground: None,
                bindings: Bindings::new(),
                attachments: 0,
                attached: BTreeMap::new(),
            }),
            events,
            identity: None,
        }
    }

    /// The same bus, saying who it belongs to on every snapshot it hands out.
    ///
    /// What a subscriber does with that is compare it with another daemon's:
    /// two ways of reaching one machine are not two machines, and the daemon
    /// itself is the only party that can settle which it is. A bus that has not
    /// been told says nothing, which is what a reader of an older daemon's
    /// stream finds too.
    #[must_use]
    pub fn identified(mut self, identity: DaemonIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Who this bus says it is, where anything has told it.
    pub fn identity(&self) -> Option<&DaemonIdentity> {
        self.identity.as_ref()
    }

    /// The same bus, reporting foreground observations.
    ///
    /// Whether anything is watching the process table is settled before the bus
    /// serves anything, because "nobody is looking" and "nobody is running
    /// anything" are different facts and a snapshot has to be able to say which
    /// one it means. A bus that is not watching says nothing whatever about the
    /// foreground; one that is says so from its first snapshot, when the answer
    /// is still an empty list.
    #[must_use]
    pub fn observing(mut self) -> Self {
        self.state
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .foreground = Some(BTreeMap::new());
        self
    }

    /// Turns one received line into a stamped event, folds it into its session,
    /// records it and publishes it. Returns the event, or nothing if the line
    /// was not an event at all.
    ///
    /// A line that does not parse is dropped: the emitter is a hook inside
    /// somebody's coding agent and cannot be told about it, and a daemon that
    /// stopped ingesting because one client sent nonsense would be trading every
    /// other session's status for a message nobody reads.
    ///
    /// An event is also where a session is bound to the process it is speaking
    /// from, because an event is the only moment the two are known to be the
    /// same thing: whatever is in front of that terminal now is what just spoke.
    pub fn ingest(&self, line: &[u8]) -> Option<Event> {
        let (mut event, reported_ts) = match parse(line) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug!(%error, "dropped a line that is not an event");
                return None;
            }
        };
        // Nothing received here came from anywhere else.
        event.origin.clear();
        let ts = reported_ts.unwrap_or_else(clock::now);

        let event = {
            let mut state = self.lock();
            state.seq += 1;
            let event = event.stamp(state.seq, ts);
            if let Some(conflict) = state.table.apply_event(&event) {
                warn!(
                    agent = %conflict.key.agent,
                    session = %conflict.key.session,
                    "an event's origin disagrees with the chain this session was first seen with"
                );
            }
            state.bind(&event);
            if state.recent.len() == RECENT_EVENTS {
                state.recent.pop_front();
            }
            state.recent.push_back(event.clone());
            // Published while the number is still being handed out, because
            // subscribers are promised the stream in `seq` order and two
            // connections ingesting at once would otherwise be free to reach the
            // publisher in the opposite order to the one they were numbered in.
            // Nobody may be listening, and that is not a failure: the bus exists
            // whether or not anything is watching it.
            let _ = self.events.send(Published::Event(event.clone()));
            event
        };
        Some(event)
    }

    /// Moves every session's clock forward, so that a session that has gone
    /// quiet becomes stale and one that is over is eventually forgotten.
    pub fn tick(&self, now: &Timestamp) {
        let mut state = self.lock();
        state.table.tick(now);
        let State {
            table, bindings, ..
        } = &mut *state;
        // A session the table has dropped can never be told anything again, and
        // its binding would otherwise be a row this daemon keeps for as long as
        // it runs.
        bindings.retain(|key| table.get(key).is_some());
    }

    /// Records what a foreground monitor saw, publishes a line for each change,
    /// and hands back the pids it reported as having ended.
    ///
    /// A transition is by construction news — the monitor produces one only when
    /// something differs from what it last reported — so every one of them is
    /// numbered and published. That is also why the numbering happens here,
    /// under the lock ingest stamps events under: a subscriber reads one stream
    /// and is owed one order for it.
    ///
    /// The table a snapshot is built from is maintained *from the transitions*
    /// rather than copied from whatever produced them, which is what makes the
    /// two halves of a subscription agree: a subscriber that applies the lines
    /// it reads to the snapshot it started with arrives at exactly this table.
    ///
    /// A pid that ended takes every session bound to it with it. That is the
    /// one thing in this system that is certain rather than inferred — the
    /// process is not in the table, so nothing is going to be reported from it
    /// again — and it is why a killed agent reaches `done` without ever having
    /// said so. The pids are returned as well, for a caller that wants to say
    /// what it saw.
    pub fn observed(&self, transitions: &[Transition], now: &Timestamp) -> Vec<Pid> {
        let gone: Vec<Pid> = transitions
            .iter()
            .filter_map(|change| change.gone)
            .collect();
        let mut state = self.lock();
        {
            let State {
                seq, foreground, ..
            } = &mut *state;
            // Nothing is watching, so there is nothing to report — and nothing
            // that could have produced a transition in the first place, nor a
            // binding for one to end.
            if let Some(table) = foreground.as_mut() {
                for transition in transitions {
                    *seq += 1;
                    let key = (transition.slot.value().to_owned(), transition.shell);
                    let line = match &transition.foreground {
                        Some(entry) => {
                            table.insert(key, entry.clone());
                            ForegroundChange::observed(*seq, now.clone(), entry.clone())
                        }
                        None => {
                            table.remove(&key);
                            withdrawing(*seq, now.clone(), &transition.slot)
                        }
                    };
                    let _ = self.events.send(Published::Foreground(line));
                }
            }
        }
        for pid in &gone {
            state.reap(*pid, now);
        }
        gone
    }

    /// Registers a daemon this one is about to start reading, and hands back the
    /// identity everything it says will be filed under.
    pub fn attach(&self) -> AttachmentId {
        let mut state = self.lock();
        state.attachments += 1;
        let id = AttachmentId(state.attachments);
        state.attached.insert(id, Attached::default());
        id
    }

    /// Replaces everything an attachment has reported with what its daemon says
    /// is true now.
    ///
    /// This is the whole of what reaching another daemon brings back. It has
    /// already folded its own events, so its sessions are *seeded* — set to the
    /// status it reports — rather than replayed from events this one never saw.
    /// A session it used to report and no longer mentions is over: either it
    /// finished and was forgotten over there, or the daemon that knew it is
    /// gone, and in both cases nothing further is ever going to be said about
    /// it.
    ///
    /// `foreground` is `None` for a far end that watches no process table,
    /// which is a different answer from an empty list and is carried as one.
    ///
    /// Sessions arrive on the far end's own account and there is no line on this
    /// daemon's stream that could carry one, so a subscriber learns them from
    /// its next snapshot. Observations do have a line, and the differences from
    /// what this attachment last said are published as ordinary changes, so that
    /// a subscriber applying the stream to the snapshot it started with stays
    /// level with what a new one would be handed.
    pub fn seed(
        &self,
        id: AttachmentId,
        sessions: &[SessionEntry],
        foreground: Option<&[ForegroundEntry]>,
        now: &Timestamp,
    ) {
        let mut state = self.lock();
        let mut seeded = BTreeSet::new();
        for entry in sessions {
            seeded.insert(state.table.seed(entry, now));
        }
        let mut arriving: Vec<(String, ForegroundEntry)> = foreground
            .unwrap_or_default()
            .iter()
            .map(|entry| (slot_of(entry), entry.clone()))
            .collect();
        for (_, entry) in &mut arriving {
            state.correlate(id, entry);
        }

        let attached = state.attached.entry(id).or_default();
        let dropped: Vec<SessionKey> = attached.sessions.difference(&seeded).cloned().collect();
        attached.sessions = seeded;

        let reported: BTreeMap<(String, u32), ForegroundEntry> = arriving
            .into_iter()
            .map(|(filed, entry)| ((filed, entry.pid), entry))
            .collect();
        attached.watching = foreground.is_some();
        let previous = std::mem::replace(&mut attached.foreground, reported.clone());

        // A slot the far end no longer reports at all has lost every
        // observation under it, and the line that says so is built from one of
        // the observations being withdrawn, so that it names the slot in the
        // field that slot came from.
        let mut withdrawn: BTreeMap<String, ForegroundEntry> = BTreeMap::new();
        for ((slot, _), entry) in previous
            .iter()
            .filter(|((slot, _), _)| !reported.keys().any(|(still, _)| still == slot))
        {
            withdrawn.entry(slot.clone()).or_insert(entry.clone());
        }
        let observed: Vec<ForegroundEntry> = reported
            .iter()
            .filter(|(key, entry)| previous.get(*key) != Some(*entry))
            .map(|(_, entry)| entry.clone())
            .collect();

        for key in &dropped {
            if !state.still_reported(key) {
                state.table.ended(key, now);
            }
        }
        for entry in withdrawn.into_values() {
            state.seq += 1;
            let line = ForegroundChange::withdrawing(state.seq, now.clone(), &entry);
            let _ = self.events.send(Published::Foreground(line));
        }
        for entry in observed {
            state.seq += 1;
            let line = ForegroundChange::observed(state.seq, now.clone(), entry);
            let _ = self.events.send(Published::Foreground(line));
        }
    }

    /// Merges an event another daemon folded, numbering it in this daemon's
    /// sequence and publishing it as though it had happened here.
    ///
    /// Everything ingest does, except the two things that would be claims about
    /// somewhere else. The `origin` is the caller's and is left alone: only what
    /// is reading the far end knows what boundary the event crossed to get here.
    /// And nothing is bound to a process, because the pids an attached daemon
    /// reports are numbers in its own process table; a session over there ends
    /// when that daemon says so, and its `session_end` arrives here like any
    /// other event.
    pub fn merge(&self, id: AttachmentId, event: Event) -> Event {
        let mut state = self.lock();
        state.seq += 1;
        let mut event = event;
        event.seq = state.seq;
        state.correlate_event(id, &mut event);
        if let Some(conflict) = state.table.apply_event(&event) {
            warn!(
                agent = %conflict.key.agent,
                session = %conflict.key.session,
                "an event's origin disagrees with the chain this session was first seen with"
            );
        }
        state
            .attached
            .entry(id)
            .or_default()
            .sessions
            .insert(SessionKey::of(&event));
        if state.recent.len() == RECENT_EVENTS {
            state.recent.pop_front();
        }
        state.recent.push_back(event.clone());
        let _ = self.events.send(Published::Event(event.clone()));
        event
    }

    /// Merges a change in what an attached daemon says is in front of one of its
    /// correlated shells, numbering it here and publishing it.
    pub fn merge_foreground(&self, id: AttachmentId, change: ForegroundChange) -> ForegroundChange {
        let mut state = self.lock();
        state.seq += 1;
        let mut change = change;
        change.seq = state.seq;
        // What the far end filed this under, read before anything here renames
        // it: this daemon's table of that daemon's shells is keyed by the names
        // that daemon uses for them.
        let filed = change.foreground.as_ref().map(slot_of);
        state.correlate_change(id, &mut change);

        let attached = state.attached.entry(id).or_default();
        // A daemon that reports a change is a daemon that is watching, whatever
        // its last snapshot said.
        attached.watching = true;
        match &change.foreground {
            Some(entry) => {
                attached
                    .foreground
                    .insert((filed.unwrap_or_default(), entry.pid), entry.clone());
            }
            // A withdrawal names a slot and nothing else, so it takes every
            // observation filed under that slot with it. That is what the line
            // means at the far end too: the daemon that wrote it keeps its own
            // table by exactly this rule. Both fields are read because renaming
            // the line put a correlation on it that the far end never used, and
            // the name it did use is the one this table is keyed by.
            None => {
                let named: Vec<&String> = [&change.correlation, &change.ssh_connection]
                    .into_iter()
                    .flatten()
                    .collect();
                attached
                    .foreground
                    .retain(|(slot, _), _| !named.contains(&slot));
            }
        }
        let _ = self.events.send(Published::Foreground(change.clone()));
        change
    }

    /// Forgets everything an attachment reported: its sessions are over, and its
    /// observations are withdrawn.
    ///
    /// The daemon at the far end is untouched by this and carries on. What ends
    /// is this daemon's account of it, which is the only thing it was ever
    /// entitled to end.
    pub fn detach(&self, id: AttachmentId, now: &Timestamp) {
        let mut state = self.lock();
        let Some(attached) = state.attached.remove(&id) else {
            return;
        };
        for key in &attached.sessions {
            if !state.still_reported(key) {
                state.table.ended(key, now);
            }
        }
        let mut withdrawn: BTreeMap<String, ForegroundEntry> = BTreeMap::new();
        for ((slot, _), entry) in attached.foreground {
            withdrawn.entry(slot).or_insert(entry);
        }
        withdrawn.retain(|slot, _| !state.still_observed(slot));
        for entry in withdrawn.into_values() {
            state.seq += 1;
            let line = ForegroundChange::withdrawing(state.seq, now.clone(), &entry);
            let _ = self.events.send(Published::Foreground(line));
        }
    }

    /// Lets go of an attachment without withdrawing anything it reported,
    /// because another attachment is reporting the same daemon.
    ///
    /// The difference from [`Bus::detach`] is the whole of the point. Detaching
    /// says "nobody can speak for these sessions any more", and that is a claim
    /// about the daemon at the far end, not about the connection: where two ways
    /// in turned out to reach one daemon, the sessions are still being reported,
    /// by the other one, and ending them here would take away rows that are true
    /// and that nothing would put back until the far end next said something.
    pub fn forget(&self, id: AttachmentId) {
        self.lock().attached.remove(&id);
    }

    /// A receiver of every line published from now on.
    pub fn events(&self) -> broadcast::Receiver<Published> {
        self.events.subscribe()
    }

    /// Everything the bus knows now, and a receiver of everything it learns
    /// afterwards.
    ///
    /// The two are taken together under the lock that ingest also stamps and
    /// publishes under, so an event is on exactly one side of the join: either
    /// it is in the snapshot, or it arrives on the receiver. A reader that
    /// ignores what the snapshot already covers is therefore reading a
    /// continuation of it, and stays right whatever the bus does with its own
    /// ordering later.
    pub fn subscribe(&self) -> (Snapshot, broadcast::Receiver<Published>) {
        let state = self.lock();
        let snapshot = state.table.snapshot(state.seq);
        let snapshot = match state.watched() {
            Some(foreground) => snapshot.with_foreground(foreground),
            None => snapshot,
        };
        let snapshot = match &self.identity {
            Some(identity) => snapshot.with_daemon(identity.clone()),
            None => snapshot,
        };
        (snapshot, self.events.subscribe())
    }

    /// The sequence number the most recently published line was stamped with;
    /// zero before there has been one.
    pub fn last_seq(&self) -> u64 {
        self.lock().seq
    }

    /// The sessions worth reporting, in the order a snapshot lists them.
    pub fn sessions(&self) -> Vec<SessionEntry> {
        self.lock().table.snapshot_sessions()
    }

    /// The foreground observations a snapshot would carry, or nothing where
    /// neither this daemon nor any daemon it is attached to is watching a
    /// process table.
    pub fn foreground(&self) -> Option<Vec<ForegroundEntry>> {
        self.lock().watched()
    }

    /// The process a session is currently bound to, if it has been seen
    /// speaking from one.
    ///
    /// Nothing this daemon serves reports a binding: it is an internal join
    /// between a session and a row of the process table, and it changes nothing
    /// a subscriber can see except the moment a session becomes `done`. It is
    /// readable because a binding that is never observable is a rule that can
    /// only be tested through its consequences.
    pub fn bound_to(&self, key: &SessionKey) -> Option<Pid> {
        self.lock().bindings.pid_of(key)
    }

    /// The events still in the recent buffer, oldest first.
    pub fn recent(&self) -> Vec<Event> {
        self.lock().recent.iter().cloned().collect()
    }

    /// The shared state.
    ///
    /// A panic somewhere under this lock poisons it. The daemon carries on with
    /// the state as it was left: the alternative is that one malformed session
    /// takes down the bus for every other session on the machine.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    /// Whether any attachment is still reporting `key`.
    ///
    /// This is what decides whether a session learned from somewhere else is
    /// over, and it is a question about the daemon that folds it rather than
    /// about any one connection to it. Two attachments may be reading one daemon
    /// — two names for one machine, before either has said which — and one of
    /// them stopping is not news about what the other is still reporting.
    fn still_reported(&self, key: &SessionKey) -> bool {
        self.attached
            .values()
            .any(|attached| attached.sessions.contains(key))
    }

    /// Whether any attachment is still observing something in `correlation`,
    /// which is the same question as above about the other half of what an
    /// attached daemon reports.
    fn still_observed(&self, slot: &str) -> bool {
        self.attached
            .values()
            .any(|attached| attached.foreground.keys().any(|(filed, _)| filed == slot))
    }

    /// Every foreground observation this daemon would report, its own and those
    /// relayed from the daemons it is attached to, or nothing where nobody
    /// anywhere in the chain is watching a process table at all.
    ///
    /// A daemon on a machine with no process table to read still reports what
    /// the ones it reaches can see. Saying nothing there would be saying that
    /// nothing is running, which is a different and false claim.
    fn watched(&self) -> Option<Vec<ForegroundEntry>> {
        let relayed = self
            .attached
            .values()
            .filter(|attached| attached.watching)
            .flat_map(|attached| attached.foreground.values().cloned());
        match &self.foreground {
            Some(table) => Some(table.values().cloned().chain(relayed).collect()),
            None => {
                let relayed: Vec<ForegroundEntry> = relayed.collect();
                self.attached
                    .values()
                    .any(|attached| attached.watching)
                    .then_some(relayed)
            }
        }
    }

    /// Binds the session an event belongs to to the process the event was
    /// spoken from, where the process table says what that is.
    ///
    /// The correlation is the event's own rather than the session's: it is what
    /// the emitter carried this time, and a session that moved to another
    /// terminal is speaking from a different process now. Nothing here parses
    /// it — the value is compared with `==` against what a shell exported, and
    /// that is the whole of the matching.
    fn bind(&mut self, event: &Event) {
        let Some(correlation) = event
            .correlation
            .as_deref()
            .filter(|correlation| !correlation.is_empty())
        else {
            return;
        };
        let Some(foreground) = &self.foreground else {
            return;
        };
        let Some(pid) = tracked(foreground, correlation, &event.origin) else {
            return;
        };

        let key = SessionKey::of(event);
        match self.bindings.bind(&key, pid) {
            Bind::Unchanged => {}
            Bind::Bound => debug!(
                agent = %key.agent,
                session = %key.session,
                pid,
                "a session is running as the process in front of the terminal it spoke from"
            ),
            Bind::Rebound { from } => debug!(
                agent = %key.agent,
                session = %key.session,
                pid,
                previous = from,
                "a session spoke from a terminal something else is now running in"
            ),
        }
    }

    /// Stamps the correlation of one of this daemon's own shells onto an
    /// observation another daemon made through the connection that shell holds
    /// open.
    ///
    /// This is the whole of what makes two views of one terminal one row.
    /// Neither end sent the other anything: the far end reports the connection
    /// its shell arrived over, this end reports the source port of the one
    /// connection its own foreground process has open, and the two are the same
    /// connection seen from its two halves. An observation that already carries
    /// a correlation is left exactly as it is — the shell said what it was, and
    /// nothing inferred here outranks that.
    ///
    /// Done for every attachment rather than for connections of some particular
    /// kind, because the match is on values both ends produced and holds
    /// wherever it holds. A far end whose shells carry no connection at all
    /// never reaches the comparison.
    ///
    /// The pairing is remembered, so that an event which arrives from that same
    /// shell can be attributed even at a moment when this end's own view of the
    /// connection has not been re-read. It is dropped again as soon as an
    /// observation of that shell arrives and no local shell holds the
    /// connection any more; what was stamped before then stands, because it was
    /// true when it was stamped.
    fn correlate(&mut self, id: AttachmentId, entry: &mut ForegroundEntry) {
        if entry.correlation.is_some() {
            return;
        }
        let Some(connection) = entry.ssh_connection.clone() else {
            return;
        };
        let held = self.holding(&connection);
        let remembered = &mut self.attached.entry(id).or_default().connections;
        match held {
            Some(correlation) => {
                entry.correlation = Some(correlation.clone());
                remembered.insert(connection, correlation);
            }
            None => {
                remembered.remove(&connection);
            }
        }
    }

    /// The same, for a whole line: an observation is stamped, and a withdrawal
    /// is renamed.
    ///
    /// A withdrawal has to be renamed because of what was done to the
    /// observations it withdraws. They went into this daemon's table under the
    /// correlation stamped on them, so a line still naming the connection would
    /// withdraw nothing, and a subscriber reading this daemon's stream would be
    /// left holding an observation that has ended.
    fn correlate_change(&mut self, id: AttachmentId, change: &mut ForegroundChange) {
        if let Some(entry) = &mut change.foreground {
            self.correlate(id, entry);
            return;
        }
        let Some(connection) = change.ssh_connection.clone() else {
            return;
        };
        if change.correlation.is_none()
            && let Some(correlation) = self
                .attached
                .entry(id)
                .or_default()
                .connections
                .remove(&connection)
        {
            change.correlation = Some(correlation);
        }
    }

    /// Stamps a correlation on an event that came from a shell reached over a
    /// connection one of this daemon's own shells holds open.
    ///
    /// The same match as [`State::correlate`] on the same value, because an
    /// emitter with no correlation to copy reports the connection it was
    /// reached over instead, and that says which shell here it came from just
    /// as well. What this daemon can see now is preferred, and the pairing an
    /// observation left behind stands in where it cannot see anything: an event
    /// arrives whenever the agent produces one, which need not be a moment when
    /// this end's own view of the connection has just been read.
    fn correlate_event(&mut self, id: AttachmentId, event: &mut Event) {
        if event
            .correlation
            .as_deref()
            .is_some_and(|correlation| !correlation.is_empty())
        {
            return;
        }
        let Some(connection) = event
            .detail
            .as_ref()
            .and_then(|detail| detail.get(SSH_CONNECTION_DETAIL))
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        let correlation = self.holding(&connection).or_else(|| {
            self.attached
                .get(&id)
                .and_then(|attached| attached.connections.get(&connection).cloned())
        });
        if let Some(correlation) = correlation {
            event.correlation = Some(correlation);
        }
    }

    /// The correlation of this daemon's own shell holding open the connection a
    /// far end described, where exactly one of them does.
    ///
    /// `sshd` documents `SSH_CONNECTION` as four space-separated fields: the
    /// client address, the client port, the server address and the server port.
    /// Taking the second is the only structural thing done to the value
    /// anywhere, and it is done against that documented format rather than
    /// against a convention anybody here invented.
    ///
    /// Only the port is compared. It is the one part of the string that
    /// identifies the connection rather than the machines at its ends;
    /// addresses are rewritten by network address translation, mapped between
    /// address families and replaced outright by a jump host, and none of that
    /// changes which connection this is. The port is compared as a string,
    /// against the decimal form of what was read from this machine's own
    /// connection tables.
    ///
    /// Only this daemon's own observations are eligible. One relayed from
    /// somewhere else is a view of a third machine's connections, whose ports
    /// are numbers in a space this one shares nothing with.
    ///
    /// Where two of this daemon's shells would answer differently, none of them
    /// answers: a correlation stamped on a guess is worse than none at all.
    fn holding(&self, connection: &str) -> Option<String> {
        let port = client_port(connection)?;
        let mut correlations = self
            .foreground
            .as_ref()?
            .values()
            .filter(|entry| entry.origin.is_empty())
            .filter(|entry| {
                entry
                    .ssh_client_port
                    .is_some_and(|open| open.to_string() == port)
            })
            .filter_map(|entry| entry.correlation.as_deref());
        let first = correlations.next()?;
        correlations
            .all(|other| other == first)
            .then(|| first.to_owned())
    }

    /// Ends every session bound to a process that has left the process table.
    ///
    /// A session that is already over is left where it is, so that the moment it
    /// finished stays the moment it finished and its retention is not extended
    /// by its terminal outliving it.
    fn reap(&mut self, pid: Pid, now: &Timestamp) {
        for key in self.bindings.release(pid) {
            let over = self
                .table
                .get(&key)
                .is_none_or(|session| session.state.status == SessionStatus::Done);
            if over {
                continue;
            }
            self.table.process_gone(&key, now);
            debug!(
                agent = %key.agent,
                session = %key.session,
                pid,
                "the process a session was running as has left the process table"
            );
        }
    }
}

/// The client port field of an `SSH_CONNECTION` value.
///
/// Exactly four fields or nothing: a value with any other shape is not the one
/// `sshd` documents, and guessing which of its parts was meant to be the port
/// would be inventing a format.
fn client_port(connection: &str) -> Option<&str> {
    let mut fields = connection.split(' ');
    let (_client, port, _server, _port) = (
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
    );
    fields.next().is_none().then_some(port)
}

/// What an observation is filed under, for the tables keyed by it.
///
/// An observation with neither of the two values is filed under nothing, which
/// is a key like any other: it is one shell's worth of observations, told apart
/// from every other by the pid that goes in the key beside this.
fn slot_of(entry: &ForegroundEntry) -> String {
    entry.slot().unwrap_or_default().to_owned()
}

/// The line that withdraws every observation filed under one slot.
fn withdrawing(seq: u64, ts: Timestamp, slot: &Slot) -> ForegroundChange {
    match slot {
        Slot::Correlation(value) => ForegroundChange::withdrawn(seq, ts, value),
        Slot::Connection(value) => ForegroundChange::withdrawn_connection(seq, ts, value),
    }
}

/// The process to follow for a session that spoke with `correlation` from
/// `origin`, where the observations say what that is beyond doubt.
///
/// Beyond doubt is the whole of it. Two shells may carry one correlation, and
/// then two processes are in front of two terminals a session might have spoken
/// from; binding to either would be a guess, and a guess here ends a session
/// that is still running. So an answer is given only where every observation
/// agrees on it.
///
/// The origin decides which daemon's process table an observation belongs to, so
/// a session whose events crossed a boundary is matched against observations
/// made on the far side of that same boundary and never against a nearer view of
/// the connection itself. A hop's `name` is a display string that several ids
/// may share, and takes no part in it.
fn tracked(
    foreground: &BTreeMap<(String, Pid), ForegroundEntry>,
    correlation: &str,
    origin: &[OriginHop],
) -> Option<Pid> {
    let mut running = foreground
        .values()
        .filter(|entry| {
            entry.correlation.as_deref() == Some(correlation) && same_path(&entry.origin, origin)
        })
        .filter(|entry| {
            matches!(
                entry.state,
                Some(ForegroundState::Foreground | ForegroundState::Suspended)
            )
        })
        .filter_map(|entry| Pid::try_from(entry.pid).ok());
    let first = running.next()?;
    running.all(|pid| pid == first).then_some(first)
}

/// Whether two chains of hops lead to the same place. `name` is display only and
/// is not compared.
fn same_path(left: &[OriginHop], right: &[OriginHop]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.kind == right.kind && left.id == right.id)
}

/// Reads one line as an event, along with the timestamp it reported if that
/// timestamp is one this protocol can carry.
///
/// An emitter's `ts` is kept because it is closer to when the thing actually
/// happened than the moment the daemon got round to reading the socket. It is
/// not trusted any further than that: anything that is not a well-formed
/// timestamp is replaced rather than rejected, since the event itself is still
/// news.
fn parse(line: &[u8]) -> Result<(UnstampedEvent, Option<Timestamp>), serde_json::Error> {
    let value: Value = serde_json::from_slice(line)?;
    let ts = value
        .get("ts")
        .and_then(Value::as_str)
        .and_then(|ts| Timestamp::parse(ts).ok());
    Ok((serde_json::from_value(value)?, ts))
}

#[cfg(test)]
mod tests {
    use super::*;

    use agentbus_protocol::{Agent, Kind, Source, observed_session_id};
    use serde_json::{Map, json};

    /// A bus that is watching a process table, as one with a monitor behind it
    /// is.
    fn watching() -> Bus {
        Bus::new().observing()
    }

    fn at(text: &str) -> Timestamp {
        Timestamp::parse(text).expect("not a timestamp")
    }

    fn now() -> Timestamp {
        at("2026-08-17T10:32:01.412Z")
    }

    /// What a monitor produces when it first sees something in front of a shell.
    fn appeared(correlation: &str, shell: Pid, pid: u32, process: &str) -> Transition {
        let mut entry =
            ForegroundEntry::new(pid, process, process, now()).with_correlation(correlation);
        entry.state = Some(ForegroundState::Foreground);
        Transition {
            slot: Slot::Correlation(correlation.to_owned()),
            shell,
            foreground: Some(entry),
            gone: None,
        }
    }

    /// What it produces when there is no longer anything to report there.
    fn withdrawn(correlation: &str, shell: Pid, gone: Option<Pid>) -> Transition {
        Transition {
            slot: Slot::Correlation(correlation.to_owned()),
            shell,
            foreground: None,
            gone,
        }
    }

    fn line(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    fn tool_start() -> Value {
        json!({"v": 1, "agent": "claude", "session": "abc123", "kind": "tool_start"})
    }

    /// The same, for a process that is alive but no longer holds the terminal.
    fn backgrounded(correlation: &str, shell: Pid, pid: u32, process: &str) -> Transition {
        let mut transition = appeared(correlation, shell, pid, process);
        if let Some(entry) = transition.foreground.as_mut() {
            entry.state = Some(ForegroundState::Suspended);
        }
        transition
    }

    /// A line from `session`, carrying `correlation` where the emitter had one.
    fn spoke(session: &str, correlation: Option<&str>) -> Vec<u8> {
        let mut event = tool_start();
        event["session"] = json!(session);
        if let Some(correlation) = correlation {
            event["correlation"] = json!(correlation);
        }
        line(&event)
    }

    fn key(session: &str) -> SessionKey {
        SessionKey::new("claude", session)
    }

    /// What the bus says about one session, which it was expected to know.
    fn status(bus: &Bus, session: &str) -> SessionStatus {
        let sessions = bus.sessions();
        sessions
            .iter()
            .find(|entry| entry.session == session)
            .unwrap_or_else(|| panic!("{session} is not in {sessions:?}"))
            .status
    }

    #[test]
    fn an_event_is_stamped_folded_and_recorded() {
        let bus = Bus::new();

        let event = bus.ingest(&line(&tool_start())).unwrap();

        assert_eq!(event.seq, 1);
        assert_eq!(event.agent, Agent::Claude);
        assert_eq!(event.kind, Kind::ToolStart);
        assert_eq!(bus.last_seq(), 1);
        assert_eq!(bus.recent(), vec![event.clone()]);

        let sessions = bus.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session, "abc123");
        assert_eq!(sessions[0].status, SessionStatus::Working);
        assert_eq!(sessions[0].since, event.ts);
    }

    #[test]
    fn sequence_numbers_start_at_one_and_never_repeat() {
        let bus = Bus::new();

        let seqs: Vec<u64> = (0..3)
            .map(|_| bus.ingest(&line(&tool_start())).unwrap().seq)
            .collect();

        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(bus.last_seq(), 3);
    }

    #[test]
    fn a_reported_timestamp_is_kept_and_a_missing_one_is_stamped() {
        let bus = Bus::new();
        let mut reported = tool_start();
        reported["ts"] = json!("2026-08-17T10:32:01.412Z");

        let kept = bus.ingest(&line(&reported)).unwrap();
        assert_eq!(kept.ts.as_str(), "2026-08-17T10:32:01.412Z");

        let stamped = bus.ingest(&line(&tool_start())).unwrap();
        assert!(stamped.ts.as_str() > "2020-01-01T00:00:00.000Z");
    }

    #[test]
    fn a_timestamp_this_protocol_cannot_carry_is_replaced() {
        let bus = Bus::new();
        let mut event = tool_start();
        event["ts"] = json!("yesterday afternoon");

        let ingested = bus.ingest(&line(&event)).unwrap();

        assert!(ingested.ts.as_str() > "2020-01-01T00:00:00.000Z");
    }

    #[test]
    fn an_origin_chain_claimed_by_an_emitter_is_discarded() {
        let bus = Bus::new();
        let mut event = tool_start();
        event["origin"] = json!([{"kind": "ssh", "id": "9f3c:1000", "name": "fileserver"}]);

        let ingested = bus.ingest(&line(&event)).unwrap();

        assert_eq!(ingested.origin, Vec::<OriginHop>::new());
        assert_eq!(bus.sessions()[0].origin, Vec::<OriginHop>::new());
    }

    #[test]
    fn the_rest_of_the_envelope_survives_verbatim() {
        let bus = Bus::new();
        let mut event = tool_start();
        event["source"] = json!("observed");
        event["cwd"] = json!("/srv/project");
        event["correlation"] = json!("w9:p3");
        event["detail"] = json!({"tool": "Bash"});
        event["raw"] = json!({"hook_event_name": "PreToolUse"});

        let ingested = bus.ingest(&line(&event)).unwrap();

        assert_eq!(ingested.source, Source::Observed);
        assert_eq!(ingested.cwd.as_deref(), Some("/srv/project"));
        assert_eq!(ingested.correlation.as_deref(), Some("w9:p3"));
        assert_eq!(ingested.detail.unwrap()["tool"], json!("Bash"));
        assert_eq!(
            ingested.raw.unwrap(),
            json!({"hook_event_name": "PreToolUse"})
        );
    }

    #[test]
    fn a_line_that_is_not_an_event_is_dropped_without_consuming_a_sequence_number() {
        let bus = Bus::new();

        assert!(bus.ingest(b"not json at all").is_none());
        assert!(bus.ingest(b"{}").is_none());
        assert!(
            bus.ingest(&line(&json!({"v": 1, "agent": "claude"})))
                .is_none()
        );
        assert!(bus.ingest(b"").is_none());
        assert_eq!(bus.last_seq(), 0);
        assert!(bus.sessions().is_empty());

        assert_eq!(bus.ingest(&line(&tool_start())).unwrap().seq, 1);
    }

    #[test]
    fn an_unknown_kind_is_carried_rather_than_rejected() {
        let bus = Bus::new();
        let mut event = tool_start();
        event["kind"] = json!("sang_a_song");

        let ingested = bus.ingest(&line(&event)).unwrap();

        assert_eq!(ingested.kind, Kind::Unknown("sang_a_song".to_owned()));
    }

    #[test]
    fn every_ingested_event_reaches_a_subscriber() {
        let bus = Bus::new();
        let mut events = bus.events();

        let ingested = bus.ingest(&line(&tool_start())).unwrap();

        assert_eq!(events.try_recv().unwrap(), Published::Event(ingested));
    }

    #[test]
    fn ingest_does_not_depend_on_anything_listening() {
        let bus = Bus::new();
        drop(bus.events());

        assert!(bus.ingest(&line(&tool_start())).is_some());
    }

    #[test]
    fn the_recent_buffer_keeps_the_newest_events_and_no_more() {
        let bus = Bus::new();

        for _ in 0..RECENT_EVENTS + 10 {
            bus.ingest(&line(&tool_start()));
        }

        let recent = bus.recent();
        assert_eq!(recent.len(), RECENT_EVENTS);
        assert_eq!(recent.first().unwrap().seq, 11);
        assert_eq!(recent.last().unwrap().seq, (RECENT_EVENTS + 10) as u64);
    }

    #[test]
    fn an_observation_is_recorded_numbered_and_published() {
        let bus = watching();
        let mut published = bus.events();

        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());

        let observations = bus.foreground().expect("this bus is watching");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].correlation.as_deref(), Some("w9:p3"));
        assert_eq!(observations[0].pid, 4471);
        assert_eq!(bus.last_seq(), 1);
        let Published::Foreground(line) = published.try_recv().unwrap() else {
            panic!("an observation was published as something else");
        };
        assert_eq!(line.seq, 1);
        assert_eq!(line.ts, now());
        assert_eq!(line.foreground.unwrap().process, "claude");
    }

    #[test]
    fn observations_and_events_are_numbered_from_the_one_counter() {
        let bus = watching();

        bus.ingest(&line(&tool_start()));
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());
        let last = bus.ingest(&line(&tool_start())).unwrap();

        assert_eq!(last.seq, 3);
        assert_eq!(bus.last_seq(), 3);
    }

    #[test]
    fn a_withdrawal_takes_the_observation_away_and_says_which_correlation() {
        let bus = watching();
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());
        let mut published = bus.events();

        bus.observed(&[withdrawn("w9:p3", 100, Some(4471))], &now());

        assert_eq!(bus.foreground(), Some(Vec::new()));
        let Published::Foreground(line) = published.try_recv().unwrap() else {
            panic!("a withdrawal was published as something else");
        };
        assert_eq!(line.foreground, None);
        assert_eq!(line.correlation.as_deref(), Some("w9:p3"));
    }

    #[test]
    fn a_process_that_ended_is_handed_back_to_the_caller() {
        let bus = watching();

        let gone = bus.observed(
            &[
                withdrawn("w9:p3", 100, Some(4471)),
                withdrawn("w9:p4", 101, None),
            ],
            &now(),
        );

        assert_eq!(gone, vec![4471]);
    }

    #[test]
    fn two_shells_carrying_one_correlation_are_two_observations() {
        let bus = watching();

        bus.observed(
            &[
                appeared("w9:p3", 100, 4471, "claude"),
                appeared("w9:p3", 200, 5512, "vim"),
            ],
            &now(),
        );

        let observations = bus.foreground().expect("this bus is watching");
        assert_eq!(observations.len(), 2);
        // One of them going away leaves the other where it was.
        bus.observed(&[withdrawn("w9:p3", 100, None)], &now());
        let observations = bus.foreground().expect("this bus is watching");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].pid, 5512);
    }

    #[test]
    fn a_bus_that_is_not_watching_says_nothing_about_the_foreground() {
        let bus = Bus::new();
        let mut published = bus.events();

        let gone = bus.observed(&[withdrawn("w9:p3", 100, Some(4471))], &now());

        assert_eq!(
            gone,
            vec![4471],
            "what ended is news to the caller either way"
        );
        assert_eq!(bus.foreground(), None);
        assert_eq!(bus.last_seq(), 0, "a line nobody was sent was numbered");
        assert!(published.try_recv().is_err());
    }

    /// One session as an attached daemon reports it.
    fn relayed(session: &str) -> SessionEntry {
        SessionEntry {
            session: session.to_owned(),
            agent: Agent::Claude,
            status: SessionStatus::Blocked,
            source: Source::Hook,
            cwd: None,
            correlation: None,
            origin: vec![OriginHop::new(OriginHop::SSH, "9f3c:1000", "fileserver")],
            since: now(),
        }
    }

    #[test]
    fn losing_one_way_in_to_a_daemon_leaves_what_another_is_still_reporting() {
        // Two attachments reading one daemon, which is what two names for one
        // machine gets you until something has said they are one.
        let bus = Bus::new();
        let one = bus.attach();
        let other = bus.attach();
        bus.seed(one, &[relayed("abc123")], None, &now());
        bus.seed(other, &[relayed("abc123")], None, &now());

        bus.detach(one, &now());

        assert_eq!(status(&bus, "abc123"), SessionStatus::Blocked);
        // And when the last of them goes, nothing can speak for it any more.
        bus.detach(other, &now());
        assert_eq!(status(&bus, "abc123"), SessionStatus::Done);
    }

    #[test]
    fn two_names_for_one_daemon_relay_one_session_and_not_two() {
        let bus = Bus::new();
        let one = bus.attach();
        let other = bus.attach();
        let mut under_another_name = relayed("abc123");
        under_another_name.origin =
            vec![OriginHop::new(OriginHop::SSH, "9f3c:1000", "192.168.0.4")];

        bus.seed(one, &[relayed("abc123")], None, &now());
        bus.seed(other, &[under_another_name], None, &now());

        // One session, because a chain leads to the same place when the hops
        // have the same ids; what a hop is called is for whoever reads it.
        let sessions = bus.sessions();
        assert_eq!(sessions.len(), 1, "{sessions:?}");
        assert_eq!(sessions[0].origin[0].name, "fileserver");
    }

    #[test]
    fn letting_go_of_a_way_in_that_turned_out_to_be_another_withdraws_nothing() {
        let bus = Bus::new();
        let one = bus.attach();
        bus.seed(one, &[relayed("abc123")], None, &now());

        bus.forget(one);

        // Nothing is reporting it here any more, and it is still not this
        // daemon's place to say a session on somebody else's machine is over:
        // what was let go of is a connection, not the daemon behind it.
        assert_eq!(status(&bus, "abc123"), SessionStatus::Blocked);
    }

    #[test]
    fn a_snapshot_says_which_daemon_it_is_the_account_of() {
        let bus = Bus::new().identified(DaemonIdentity::new("9f3c1000:1000"));

        let (snapshot, _) = bus.subscribe();

        assert_eq!(snapshot.daemon, Some(DaemonIdentity::new("9f3c1000:1000")));
        // And on the wire, which is where a daemon reading another one's stream
        // finds it.
        let written = serde_json::to_string(&snapshot).expect("cannot write a snapshot");
        assert!(
            written.contains(r#""daemon":{"id":"9f3c1000:1000"}"#),
            "{written}"
        );
        // A bus nothing has told says nothing, rather than making something up.
        let (anonymous, _) = Bus::new().subscribe();
        assert_eq!(anonymous.daemon, None);
    }

    #[test]
    fn a_snapshot_reports_the_foreground_only_from_a_bus_that_is_watching() {
        let watching = watching();
        watching.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());

        let (snapshot, _) = watching.subscribe();
        assert_eq!(snapshot.foreground.as_deref().map(<[_]>::len), Some(1));
        assert_eq!(snapshot.seq, 1);

        let (snapshot, _) = Bus::new().subscribe();
        assert_eq!(snapshot.foreground, None);

        let (snapshot, _) = Bus::new().observing().subscribe();
        assert_eq!(snapshot.foreground, Some(Vec::new()));
    }

    #[test]
    fn an_event_binds_its_session_to_what_is_running_in_front_of_its_terminal() {
        let bus = watching();
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());

        bus.ingest(&spoke("abc123", Some("w9:p3")));

        assert_eq!(bus.bound_to(&key("abc123")), Some(4471));
    }

    #[test]
    fn a_process_that_ends_takes_every_session_bound_to_it_with_it() {
        let bus = watching();
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());
        bus.ingest(&spoke("abc123", Some("w9:p3")));
        bus.ingest(&spoke("sub456", Some("w9:p3")));

        bus.observed(&[withdrawn("w9:p3", 100, Some(4471))], &now());

        assert_eq!(status(&bus, "abc123"), SessionStatus::Done);
        assert_eq!(status(&bus, "sub456"), SessionStatus::Done);
        assert_eq!(bus.bound_to(&key("abc123")), None);
        assert_eq!(bus.bound_to(&key("sub456")), None);
    }

    #[test]
    fn a_session_is_reaped_without_anything_having_reported_it_over() {
        let bus = watching();
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());
        bus.ingest(&spoke("abc123", Some("w9:p3")));
        let published = bus.recent().len();

        bus.observed(&[withdrawn("w9:p3", 100, Some(4471))], &now());

        assert_eq!(status(&bus, "abc123"), SessionStatus::Done);
        assert_eq!(
            bus.recent().len(),
            published,
            "an event was invented to end the session"
        );
    }

    #[test]
    fn a_process_that_was_suspended_keeps_its_session() {
        let bus = watching();
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());
        bus.ingest(&spoke("abc123", Some("w9:p3")));

        // Suspended is a positive observation of a live process: it stops
        // holding the terminal and goes on existing.
        bus.observed(&[backgrounded("w9:p3", 100, 4471, "claude")], &now());
        bus.ingest(&spoke("abc123", Some("w9:p3")));

        assert_eq!(bus.bound_to(&key("abc123")), Some(4471));
        assert_eq!(status(&bus, "abc123"), SessionStatus::Working);
    }

    #[test]
    fn a_session_that_was_never_seen_running_is_never_reaped() {
        let bus = watching();
        bus.ingest(&spoke("abc123", Some("w9:p3")));
        assert_eq!(bus.bound_to(&key("abc123")), None);

        bus.observed(&[withdrawn("w9:p3", 100, Some(4471))], &now());

        assert_eq!(
            status(&bus, "abc123"),
            SessionStatus::Working,
            "a session nothing was ever observed for was ended anyway"
        );
    }

    #[test]
    fn a_session_that_named_no_terminal_is_never_bound() {
        let bus = watching();
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());

        bus.ingest(&spoke("abc123", None));
        bus.ingest(&spoke("empty", Some("")));

        assert_eq!(bus.bound_to(&key("abc123")), None);
        assert_eq!(bus.bound_to(&key("empty")), None);
        bus.observed(&[withdrawn("w9:p3", 100, Some(4471))], &now());
        assert_eq!(status(&bus, "abc123"), SessionStatus::Working);
        assert_eq!(status(&bus, "empty"), SessionStatus::Working);
    }

    #[test]
    fn a_session_nobody_reported_an_id_for_is_bound_and_reaped_like_any_other() {
        let bus = watching();
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());
        let mut event = tool_start();
        event["agent"] = json!(Agent::UNKNOWN);
        event["session"] = json!(observed_session_id("w9:p3"));
        event["source"] = json!("observed");
        event["correlation"] = json!("w9:p3");
        bus.ingest(&line(&event));
        let observed = SessionKey::new(Agent::UNKNOWN, observed_session_id("w9:p3"));
        assert_eq!(bus.bound_to(&observed), Some(4471));

        bus.observed(&[withdrawn("w9:p3", 100, Some(4471))], &now());

        assert_eq!(
            status(&bus, &observed_session_id("w9:p3")),
            SessionStatus::Done
        );
    }

    #[test]
    fn a_session_binds_to_whatever_is_running_when_it_last_spoke() {
        let bus = watching();
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());
        bus.ingest(&spoke("abc123", Some("w9:p3")));

        bus.observed(&[appeared("w9:p3", 100, 5512, "claude")], &now());
        bus.ingest(&spoke("abc123", Some("w9:p3")));
        assert_eq!(bus.bound_to(&key("abc123")), Some(5512));

        bus.observed(&[withdrawn("w9:p3", 100, Some(4471))], &now());
        assert_eq!(
            status(&bus, "abc123"),
            SessionStatus::Working,
            "the process the session had left ended it"
        );

        bus.observed(&[withdrawn("w9:p3", 100, Some(5512))], &now());
        assert_eq!(status(&bus, "abc123"), SessionStatus::Done);
    }

    #[test]
    fn a_terminal_two_shells_answer_for_binds_nothing() {
        let bus = watching();
        bus.observed(
            &[
                appeared("w9:p3", 100, 4471, "claude"),
                appeared("w9:p3", 200, 5512, "claude"),
            ],
            &now(),
        );

        bus.ingest(&spoke("abc123", Some("w9:p3")));

        assert_eq!(
            bus.bound_to(&key("abc123")),
            None,
            "one of two possible processes was picked"
        );
    }

    #[test]
    fn an_observation_made_somewhere_else_does_not_bind_a_session_from_here() {
        let bus = watching();
        let mut transition = appeared("w9:p3", 100, 4471, "claude");
        if let Some(entry) = transition.foreground.as_mut() {
            entry.origin = vec![OriginHop::new(OriginHop::SSH, "9f3c:1000", "fileserver")];
        }
        bus.observed(&[transition], &now());

        // The event crossed nothing to get here, so the process it names is a
        // process on this machine, and the one that was observed is not.
        bus.ingest(&spoke("abc123", Some("w9:p3")));

        assert_eq!(bus.bound_to(&key("abc123")), None);
    }

    #[test]
    fn a_bus_that_is_not_watching_binds_nothing() {
        let bus = Bus::new();

        bus.ingest(&spoke("abc123", Some("w9:p3")));
        bus.observed(&[withdrawn("w9:p3", 100, Some(4471))], &now());

        assert_eq!(bus.bound_to(&key("abc123")), None);
        assert_eq!(status(&bus, "abc123"), SessionStatus::Working);
    }

    #[test]
    fn a_session_that_had_already_finished_is_left_where_it_was() {
        let bus = watching();
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());
        let mut ended = tool_start();
        ended["kind"] = json!("session_end");
        ended["correlation"] = json!("w9:p3");
        ended["ts"] = json!("2026-08-17T10:32:01.412Z");
        bus.ingest(&line(&ended));
        let finished = bus.sessions()[0].since.clone();

        bus.observed(
            &[withdrawn("w9:p3", 100, Some(4471))],
            &at("2026-08-17T10:40:00.000Z"),
        );

        assert_eq!(status(&bus, "abc123"), SessionStatus::Done);
        assert_eq!(
            bus.sessions()[0].since,
            finished,
            "the moment the session finished moved when its terminal closed"
        );
    }

    #[test]
    fn a_session_the_table_has_forgotten_leaves_no_binding_behind() {
        let bus = watching();
        bus.observed(&[appeared("w9:p3", 100, 4471, "claude")], &now());
        // The session says it is over itself, so its binding outlives it: the
        // process it was running as is still there, running something else.
        let mut ended = tool_start();
        ended["kind"] = json!("session_end");
        ended["correlation"] = json!("w9:p3");
        ended["ts"] = json!("2026-08-17T10:32:01.412Z");
        bus.ingest(&line(&ended));
        assert_eq!(bus.bound_to(&key("abc123")), Some(4471));

        bus.tick(&at("2026-08-17T12:00:00.000Z"));

        assert!(bus.sessions().is_empty(), "the session was still reported");
        assert_eq!(bus.bound_to(&key("abc123")), None);
    }

    #[test]
    fn ticking_advances_the_sessions_clock() {
        let bus = Bus::new();
        let mut event = tool_start();
        event["ts"] = json!("2026-08-17T10:32:01.412Z");
        bus.ingest(&line(&event));
        assert_eq!(bus.sessions()[0].status, SessionStatus::Working);

        bus.tick(&Timestamp::parse("2026-08-17T11:32:01.412Z").unwrap());

        assert_eq!(bus.sessions()[0].status, SessionStatus::Stale);
    }

    /// An event as one arrives from a daemon at the far end.
    fn from_far_away(connection: Option<&str>) -> Event {
        let mut raw = tool_start();
        raw["seq"] = json!(0);
        raw["ts"] = json!(now());
        let mut event: Event = serde_json::from_value(raw).expect("that is a tool_start line");
        event.origin = vec![OriginHop::new(OriginHop::SSH, "9f3c:1000", "fileserver")];
        event.detail = connection.map(|connection| {
            Map::from_iter([(
                SSH_CONNECTION_DETAIL.to_owned(),
                Value::from(connection.to_owned()),
            )])
        });
        event
    }

    /// A local shell whose foreground process holds one connection open.
    fn holding(correlation: &str, shell: Pid, port: u32) -> Transition {
        let mut transition = appeared(correlation, shell, 4471, "ssh");
        if let Some(entry) = transition.foreground.as_mut() {
            entry.ssh_client_port = Some(port);
        }
        transition
    }

    /// What a daemon at the far end of an ssh hop reports about a shell that
    /// arrived there carrying no correlation.
    fn over(connection: &str, pid: u32) -> ForegroundEntry {
        let mut entry = ForegroundEntry::new(pid, "claude", "claude --resume", now());
        entry.state = Some(ForegroundState::Foreground);
        entry.ssh_connection = Some(connection.to_owned());
        entry.origin = vec![OriginHop::new(OriginHop::SSH, "9f3c:1000", "fileserver")];
        entry
    }

    #[test]
    fn a_shell_at_the_far_end_of_a_connection_this_one_holds_open_is_that_shells_slot() {
        let bus = watching();
        let far = bus.attach();
        bus.observed(&[holding("w1", 100, 51234)], &now());

        let line = bus.merge_foreground(
            far,
            ForegroundChange::observed(0, now(), over("10.0.0.5 51234 10.0.0.9 22", 812)),
        );

        let entry = line.foreground.expect("an observation was merged");
        assert_eq!(entry.correlation.as_deref(), Some("w1"));
        assert_eq!(
            entry.ssh_connection.as_deref(),
            Some("10.0.0.5 51234 10.0.0.9 22")
        );
        assert_eq!(
            entry.origin,
            [OriginHop::new(OriginHop::SSH, "9f3c:1000", "fileserver")]
        );
        // And it is reported that way to anybody reading this daemon.
        let observations = bus.foreground().expect("this bus is watching");
        assert_eq!(
            observations
                .iter()
                .filter(|entry| entry.correlation.as_deref() == Some("w1"))
                .count(),
            2,
            "{observations:?}"
        );
    }

    #[test]
    fn a_connection_no_shell_here_holds_open_leaves_the_far_end_uncorrelated() {
        let bus = watching();
        let far = bus.attach();
        bus.observed(&[holding("w1", 100, 51234)], &now());

        for connection in [
            // The right shape, a port nothing here has open.
            "10.0.0.5 51235 10.0.0.9 22",
            // The port is there, and it is not the field a port goes in.
            "51234 22 10.0.0.9 22",
            // Not the four fields sshd writes.
            "10.0.0.5 51234 10.0.0.9",
            "10.0.0.5 51234 10.0.0.9 22 extra",
            "51234",
            "",
        ] {
            let line = bus.merge_foreground(
                far,
                ForegroundChange::observed(0, now(), over(connection, 812)),
            );
            let entry = line.foreground.expect("an observation was merged");
            assert_eq!(entry.correlation, None, "matched on {connection:?}");
        }
    }

    #[test]
    fn a_far_end_that_says_which_shell_it_is_is_believed_over_any_port() {
        let bus = watching();
        let far = bus.attach();
        bus.observed(&[holding("w1", 100, 51234)], &now());

        let mut stated = over("10.0.0.5 51234 10.0.0.9 22", 812);
        stated.correlation = Some("said-so".to_owned());
        let line = bus.merge_foreground(far, ForegroundChange::observed(0, now(), stated));

        assert_eq!(
            line.foreground.unwrap().correlation.as_deref(),
            Some("said-so")
        );
    }

    #[test]
    fn a_shell_here_that_has_gone_leaves_the_next_observation_over_it_uncorrelated() {
        let bus = watching();
        let far = bus.attach();
        bus.observed(&[holding("w1", 100, 51234)], &now());
        let first = bus.merge_foreground(
            far,
            ForegroundChange::observed(0, now(), over("10.0.0.5 51234 10.0.0.9 22", 812)),
        );
        assert_eq!(
            first.foreground.unwrap().correlation.as_deref(),
            Some("w1"),
            "the connection was held open at the time"
        );

        // The pane closed, so nothing here holds that connection any more.
        bus.observed(&[withdrawn("w1", 100, None)], &now());
        let next = bus.merge_foreground(
            far,
            ForegroundChange::observed(0, now(), over("10.0.0.5 51234 10.0.0.9 22", 813)),
        );

        assert_eq!(next.foreground.unwrap().correlation, None);
    }

    #[test]
    fn withdrawing_a_connection_withdraws_what_it_was_correlated_as() {
        let bus = watching();
        let far = bus.attach();
        bus.observed(&[holding("w1", 100, 51234)], &now());
        bus.merge_foreground(
            far,
            ForegroundChange::observed(0, now(), over("10.0.0.5 51234 10.0.0.9 22", 812)),
        );

        let line = bus.merge_foreground(
            far,
            ForegroundChange::withdrawn_connection(0, now(), "10.0.0.5 51234 10.0.0.9 22"),
        );

        // The line is renamed on the way through, because the observations it
        // withdraws went into this daemon under the correlation put on them.
        assert_eq!(line.correlation.as_deref(), Some("w1"));
        let observations = bus.foreground().expect("this bus is watching");
        assert_eq!(
            observations.len(),
            1,
            "only this daemon's own shell should be left: {observations:?}"
        );
        assert!(observations[0].origin.is_empty());
    }

    #[test]
    fn withdrawing_a_connection_takes_what_was_correlated_before_the_shell_here_went() {
        let bus = watching();
        let far = bus.attach();
        bus.observed(&[holding("w1", 100, 51234)], &now());
        bus.merge_foreground(
            far,
            ForegroundChange::observed(0, now(), over("10.0.0.5 51234 10.0.0.9 22", 812)),
        );

        // The shell here goes, so the next observation of the far end's shell is
        // uncorrelated — and what was stamped before it stands.
        bus.observed(&[withdrawn("w1", 100, None)], &now());
        bus.merge_foreground(
            far,
            ForegroundChange::observed(0, now(), over("10.0.0.5 51234 10.0.0.9 22", 900)),
        );
        bus.merge_foreground(
            far,
            ForegroundChange::withdrawn_connection(0, now(), "10.0.0.5 51234 10.0.0.9 22"),
        );

        assert_eq!(
            bus.foreground().expect("this bus is watching"),
            Vec::new(),
            "a withdrawal takes every observation the far end made of that shell"
        );
    }

    #[test]
    fn an_event_from_a_shell_over_a_connection_this_one_holds_open_is_that_shells() {
        let bus = watching();
        let far = bus.attach();
        bus.observed(&[holding("w1", 100, 51234)], &now());

        let merged = bus.merge(far, from_far_away(Some("10.0.0.5 51234 10.0.0.9 22")));

        assert_eq!(merged.correlation.as_deref(), Some("w1"));
    }

    #[test]
    fn an_event_arriving_when_nothing_here_can_be_seen_uses_what_an_observation_settled() {
        let bus = watching();
        let far = bus.attach();
        bus.observed(&[holding("w1", 100, 51234)], &now());
        bus.merge_foreground(
            far,
            ForegroundChange::observed(0, now(), over("10.0.0.5 51234 10.0.0.9 22", 812)),
        );

        // The monitor has not looked since, and this end's own view of the
        // connection is momentarily gone; what the observation settled stands.
        bus.observed(&[withdrawn("w1", 100, None)], &now());
        let event = from_far_away(Some("10.0.0.5 51234 10.0.0.9 22"));

        assert_eq!(bus.merge(far, event).correlation.as_deref(), Some("w1"));
    }

    #[test]
    fn an_event_that_says_which_shell_it_came_from_keeps_saying_it() {
        let bus = watching();
        let far = bus.attach();
        bus.observed(&[holding("w1", 100, 51234)], &now());

        let mut event = from_far_away(Some("10.0.0.5 51234 10.0.0.9 22"));
        event.correlation = Some("said-so".to_owned());

        assert_eq!(
            bus.merge(far, event).correlation.as_deref(),
            Some("said-so")
        );
    }

    #[test]
    fn two_shells_here_holding_one_port_settle_nothing() {
        let bus = watching();
        let far = bus.attach();
        bus.observed(
            &[holding("w1", 100, 51234), holding("w2", 300, 51234)],
            &now(),
        );

        let line = bus.merge_foreground(
            far,
            ForegroundChange::observed(0, now(), over("10.0.0.5 51234 10.0.0.9 22", 812)),
        );

        assert_eq!(
            line.foreground.unwrap().correlation,
            None,
            "a correlation stamped on a guess is worse than none"
        );
    }

    #[test]
    fn a_connection_seen_through_one_far_end_is_not_a_connection_seen_through_another() {
        let bus = watching();
        let far = bus.attach();
        let elsewhere = bus.attach();
        bus.observed(&[holding("w1", 100, 51234)], &now());
        bus.merge_foreground(
            far,
            ForegroundChange::observed(0, now(), over("10.0.0.5 51234 10.0.0.9 22", 812)),
        );
        bus.observed(&[withdrawn("w1", 100, None)], &now());

        let event = from_far_away(Some("10.0.0.5 51234 10.0.0.9 22"));

        assert_eq!(
            bus.merge(elsewhere, event).correlation,
            None,
            "a port only means anything within one machine's view of it"
        );
    }
}
