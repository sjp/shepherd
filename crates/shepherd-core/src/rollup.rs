//! One status standing for many.
//!
//! A shell may host more than one agent session, a tab holds shells, and a
//! workspace holds tabs. Every one of those levels has to show a single badge,
//! and the question being asked at each is the same one: given several
//! statuses, which is the one somebody glancing at this needs to see? So it is
//! answered once, by [`rollup`], and asked at all three levels. Three folds
//! that happened to agree today would be three folds to keep agreeing forever,
//! and the level at which they stopped agreeing would be the level whose badge
//! quietly started lying.
//!
//! # The precedence
//!
//! `blocked > working > starting > stale > idle > done > none`
//!
//! `blocked` wins outright: it is the only status that needs a person *right
//! now*, and everything else can wait behind it. Below that the order runs from
//! "something is happening here" down to "nothing is". `starting` sits under
//! `working` and above `stale` because a session that has just begun is a
//! session that is alive — closer to work in progress than to a session we have
//! stopped hearing from — but it has not done anything yet, so it does not
//! outrank one that has. `stale` outranks `idle` because "we lost track of it"
//! is a worse thing to leave unnoticed than "it finished its turn", and `done`
//! comes last of the statuses because a session that is over asks for nothing.
//!
//! `none` is bottom, and it is an ordinary answer rather than a failure: a
//! shell running an editor, a tab of plain shells, a workspace nobody has
//! started an agent in. Anything that treats it as missing information will
//! draw an error where there is nothing wrong.
//!
//! # What it does not fold
//!
//! Where a status came from — an agent's own hooks, or something watching from
//! outside — is not part of this and is not folded away by it. Two statuses
//! that disagree are settled before they get here; a receiver still has to be
//! able to tell what it is looking at afterwards, and that fact travels
//! alongside the rolled-up status rather than through it.

use std::cmp::Ordering;
use std::fmt;

use agentbus_protocol::SessionStatus;

/// What is happening somewhere in the model — at a shell, a tab or a workspace
/// — where the honest answer may be that nothing is.
///
/// Ordering is by urgency, so the greater of two is the one that wins and the
/// fold over a collection is its maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RollupStatus {
    /// There is at least one session here, and this is what it is doing.
    Session(SessionStatus),
    /// Nothing here is an agent session.
    #[default]
    None,
}

impl RollupStatus {
    /// Every status, most urgent first. Reading this list is reading the
    /// precedence.
    pub const ALL: [Self; 7] = [
        Self::Session(SessionStatus::Blocked),
        Self::Session(SessionStatus::Working),
        Self::Session(SessionStatus::Starting),
        Self::Session(SessionStatus::Stale),
        Self::Session(SessionStatus::Idle),
        Self::Session(SessionStatus::Done),
        Self::None,
    ];

    /// The session status here, if there is a session here at all.
    pub const fn session(self) -> Option<SessionStatus> {
        match self {
            Self::Session(status) => Some(status),
            Self::None => None,
        }
    }

    /// What this status is called.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session(status) => status.as_str(),
            Self::None => "none",
        }
    }

    /// How loudly this status asks to be noticed, as a number that exists only
    /// to be compared with another one. Every status has a different one, which
    /// is what makes the ordering total and lets the fold be a maximum.
    const fn urgency(self) -> u8 {
        match self {
            Self::Session(SessionStatus::Blocked) => 6,
            Self::Session(SessionStatus::Working) => 5,
            Self::Session(SessionStatus::Starting) => 4,
            Self::Session(SessionStatus::Stale) => 3,
            Self::Session(SessionStatus::Idle) => 2,
            Self::Session(SessionStatus::Done) => 1,
            Self::None => 0,
        }
    }
}

impl From<SessionStatus> for RollupStatus {
    fn from(status: SessionStatus) -> Self {
        Self::Session(status)
    }
}

impl Ord for RollupStatus {
    fn cmp(&self, other: &Self) -> Ordering {
        self.urgency().cmp(&other.urgency())
    }
}

