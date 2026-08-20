//! Every session a daemon knows about, and the snapshot it produces.
//!
//! The fold decides what one session is doing; this module holds all of them,
//! decides which of them are worth reporting, and builds the `sessions` array of
//! a [`Snapshot`]. It keeps the fold's discipline: no clock, no sockets, no
//! process table — time arrives through [`SessionTable::tick`] — so every rule
//! below is reachable from a unit test without waiting for anything.
//!
//! # Why some sessions are not reported
//!
//! One agent can be known twice over: reported by its own hook, and inferred by
//! something watching from the outside. Both are useful — the observation is
//! what covers an agent whose hooks are not installed — but putting both on
//! screen means two rows for one agent, disagreeing with each other. Three rules
//! decide which row survives, and not one of them needs to know what a
//! correlation means:
//!
//! 1. **Strict precedence, never blending.** A hook-backed session and an
//!    observed one never share a key, because an observer has no agent-reported
//!    session id and synthesizes one with [`observed_session_id`]. So there is
//!    never a state to blend: precedence is expressed by suppressing whole
//!    sessions, below.
//! 2. **A hook shadows an observation of the same slot.** An observed session is
//!    omitted while any hook-backed session carries an equal `correlation`. The
//!    hook is the agent speaking for itself; the observation is a guess about
//!    the same thing.
//! 3. **The longest origin chain wins.** Two daemons — one on the host, one
//!    inside a container — can each observe the same correlated shell. The one
//!    further down the chain is closer to what is running, so shallower
//!    observations of an equal `correlation` are omitted. This settles
//!    observations against each other; it never suppresses a hook-backed
//!    session, because one correlated slot may legitimately host two agents.
//!
//! # The one thing a watcher can still put on the screen
//!
//! Shadowing decides whole rows, and it decides them right nearly always: an
//! agent speaking for itself beats a guess about it. It has one bounded
//! exception, and the exception is about a single field. `blocked` is the state
//! this bus exists to report, and the way it goes missing is that an agent's
//! hooks never said it — while something watching that terminal can see the
//! prompt sitting there. So a live claim of `blocked` about a slot a hook is
//! already speaking for is shown on that session's snapshot entry, and nowhere
//! else:
//!
//! - **Live only.** The claim must be [`visible`](StateAssertion::visible) —
//!   the observer can see the evidence as it speaks — must have arrived after
//!   the session's last hook event, and must still be fresh within
//!   [`SessionTable::assert_hold`]. Any hook event for that correlation drops it
//!   at once, because an agent calling tools is not sitting on a prompt.
//! - **Upgrade only.** It applies to `working`, `idle` and `stale`. On `blocked`
//!   it would be saying the same thing twice, and `done` is a session that is
//!   over.
//! - **Never a rewrite.** The record goes on saying what the hooks said. The
//!   claim is put on the entry as it is built, labelled with
//!   [`status_source`](SessionEntry::status_source) so that nobody can mistake a
//!   guess for the agent's own word, and it leaves nothing behind when it goes.
//!
//! No other claim has that power. One that is not `blocked`, or not visible, is
//! a floor, and a floor is exactly what [`SessionTable::apply_assertion`] gives
//! it: a session of its own, reported whenever no hook is speaking for the slot.
//!
//! `correlation` is an opaque string throughout. It is compared with `==` and
//! nothing else: never split, never prefixed, never assumed to have a shape.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::assertion::{AssertedState, StateAssertion};
use crate::event::{Agent, Event, OriginHop, Source};
use crate::fold::{Fold, Input, SessionState, advance};
use crate::status::SessionStatus;
use crate::stream::{SessionEntry, Snapshot};
use crate::timestamp::Timestamp;

/// How long a session is still reported after it is over.
///
/// Long enough that someone who looked away sees that their agent finished,
/// short enough that a finished session is not still cluttering the list when
/// they next start one.
pub const DEFAULT_DONE_RETENTION: Duration = Duration::from_secs(30);

/// How long a claim made by an observer keeps the standing to be shown over a
/// hook-backed session's own record.
///
/// An observer that can still see what it claimed says so again every second or
/// two, so five seconds is several missed repeats: long enough to ride out one
/// that stutters, short enough that a claim nobody is making any more stops
/// being shown at about the moment it stops being true.
pub const DEFAULT_ASSERT_HOLD: Duration = Duration::from_secs(5);

/// The prefix of a session id synthesized for an observed session.
pub const OBSERVED_SESSION_PREFIX: &str = "observed:";

/// The session id to use for a session nobody reported an id for.
///
/// An observer knows which terminal slot it is watching and nothing else, so the
/// only stable identity available is the slot itself. Deriving the id from the
/// correlation keeps `(agent, session)` a real identity for observed sessions
/// too, and keeps two observations of one slot from being two sessions.
///
/// This builds a session id out of an opaque string; it does not interpret one.
pub fn observed_session_id(correlation: &str) -> String {
    format!("{OBSERVED_SESSION_PREFIX}{correlation}")
}

/// Whether a session id was synthesized by [`observed_session_id`] rather than
/// reported by an agent.
///
/// The test is on the session id — a value this protocol makes up — and not on
/// the correlation inside it.
pub fn is_observed_session_id(session: &str) -> bool {
    session.starts_with(OBSERVED_SESSION_PREFIX)
}

/// The identity of a session.
///
/// `session` alone is the agent's own id and is not unique across agents, so
/// nothing in this crate ever keys on it by itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionKey {
    /// The agent running the session.
    pub agent: Agent,
    /// The agent's own session id.
    pub session: String,
}

impl SessionKey {
    /// Builds a key.
    pub fn new(agent: impl Into<Agent>, session: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            session: session.into(),
        }
    }

    /// The key of the session an event belongs to.
    pub fn of(event: &Event) -> Self {
        Self {
            agent: event.agent.clone(),
            session: event.session.clone(),
        }
    }
}

/// One session in the table: what the fold decided, plus the descriptive fields
/// the fold has no opinion about.
///
/// Which fields move and which are fixed is the whole content of this type.
/// `cwd` and `correlation` follow the newest event that reported one, because an
/// agent can be told to work somewhere else. `source` and `origin` are fixed the
/// moment the session is first seen: they say *where this session is and who is
/// speaking for it*, and a session that changed either would be a different
/// session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedSession {
    /// What the fold makes of the events so far.
    pub state: SessionState,
    /// Whether the session is reported by its agent or inferred from outside,
    /// as of the first event seen for it.
    pub source: Source,
    /// The agent's working directory, from the most recent event that named one.
    pub cwd: Option<String>,
    /// The opaque correlation string, from the most recent event that carried
    /// one.
    pub correlation: Option<String>,
    /// The chain of hops to the daemon that folded this session, as of the first
    /// event seen for it.
    pub origin: Vec<OriginHop>,
}

/// The claim standing for one correlated slot, and when it was made.
///
/// At most one of these exists per correlation: a claim is level-triggered, so
/// the newest one is the whole of what the observer is saying and there is no
/// history to keep. It survives until it is contradicted, withdrawn, cancelled
/// by the agent's own hooks, or simply not repeated for
/// [`SessionTable::assert_hold`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldAssertion {
    /// The claimed level. Never [`AssertedState::Unknown`]: a withdrawal takes
    /// the hold away rather than being held.
    pub assert: AssertedState,
    /// Whether the observer could see the evidence as it made the latest claim.
    pub visible: bool,
    /// When the most recent claim of this state arrived. Freshness is measured
    /// from here, so repeating a claim keeps it alive.
    pub received_at: Timestamp,
    /// When this state was first claimed, unbroken since. Repeating a state that
    /// is already held does not move it — that is what makes an assertion a
    /// level and not a transition.
    pub since: Timestamp,
}

/// An event whose `origin` disagrees with the chain recorded for its session.
///
/// A session does not move: one key names one session on one machine, so a
/// second chain for it means an aggregator merged two different sessions onto
/// one key. The event is folded anyway — its status is still news — but the
/// recorded chain is left alone, and the caller is told so it can report the
/// bug. Chains that differ only in a hop's display `name` are not a conflict:
/// `name` is not identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginConflict {
    /// The session the event claimed to belong to.
    pub key: SessionKey,
    /// The chain the session was first seen with, which is kept.
    pub recorded: Vec<OriginHop>,
    /// The chain the event carried, which is discarded.
    pub rejected: Vec<OriginHop>,
}

