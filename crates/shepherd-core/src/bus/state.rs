//! What the bus has said, kept the way the bus keeps it.
//!
//! A subscriber that only forwarded lines would leave every consumer to work
//! out for itself what a `session_end` does to a status, which of two views of
//! one slot to believe, and when a session that has gone quiet becomes stale.
//! Those rules are the protocol's, they are subtle, and they are already
//! written: [`SessionTable`] is the same fold the daemon runs, so feeding it the
//! same lines gives the same answers. This holds one of those, plus the
//! foreground observations, and applies each [`Update`] to whichever of them it
//! is about.
//!
//! # Time arrives, it is not read
//!
//! Every method that needs to know the time is told it. A session goes stale
//! because nothing has been heard from it for a while, and a finished one is
//! forgotten a while after that, so something has to move the clock forward:
//! that is [`BusState::tick`], and a caller that never calls it has a view where
//! nothing ever goes quiet. Keeping the clock outside is what lets all of it be
//! tested without waiting for anything, and it is the discipline the fold itself
//! is written to.
//!
//! # One thing a snapshot cannot carry
//!
//! A live claim by an observer can be shown over a hook-backed session's own
//! record — that is how an agent whose hooks never said `blocked` still shows as
//! blocked while something can see the prompt. What travels in a snapshot is the
//! *result*: the entry's status, labelled with `status_source` to say the claim
//! is not the agent's own word. The claim behind it does not travel, so a
//! subscriber seeding from a snapshot adopts that status as the session's own
//! and keeps it until the next line about that session arrives. It is a fresh
//! connection's worth of fidelity, and the alternative — dropping the status a
//! snapshot went to the trouble of computing — would be worse.

use std::collections::BTreeMap;

use agentbus_protocol::{
    DaemonIdentity, ForegroundChange, ForegroundEntry, SessionEntry, SessionTable, Snapshot,
    StampedAssertion, StateAssertion, Timestamp,
};
use tracing::{debug, trace};

use super::subscriber::Update;

/// Everything the bus has said, folded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BusState {
    sessions: SessionTable,
    /// What is running in each correlated slot, keyed by the opaque string it is
    /// filed under. `None` where the bus reported no observations at all, which
    /// is not the same as reporting none: nobody is looking, rather than nobody
    /// is running anything.
    foreground: Option<BTreeMap<String, ForegroundEntry>>,
    daemon: Option<DaemonIdentity>,
    connected: bool,
}

impl BusState {
    /// A view that has heard nothing.
    pub fn new() -> Self {
        Self {
            sessions: SessionTable::new(),
            foreground: None,
            daemon: None,
            connected: false,
        }
    }

    /// Folds one update in, as of `now`.
    ///
    /// `now` is when this heard it, which is what a session's quiet timer runs
    /// from. It is deliberately not the timestamp on the line: a stream that has
    /// just been reconnected to may hand over a session whose last event was
    /// minutes ago, and that session is not stale — it is one this has only just
    /// been told about.
    pub fn apply(&mut self, update: &Update, now: &Timestamp) {
        match update {
            Update::Reset(snapshot) => self.reset(snapshot, now),
            Update::Event(event) => {
                if let Some(conflict) = self.sessions.apply_event(event) {
                    // The bus settles this for itself and reports what it
                    // settled on; there is nothing to do here but notice.
                    debug!(
                        agent = %conflict.key.agent,
                        session = %conflict.key.session,
                        "an event's origin disagrees with the chain its session was first seen with"
                    );
                }
            }
            Update::Foreground(change) => self.observe(change),
            Update::Assertion(assertion) => {
                self.sessions
                    .apply_assertion(&claim(assertion), &assertion.ts);
            }
            // A heartbeat says the stream is alive, which is the reader's
            // business rather than this one's, and a disconnection changes what
            // is worth showing about this view rather than anything in it.
            Update::Heartbeat(_) => {}
            Update::Disconnected => self.connected = false,
        }
    }