impl PartialOrd for RollupStatus {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for RollupStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The one status that stands for all of `statuses`: the most urgent of them,
/// or [`RollupStatus::None`] if there are none.
///
/// This is the fold, and it is the only one. A shell folds the sessions
/// attributed to it with it, a tab folds its shells with it, and a workspace
/// folds its tabs with it.
pub fn rollup<I>(statuses: I) -> RollupStatus
where
    I: IntoIterator<Item = RollupStatus>,
{
    statuses.into_iter().max().unwrap_or(RollupStatus::None)
}

/// One shell's status, folded over the sessions attributed to it.
///
/// A shell hosting several at once is ordinary — an agent that starts another,
/// or somebody who has run two in the same terminal — and they are folded by
/// the same rule a tab folds its shells by. A shell nothing has been attributed
/// to is [`RollupStatus::None`], which is the usual case for the shell somebody
/// is typing in.
pub fn shell_status<I>(sessions: I) -> RollupStatus
where
    I: IntoIterator<Item = SessionStatus>,
{
    rollup(sessions.into_iter().map(RollupStatus::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    use SessionStatus::{Blocked, Done, Idle, Stale, Starting, Working};

    /// The precedence, written out here rather than read from the code under
    /// test, so that the two have to agree with each other. A status added to
    /// the bus lands in neither list until somebody puts it in both.
    const PRECEDENCE: [SessionStatus; 6] = [Blocked, Working, Starting, Stale, Idle, Done];

    /// The winner among `present`, found by walking the precedence rather than
    /// by folding.
    fn winner(present: &[SessionStatus]) -> RollupStatus {
        PRECEDENCE
            .into_iter()
            .find(|status| present.contains(status))
            .map_or(RollupStatus::None, RollupStatus::from)
    }

    #[test]
    fn the_precedence_places_every_status_the_bus_can_report() {
        assert_eq!(PRECEDENCE.len(), SessionStatus::ALL.len());
        for status in SessionStatus::ALL {
            assert!(
                PRECEDENCE.contains(&status),
                "{status} has no place in the precedence"
            );
        }
        assert_eq!(RollupStatus::ALL.len(), PRECEDENCE.len() + 1);
    }

    #[test]
    fn the_ordering_is_the_precedence_and_nothing_else() {
        for pair in RollupStatus::ALL.windows(2) {
            assert!(pair[0] > pair[1], "{} should outrank {}", pair[0], pair[1]);
        }
        for status in RollupStatus::ALL {
            assert_eq!(
                status.cmp(&status),
                Ordering::Equal,
                "{status} beside itself"
            );
            assert!(status >= RollupStatus::None);
        }
    }

    #[test]
    fn every_combination_of_statuses_folds_to_the_most_urgent_one_present() {
        for subset in 0..(1_u32 << PRECEDENCE.len()) {
            let present: Vec<SessionStatus> = PRECEDENCE
                .into_iter()
                .enumerate()
                .filter(|(place, _)| subset & (1 << place) != 0)
                .map(|(_, status)| status)
                .collect();
            let expected = winner(&present);

            let statuses: Vec<RollupStatus> =
                present.iter().copied().map(RollupStatus::from).collect();
            assert_eq!(rollup(statuses.iter().copied()), expected, "{present:?}");
            assert_eq!(
                rollup(statuses.iter().rev().copied()),
                expected,
                "{present:?}, in the other order"
            );

            // Somewhere with no session does not drag down somewhere that has
            // one: at a tab of a dozen shells, one blocked agent still shows.
            let mut padded = vec![RollupStatus::None];
            padded.extend(statuses);
            padded.push(RollupStatus::None);
            assert_eq!(rollup(padded), expected, "{present:?}, padded with none");
        }
    }

    #[test]
    fn nothing_at_all_folds_to_none() {
        assert_eq!(rollup(Vec::new()), RollupStatus::None);
        assert_eq!(rollup([RollupStatus::None]), RollupStatus::None);
        assert_eq!(shell_status(Vec::new()), RollupStatus::None);
    }

    #[test]
    fn a_shell_hosting_several_sessions_shows_the_most_urgent_of_them() {
        assert_eq!(shell_status([Idle, Blocked, Done]), Blocked.into());
        assert_eq!(shell_status([Done, Done, Idle]), Idle.into());
        assert_eq!(shell_status([Stale, Starting]), Starting.into());
        assert_eq!(shell_status([Working]), Working.into());
    }

    #[test]
    fn a_status_spells_itself_the_way_the_bus_does_or_says_none() {
        for status in SessionStatus::ALL {
            let rolled = RollupStatus::from(status);
            assert_eq!(rolled.to_string(), status.to_string());
            assert_eq!(rolled.session(), Some(status));
        }
        assert_eq!(RollupStatus::None.to_string(), "none");
        assert_eq!(RollupStatus::None.session(), None);
        assert_eq!(RollupStatus::default(), RollupStatus::None);
    }
}
