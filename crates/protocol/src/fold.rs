//! The status fold: what the bus reports about one session.
//!
//! Everything a receiver renders comes from here. Per `(agent, session)` — the
//! caller owns that key and the fold never looks at it — a pure function takes
//! the state so far and one [`Input`] and returns the state that follows. There
//! is no clock, no socket and no process table in this module: the passage of
//! time arrives as [`Input::Tick`] and the death of a process as
//! [`Input::ProcessGone`], because a rule about time that cannot be tested
//! without waiting is a rule that will not be tested.
//!
//! # What the states mean
//!
//! [`SessionStatus`] is coarse on purpose; the fold's job is to keep each value
//! honest.
//!
//! - `starting` — we have heard of the session and nothing more.
//! - `working` — the agent is doing something.
//! - `blocked` — the agent is waiting on a human. This is the state the whole
//!   bus exists to report, so nothing may enter or leave it by inference.
//! - `idle` — the agent finished its turn.
//! - `stale` — we stopped hearing from a session that was working. This is not
//!   `idle`: "it finished" and "we lost track of it" are different facts, and a
//!   receiver that shows them the same way is lying to its user.
//! - `done` — the session is over.
//!
//! # The rules that make this survive reality
//!
//! Events are dropped, duplicated and delivered late, and an agent may be
//! started, killed or reused at any moment. Every rule below exists because the
//! obvious implementation gets one of those wrong.
//!
//! 1. **Any tool event implies `working`.** A session that never reported a turn
//!    start, because the event was lost or the agent never sends one, still
//!    counts as working the moment it calls a tool. The fold is written to
//!    tolerate gaps rather than to assume a well-formed sequence.
//! 2. **An unheard-of session is created by whatever event arrives first**, as
//!    if it had been in `starting`. A missed `session_start` therefore costs
//!    nothing; without this rule it would strand a real, running session.
//! 3. **Leaving `blocked` is inferred.** No agent reliably says it stopped
//!    waiting for its human, so any activity while blocked — a tool call, an
//!    explicit `unblocked`, the end of the turn — clears the block. Entering
//!    `blocked`, by contrast, is only ever something an emitter said.
//! 4. **Duplicates are inert.** Applying the same event twice gives exactly the
//!    state applying it once gave, `since` included, which is what lets a
//!    delivery path retry without inventing transitions.
//! 5. **Late events still count, but time never runs backwards.** An event older
//!    than one already applied updates the status — there is no reordering
//!    buffer anywhere in this system, and refusing it would leave the status
//!    wrong forever — but it can move neither `since` nor `last_event` earlier.
//! 6. **Only hook-backed sessions go `stale`.** An observer emits when it
//!    notices something change, and an agent waiting on its human changes
//!    nothing at all — so silence from an observer is not evidence of silence
//!    from the agent, and timing one out would report "we lost it" about a
//!    session that is sitting right there. An observed session leaves instead
//!    through `session_end` or [`Input::ProcessGone`], which is the observer's
//!    responsibility rather than the fold's guesswork.

use std::time::Duration;

use crate::event::{Event, Kind, Source};
use crate::status::SessionStatus;
use crate::timestamp::Timestamp;

/// How long a hook-backed `working` session may go without an event before the
/// fold reports it [`SessionStatus::Stale`].
///
/// Two minutes is longer than any single tool call a receiver should worry
/// about, and short enough that an agent whose host went away is not still
/// reported as working when its user next looks.
pub const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(120);

/// Everything the fold knows about one session.
///
/// The fields are public because this is data, not an object: the fold builds
/// it, a session table stores it, and a snapshot builder reads it. The only
/// invariant is that `since` and `last_event` never decrease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    /// What the session is doing.
    pub status: SessionStatus,
    /// When the current `status` began. Unchanged while the status is, so a
    /// receiver can say "blocked for six minutes" without keeping its own
    /// history.
    pub since: Timestamp,
    /// The timestamp of the most recent event applied. This is the fold's
    /// measure of liveness, and neither a tick nor a dead process moves it —
    /// only the session itself saying something does.
    pub last_event: Timestamp,
    /// Where the most recent event's claim came from. It decides whether the
    /// stale timer applies at all.
    pub source: Source,
    /// When the session last reported an error, if it ever did. An error is not
    /// a status — an agent that recovers from a failed tool call is still
    /// working — so it is recorded here rather than being allowed to displace
    /// what the session is actually doing. The event itself stays on the stream
    /// for anyone who wants its detail.
    pub last_error: Option<Timestamp>,
}

