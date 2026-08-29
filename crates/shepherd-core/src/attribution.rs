//! Which shell each agent session is running in.
//!
//! The bus reports sessions, the model holds shells, and nothing on the wire
//! joins the two — the bus is not allowed to know that shells exist, so the join
//! is Shepherd's to make. [`Attribution::derive`] makes it: give it the
//! workspaces as they currently are and the sessions as the bus currently
//! reports them, and it says where each session is running. A session it cannot
//! place is not dropped — it is reported [`elsewhere`](Attribution::elsewhere)
//! along with the working directory it named, because an agent running somewhere
//! this application did not start is still an agent somebody may need to attend
//! to, and hiding it would be the sidebar's first lie.
//!
//! # Derived, never patched
//!
//! Nothing here is incremental. A session starting, a session ending, a status
//! changing and a whole reconnection all have one remedy: work it out again from
//! what is currently true. That is what makes a reconnection cost nothing to get
//! right — a fresh snapshot supersedes everything said before it, and an
//! attribution derived from the new snapshot has no way to carry a stale
//! placement across, because there is nothing to carry it in. It is also cheap:
//! a walk over the shells that are open and the sessions that exist, both of
//! which are counted in tens.
//!
//! # Three ways a session gets placed
//!
//! **By its correlation, which is the answer.** Every shell is started knowing
//! the string [`correlation_for`](crate::correlation::correlation_for) gives it;
//! anything the shell runs inherits it, and the bus copies it back verbatim
//! without ever being told what is in it. A session carrying one of those
//! strings has told us exactly where it is.
//!
//! **By its working directory, when it carries no correlation we wrote.** An
//! agent may be started by hand, over a transport that dropped the environment,
//! or by somebody else's tooling entirely. If its directory is inside exactly one
//! open workspace, and that workspace has exactly one shell with nothing else
//! attributed to it, that is the only shell it can be — so it is attributed
//! there. Any ambiguity at either step and it is not: no scoring, no nearest
//! match, no tie-break. A rule a person can hold in their head is worth more
//! here than a cleverer one that is right slightly more often and inexplicable
//! when it is wrong.
//!
//! **Not at all, which is an ordinary answer.** Everything else is `elsewhere`.
//!
//! # A correlation that names nothing here
//!
//! Two different things arrive looking similar, and they are not treated the
//! same.
//!
//! A correlation this application wrote, naming a workspace or a shell that is
//! no longer open, is a session whose shell has been closed underneath it. It
//! goes to `elsewhere` and is not offered to the directory rule: it has already
//! said where it belongs, that place is gone, and putting it on a different
//! shell — one somebody is typing in — would be inventing a home for it and
//! badging an innocent terminal with somebody else's blocked agent.
//!
//! A correlation this application did not write is a different matter. The
//! environment variable it travels in belongs to the bus, and any host may put
//! anything in it; a string that does not parse tells us nothing about our
//! shells, which is exactly as much as no string at all tells us. So it is
//! treated as absent, and the directory rule gets its chance.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use agentbus_protocol::{SessionEntry, Source};

use crate::correlation::parse_correlation;
use crate::ids::{ShellAddress, ShellId, WorkspaceId};
use crate::rollup::{RollupStatus, rollup};
use crate::workspace::Workspace;

#[cfg(test)]
mod tests;

/// Where the status on an entry came from: the agent's own hooks, or something
/// watching it from outside.
///
/// An entry names two sources and they mean different things. `source` is where
/// the session's *record* came from, and `status_source` is where the status
/// being shown came from when that is somewhere else — an observer's live claim
/// standing over a quieter hook-backed record. It is the second, where there is
/// one, that answers "on whose word does this say blocked", and it is that
/// question a receiver has to be able to answer: a floor presented as authority
/// is how trust in the whole stream dies.
pub fn status_source(entry: &SessionEntry) -> Source {
    entry.status_source.unwrap_or(entry.source)
}

/// What one badge says, and on whose word it says it.
///
/// The status is [`rollup`]'s fold over whatever is being stood for — the
/// sessions attributed to a shell, the shells of a tab, the tabs of a
/// workspace. The source belongs to whichever of them won that fold, and where
/// several share the winning status, to the most authoritative of those: a
/// shell that is blocked on an agent's own word is blocked on an agent's own
/// word whatever else is also going on in it.
///
/// The pair travels together because the second half is not foldable away.
/// A receiver has to be able to say whether what it is showing came from an
/// agent's own hooks or from something watching it from outside — a floor
/// presented as authority is how trust in the whole stream dies — so the fold
/// that answers "what does this say" answers "on whose word" in the same
/// breath, at every level that has a badge to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShellStatus {
    /// The one status standing for everything below here.
    pub status: RollupStatus,
    /// Where that status came from, or `None` where there is no session below
    /// here to have come from anywhere.
    pub source: Option<Source>,
}