/// Every session a daemon currently knows about.
///
/// A daemon feeds it events, ticks it, and asks it for a [`Snapshot`]; nothing
/// else. Sessions appear when their first event arrives and leave when they have
/// been over for [`SessionTable::done_retention`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTable {
    fold: Fold,
    done_retention: Duration,
    assert_hold: Duration,
    sessions: BTreeMap<SessionKey, TrackedSession>,
    /// What an observer currently claims about each correlation. Keyed by the
    /// opaque string, which is only ever compared for equality; the ordering is
    /// the map's own business and gives the table a stable shape to compare.
    holds: BTreeMap<String, HeldAssertion>,
}

impl Default for SessionTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionTable {
    /// An empty table with the default fold and [`DEFAULT_DONE_RETENTION`].
    pub const fn new() -> Self {
        Self {
            fold: Fold::new(),
            done_retention: DEFAULT_DONE_RETENTION,
            assert_hold: DEFAULT_ASSERT_HOLD,
            sessions: BTreeMap::new(),
            holds: BTreeMap::new(),
        }
    }

    /// Uses a different fold, for a daemon that wants another stale timeout.
    #[must_use]
    pub const fn with_fold(mut self, fold: Fold) -> Self {
        self.fold = fold;
        self
    }

    /// Reports finished sessions for `done_retention` before dropping them.
    #[must_use]
    pub const fn with_done_retention(mut self, done_retention: Duration) -> Self {
        self.done_retention = done_retention;
        self
    }

    /// Keeps an observer's claim standing for `assert_hold` after it is made.
    #[must_use]
    pub const fn with_assert_hold(mut self, assert_hold: Duration) -> Self {
        self.assert_hold = assert_hold;
        self
    }

    /// The fold this table applies to every session.
    pub const fn fold(&self) -> &Fold {
        &self.fold
    }

    /// How long this table keeps reporting a session that is over.
    pub const fn done_retention(&self) -> Duration {
        self.done_retention
    }

    /// How long a claim by an observer stands before it needs repeating.
    pub const fn assert_hold(&self) -> Duration {
        self.assert_hold
    }

    /// What an observer currently claims about one correlated slot, if anything.
    pub fn held_assertion(&self, correlation: &str) -> Option<&HeldAssertion> {
        self.holds.get(correlation)
    }

    /// How many sessions the table holds, reported or shadowed.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether the table holds no sessions at all.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// One session, if it is known.
    pub fn get(&self, key: &SessionKey) -> Option<&TrackedSession> {
        self.sessions.get(key)
    }

    /// Every session the table holds, in key order. Includes sessions that a
    /// snapshot would shadow.
    pub fn iter(&self) -> impl Iterator<Item = (&SessionKey, &TrackedSession)> {
        self.sessions.iter()
    }

    /// Folds one event into its session, creating the session if this is the
    /// first that has been heard of it.
    ///
    /// Returns the conflict to report when the event's `origin` disagrees with
    /// the chain the session was first seen with.
    pub fn apply_event(&mut self, event: &Event) -> Option<OriginConflict> {
        // The agent has just spoken for this slot, which settles anything an
        // observer was claiming about it: whatever it thought it could see, an
        // agent that is emitting is not sitting in front of a prompt waiting for
        // its human. Withdrawing the claim here rather than at snapshot time is
        // what makes it instant.
        if event.source == Source::Hook {
            if let Some(correlation) = reported(&event.correlation) {
                self.holds.remove(correlation);
            }
        }
        let key = SessionKey::of(event);
        let Some(session) = self.sessions.get_mut(&key) else {
            let state = self
                .fold
                .apply(None, Input::Event(event))
                .expect("an event always leaves a state");
            self.sessions.insert(
                key,
                TrackedSession {
                    state,
                    source: event.source,
                    cwd: reported(&event.cwd).map(str::to_owned),
                    correlation: reported(&event.correlation).map(str::to_owned),
                    origin: event.origin.clone(),
                },
            );
            return None;
        };

        let conflict = (!same_path(&session.origin, &event.origin)).then(|| OriginConflict {
            key,
            recorded: session.origin.clone(),
            rejected: event.origin.clone(),
        });
        if let Some(cwd) = reported(&event.cwd) {
            session.cwd = Some(cwd.to_owned());
        }
        if let Some(correlation) = reported(&event.correlation) {
            session.correlation = Some(correlation.to_owned());
        }
        session.state = self
            .fold
            .apply(Some(session.state.clone()), Input::Event(event))
            .expect("a session that exists still exists after an event");
        conflict
    }

    /// Records what an observer claims about one correlated slot.
    ///
    /// Two things happen, and they are independent. The claim becomes the one
    /// held for its correlation, where the snapshot builder may show it over a
    /// hook-backed session for as long as it stays live. And, unless it is a
    /// withdrawal, it drives an observed session of its own — the session that
    /// covers an agent whose hooks are not installed, and which is shadowed the
    /// moment one is.
    ///
    /// The claim is level-triggered, so it *sets* a status rather than moving
    /// one: repeating it changes nothing at all, `since` included, and a
    /// different state begins at the moment the claim making it arrived.
    /// `received_at` is when this table heard the claim, which is the only
    /// timing an observer's word can be trusted on — it cannot tell a state that
    /// just began from one that has been true for a minute.
    ///
    /// A claim never touches a session an agent speaks for, even when it names
    /// that session's id: an observer's word is a floor, and the record of a
    /// session reporting itself is not a floor's to rewrite. Returns the key the
    /// claim drove, or `None` where it drove no session at all.
    pub fn apply_assertion(
        &mut self,
        assertion: &StateAssertion,
        received_at: &Timestamp,
    ) -> Option<SessionKey> {
        // A claim is *about* something, and the correlation is the only thing
        // that says what. One nobody can attribute is not a weak claim, it is
        // not a claim.
        let correlation = given(&assertion.correlation)?;
        self.hold(correlation, assertion, received_at);

        let status = match assertion.assert {
            AssertedState::Working => SessionStatus::Working,
            AssertedState::Idle => SessionStatus::Idle,
            AssertedState::Blocked => SessionStatus::Blocked,
            // "I no longer know" takes back the hold above and says nothing
            // else. It is not a claim that anything changed, so the session is
            // left sitting in the last state somebody did know.
            AssertedState::Unknown => return None,
        };
        let key = SessionKey::new(
            assertion.agent.clone(),
            reported(&assertion.session)
                .map_or_else(|| observed_session_id(correlation), str::to_owned),
        );

        match self.sessions.get_mut(&key) {
            // The key belongs to an agent reporting itself, which an observer
            // that read the session id off the screen can easily land on. The
            // claim is held above, where the snapshot may show it; the record
            // it would be overwriting here is not a guess's to touch.
            Some(session) if session.source == Source::Hook => return None,
            Some(session) => {
                if let Some(cwd) = reported(&assertion.cwd) {
                    session.cwd = Some(cwd.to_owned());
                }
                session.correlation = Some(correlation.to_owned());
                session.state.source = Source::Observed;
                advance(&mut session.state.last_event, received_at);
                if session.state.status != status {
                    session.state.status = status;
                    // As in the fold: the status begins at the newest moment
                    // this table knows about, never at a straggler's.
                    let began = session.state.last_event.clone();
                    advance(&mut session.state.since, &began);
                }
            }
            None => {
                self.sessions.insert(
                    key.clone(),
                    TrackedSession {
                        state: SessionState {
                            status,
                            since: received_at.clone(),
                            last_event: received_at.clone(),
                            source: Source::Observed,
                            last_error: None,
                        },
                        source: Source::Observed,
                        cwd: reported(&assertion.cwd).map(str::to_owned),
                        correlation: Some(correlation.to_owned()),
                        origin: Vec::new(),
                    },
                );
            }
        }
        Some(key)
    }