/// One thing that can happen to a session.
///
/// Time and process liveness are inputs rather than things the fold goes and
/// finds out, which is what keeps every rule in this module reachable from a
/// unit test.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input<'a> {
    /// A normalized event belonging to this session.
    Event(&'a Event),
    /// Time has passed. The caller applies these periodically; how often only
    /// affects how promptly a session is noticed to have gone quiet.
    Tick {
        /// The caller's idea of now.
        now: &'a Timestamp,
    },
    /// The caller has definitive evidence that the process behind this session
    /// no longer exists. This is what separates a killed agent from a hung one:
    /// without it, both merely stop producing events.
    ProcessGone {
        /// When the process was found to be gone.
        at: &'a Timestamp,
    },
}

/// Whether a kind is activity — evidence that the agent is doing something and
/// therefore that the session is `working`.
///
/// One definition, shared by the fold and by anything that needs to reason about
/// the same question, so that the two cannot drift apart. Note what is not here:
/// `blocked` and `turn_end` are statuses of their own, `error` says nothing
/// about what the session is doing next, and `unblocked` only means something
/// relative to a session that is currently blocked.
pub fn is_activity(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::TurnStart
            | Kind::ToolStart
            | Kind::ToolEnd
            | Kind::SubagentStart
            | Kind::SubagentEnd
            | Kind::Compact
    )
}

/// The fold itself: the transition rules, plus how long a working session may go
/// quiet before it is reported stale.
///
/// It holds no session state — that is [`SessionState`], which the caller keys
/// and stores — so one of these can be shared by every session a daemon knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    stale_after: Duration,
}

impl Default for Fold {
    fn default() -> Self {
        Self::new()
    }
}

impl Fold {
    /// A fold with the default stale timeout, [`DEFAULT_STALE_AFTER`].
    pub const fn new() -> Self {
        Self::with_stale_after(DEFAULT_STALE_AFTER)
    }

    /// A fold that waits `stale_after` before reporting a quiet working session
    /// as stale.
    pub const fn with_stale_after(stale_after: Duration) -> Self {
        Self { stale_after }
    }

    /// How long this fold waits before reporting a quiet working session stale.
    pub const fn stale_after(&self) -> Duration {
        self.stale_after
    }

    /// Applies one input to one session's state.
    ///
    /// `state` is `None` for a session that has never been seen; an event
    /// creates one, while a tick and a dead process have nothing to create a
    /// session from and leave it that way. The fold never forgets a session it
    /// has: given `Some`, it returns `Some`. Deciding when a finished session
    /// stops being interesting belongs to whatever holds the table, not here.
    pub fn apply(&self, state: Option<SessionState>, input: Input<'_>) -> Option<SessionState> {
        match input {
            Input::Event(event) => Some(self.apply_event(state, event)),
            Input::Tick { now } => state.map(|state| self.apply_tick(state, now)),
            Input::ProcessGone { at } => state.map(|state| apply_process_gone(state, at)),
        }
    }

    fn apply_event(&self, state: Option<SessionState>, event: &Event) -> SessionState {
        // A session nobody told us about starts here, in `starting`, at the
        // moment of the event that revealed it. The transition below then puts
        // it wherever that event implies, so a lost `session_start` costs a
        // status of `starting` for no time at all rather than a lost session.
        let mut state = state.unwrap_or_else(|| SessionState {
            status: SessionStatus::Starting,
            since: event.ts.clone(),
            last_event: event.ts.clone(),
            source: event.source,
            last_error: None,
        });

        state.source = event.source;
        advance(&mut state.last_event, &event.ts);
        match event.kind {
            // A new life for a reused session id: whatever went wrong in the
            // previous one is not this one's problem.
            Kind::SessionStart => state.last_error = None,
            Kind::Error => state.last_error = Some(event.ts.clone()),
            _ => {}
        }

        let next = next_status(state.status, &event.kind);
        if next != state.status {
            state.status = next;
            // The status begins now, and "now" is the newest moment we know
            // about — never the timestamp of a straggler that arrived late.
            let began = state.last_event.clone();
            advance(&mut state.since, &began);
        }
        state
    }

