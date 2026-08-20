//! Which process each session was last heard speaking from.
//!
//! A session is a name an agent chose for itself; a pid is a row in the process
//! table. This is the join between them, and it is the whole reason a daemon can
//! say that a session is *over* rather than merely quiet: an agent that was
//! killed, crashed or interrupted before it could say goodbye leaves nothing
//! behind except the absence of its process.
//!
//! # A binding is a memory of one moment
//!
//! Nothing here decides when to bind or what a binding means; a binding is
//! recorded and later looked up, and both directions are needed. A session is
//! bound to at most one pid, because a session runs in one process, while a pid
//! may carry several sessions — an agent and the subagent it spawned are two
//! sessions in one process — so the end of a pid is news for all of them at
//! once.
//!
//! # Nothing in here is a pid's opinion of a session
//!
//! The keys are opaque on both sides: a session key is whatever an agent
//! reported and a pid is a number the process table used. Nothing is parsed,
//! validated or interpreted, and no process name appears anywhere.

use std::collections::{BTreeMap, BTreeSet};

use agentbus_protocol::SessionKey;

use crate::procfs::Pid;

/// What binding a session to a pid changed, so that a caller can say so once
/// rather than on every event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bind {
    /// The session was already bound to this pid.
    Unchanged,
    /// The session had no binding and now has one.
    Bound,
    /// The session was bound to a different pid, which it no longer is.
    Rebound {
        /// The pid it was bound to until now.
        from: Pid,
    },
}

/// The sessions currently bound to a process, both ways round.
///
/// The two maps are one fact stored twice, and every operation keeps them
/// agreeing: `sessions` is what a pid's disappearance is answered from, and
/// `pids` is what tells a rebinding from a repetition.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Bindings {
    pids: BTreeMap<SessionKey, Pid>,
    sessions: BTreeMap<Pid, BTreeSet<SessionKey>>,
}

impl Bindings {
    /// No bindings at all.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `key` is running as `pid`, replacing whatever it was bound
    /// to before.
    pub fn bind(&mut self, key: &SessionKey, pid: Pid) -> Bind {
        match self.pids.insert(key.clone(), pid) {
            Some(previous) if previous == pid => Bind::Unchanged,
            previous => {
                if let Some(previous) = previous {
                    self.detach(key, previous);
                }
                self.sessions.entry(pid).or_default().insert(key.clone());
                match previous {
                    Some(from) => Bind::Rebound { from },
                    None => Bind::Bound,
                }
            }
        }
    }

    /// The pid a session is bound to, if it is bound to one.
    pub fn pid_of(&self, key: &SessionKey) -> Option<Pid> {
        self.pids.get(key).copied()
    }

    /// Drops every binding to `pid` and hands back the sessions that had one.
    ///
    /// Taking the bindings away is part of the answer rather than a tidy-up: the
    /// pid is gone, so a later pid that happens to reuse the number has nothing
    /// to do with these sessions and must not be able to reap them again.
    pub fn release(&mut self, pid: Pid) -> Vec<SessionKey> {
        let released: Vec<SessionKey> = self
            .sessions
            .remove(&pid)
            .unwrap_or_default()
            .into_iter()
            .collect();
        for key in &released {
            self.pids.remove(key);
        }
        released
    }

    /// Keeps only the bindings of sessions `keep` still recognizes.
    ///
    /// A session that has been forgotten cannot be told anything, so its binding
    /// is nothing but a row that would otherwise be kept for as long as the
    /// daemon runs.
    pub fn retain(&mut self, keep: impl Fn(&SessionKey) -> bool) {
        let dropped: Vec<(SessionKey, Pid)> = self
            .pids
            .iter()
            .filter(|(key, _)| !keep(key))
            .map(|(key, pid)| (key.clone(), *pid))
            .collect();
        for (key, pid) in dropped {
            self.pids.remove(&key);
            self.detach(&key, pid);
        }
    }

    /// How many sessions are bound to anything.
    pub fn len(&self) -> usize {
        self.pids.len()
    }

    /// Whether nothing is bound at all.
    pub fn is_empty(&self) -> bool {
        self.pids.is_empty()
    }

    /// Takes one session off one pid, and the pid off the table if that was the
    /// last session it carried.
    fn detach(&mut self, key: &SessionKey, pid: Pid) {
        if let Some(sessions) = self.sessions.get_mut(&pid) {
            sessions.remove(key);
            if sessions.is_empty() {
                self.sessions.remove(&pid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str) -> SessionKey {
        let agent = agentbus_protocol::Agent::new("claude").expect("that is a valid agent id");
        SessionKey::new(agent, name)
    }

    #[test]
    fn a_session_is_bound_to_one_pid_and_found_again() {
        let mut bindings = Bindings::new();

        assert_eq!(bindings.bind(&session("one"), 200), Bind::Bound);

        assert_eq!(bindings.pid_of(&session("one")), Some(200));
        assert_eq!(bindings.pid_of(&session("two")), None);
        assert_eq!(bindings.len(), 1);
    }

    #[test]
    fn binding_the_same_pid_again_is_not_news() {
        let mut bindings = Bindings::new();
        bindings.bind(&session("one"), 200);

        assert_eq!(bindings.bind(&session("one"), 200), Bind::Unchanged);
        assert_eq!(bindings.len(), 1);
    }

    #[test]
    fn a_rebound_session_says_what_it_left_and_leaves_nothing_behind() {
        let mut bindings = Bindings::new();
        bindings.bind(&session("one"), 200);

        assert_eq!(
            bindings.bind(&session("one"), 300),
            Bind::Rebound { from: 200 }
        );

        assert_eq!(bindings.pid_of(&session("one")), Some(300));
        assert!(
            bindings.release(200).is_empty(),
            "the old pid still carried the session"
        );
        assert_eq!(bindings.release(300), vec![session("one")]);
    }

    #[test]
    fn every_session_on_one_pid_is_released_together() {
        let mut bindings = Bindings::new();
        bindings.bind(&session("one"), 200);
        bindings.bind(&session("two"), 200);
        bindings.bind(&session("elsewhere"), 300);

        assert_eq!(
            bindings.release(200),
            vec![session("one"), session("two")],
            "a pid carries every session that was bound to it"
        );

        assert_eq!(bindings.pid_of(&session("one")), None);
        assert_eq!(bindings.pid_of(&session("two")), None);
        assert_eq!(bindings.pid_of(&session("elsewhere")), Some(300));
    }

    #[test]
    fn a_released_pid_cannot_release_the_same_sessions_twice() {
        let mut bindings = Bindings::new();
        bindings.bind(&session("one"), 200);

        assert_eq!(bindings.release(200), vec![session("one")]);
        assert!(bindings.release(200).is_empty());
        assert!(bindings.is_empty());
    }

    #[test]
    fn releasing_a_pid_nothing_was_bound_to_does_nothing() {
        let mut bindings = Bindings::new();
        bindings.bind(&session("one"), 200);

        assert!(bindings.release(999).is_empty());
        assert_eq!(bindings.len(), 1);
    }

    #[test]
    fn a_session_that_is_no_longer_known_is_forgotten_on_both_sides() {
        let mut bindings = Bindings::new();
        bindings.bind(&session("one"), 200);
        bindings.bind(&session("two"), 200);

        bindings.retain(|key| key.session != "one");

        assert_eq!(bindings.pid_of(&session("one")), None);
        assert_eq!(
            bindings.release(200),
            vec![session("two")],
            "the pid still carried a session nobody knows about"
        );
    }
}
