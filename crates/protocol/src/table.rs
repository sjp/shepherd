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
//! `correlation` is an opaque string throughout. It is compared with `==` and
//! nothing else: never split, never prefixed, never assumed to have a shape.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::event::{Agent, Event, OriginHop, Source};
use crate::fold::{Fold, Input, SessionState};
use crate::status::SessionStatus;
use crate::stream::{SessionEntry, Snapshot};
use crate::timestamp::Timestamp;

/// How long a session is still reported after it is over.
///
/// Long enough that someone who looked away sees that their agent finished,
/// short enough that a finished session is not still cluttering the list when
/// they next start one.
pub const DEFAULT_DONE_RETENTION: Duration = Duration::from_secs(30);

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
    sessions: BTreeMap<SessionKey, TrackedSession>,
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
            sessions: BTreeMap::new(),
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

    /// The fold this table applies to every session.
    pub const fn fold(&self) -> &Fold {
        &self.fold
    }

    /// How long this table keeps reporting a session that is over.
    pub const fn done_retention(&self) -> Duration {
        self.done_retention
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
            .map(|(key, session)| SessionEntry {
                session: key.session.clone(),
                agent: key.agent.clone(),
                status: session.state.status,
                source: session.source,
                cwd: session.cwd.clone(),
                correlation: session.correlation.clone(),
                origin: session.origin.clone(),
                since: session.state.since.clone(),
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
    field.as_deref().filter(|value| !value.is_empty())
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
        UnstampedEvent::new(Agent::Claude, session, kind)
            .with_correlation(SLOT)
            .stamp(second, at(second))
    }

    /// Something watching that slot from the outside. It knows the slot and
    /// synthesizes a session id from it; it may or may not know the agent.
    fn observed(kind: Kind, second: u64) -> Event {
        observed_by(Agent::UNKNOWN, Vec::new(), kind, second)
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
            .get(&SessionKey::new(Agent::Claude, "abc123"))
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
            UnstampedEvent::new(Agent::Codex, "s", Kind::ToolStart).stamp(1, at(1)),
            hook("s", Kind::TurnEnd, 2),
        ]);
        assert_eq!(table.len(), 2);
        assert_eq!(status_of(&table, Agent::Claude, "s"), Some(Idle));
        assert_eq!(status_of(&table, Agent::Codex, "s"), Some(Working));
    }

    #[test]
    fn the_working_directory_and_the_correlation_follow_the_newest_event_to_report_one() {
        let mut table = SessionTable::new();
        let with_cwd = |cwd: Option<&str>, correlation: Option<&str>, second: u64| {
            let mut event = UnstampedEvent::new(Agent::Claude, "abc123", Kind::ToolStart);
            event.cwd = cwd.map(str::to_owned);
            event.correlation = correlation.map(str::to_owned);
            event.stamp(second, at(second))
        };

        table.apply_event(&with_cwd(Some("/srv/project"), Some(SLOT), 0));
        let known = |table: &SessionTable| {
            let session = table
                .get(&SessionKey::new(Agent::Claude, "abc123"))
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
        let key = SessionKey::new(Agent::UNKNOWN, observed_session_id(SLOT));
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
                match status_of(&table, Agent::Claude, "abc123") {
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
            UnstampedEvent::new(Agent::Codex, "def456", Kind::Blocked)
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
        let table = table_of(&[observed_by(Agent::UNKNOWN, Vec::new(), Kind::ToolStart, 0)]);
        assert_eq!(depths(&table), [0]);

        // The host can see that something is running in the slot; the daemon in
        // the container that something is a shell into can see that it is
        // claude. Both are guesses about one slot, and the inner one is better.
        let table = table_of(&[
            observed_by(Agent::UNKNOWN, Vec::new(), Kind::ToolStart, 0),
            observed_by(
                Agent::Claude,
                vec![container("a1b2c3", "eager_mclean")],
                Kind::ToolStart,
                1,
            ),
        ]);
        assert_eq!(depths(&table), [1]);
        assert_eq!(table.snapshot_sessions()[0].agent, Agent::Claude);
        assert_eq!(
            table.len(),
            2,
            "the shallower view is shadowed, not dropped"
        );

        // Nesting is ordinary: ssh to a machine that runs containers.
        let table = table_of(&[
            observed_by(Agent::UNKNOWN, vec![ssh("9f3c:1000")], Kind::ToolStart, 0),
            observed_by(
                Agent::Claude,
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
            UnstampedEvent::new(Agent::Codex, "def456", Kind::ToolStart)
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
            Agent::Claude,
            vec![container("a1b2c3", "eager_mclean")],
            Kind::ToolStart,
            0,
        );
        let renamed = observed_by(
            Agent::Claude,
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
            Agent::Claude,
            vec![container("a1b2c3", "eager_mclean")],
            Kind::ToolStart,
            0,
        ));
        let elsewhere = observed_by(
            Agent::Claude,
            vec![container("d4e5f6", "eager_mclean")],
            Kind::TurnEnd,
            1,
        );

        assert_eq!(
            table.apply_event(&elsewhere),
            Some(OriginConflict {
                key: SessionKey::new(Agent::Claude, observed_session_id(SLOT)),
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
        assert_eq!(status_of(&table, Agent::Claude, "abc123"), Some(Stale));
        assert_eq!(status_of(&table, Agent::Claude, "def456"), Some(Working));

        table.tick(&at(6));
        assert_eq!(status_of(&table, Agent::Claude, "def456"), Some(Stale));
    }

    #[test]
    fn a_dead_process_ends_the_session_it_belonged_to_and_nothing_else() {
        let mut table = table_of(&[
            hook("abc123", Kind::ToolStart, 0),
            hook("def456", Kind::ToolStart, 1),
        ]);

        // A process nobody has heard of does not conjure a session.
        table.process_gone(&SessionKey::new(Agent::Codex, "ghi789"), &at(2));
        assert_eq!(table.len(), 2);

        table.process_gone(&SessionKey::new(Agent::Claude, "abc123"), &at(2));
        assert_eq!(status_of(&table, Agent::Claude, "abc123"), Some(Done));
        assert_eq!(status_of(&table, Agent::Claude, "def456"), Some(Working));
    }

    #[test]
    fn the_order_of_a_snapshot_is_stable() {
        let table = table_of(&[
            hook("later", Kind::ToolStart, 30),
            hook("first", Kind::ToolStart, 10),
            UnstampedEvent::new(Agent::Codex, "same-moment", Kind::ToolStart).stamp(20, at(20)),
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
                (Agent::Claude, "first".to_owned()),
                // Two sessions that began in the same moment fall back to the
                // key, so the order never depends on which arrived first.
                (Agent::Claude, "same-moment".to_owned()),
                (Agent::Codex, "same-moment".to_owned()),
                (Agent::Claude, "later".to_owned()),
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
            agent: Agent::Claude,
            status,
            source: Source::Hook,
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

        assert_eq!(key, SessionKey::new(Agent::Claude, "abc123"));
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
            .get(&SessionKey::new(Agent::Claude, "abc123"))
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

        assert_eq!(status_of(&table, Agent::Claude, "abc123"), Some(Working));
        table.tick(&at(106));
        assert_eq!(status_of(&table, Agent::Claude, "abc123"), Some(Stale));
    }

    #[test]
    fn a_session_that_can_no_longer_be_spoken_for_is_over() {
        let mut table = SessionTable::new();
        table.seed(&reported_by_another(Blocked, 5), &at(100));

        table.ended(&SessionKey::new(Agent::Codex, "never-heard-of"), &at(101));
        assert_eq!(table.len(), 1);

        table.ended(&SessionKey::new(Agent::Claude, "abc123"), &at(101));
        assert_eq!(status_of(&table, Agent::Claude, "abc123"), Some(Done));
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
                .get(&SessionKey::new(Agent::Claude, "abc123"))
                .is_none()
        );
    }
}