    fn apply_tick(&self, mut state: SessionState, now: &Timestamp) -> SessionState {
        if state.status != SessionStatus::Working || state.source != Source::Hook {
            return state;
        }
        // A tick from before the last event we heard is not evidence of silence.
        let Ok(quiet_for) = u128::try_from(now.millis_since(&state.last_event)) else {
            return state;
        };
        if quiet_for >= self.stale_after.as_millis() {
            state.status = SessionStatus::Stale;
            advance(&mut state.since, now);
        }
        state
    }
}

fn apply_process_gone(mut state: SessionState, at: &Timestamp) -> SessionState {
    if state.status == SessionStatus::Done {
        return state;
    }
    state.status = SessionStatus::Done;
    advance(&mut state.since, at);
    state
}

/// The transition table. `current` is where the session is; the result is where
/// the event puts it.
fn next_status(current: SessionStatus, kind: &Kind) -> SessionStatus {
    // A session id can be reused, and the start of a new life outranks the end
    // of the old one. Every other event finds `done` terminal, so a straggler
    // arriving after the session ended cannot resurrect it.
    if matches!(kind, Kind::SessionStart) {
        return SessionStatus::Starting;
    }
    if current == SessionStatus::Done {
        return SessionStatus::Done;
    }

    match kind {
        Kind::SessionEnd => SessionStatus::Done,
        Kind::Blocked => SessionStatus::Blocked,
        Kind::TurnEnd => SessionStatus::Idle,
        Kind::Unblocked if current == SessionStatus::Blocked => SessionStatus::Working,
        kind if is_activity(kind) => SessionStatus::Working,
        // An error, an `unblocked` for a session that was not blocked, and a
        // kind this build has never heard of all say the session is alive and
        // say nothing about what it is doing. Leave the status alone.
        _ => current,
    }
}