impl ShellStatus {
    /// Nothing is attributed here.
    pub const NONE: Self = Self {
        status: RollupStatus::None,
        source: None,
    };

    /// One session's own status, and where it came from.
    pub fn of_session(session: &SessionEntry) -> Self {
        Self {
            status: session.status.into(),
            source: Some(status_source(session)),
        }
    }

    /// The one badge standing for several, folded by [`rollup`] and carrying
    /// the provenance of whichever of them won.
    ///
    /// This is the fold at every level above a session: a tab folds its shells
    /// with it, a workspace folds its tabs with it, and a shell folds its
    /// sessions with it by way of [`ShellStatus::of_session`]. Folding badges
    /// rather than bare statuses is what keeps a tab able to say that the
    /// `blocked` it is showing is an agent's own word — which a status alone,
    /// having left its provenance behind a level down, could not.
    pub fn fold<I>(statuses: I) -> Self
    where
        I: IntoIterator<Item = Self>,
    {
        let badges: Vec<Self> = statuses.into_iter().collect();
        let status = rollup(badges.iter().map(|badge| badge.status));
        let source = badges
            .iter()
            .filter(|badge| badge.status == status)
            .filter_map(|badge| badge.source)
            // `Hook` sorts before `Observed`, and the minimum is therefore the
            // strongest evidence for the status being shown.
            .min();
        Self { status, source }
    }

    /// The status standing for `sessions`, with the provenance of whichever of
    /// them it came from.
    fn of(sessions: &[SessionEntry]) -> Self {
        Self::fold(sessions.iter().map(Self::of_session))
    }
}

/// Where every session the bus is reporting is running.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Attribution {
    /// The sessions attributed to each shell that has any. A shell with none is
    /// absent rather than present and empty, which is what makes "has anything
    /// been attributed here" a lookup.
    attributed: BTreeMap<ShellAddress, Vec<SessionEntry>>,
    elsewhere: Vec<SessionEntry>,
}

impl Attribution {
    /// Works out where every one of `sessions` is running among `workspaces`.
    ///
    /// The sessions are taken in the order the bus reports them, which is the
    /// order of its own precedence, and that order is kept in what comes back.
    /// It also settles the one case where the directory rule has a choice to
    /// make: where two sessions could each be the sole occupant of one free
    /// shell, the first of them takes it and the second goes to `elsewhere`,
    /// so the shell shows the session the bus itself considers the more urgent.
    pub fn derive<'a, W, S>(workspaces: W, sessions: S) -> Self
    where
        W: IntoIterator<Item = &'a Workspace>,
        S: IntoIterator<Item = SessionEntry>,
    {
        let shape = Shape::of(workspaces);
        let mut attribution = Self::default();

        // Correlations first, all of them, before any guess is made. A guess
        // asks which shells have nothing attributed to them, and a shell whose
        // own session is still further down the list is not one of them.
        let mut guesswork = Vec::new();
        for session in sessions {
            match shape.named_by(session.correlation.as_deref()) {
                Named::Shell(shell) => attribution.place(shell, session),
                Named::Closed => attribution.elsewhere.push(session),
                Named::Nothing => guesswork.push(session),
            }
        }

        for session in guesswork {
            let free = |shell: &ShellAddress| !attribution.attributed.contains_key(shell);
            match shape.only_shell_it_could_be(session.cwd.as_deref(), free) {
                Some(shell) => attribution.place(shell, session),
                None => attribution.elsewhere.push(session),
            }
        }

        attribution
    }

    /// Files one session under the shell it is running in.
    fn place(&mut self, shell: ShellAddress, session: SessionEntry) {
        self.attributed.entry(shell).or_default().push(session);
    }