    /// Replaces everything with what a fresh snapshot says.
    ///
    /// Everything: the sessions, the observations, and which daemon they came
    /// from. A snapshot is the bus's whole account of itself as of one moment,
    /// so a session that was here and is not in it is a session that is over,
    /// and keeping it because it used to be here would keep exactly the rows the
    /// bus has stopped vouching for.
    fn reset(&mut self, snapshot: &Snapshot, now: &Timestamp) {
        self.sessions = SessionTable::new();
        for entry in &snapshot.sessions {
            self.sessions.seed(entry, now);
        }
        self.foreground = snapshot.foreground.as_ref().map(|observations| {
            observations
                .iter()
                .filter_map(|entry| Some((entry.slot()?.to_owned(), entry.clone())))
                .collect()
        });
        self.daemon = snapshot.daemon.clone();
        self.connected = true;
    }

    /// Files, or withdraws, one observation.
    fn observe(&mut self, change: &ForegroundChange) {
        let Some(slot) = change.slot().map(str::to_owned) else {
            // An observation about a shell carrying nothing to file it under
            // cannot be found again, and so cannot be withdrawn either.
            trace!("an observation about a slot with no name");
            return;
        };
        // A change proves somebody is looking, whatever the last snapshot said.
        let observations = self.foreground.get_or_insert_with(BTreeMap::new);
        match &change.foreground {
            Some(entry) => {
                observations.insert(slot, entry.clone());
            }
            None => {
                observations.remove(&slot);
            }
        }
    }

    /// Moves every session's clock to `now`, which is what makes one that has
    /// gone quiet stale and eventually forgets one that is over.
    pub fn tick(&mut self, now: &Timestamp) {
        self.sessions.tick(now);
    }

    /// The sessions worth showing, in the order and with the precedence the bus
    /// itself would report them in.
    pub fn sessions(&self) -> Vec<SessionEntry> {
        self.sessions.snapshot_sessions()
    }

    /// The table underneath, for a caller that wants a session by key or the
    /// claim standing against a correlation.
    pub fn table(&self) -> &SessionTable {
        &self.sessions
    }

    /// Whether anything is watching the processes in front of correlated shells
    /// at all.
    ///
    /// False means no daemon in the chain can see them, which is not the same as
    /// nothing running: it is the difference between an empty answer and no
    /// answer, and a caller that renders them the same way says something untrue
    /// about every shell.
    pub fn observing(&self) -> bool {
        self.foreground.is_some()
    }

    /// Every foreground observation, in slot order.
    pub fn foreground(&self) -> impl Iterator<Item = &ForegroundEntry> {
        self.foreground.iter().flat_map(BTreeMap::values)
    }

    /// What is running in one slot, if anything is known to be.
    ///
    /// `slot` is an opaque string, compared and never interpreted.
    pub fn foreground_in(&self, slot: &str) -> Option<&ForegroundEntry> {
        self.foreground.as_ref()?.get(slot)
    }

    /// Which daemon this account came from, where it said.
    pub fn daemon(&self) -> Option<&DaemonIdentity> {
        self.daemon.as_ref()
    }

    /// Whether the stream this was built from is still connected.
    ///
    /// What it holds while this is false is the last thing the bus said, which
    /// stays the best available answer until the next snapshot supersedes it.
    pub fn connected(&self) -> bool {
        self.connected
    }
}

/// The claim inside a stamped one.
///
/// The bus stamps a claim and drops the evidence before publishing it, and the
/// table folds the claim rather than the line, so the one has to be built back
/// out of the other. Everything the table reads survives the stamping; `raw`,
/// which does not, is evidence for a human and takes no part in any rule.
fn claim(stamped: &StampedAssertion) -> StateAssertion {
    let mut claim = StateAssertion::new(
        stamped.agent.clone(),
        stamped.correlation.clone(),
        stamped.assert,
    )
    .with_visible(stamped.visible);
    if let Some(session) = &stamped.session {
        claim = claim.with_session(session.as_str());
    }
    if let Some(cwd) = &stamped.cwd {
        claim = claim.with_cwd(cwd.as_str());
    }
    if let Some(detail) = &stamped.detail {
        claim = claim.with_detail(detail.clone());
    }
    claim
}