/// Moves `current` forward to `candidate`, never backwards.
///
/// Shared with the session table, which applies the same rule to the claims an
/// observer makes about a session: there is one answer to "an older timestamp
/// arrived" in this crate and it lives here.
pub(crate) fn advance(current: &mut Timestamp, candidate: &Timestamp) {
    if candidate > current {
        *current = candidate.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Agent, UnstampedEvent};

    /// Builds an agent id from a literal, which is what every one of these is.
    fn agent(name: &str) -> Agent {
        Agent::new(name).expect("a test's own agent id is a valid one")
    }

    use SessionStatus::{Blocked, Done, Idle, Stale, Starting, Working};

    /// A timestamp `seconds` after 2026-08-17T10:00:00Z. Every test works in
    /// whole seconds from that base, so a sequence reads as a timeline.
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

    fn event(kind: Kind, seconds: u64) -> Event {
        UnstampedEvent::new(agent("claude"), "abc123", kind).stamp(seconds, at(seconds))
    }

    fn observed(kind: Kind, seconds: u64) -> Event {
        UnstampedEvent::new(agent("claude"), "observed:w9:p3", kind)
            .with_source(Source::Observed)
            .stamp(seconds, at(seconds))
    }

    /// A session sitting in `status` since the base of the test clock.
    fn state_in(status: SessionStatus) -> SessionState {
        SessionState {
            status,
            since: at(0),
            last_event: at(0),
            source: Source::Hook,
            last_error: None,
        }
    }

    /// Applies an input to a state that must exist afterwards.
    fn feed(fold: &Fold, state: Option<SessionState>, input: Input<'_>) -> SessionState {
        fold.apply(state, input)
            .expect("an event or an existing session always leaves a state")
    }

    /// Runs a sequence of `(kind, second)` events from nothing, returning the
    /// status after each one.
    fn statuses(sequence: &[(Kind, u64)]) -> Vec<SessionStatus> {
        let fold = Fold::new();
        let mut state = None;
        let mut seen = Vec::new();
        for (kind, second) in sequence {
            let next = feed(
                &fold,
                state.take(),
                Input::Event(&event(kind.clone(), *second)),
            );
            seen.push(next.status);
            state = Some(next);
        }
        seen
    }

    #[test]
    fn every_cell_of_the_transition_table() {
        // One row per starting status; one column per kind, in `Kind::ALL`
        // order. Reading a row tells you everything that can happen to a
        // session in that status.
        //
        //                  session_start  session_end  turn_start  turn_end
        //                  tool_start  tool_end  blocked  unblocked
        //                  subagent_start  subagent_end  compact  error
        let table = [
            (
                Starting,
                [
                    Starting, Done, Working, Idle, Working, Working, Blocked, Starting, Working,
                    Working, Working, Starting,
                ],
            ),
            (
                Working,
                [
                    Starting, Done, Working, Idle, Working, Working, Blocked, Working, Working,
                    Working, Working, Working,
                ],
            ),
            (
                Blocked,
                [
                    Starting, Done, Working, Idle, Working, Working, Blocked, Working, Working,
                    Working, Working, Blocked,
                ],
            ),
            (
                Idle,
                [
                    Starting, Done, Working, Idle, Working, Working, Blocked, Idle, Working,
                    Working, Working, Idle,
                ],
            ),
            (
                Stale,
                [
                    Starting, Done, Working, Idle, Working, Working, Blocked, Stale, Working,
                    Working, Working, Stale,
                ],
            ),
            (
                // `done` is terminal, and a reused session id is the one thing
                // that starts a fresh life.
                Done,
                [
                    Starting, Done, Done, Done, Done, Done, Done, Done, Done, Done, Done, Done,
                ],
            ),
        ];

        let fold = Fold::new();
        for (current, expected) in table {
            assert_eq!(Kind::ALL.len(), expected.len());
            for (kind, want) in Kind::ALL.iter().zip(expected) {
                let next = feed(
                    &fold,
                    Some(state_in(current)),
                    Input::Event(&event(kind.clone(), 10)),
                );
                assert_eq!(next.status, want, "{current} + {kind}");
            }
        }
    }

    #[test]
    fn a_kind_this_build_does_not_know_is_alive_but_says_nothing() {
        let fold = Fold::new();
        let unknown = UnstampedEvent::new(agent("claude"), "abc123", Kind::from("invented_later"))
            .stamp(1, at(10));
        for status in SessionStatus::ALL {
            let next = feed(&fold, Some(state_in(status)), Input::Event(&unknown));
            assert_eq!(next.status, status);
            assert_eq!(next.since, at(0));
            // It is still evidence that the session exists, so the stale timer
            // restarts.
            assert_eq!(next.last_event, at(10));
        }
    }

    #[test]
    fn whatever_arrives_first_creates_the_session() {
        let fold = Fold::new();
        for kind in Kind::ALL {
            let event = event(kind.clone(), 10);
            let created = feed(&fold, None, Input::Event(&event));
            let from_starting = feed(&fold, Some(state_in(Starting)), Input::Event(&event));
            assert_eq!(
                created.status, from_starting.status,
                "{kind} on an unheard-of session"
            );
            assert_eq!(created.since, at(10));
            assert_eq!(created.last_event, at(10));
        }
    }

    #[test]
    fn nothing_but_an_event_can_conjure_a_session() {
        let fold = Fold::new();
        assert_eq!(fold.apply(None, Input::Tick { now: &at(600) }), None);
        assert_eq!(fold.apply(None, Input::ProcessGone { at: &at(600) }), None);
    }

    #[test]
    fn a_whole_claude_session() {
        assert_eq!(
            statuses(&[
                (Kind::SessionStart, 0),
                (Kind::TurnStart, 1),
                (Kind::ToolStart, 2),
                (Kind::ToolEnd, 3),
                (Kind::Blocked, 4),
                (Kind::ToolStart, 30),
                (Kind::ToolEnd, 31),
                (Kind::TurnEnd, 32),
                (Kind::SessionEnd, 33),
            ]),
            [
                Starting, Working, Working, Working, Blocked, Working, Working, Idle, Done
            ]
        );
    }

    #[test]
    fn the_same_session_with_its_start_dropped() {
        assert_eq!(
            statuses(&[
                (Kind::TurnStart, 1),
                (Kind::ToolStart, 2),
                (Kind::ToolEnd, 3),
                (Kind::Blocked, 4),
                (Kind::ToolStart, 30),
                (Kind::TurnEnd, 32),
                (Kind::SessionEnd, 33),
            ]),
            [Working, Working, Working, Blocked, Working, Idle, Done]
        );
    }

    #[test]
    fn the_same_session_with_its_turn_start_dropped() {
        assert_eq!(
            statuses(&[
                (Kind::SessionStart, 0),
                (Kind::ToolStart, 2),
                (Kind::ToolEnd, 3),
                (Kind::Blocked, 4),
                (Kind::ToolStart, 30),
                (Kind::TurnEnd, 32),
                (Kind::SessionEnd, 33),
            ]),
            [Starting, Working, Working, Blocked, Working, Idle, Done]
        );
    }

    #[test]
    fn a_block_is_cleared_by_activity_because_no_agent_says_unblocked() {
        // Not one `unblocked` in the sequence; the block clears anyway.
        assert_eq!(
            statuses(&[
                (Kind::SessionStart, 0),
                (Kind::Blocked, 4),
                (Kind::ToolStart, 30),
                (Kind::Blocked, 40),
                (Kind::TurnEnd, 50),
            ]),
            [Starting, Blocked, Working, Blocked, Idle]
        );
    }

    #[test]
    fn a_session_that_never_said_goodbye_is_ended_by_its_process_dying() {
        let fold = Fold::new();
        let mut state = None;
        for (kind, second) in [(Kind::SessionStart, 0), (Kind::ToolStart, 2)] {
            state = Some(feed(&fold, state, Input::Event(&event(kind, second))));
        }
        assert_eq!(state.as_ref().unwrap().status, Working);

        let ended = feed(&fold, state, Input::ProcessGone { at: &at(60) });
        assert_eq!(ended.status, Done);
        assert_eq!(ended.since, at(60));
        // The process dying is not the session saying something.
        assert_eq!(ended.last_event, at(2));
    }

    #[test]
    fn a_dead_process_ends_a_session_in_any_state_but_never_restates_done() {
        let fold = Fold::new();
        for status in SessionStatus::ALL {
            let ended = feed(
                &fold,
                Some(state_in(status)),
                Input::ProcessGone { at: &at(60) },
            );
            assert_eq!(ended.status, Done);
            let expected_since = if status == Done { at(0) } else { at(60) };
            assert_eq!(ended.since, expected_since, "{status} then a dead process");
        }
    }

    #[test]
    fn applying_an_event_twice_changes_nothing() {
        let fold = Fold::new();
        for status in SessionStatus::ALL {
            for kind in Kind::ALL {
                let event = event(kind.clone(), 10);
                let once = feed(&fold, Some(state_in(status)), Input::Event(&event));
                let twice = feed(&fold, Some(once.clone()), Input::Event(&event));
                assert_eq!(once, twice, "{status} + {kind} twice");
            }
        }
    }

    #[test]
    fn a_late_event_moves_the_status_but_not_the_clock() {
        let fold = Fold::new();
        let mut state = None;
        for (kind, second) in [
            (Kind::SessionStart, 0),
            (Kind::TurnStart, 10),
            (Kind::TurnEnd, 20),
        ] {
            state = Some(feed(&fold, state, Input::Event(&event(kind, second))));
        }
        let idle = state.unwrap();
        assert_eq!(idle.status, Idle);
        assert_eq!(idle.since, at(20));

        // A `tool_end` from five seconds before the turn ended, delivered after
        // it. The session is working again — there is no reordering buffer, and
        // reporting it idle forever would be worse — but neither timestamp
        // moves backwards.
        let late = feed(&fold, Some(idle), Input::Event(&event(Kind::ToolEnd, 15)));
        assert_eq!(late.status, Working);
        assert_eq!(late.since, at(20));
        assert_eq!(late.last_event, at(20));
    }

    #[test]
    fn a_working_session_goes_stale_only_once_it_has_been_quiet_long_enough() {
        let fold = Fold::new();
        let working = feed(&fold, None, Input::Event(&event(Kind::ToolStart, 10)));
        assert_eq!((working.status, &working.since), (Working, &at(10)));

        let almost = feed(&fold, Some(working.clone()), Input::Tick { now: &at(129) });
        assert_eq!(almost.status, Working, "119 seconds of quiet is not stale");
        assert_eq!(almost.since, at(10));

        let stale = feed(&fold, Some(working), Input::Tick { now: &at(130) });
        assert_eq!(stale.status, Stale, "120 seconds of quiet is stale");
        assert_eq!(stale.since, at(130));
        // A tick is not the session saying something, so the timer is not
        // restarted by having fired.
        assert_eq!(stale.last_event, at(10));

        let back = feed(
            &fold,
            Some(stale),
            Input::Event(&event(Kind::ToolStart, 200)),
        );
        assert_eq!(back.status, Working, "activity revives a stale session");
        assert_eq!(back.since, at(200));
    }

    #[test]
    fn an_observed_session_is_never_timed_out() {
        // An agent waiting on its human changes nothing for an observer to
        // notice, so silence from an observer is not evidence of silence.
        let fold = Fold::new();
        let working = feed(&fold, None, Input::Event(&observed(Kind::ToolStart, 10)));
        assert_eq!(working.source, Source::Observed);

        let later = feed(&fold, Some(working), Input::Tick { now: &at(10_000) });
        assert_eq!(later.status, Working);
        assert_eq!(later.since, at(10));
    }

    #[test]
    fn only_a_working_session_can_go_stale() {
        let fold = Fold::new();
        for status in SessionStatus::ALL {
            if status == Working {
                continue;
            }
            let next = feed(
                &fold,
                Some(state_in(status)),
                Input::Tick { now: &at(10_000) },
            );
            assert_eq!(next.status, status, "{status} after a long silence");
            assert_eq!(next.since, at(0));
        }
    }

    #[test]
    fn a_tick_from_the_past_is_not_evidence_of_silence() {
        let fold = Fold::new();
        let working = feed(&fold, None, Input::Event(&event(Kind::ToolStart, 600)));
        let next = feed(&fold, Some(working), Input::Tick { now: &at(0) });
        assert_eq!(next.status, Working);
    }

    #[test]
    fn the_stale_timeout_is_a_parameter() {
        let fold = Fold::with_stale_after(Duration::from_secs(5));
        assert_eq!(fold.stale_after(), Duration::from_secs(5));
        assert_eq!(Fold::new().stale_after(), DEFAULT_STALE_AFTER);
        assert_eq!(Fold::default(), Fold::new());

        let working = feed(&fold, None, Input::Event(&event(Kind::ToolStart, 10)));
        assert_eq!(
            feed(&fold, Some(working.clone()), Input::Tick { now: &at(14) }).status,
            Working
        );
        assert_eq!(
            feed(&fold, Some(working), Input::Tick { now: &at(15) }).status,
            Stale
        );
    }

    #[test]
    fn an_error_is_recorded_rather_than_reported_as_a_status() {
        let fold = Fold::new();
        let working = feed(&fold, None, Input::Event(&event(Kind::ToolStart, 10)));
        let failed = feed(&fold, Some(working), Input::Event(&event(Kind::Error, 20)));
        assert_eq!(failed.status, Working);
        assert_eq!(failed.since, at(10));
        assert_eq!(failed.last_error, Some(at(20)));

        // The session recovers and carries on; the error stays recorded.
        let recovered = feed(
            &fold,
            Some(failed),
            Input::Event(&event(Kind::ToolStart, 30)),
        );
        assert_eq!(recovered.last_error, Some(at(20)));

        // A reused session id is a new life and inherits none of it.
        let reused = feed(
            &fold,
            Some(recovered),
            Input::Event(&event(Kind::SessionStart, 40)),
        );
        assert_eq!((reused.status, reused.last_error), (Starting, None));
        assert_eq!(reused.since, at(40));
    }

    #[test]
    fn the_source_of_a_session_is_the_source_of_what_it_last_said() {
        let fold = Fold::new();
        let seen = feed(&fold, None, Input::Event(&observed(Kind::ToolStart, 10)));
        assert_eq!(seen.source, Source::Observed);
        let told = feed(&fold, Some(seen), Input::Event(&event(Kind::ToolEnd, 11)));
        assert_eq!(told.source, Source::Hook);
    }

    #[test]
    fn activity_is_the_six_kinds_that_mean_the_agent_is_doing_something() {
        for kind in Kind::ALL {
            let expected = matches!(
                kind,
                Kind::TurnStart
                    | Kind::ToolStart
                    | Kind::ToolEnd
                    | Kind::SubagentStart
                    | Kind::SubagentEnd
                    | Kind::Compact
            );
            assert_eq!(is_activity(&kind), expected, "{kind}");
        }
        assert!(!is_activity(&Kind::from("invented_later")));
    }
}