    /// The sessions running in one shell, in the bus's order, or nothing where
    /// the shell is running no agent at all — which is the usual case for the
    /// shell somebody is typing in.
    pub fn sessions_at(&self, shell: ShellAddress) -> &[SessionEntry] {
        self.attributed
            .get(&shell)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// What one shell's badge says.
    pub fn status_at(&self, shell: ShellAddress) -> ShellStatus {
        ShellStatus::of(self.sessions_at(shell))
    }

    /// Every shell running something, with what it is running.
    pub fn shells(&self) -> impl Iterator<Item = (ShellAddress, &[SessionEntry])> {
        self.attributed
            .iter()
            .map(|(shell, sessions)| (*shell, sessions.as_slice()))
    }

    /// The sessions that could not be placed, in the bus's order.
    ///
    /// Each carries the working directory the bus reported for it, which is all
    /// there is to say about where it is: it is an agent this application can
    /// see and cannot claim.
    pub fn elsewhere(&self) -> &[SessionEntry] {
        &self.elsewhere
    }

    /// The shell lookup one workspace's rollup takes.
    ///
    /// ```
    /// # use shepherd_core::{Attribution, RollupStatus, Workspace, WorkspaceId};
    /// # let workspace = Workspace::new(WorkspaceId::FIRST, "/tmp");
    /// # let attribution = Attribution::derive([&workspace], []);
    /// let status = workspace.status(attribution.shell_status(workspace.id()));
    /// # assert_eq!(status, RollupStatus::None);
    /// ```
    pub fn shell_status(&self, workspace: WorkspaceId) -> impl Fn(ShellId) -> RollupStatus + '_ {
        move |shell| self.status_at(ShellAddress::new(workspace, shell)).status
    }
}

/// What a correlation string turned out to name.
enum Named {
    /// A shell that is open right now.
    Shell(ShellAddress),
    /// One of this application's shells, in a workspace or at a number that is
    /// not open any more — or never was, on a string written by an earlier run.
    Closed,
    /// Nothing of ours: no correlation, or a string somebody else wrote.
    Nothing,
}

/// The workspaces as they currently are, in the shape the two rules ask about
/// them: which shells exist, and which folder each workspace is.
struct Shape<'a> {
    workspaces: Vec<Open<'a>>,
}

/// One open workspace.
struct Open<'a> {
    id: WorkspaceId,
    path: &'a Path,
    shells: BTreeSet<ShellId>,
}

impl<'a> Shape<'a> {
    fn of<W>(workspaces: W) -> Self
    where
        W: IntoIterator<Item = &'a Workspace>,
    {
        Self {
            workspaces: workspaces
                .into_iter()
                .map(|workspace| Open {
                    id: workspace.id(),
                    path: workspace.path(),
                    shells: workspace.shells().into_iter().collect(),
                })
                .collect(),
        }
    }

    /// What `correlation` names here, if anything.
    fn named_by(&self, correlation: Option<&str>) -> Named {
        let Some(correlation) = correlation else {
            return Named::Nothing;
        };
        let Ok(shell) = parse_correlation(correlation).map(ShellAddress::from) else {
            // Not a string this application wrote, which says nothing about
            // this application's shells.
            return Named::Nothing;
        };
        if self.holds(shell) {
            Named::Shell(shell)
        } else {
            Named::Closed
        }
    }

    /// Whether that shell is open.
    fn holds(&self, shell: ShellAddress) -> bool {
        self.workspaces.iter().any(|workspace| {
            workspace.id == shell.workspace && workspace.shells.contains(&shell.shell)
        })
    }

    /// The one shell a session in `cwd` could be running in, where there is
    /// exactly one it could be.
    ///
    /// Two questions, and both have to have a single answer: which open
    /// workspace contains the directory, and which of that workspace's shells is
    /// free — `free` being asked of each in turn. Anything else is a guess, and
    /// a guess is what `elsewhere` exists to avoid having to make.
    fn only_shell_it_could_be(
        &self,
        cwd: Option<&str>,
        free: impl Fn(&ShellAddress) -> bool,
    ) -> Option<ShellAddress> {
        let cwd = Path::new(cwd?);
        // Compared by path component rather than by text, so that a workspace at
        // `/src/thing` does not claim a directory in `/src/thing-other`. Neither
        // side is resolved against the filesystem: this crate is told what is
        // open and what the bus said, and asking the disk would make the answer
        // depend on where this happens to be running.
        let mut inside = self
            .workspaces
            .iter()
            .filter(|workspace| cwd.starts_with(workspace.path));
        let workspace = inside.next()?;
        if inside.next().is_some() {
            // Nested workspaces, both of which contain the directory. Preferring
            // the deeper one would be right often enough to be trusted and wrong
            // often enough to matter.
            return None;
        }

        let mut candidates = workspace
            .shells
            .iter()
            .map(|shell| ShellAddress::new(workspace.id, *shell))
            .filter(free);
        let only = candidates.next()?;
        candidates.next().is_none().then_some(only)
    }
}