    /// Files a claim as the one standing for its correlation.
    fn hold(&mut self, correlation: &str, assertion: &StateAssertion, received_at: &Timestamp) {
        if assertion.assert.is_withdrawal() {
            self.holds.remove(correlation);
            return;
        }
        let held = self
            .holds
            .entry(correlation.to_owned())
            .or_insert_with(|| HeldAssertion {
                assert: assertion.assert,
                visible: assertion.visible,
                received_at: received_at.clone(),
                since: received_at.clone(),
            });
        advance(&mut held.received_at, received_at);
        // Confidence is a property of the latest claim, not of the state: an
        // observer that can no longer see what it still infers says so by
        // sending the same state without `visible`, and loses its standing to be
        // shown over a hook without the state flapping.
        held.visible = assertion.visible;
        if held.assert != assertion.assert {
            held.assert = assertion.assert;
            let began = held.received_at.clone();
            advance(&mut held.since, &began);
        }
    }

    /// Moves every session's clock to `now`, then drops the ones that have been
    /// over for longer than [`SessionTable::done_retention`].
    ///
    /// Dropping is done here rather than on the way out of a snapshot so that a
    /// session which finished and started again under the same id — which the
    /// fold treats as a new life — is never dropped by a snapshot taken in
    /// between.
    pub fn tick(&mut self, now: &Timestamp) {
        for session in self.sessions.values_mut() {
            session.state = self
                .fold
                .apply(Some(session.state.clone()), Input::Tick { now })
                .expect("a session that exists still exists after a tick");
        }
        // A claim is only worth anything while somebody keeps making it, so one
        // that has not been repeated for the hold window is dropped rather than
        // left to be shown over an agent's own record indefinitely.
        let hold = i64::try_from(self.assert_hold.as_millis()).unwrap_or(i64::MAX);
        self.holds
            .retain(|_, held| now.millis_since(&held.received_at) < hold);
        let retention = i64::try_from(self.done_retention.as_millis()).unwrap_or(i64::MAX);
        self.sessions.retain(|_, session| {
            session.state.status != SessionStatus::Done
                || now.millis_since(&session.state.since) < retention
        });
    }

    /// Records what another daemon says about a session it folded, as the state
    /// of that session here.
    ///
    /// A daemon that has just reached another one is handed its current state,
    /// not the events it folded to reach it, so a session it reports is *set* to
    /// the status it reports rather than replayed. The far end has already done
    /// the folding, and a session it says has been blocked for six minutes is
    /// blocked, whatever this table would have made of the events had it seen
    /// them.
    ///
    /// The far end's `since` is kept, because that is when the status began and
    /// is the number a receiver renders. `now` is when this table heard it,
    /// which is what the quiet timer runs from: hearing it now is the freshest
    /// evidence there is that the session is still there, and timing it out
    /// from a `since` that is deliberately old would report a session lost that
    /// the daemon folding it says is working.
    ///
    /// `source` and `origin` are fixed at first sight, exactly as they are for a
    /// session learned from events. Returns the key it was filed under, so a
    /// caller merging a whole account can tell which of the sessions it used to
    /// be told about are no longer in it.
    pub fn seed(&mut self, entry: &SessionEntry, now: &Timestamp) -> SessionKey {
        let key = SessionKey::new(entry.agent.clone(), entry.session.clone());
        let session = self
            .sessions
            .entry(key.clone())
            .or_insert_with(|| TrackedSession {
                state: SessionState {
                    status: entry.status,
                    since: entry.since.clone(),
                    last_event: now.clone(),
                    source: entry.source,
                    last_error: None,
                },
                source: entry.source,
                cwd: entry.cwd.clone(),
                correlation: entry.correlation.clone(),
                origin: entry.origin.clone(),
            });
        session.state.status = entry.status;
        session.state.since = entry.since.clone();
        session.state.source = entry.source;
        if session.state.last_event < *now {
            session.state.last_event = now.clone();
        }
        if let Some(cwd) = reported(&entry.cwd) {
            session.cwd = Some(cwd.to_owned());
        }
        if let Some(correlation) = reported(&entry.correlation) {
            session.correlation = Some(correlation.to_owned());
        }
        key
    }

    /// Records that a session is over because nothing can speak for it any
    /// more. Does nothing for a session the table has never heard of: silence
    /// about something is not evidence that it existed.
    ///
    /// This is the definitive end, not a guess: whatever was running the session
    /// is beyond reach, so no further claim about it is ever going to arrive. A
    /// session that is already over keeps the moment it finished.
    pub fn ended(&mut self, key: &SessionKey, at: &Timestamp) {
        if let Some(session) = self.sessions.get_mut(key) {
            session.state = self
                .fold
                .apply(Some(session.state.clone()), Input::ProcessGone { at })
                .expect("a session that exists still exists after it ends");
        }
    }

    /// Records that the process behind a session is definitively gone, which is
    /// one way for it to be [`SessionTable::ended`] and the one a process table
    /// can prove.
    pub fn process_gone(&mut self, key: &SessionKey, at: &Timestamp) {
        self.ended(key, at);
    }

    /// The whole table as a snapshot, with no foreground observations reported.
    ///
    /// A daemon that observes processes adds them with
    /// [`Snapshot::with_foreground`]; the two arrays answer different questions
    /// and are built by different parts of a daemon.
    pub fn snapshot(&self, seq: u64) -> Snapshot {
        Snapshot::new(seq, self.snapshot_sessions())
    }

    /// The sessions worth reporting, in a stable order.
    ///
    /// Applies the two shadowing rules described at the top of this module. Both
    /// compare `correlation` with `==` and nothing else: the value is opaque,
    /// and the rules work precisely because they never need to know what it
    /// means.
    ///
    /// The order is by `since`, then by key, so that a subscriber comparing two
    /// snapshots sees only the differences that are real. `origin[i].name` is a
    /// display string and takes no part in any of it: two observations that
    /// agree on every hop's `kind` and `id` are one session however each one
    /// spells the name.
    pub fn snapshot_sessions(&self) -> Vec<SessionEntry> {
        // Correlations an agent is speaking for itself in. A session that is
        // over still holds its slot for as long as it is retained, so that the
        // moment an agent finishes is not also the moment a guess about it
        // appears in its place.
        let spoken_for: BTreeSet<&str> = self
            .sessions
            .values()
            .filter(|session| session.source == Source::Hook)
            .filter_map(|session| session.correlation.as_deref())
            .collect();

        // The deepest chain seen for each correlation, among the sessions the
        // depth rule applies to at all.
        let mut deepest: BTreeMap<(&str, Source), usize> = BTreeMap::new();
        for (key, session) in &self.sessions {
            if let Some(correlation) = settled_by_depth(key, session) {
                let depth = deepest.entry((correlation, session.source)).or_default();
                *depth = (*depth).max(session.origin.len());
            }
        }

        let mut entries: Vec<SessionEntry> = self
            .sessions
            .iter()
            .filter(|(key, session)| {
                let outranked_by_a_hook = session.source == Source::Observed
                    && session
                        .correlation
                        .as_deref()
                        .is_some_and(|correlation| spoken_for.contains(correlation));
                let outranked_by_a_deeper_view = settled_by_depth(key, session)
                    .and_then(|correlation| deepest.get(&(correlation, session.source)))
                    .is_some_and(|deepest| session.origin.len() < *deepest);
                !outranked_by_a_hook && !outranked_by_a_deeper_view
            })
            .map(|(key, session)| {
                // Nearly always nothing: an entry names a second source only
                // where something is being shown over the record rather than
                // from it.
                let shown = self.visible_blocker(session);
                SessionEntry {
                    session: key.session.clone(),
                    agent: key.agent.clone(),
                    status: shown.map_or(session.state.status, |_| SessionStatus::Blocked),
                    source: session.source,
                    status_source: shown.map(|_| Source::Observed),
                    cwd: session.cwd.clone(),
                    correlation: session.correlation.clone(),
                    origin: session.origin.clone(),
                    since: shown
                        .map_or(&session.state.since, |held| &held.since)
                        .clone(),
                }
            })
            .collect();
        entries.sort_by(|a, b| {
            a.since
                .cmp(&b.since)
                .then_with(|| a.agent.cmp(&b.agent))
                .then_with(|| a.session.cmp(&b.session))
        });
        entries
    }

    /// The claim to show over `session`'s own record, where there is one with
    /// the standing to be shown at all.
    ///
    /// This is the whole of the exception described at the top of this module,
    /// and the whole of what `status_source` ever reports. Everything it tests
    /// is a bound on it: `blocked` because that is the state worth overriding
    /// anything for, `visible` because only live evidence outranks an agent's
    /// own word, later than `last_event` because a claim older than the agent's
    /// latest news is stale evidence by definition, and a status the agent's own
    /// record leaves room to improve on.
    ///
    /// Freshness is not tested here, because this module has no clock: an
    /// expired claim is one [`SessionTable::tick`] has already dropped.
    fn visible_blocker(&self, session: &TrackedSession) -> Option<&HeldAssertion> {
        if session.source != Source::Hook
            || !matches!(
                session.state.status,
                SessionStatus::Working | SessionStatus::Idle | SessionStatus::Stale
            )
        {
            return None;
        }
        let held = self.holds.get(session.correlation.as_deref()?)?;
        (held.assert == AssertedState::Blocked
            && held.visible
            && held.received_at > session.state.last_event)
            .then_some(held)
    }
}

/// The correlation a session competes for against deeper views of the same slot,
/// or `None` for a session the depth rule leaves alone.
///
/// Only a session nobody gave an identity to is settled this way. Two
/// agent-reported sessions in one slot are two agents, and reporting one of them
/// would be hiding an agent that is really there; two views of a slot nobody
/// reported are two guesses about one thing, and the deeper guess is the better
/// one.
fn settled_by_depth<'a>(key: &SessionKey, session: &'a TrackedSession) -> Option<&'a str> {
    if session.source == Source::Observed || is_observed_session_id(&key.session) {
        session.correlation.as_deref()
    } else {
        None
    }
}

/// A field an event actually reported: present, and not the empty string. An
/// emitter with nothing to say leaves a field out, but an agent handing on an
/// unset environment variable produces an empty one, and neither is news.
fn reported(field: &Option<String>) -> Option<&str> {
    field.as_deref().and_then(given)
}

/// The same rule for a field whose type makes it mandatory: an emitter can still
/// fill one in with nothing, and nothing is not an answer.
fn given(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

/// Whether two origin chains describe the same path. `name` is display only —
/// several names may legitimately share one `id` — so it is not compared.
fn same_path(left: &[OriginHop], right: &[OriginHop]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.kind == right.kind && left.id == right.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Kind, UnstampedEvent};

    /// Builds an agent id from a literal, which is what every one of these is.
    fn agent(name: &str) -> Agent {
        Agent::new(name).expect("a test's own agent id is a valid one")
    }

    use SessionStatus::{Blocked, Done, Idle, Stale, Starting, Working};

    /// The opaque string every session in these tests is correlated to. Its
    /// shape is meaningless and nothing here may depend on it.
    const SLOT: &str = "w9:p3";

    /// A timestamp `seconds` after 2026-08-17T10:00:00Z.
    fn at(seconds: u64) -> Timestamp {
        assert!(
            seconds < 14 * 3_600,
            "the test clock does not leave the day"
        );
        Timestamp::from_parts(
            2026,
            8,
            17,
            10 + (seconds / 3_600) as u8,
            ((seconds / 60) % 60) as u8,
            (seconds % 60) as u8,
            0,
        )
        .unwrap()
    }

    /// An agent reporting itself from the slot every test works in.
    fn hook(session: &str, kind: Kind, second: u64) -> Event {
        UnstampedEvent::new(agent("claude"), session, kind)
            .with_correlation(SLOT)
            .stamp(second, at(second))
    }

    /// Something watching that slot from the outside. It knows the slot and
    /// synthesizes a session id from it; it may or may not know the agent.
    fn observed(kind: Kind, second: u64) -> Event {
        observed_by(Agent::unknown(), Vec::new(), kind, second)
    }

    /// A watcher `origin` hops away, which named the agent if it could tell.
    fn observed_by(
        agent: impl Into<Agent>,
        origin: Vec<OriginHop>,
        kind: Kind,
        second: u64,
    ) -> Event {
        UnstampedEvent::new(agent, observed_session_id(SLOT), kind)
            .with_source(Source::Observed)
            .with_correlation(SLOT)
            .with_origin(origin)
            .stamp(second, at(second))
    }

    fn container(id: &str, name: &str) -> OriginHop {
        OriginHop::new(OriginHop::CONTAINER, id, name)
    }

    fn ssh(id: &str) -> OriginHop {
        OriginHop::new(OriginHop::SSH, id, "fileserver")
    }

    fn table_of(events: &[Event]) -> SessionTable {
        let mut table = SessionTable::new();
        for event in events {
            table.apply_event(event);
        }
        table
    }

    /// The snapshot, reduced to what most tests care about.
    fn rows(table: &SessionTable) -> Vec<(String, SessionStatus, Source)> {
        table
            .snapshot_sessions()
            .into_iter()
            .map(|entry| (entry.session, entry.status, entry.source))
            .collect()
    }

    fn status_of(
        table: &SessionTable,
        agent: impl Into<Agent>,
        session: &str,
    ) -> Option<SessionStatus> {
        table
            .get(&SessionKey::new(agent, session))
            .map(|session| session.state.status)
    }

    /// Every way two streams can be interleaved without either losing its own
    /// order — which is every relative order the two can arrive in.
    fn interleavings(left: &[Event], right: &[Event]) -> Vec<Vec<Event>> {
        match (left.split_first(), right.split_first()) {
            (None, _) => vec![right.to_vec()],
            (_, None) => vec![left.to_vec()],
            (Some((first_left, rest_left)), Some((first_right, rest_right))) => {
                let mut orders = Vec::new();
                for (first, mut rest) in interleavings(rest_left, right)
                    .into_iter()
                    .map(|rest| (first_left, rest))
                    .chain(
                        interleavings(left, rest_right)
                            .into_iter()
                            .map(|rest| (first_right, rest)),
                    )
                {
                    rest.insert(0, first.clone());
                    orders.push(rest);
                }
                orders
            }
        }
    }

    /// Every ordering of a set of events, however unlikely.
    fn permutations(events: &[Event]) -> Vec<Vec<Event>> {
        if events.is_empty() {
            return vec![Vec::new()];
        }
        let mut orders = Vec::new();
        for index in 0..events.len() {
            let mut rest = events.to_vec();
            let first = rest.remove(index);
            for mut order in permutations(&rest) {
                order.insert(0, first.clone());
                orders.push(order);
            }
        }
        orders
    }

    /// A stream reduced to something readable in an assertion message.
    fn describe(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .map(|event| format!("{}/{}", event.source, event.kind))
            .collect()
    }

    #[test]
    fn a_session_is_created_by_whatever_event_arrives_first() {
        let table = table_of(&[hook("abc123", Kind::ToolStart, 4)]);
        let session = table
            .get(&SessionKey::new(agent("claude"), "abc123"))
            .expect("the event created it");
        assert_eq!(session.state.status, Working);
        assert_eq!(session.state.since, at(4));
        assert_eq!(session.source, Source::Hook);
        assert_eq!(session.correlation.as_deref(), Some(SLOT));
        assert_eq!(table.len(), 1);
        assert!(!table.is_empty());
    }

    #[test]
    fn one_agent_and_one_session_id_are_one_session_together() {
        // The same session id under two agents is two sessions; the same agent
        // twice is one.
        let table = table_of(&[
            hook("s", Kind::ToolStart, 0),
            UnstampedEvent::new(agent("codex"), "s", Kind::ToolStart).stamp(1, at(1)),
            hook("s", Kind::TurnEnd, 2),
        ]);
        assert_eq!(table.len(), 2);
        assert_eq!(status_of(&table, agent("claude"), "s"), Some(Idle));
        assert_eq!(status_of(&table, agent("codex"), "s"), Some(Working));
    }

    #[test]
    fn the_working_directory_and_the_correlation_follow_the_newest_event_to_report_one() {
        let mut table = SessionTable::new();
        let with_cwd = |cwd: Option<&str>, correlation: Option<&str>, second: u64| {
            let mut event = UnstampedEvent::new(agent("claude"), "abc123", Kind::ToolStart);
            event.cwd = cwd.map(str::to_owned);
            event.correlation = correlation.map(str::to_owned);
            event.stamp(second, at(second))
        };

        table.apply_event(&with_cwd(Some("/srv/project"), Some(SLOT), 0));
        let known = |table: &SessionTable| {
            let session = table
                .get(&SessionKey::new(agent("claude"), "abc123"))
                .unwrap();
            (session.cwd.clone(), session.correlation.clone())
        };
        assert_eq!(
            known(&table),
            (Some("/srv/project".to_owned()), Some(SLOT.to_owned()))
        );

        // An event that reports neither, and one that reports both as empty —
        // an unset environment variable handed on verbatim — are not news.
        table.apply_event(&with_cwd(None, None, 1));
        assert_eq!(
            known(&table),
            (Some("/srv/project".to_owned()), Some(SLOT.to_owned()))
        );
        table.apply_event(&with_cwd(Some(""), Some(""), 2));
        assert_eq!(
            known(&table),
            (Some("/srv/project".to_owned()), Some(SLOT.to_owned()))
        );

        table.apply_event(&with_cwd(Some("/srv/other"), Some("w9:p4"), 3));
        assert_eq!(
            known(&table),
            (Some("/srv/other".to_owned()), Some("w9:p4".to_owned()))
        );
    }

    #[test]
    fn a_sessions_source_is_the_one_it_was_first_seen_with() {
        // Contrived — an observer synthesizes its session ids precisely so that
        // this cannot happen — but the snapshot's provenance column is only
        // worth anything if it never changes under a reader.
        let mut table = SessionTable::new();
        table.apply_event(&observed(Kind::ToolStart, 0));
        let key = SessionKey::new(Agent::unknown(), observed_session_id(SLOT));
        assert_eq!(table.get(&key).unwrap().source, Source::Observed);

        let mut claimed = observed(Kind::ToolEnd, 1);
        claimed.source = Source::Hook;
        table.apply_event(&claimed);
        assert_eq!(table.get(&key).unwrap().source, Source::Observed);
        assert_eq!(table.snapshot_sessions()[0].source, Source::Observed);
    }

    #[test]
    fn an_observed_session_never_appears_beside_the_agent_it_is_guessing_at() {
        let reported = [
            hook("abc123", Kind::SessionStart, 0),
            hook("abc123", Kind::TurnStart, 2),
            hook("abc123", Kind::Blocked, 4),
        ];
        let watched = [
            observed(Kind::SessionStart, 1),
            observed(Kind::ToolStart, 3),
            observed(Kind::TurnEnd, 5),
        ];

        for order in interleavings(&reported, &watched) {
            let mut table = SessionTable::new();
            for (applied, event) in order.iter().enumerate() {
                table.apply_event(event);
                let entries = table.snapshot_sessions();
                let so_far = describe(&order[..=applied]);

                assert!(
                    entries.len() <= 1,
                    "one agent in one slot is one row, not {}: {so_far:?}",
                    entries.len()
                );
                match status_of(&table, agent("claude"), "abc123") {
                    // The agent is speaking for itself: that is the row, and it
                    // says what the agent says, never what the watcher inferred.
                    Some(status) => {
                        assert_eq!(entries[0].source, Source::Hook, "{so_far:?}");
                        assert_eq!(entries[0].session, "abc123", "{so_far:?}");
                        assert_eq!(entries[0].status, status, "{so_far:?}");
                    }
                    // Nothing has been heard from the agent yet, so the guess is
                    // all there is and it is better than an empty screen.
                    None => assert_eq!(entries[0].source, Source::Observed, "{so_far:?}"),
                }
            }
        }
    }

    #[test]
    fn no_ordering_at_all_puts_a_guess_beside_a_report() {
        let events = [
            hook("abc123", Kind::SessionStart, 0),
            hook("abc123", Kind::Blocked, 2),
            hook("abc123", Kind::SessionEnd, 4),
            observed(Kind::SessionStart, 1),
            observed(Kind::ToolStart, 3),
            observed(Kind::TurnEnd, 5),
        ];

        for order in permutations(&events) {
            let mut table = SessionTable::new();
            for (applied, event) in order.iter().enumerate() {
                table.apply_event(event);
                let entries = table.snapshot_sessions();
                let reported = entries.iter().filter(|e| e.source == Source::Hook).count();
                let guessed = entries.len() - reported;
                assert!(
                    reported == 0 || guessed == 0,
                    "{reported} reported and {guessed} guessed rows after {:?}",
                    describe(&order[..=applied])
                );
            }
        }
    }

    #[test]
    fn a_finished_agent_keeps_its_slot_for_as_long_as_it_is_reported() {
        // Otherwise the moment an agent finished would also be the moment a
        // guess about it appeared in its place, for the half minute the
        // completion is on screen.
        let mut table = table_of(&[
            hook("abc123", Kind::SessionStart, 0),
            observed(Kind::ToolStart, 1),
            hook("abc123", Kind::SessionEnd, 10),
        ]);
        let finished = ("abc123".to_owned(), Done, Source::Hook);
        let guessed = (observed_session_id(SLOT), Working, Source::Observed);

        assert_eq!(rows(&table), std::slice::from_ref(&finished));
        table.tick(&at(39));
        assert_eq!(rows(&table), [finished]);

        // Once it is dropped there is nothing left to shadow the watcher, which
        // is still watching a slot that may well still have something in it.
        table.tick(&at(40));
        assert_eq!(rows(&table), [guessed]);
    }

    #[test]
    fn two_agents_in_one_slot_are_two_rows() {
        // Shadowing is a hook outranking a guess about the same slot, and
        // nothing else: an agent that is really running is never hidden.
        let table = table_of(&[
            hook("abc123", Kind::ToolStart, 0),
            UnstampedEvent::new(agent("codex"), "def456", Kind::Blocked)
                .with_correlation(SLOT)
                .with_origin(vec![container("a1b2c3", "eager_mclean")])
                .stamp(1, at(1)),
        ]);
        assert_eq!(
            rows(&table),
            [
                ("abc123".to_owned(), Working, Source::Hook),
                ("def456".to_owned(), Blocked, Source::Hook),
            ]
        );
    }

    #[test]
    fn the_deeper_of_two_views_of_one_slot_is_the_one_reported() {
        let depths = |table: &SessionTable| -> Vec<usize> {
            table
                .snapshot_sessions()
                .iter()
                .map(|entry| entry.origin.len())
                .collect()
        };

        // One view of the slot, from the machine it is on.
        let table = table_of(&[observed_by(
            Agent::unknown(),
            Vec::new(),
            Kind::ToolStart,
            0,
        )]);
        assert_eq!(depths(&table), [0]);

        // The host can see that something is running in the slot; the daemon in
        // the container that something is a shell into can see that it is
        // claude. Both are guesses about one slot, and the inner one is better.
        let table = table_of(&[
            observed_by(Agent::unknown(), Vec::new(), Kind::ToolStart, 0),
            observed_by(
                agent("claude"),
                vec![container("a1b2c3", "eager_mclean")],
                Kind::ToolStart,
                1,
            ),
        ]);
        assert_eq!(depths(&table), [1]);
        assert_eq!(table.snapshot_sessions()[0].agent, agent("claude"));
        assert_eq!(
            table.len(),
            2,
            "the shallower view is shadowed, not dropped"
        );

        // Nesting is ordinary: ssh to a machine that runs containers.
        let table = table_of(&[
            observed_by(Agent::unknown(), vec![ssh("9f3c:1000")], Kind::ToolStart, 0),
            observed_by(
                agent("claude"),
                vec![ssh("9f3c:1000"), container("a1b2c3", "eager_mclean")],
                Kind::ToolStart,
                1,
            ),
        ]);
        assert_eq!(depths(&table), [2]);
    }

    #[test]
    fn depth_settles_guesses_and_leaves_agents_that_reported_themselves_alone() {
        // A hook event carries no origin of its own until an aggregator stamps
        // one, and two agents at different depths in one slot are still two
        // agents.
        let table = table_of(&[
            hook("abc123", Kind::ToolStart, 0),
            UnstampedEvent::new(agent("codex"), "def456", Kind::ToolStart)
                .with_correlation(SLOT)
                .with_origin(vec![container("a1b2c3", "eager_mclean")])
                .stamp(1, at(1)),
        ]);
        assert_eq!(table.snapshot_sessions().len(), 2);
    }

    #[test]
    fn two_views_that_differ_only_in_a_display_name_are_one_session() {
        // Three ssh aliases for one host share an id and may each spell the name
        // differently; a name that changed is not a session that moved.
        let mut table = SessionTable::new();
        let first = observed_by(
            agent("claude"),
            vec![container("a1b2c3", "eager_mclean")],
            Kind::ToolStart,
            0,
        );
        let renamed = observed_by(
            agent("claude"),
            vec![container("a1b2c3", "brave_hopper")],
            Kind::ToolEnd,
            1,
        );

        assert!(table.apply_event(&first).is_none());
        assert!(
            table.apply_event(&renamed).is_none(),
            "a display name is not a conflict"
        );

        let entries = table.snapshot_sessions();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, vec![container("a1b2c3", "eager_mclean")]);
    }

    #[test]
    fn a_second_origin_for_one_session_is_reported_rather_than_merged() {
        let mut table = SessionTable::new();
        table.apply_event(&observed_by(
            agent("claude"),
            vec![container("a1b2c3", "eager_mclean")],
            Kind::ToolStart,
            0,
        ));
        let elsewhere = observed_by(
            agent("claude"),
            vec![container("d4e5f6", "eager_mclean")],
            Kind::TurnEnd,
            1,
        );

        assert_eq!(
            table.apply_event(&elsewhere),
            Some(OriginConflict {
                key: SessionKey::new(agent("claude"), observed_session_id(SLOT)),
                recorded: vec![container("a1b2c3", "eager_mclean")],
                rejected: vec![container("d4e5f6", "eager_mclean")],
            })
        );

        // The event still says what the session is doing, which is worth having
        // whoever it really came from.
        let entries = table.snapshot_sessions();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, Idle);
        assert_eq!(entries[0].origin, vec![container("a1b2c3", "eager_mclean")]);
    }

    #[test]
    fn a_finished_session_is_reported_for_half_a_minute_of_ticks_and_then_dropped() {
        let mut table = table_of(&[
            hook("abc123", Kind::SessionStart, 0),
            hook("abc123", Kind::SessionEnd, 10),
        ]);
        assert_eq!(
            SessionTable::new().done_retention(),
            Duration::from_secs(30)
        );

        for second in [10, 11, 25, 39] {
            table.tick(&at(second));
            assert_eq!(
                rows(&table),
                [("abc123".to_owned(), Done, Source::Hook)],
                "{second}s"
            );
        }

        table.tick(&at(40));
        assert!(table.is_empty());
        assert!(table.snapshot_sessions().is_empty());
    }

    #[test]
    fn a_session_that_starts_again_while_it_is_still_reported_is_revived() {
        let mut table = table_of(&[
            hook("abc123", Kind::SessionStart, 0),
            hook("abc123", Kind::SessionEnd, 10),
        ]);
        table.tick(&at(15));
        table.apply_event(&hook("abc123", Kind::SessionStart, 20));
        assert_eq!(
            rows(&table),
            [("abc123".to_owned(), Starting, Source::Hook)]
        );

        // The retention it was serving is not still counting down underneath it.
        table.tick(&at(45));
        assert_eq!(
            rows(&table),
            [("abc123".to_owned(), Starting, Source::Hook)]
        );
    }

    #[test]
    fn how_long_a_finished_session_is_reported_is_a_parameter() {
        let mut table = SessionTable::new().with_done_retention(Duration::from_secs(5));
        assert_eq!(table.done_retention(), Duration::from_secs(5));
        table.apply_event(&hook("abc123", Kind::SessionEnd, 10));

        table.tick(&at(14));
        assert_eq!(table.len(), 1);
        table.tick(&at(15));
        assert!(table.is_empty());
    }

    #[test]
    fn a_tick_reaches_every_session_and_the_fold_is_a_parameter() {
        let mut table =
            SessionTable::new().with_fold(Fold::with_stale_after(Duration::from_secs(5)));
        assert_eq!(table.fold().stale_after(), Duration::from_secs(5));
        assert_eq!(SessionTable::new().fold(), &Fold::new());

        table.apply_event(&hook("abc123", Kind::ToolStart, 0));
        table.apply_event(&hook("def456", Kind::ToolStart, 1));
        table.tick(&at(5));
        assert_eq!(status_of(&table, agent("claude"), "abc123"), Some(Stale));
        assert_eq!(status_of(&table, agent("claude"), "def456"), Some(Working));

        table.tick(&at(6));
        assert_eq!(status_of(&table, agent("claude"), "def456"), Some(Stale));
    }

    #[test]
    fn a_dead_process_ends_the_session_it_belonged_to_and_nothing_else() {
        let mut table = table_of(&[
            hook("abc123", Kind::ToolStart, 0),
            hook("def456", Kind::ToolStart, 1),
        ]);

        // A process nobody has heard of does not conjure a session.
        table.process_gone(&SessionKey::new(agent("codex"), "ghi789"), &at(2));
        assert_eq!(table.len(), 2);

        table.process_gone(&SessionKey::new(agent("claude"), "abc123"), &at(2));
        assert_eq!(status_of(&table, agent("claude"), "abc123"), Some(Done));
        assert_eq!(status_of(&table, agent("claude"), "def456"), Some(Working));
    }

    #[test]
    fn the_order_of_a_snapshot_is_stable() {
        let table = table_of(&[
            hook("later", Kind::ToolStart, 30),
            hook("first", Kind::ToolStart, 10),
            UnstampedEvent::new(agent("codex"), "same-moment", Kind::ToolStart).stamp(20, at(20)),
            hook("same-moment", Kind::ToolStart, 20),
        ]);

        let sessions: Vec<(Agent, String)> = table
            .snapshot_sessions()
            .into_iter()
            .map(|entry| (entry.agent, entry.session))
            .collect();
        assert_eq!(
            sessions,
            [
                (agent("claude"), "first".to_owned()),
                // Two sessions that began in the same moment fall back to the
                // key, so the order never depends on which arrived first.
                (agent("claude"), "same-moment".to_owned()),
                (agent("codex"), "same-moment".to_owned()),
                (agent("claude"), "later".to_owned()),
            ]
        );
        assert_eq!(table.snapshot_sessions(), table.snapshot_sessions());
    }

    #[test]
    fn a_snapshot_reports_the_sessions_and_leaves_the_foreground_to_its_owner() {
        let table = table_of(&[hook("abc123", Kind::ToolStart, 0)]);
        let snapshot = table.snapshot(1041);

        assert_eq!(snapshot.v, crate::VERSION);
        assert_eq!(snapshot.seq, 1041);
        assert_eq!(snapshot.sessions, table.snapshot_sessions());
        assert_eq!(snapshot.foreground, None, "this table observes nothing");
        assert_eq!(snapshot.sessions[0].cwd, None);
        assert_eq!(snapshot.sessions[0].correlation.as_deref(), Some(SLOT));
    }

    #[test]
    fn an_observed_session_is_identified_by_the_slot_it_was_seen_in() {
        assert_eq!(observed_session_id("w9:p3"), "observed:w9:p3");
        assert_eq!(observed_session_id(""), OBSERVED_SESSION_PREFIX);
        assert!(is_observed_session_id("observed:w9:p3"));
        assert!(is_observed_session_id(&observed_session_id(
            "anything at all"
        )));
        assert!(!is_observed_session_id("abc123"));
        assert!(!is_observed_session_id("w9:p3"));
    }

    /// What another daemon reports about one of its sessions.
    fn reported_by_another(status: SessionStatus, second: u64) -> SessionEntry {
        SessionEntry {
            session: "abc123".to_owned(),
            agent: agent("claude"),
            status,
            source: Source::Hook,
            status_source: None,
            cwd: Some("/srv/project".to_owned()),
            correlation: Some(SLOT.to_owned()),
            origin: vec![container("9f3c", "build")],
            since: at(second),
        }
    }

    #[test]
    fn a_seeded_session_is_set_to_the_status_it_was_reported_with() {
        let mut table = SessionTable::new();

        let key = table.seed(&reported_by_another(Blocked, 5), &at(100));

        assert_eq!(key, SessionKey::new(agent("claude"), "abc123"));
        let session = table.get(&key).expect("the session was not seeded");
        assert_eq!(session.state.status, Blocked);
        // The far end's own reckoning of how long it has been blocked survives,
        // which is the whole reason for seeding rather than replaying.
        assert_eq!(session.state.since, at(5));
        assert_eq!(session.source, Source::Hook);
        assert_eq!(session.cwd.as_deref(), Some("/srv/project"));
        assert_eq!(session.correlation.as_deref(), Some(SLOT));
        assert_eq!(session.origin, vec![container("9f3c", "build")]);

        let entry = &table.snapshot_sessions()[0];
        assert_eq!(entry.status, Blocked);
        assert_eq!(entry.since, at(5));
    }

    #[test]
    fn seeding_again_updates_the_session_rather_than_making_another_one() {
        let mut table = SessionTable::new();
        table.seed(&reported_by_another(Blocked, 5), &at(100));

        table.seed(&reported_by_another(Working, 110), &at(120));

        assert_eq!(table.len(), 1);
        let session = table
            .get(&SessionKey::new(agent("claude"), "abc123"))
            .expect("the session went missing");
        assert_eq!(session.state.status, Working);
        assert_eq!(session.state.since, at(110));
    }

    #[test]
    fn a_seeded_session_is_not_timed_out_for_the_silence_before_it_was_heard_of() {
        let mut table =
            SessionTable::new().with_fold(Fold::with_stale_after(Duration::from_secs(5)));

        // Reported as working since long ago, and reported *now*: the far end is
        // folding it and has just said so, so this end has no business calling
        // it lost.
        table.seed(&reported_by_another(Working, 0), &at(100));
        table.tick(&at(101));

        assert_eq!(status_of(&table, agent("claude"), "abc123"), Some(Working));
        table.tick(&at(106));
        assert_eq!(status_of(&table, agent("claude"), "abc123"), Some(Stale));
    }

    #[test]
    fn a_session_that_can_no_longer_be_spoken_for_is_over() {
        let mut table = SessionTable::new();
        table.seed(&reported_by_another(Blocked, 5), &at(100));

        table.ended(&SessionKey::new(agent("codex"), "never-heard-of"), &at(101));
        assert_eq!(table.len(), 1);

        table.ended(&SessionKey::new(agent("claude"), "abc123"), &at(101));
        assert_eq!(status_of(&table, agent("claude"), "abc123"), Some(Done));
        // And it leaves on the ordinary retention, like any finished session.
        table.tick(&at(140));
        assert!(table.is_empty());
    }

    #[test]
    fn an_empty_table_is_an_empty_snapshot() {
        let table = SessionTable::new();
        assert_eq!(table, SessionTable::default());
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert_eq!(table.iter().count(), 0);
        assert!(table.snapshot_sessions().is_empty());
        assert!(
            table
                .get(&SessionKey::new(agent("claude"), "abc123"))
                .is_none()
        );
    }

    /// An observer's claim about the slot every test works in.
    fn claim(state: AssertedState) -> StateAssertion {
        StateAssertion::new(agent("claude"), SLOT, state)
    }

    /// The one that can be shown over an agent's own record, and the only one.
    fn live_block() -> StateAssertion {
        claim(AssertedState::Blocked).with_visible(true)
    }

    /// When the agent in the truth table below last said anything for itself.
    const LAST_EVENT: u64 = 200;

    /// A hook-backed session sitting in `status`, last heard from at
    /// [`LAST_EVENT`].
    ///
    /// Seeding is how a status is chosen outright: the fold reaches `stale`
    /// only by two minutes of silence, and a truth table that spent them would
    /// be a truth table nobody runs.
    fn hook_session_in(status: SessionStatus) -> SessionTable {
        let mut table = SessionTable::new();
        table.seed(
            &SessionEntry {
                session: "abc123".to_owned(),
                agent: agent("claude"),
                status,
                source: Source::Hook,
                status_source: None,
                cwd: None,
                correlation: Some(SLOT.to_owned()),
                origin: Vec::new(),
                since: at(LAST_EVENT),
            },
            &at(LAST_EVENT),
        );
        table
    }

    /// The single row a snapshot of a one-slot table has.
    fn only_row(table: &SessionTable) -> SessionEntry {
        let mut entries = table.snapshot_sessions();
        assert_eq!(entries.len(), 1, "one slot is one row: {entries:?}");
        entries.remove(0)
    }

    /// What an agent's own record says, which no claim may ever change.
    fn record(table: &SessionTable) -> (SessionStatus, Timestamp) {
        let session = table
            .get(&SessionKey::new(agent("claude"), "abc123"))
            .expect("the session went missing");
        (session.state.status, session.state.since.clone())
    }

    #[test]
    fn a_claim_drives_a_session_of_its_own_where_nothing_else_speaks_for_the_slot() {
        let mut table = SessionTable::new();

        let key = table
            .apply_assertion(&claim(AssertedState::Working), &at(10))
            .expect("a claim about an unspoken-for slot is a session");
        assert_eq!(
            key,
            SessionKey::new(agent("claude"), observed_session_id(SLOT))
        );
        assert_eq!(
            rows(&table),
            vec![(observed_session_id(SLOT), Working, Source::Observed)]
        );
        let since = |table: &SessionTable| {
            table
                .get(&key)
                .expect("it went missing")
                .state
                .since
                .clone()
        };
        assert_eq!(since(&table), at(10));
        // A claim is a level, not a transition: making it again is making it
        // once, and the state has been true since it was first claimed.
        table.apply_assertion(&claim(AssertedState::Working), &at(11));
        assert_eq!(table.len(), 1);
        assert_eq!(since(&table), at(10));

        // A different state does begin when it is claimed.
        table.apply_assertion(&claim(AssertedState::Blocked), &at(12));
        assert_eq!(
            rows(&table),
            vec![(observed_session_id(SLOT), Blocked, Source::Observed)]
        );
        assert_eq!(since(&table), at(12));
        // Nothing about a session anybody is guessing at claims to be the
        // agent's own word.
        assert_eq!(only_row(&table).status_source, None);

        // "I no longer know" takes back the claim without pretending to know
        // something else instead.
        assert_eq!(
            table.apply_assertion(&claim(AssertedState::Unknown), &at(13)),
            None
        );
        assert_eq!(table.held_assertion(SLOT), None);
        assert_eq!(
            rows(&table),
            vec![(observed_session_id(SLOT), Blocked, Source::Observed)]
        );
        assert_eq!(since(&table), at(12));

        // And it leaves the way an observed session always has.
        table.process_gone(&key, &at(14));
        assert_eq!(
            rows(&table),
            vec![(observed_session_id(SLOT), Done, Source::Observed)]
        );
    }

    #[test]
    fn a_session_a_claim_drives_is_not_timed_out_for_going_quiet() {
        // Silence from an observer is not silence from the agent: an agent
        // waiting on its human changes nothing on screen for as long as it
        // waits, so there is nothing for the observer to say.
        let mut table =
            SessionTable::new().with_fold(Fold::with_stale_after(Duration::from_secs(5)));
        let key = table
            .apply_assertion(&claim(AssertedState::Working), &at(10))
            .expect("a claim is a session");

        table.tick(&at(100));

        let session = table.get(&key).expect("it went missing");
        assert_eq!(session.state.status, Working);
        assert_eq!(session.state.since, at(10));
        // The claim itself is long expired, which is a different question.
        assert_eq!(table.held_assertion(SLOT), None);
    }

    #[test]
    fn a_claim_about_nothing_in_particular_is_not_a_claim() {
        let mut table = SessionTable::new();

        let unattributable = StateAssertion::new(agent("claude"), "", AssertedState::Blocked);
        assert_eq!(table.apply_assertion(&unattributable, &at(10)), None);

        assert!(table.is_empty());
        assert_eq!(table.held_assertion(""), None);
    }

    #[test]
    fn only_a_live_visible_block_is_shown_over_what_an_agent_says_about_itself() {
        // Every claim that can be made about a slot an agent is speaking for,
        // in every state that agent can be in, made both before and after its
        // last word and read both inside and outside the hold window. Exactly
        // three of the combinations are the exception; the rest are the rule.
        const BEFORE: u64 = LAST_EVENT - 1;
        const AFTER: u64 = LAST_EVENT + 10;

        for status in [Starting, Working, Blocked, Idle, Stale, Done] {
            for state in AssertedState::ALL {
                for visible in [false, true] {
                    for received in [BEFORE, AFTER] {
                        for fresh in [true, false] {
                            let mut table = hook_session_in(status);
                            table.apply_assertion(
                                &claim(state).with_visible(visible),
                                &at(received),
                            );
                            // Both ticks are close enough to the agent's last
                            // word to leave the fold's own reckoning alone; they
                            // differ only in whether the claim has been left
                            // unrepeated for longer than the hold window.
                            table.tick(&at(received + if fresh { 1 } else { 5 }));

                            let upgraded = state == AssertedState::Blocked
                                && visible
                                && fresh
                                && received == AFTER
                                && matches!(status, Working | Idle | Stale);
                            let case = format!(
                                "{status} agent, claimed {state}, visible {visible}, \
                                 received at {received}, fresh {fresh}"
                            );

                            let entry = only_row(&table);
                            assert_eq!(entry.session, "abc123", "{case}");
                            assert_eq!(entry.source, Source::Hook, "{case}");
                            if upgraded {
                                assert_eq!(entry.status, Blocked, "{case}");
                                assert_eq!(entry.status_source, Some(Source::Observed), "{case}");
                                assert_eq!(entry.since, at(received), "{case}");
                            } else {
                                assert_eq!(entry.status, status, "{case}");
                                assert_eq!(entry.status_source, None, "{case}");
                                assert_eq!(entry.since, at(LAST_EVENT), "{case}");
                            }
                            // Shown or not, the agent's own record is exactly
                            // what its hooks left behind.
                            assert_eq!(record(&table), (status, at(LAST_EVENT)), "{case}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_slot_a_claim_is_shown_on_is_still_one_row() {
        let mut table = table_of(&[hook("abc123", Kind::ToolStart, LAST_EVENT)]);
        table.apply_assertion(&live_block(), &at(210));

        // Two sessions are known — the agent's, and the one the observer's
        // claim drives — and one of them is reported.
        assert_eq!(table.len(), 2);
        assert_eq!(
            rows(&table),
            vec![("abc123".to_owned(), Blocked, Source::Hook)]
        );
        let entry = only_row(&table);
        assert_eq!(entry.status_source, Some(Source::Observed));
        assert_eq!(entry.since, at(210));
        assert_eq!(record(&table), (Working, at(LAST_EVENT)));
    }

    #[test]
    fn an_agents_own_word_takes_back_a_claim_about_its_slot_at_once() {
        let mut table = table_of(&[hook("abc123", Kind::ToolStart, LAST_EVENT)]);
        table.apply_assertion(&live_block(), &at(210));
        assert_eq!(only_row(&table).status, Blocked);

        // It is calling a tool, so it is not sitting in front of a prompt
        // waiting for its human, whatever the observer thought it could see.
        table.apply_event(&hook("abc123", Kind::ToolStart, 211));

        assert_eq!(table.held_assertion(SLOT), None);
        let entry = only_row(&table);
        assert_eq!(entry.status, Working);
        assert_eq!(entry.status_source, None);
        assert_eq!(entry.since, at(LAST_EVENT));
    }

    #[test]
    fn a_claim_stops_being_shown_when_nobody_repeats_it_and_leaves_nothing_behind() {
        let mut table = table_of(&[hook("abc123", Kind::ToolStart, LAST_EVENT)]);
        table.apply_assertion(&live_block(), &at(210));

        // Repeating it keeps it alive without restarting it: the agent has been
        // blocked since it was first seen to be, not since it was last looked at.
        table.tick(&at(212));
        table.apply_assertion(&live_block(), &at(213));
        table.tick(&at(215));
        let entry = only_row(&table);
        assert_eq!(entry.status, Blocked);
        assert_eq!(entry.since, at(210));

        // Then the observer stops — it was killed, or it lost sight of the
        // terminal, and either way nothing is speaking for that claim now.
        table.tick(&at(218));

        assert_eq!(table.held_assertion(SLOT), None);
        let entry = only_row(&table);
        assert_eq!(entry.status, Working);
        assert_eq!(entry.status_source, None);
        assert_eq!(entry.since, at(LAST_EVENT));
    }

    #[test]
    fn a_withdrawal_stops_a_claim_being_shown_as_surely_as_expiry_does() {
        let mut table = table_of(&[hook("abc123", Kind::ToolStart, LAST_EVENT)]);
        table.apply_assertion(&live_block(), &at(210));

        table.apply_assertion(&claim(AssertedState::Unknown), &at(211));

        assert_eq!(table.held_assertion(SLOT), None);
        let entry = only_row(&table);
        assert_eq!(entry.status, Working);
        assert_eq!(entry.status_source, None);
    }

    #[test]
    fn a_claim_never_rewrites_the_record_of_a_session_speaking_for_itself() {
        let mut table = table_of(&[hook("abc123", Kind::ToolStart, LAST_EVENT)]);

        // An observer that can read the agent's own session id off the screen
        // may well name it. That makes the claim easier to attribute; it does
        // not promote a guess to the agent's own word.
        let named = live_block().with_session("abc123").with_cwd("/srv/guess");
        assert_eq!(table.apply_assertion(&named, &at(210)), None);

        assert_eq!(table.len(), 1);
        let session = table
            .get(&SessionKey::new(agent("claude"), "abc123"))
            .expect("the session went missing");
        assert_eq!(session.state.status, Working);
        assert_eq!(session.state.since, at(LAST_EVENT));
        assert_eq!(session.source, Source::Hook);
        assert_eq!(session.cwd, None);
        // It is still held, and still shown where a claim is allowed to show.
        let entry = only_row(&table);
        assert_eq!(entry.status, Blocked);
        assert_eq!(entry.status_source, Some(Source::Observed));
        assert_eq!(entry.since, at(210));
    }

    #[test]
    fn a_claim_that_arrives_late_moves_no_clock_backwards() {
        let mut table = SessionTable::new();
        table.apply_assertion(&live_block(), &at(210));

        // A straggler, claiming something else, stamped before what is already
        // held. It is still news about the state; it is not news about when.
        let key = table
            .apply_assertion(&claim(AssertedState::Working), &at(205))
            .expect("a claim is a session");

        let held = table
            .held_assertion(SLOT)
            .expect("something is still claimed");
        assert_eq!(held.assert, AssertedState::Working);
        assert!(!held.visible);
        assert_eq!(held.received_at, at(210));
        assert_eq!(held.since, at(210));
        let session = table.get(&key).expect("it went missing");
        assert_eq!(session.state.status, Working);
        assert_eq!(session.state.since, at(210));
        assert_eq!(session.state.last_event, at(210));
    }

    #[test]
    fn how_long_a_claim_stands_unrepeated_is_the_tables_to_choose() {
        assert_eq!(SessionTable::new().assert_hold(), DEFAULT_ASSERT_HOLD);

        let mut table = SessionTable::new().with_assert_hold(Duration::from_secs(60));
        assert_eq!(table.assert_hold(), Duration::from_secs(60));
        table.apply_assertion(&live_block(), &at(10));

        // Long past the default, and well inside this one.
        table.tick(&at(60));
        assert!(table.held_assertion(SLOT).is_some());

        table.tick(&at(70));
        assert_eq!(table.held_assertion(SLOT), None);
    }
}
