//! What a running bus does about a session whose process is no longer there.
//!
//! An agent that is killed, crashes, or is interrupted mid-turn says nothing on
//! its way out: there is no hook for being killed. So the one thing that can
//! tell a session that ended from one that went quiet is the process table, and
//! these tests are about the join between the two — a session that spoke from a
//! terminal something was seen running in ends when that process does, and one
//! nothing was ever seen running for is left exactly where it is.
//!
//! The process table is written as files and the daemon is the shipped binary
//! pointed at it, so "the agent was killed" is a directory being removed
//! between two of the daemon's own looks.
//!
//! The correlation values are strings somebody could have typed into
//! `AGENTBUS_PANE`, because that is all the bus knows them to be.

mod common;

use std::time::{Duration, Instant};

use agentbus_protocol::{Kind, SessionStatus, StreamLine};
use common::tree::{Tree, running, shell};
use common::{Bus, PATIENCE, foreground_of, payload, session_of};

/// How long a test waits before concluding that a session is not going to be
/// ended, several times the daemon's own look at the process table.
const SILENCE: Duration = Duration::from_secs(3);

/// The session in the recorded Claude Code payloads.
const CLAUDE_SESSION: &str = "9f2c1b7a-4d5e-4a91-8c33-1e6b0d7f2a48";

/// The session in the recorded Codex payloads.
const CODEX_SESSION: &str = "6f0b3a1d-2c48-4e77-b915-8ad4c7e30f52";

/// A shell carrying `w9:p3` with an agent running in front of it.
fn one_agent_running() -> Tree {
    let tree = Tree::new();
    tree.write(&shell(100, "w9:p3", 200));
    tree.write(&running(200, "claude", vec!["claude"]));
    tree
}

/// Runs the two hook payloads that make a working session, from a terminal the
/// bus may or may not be able to see into.
fn works(bus: &Bus, agent: &str, pane: Option<&str>) {
    for hook in ["SessionStart", "PreToolUse"] {
        bus.emit(agent, pane, &payload(agent, hook));
    }
}

/// Waits until the bus says `pid` is what is running in `correlation`.
fn wait_for_pid(bus: &Bus, correlation: &str, pid: u32) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let snapshot = bus.wait_for_foreground(correlation);
        let observed = foreground_of(&snapshot, correlation).pid;
        if observed == pid {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{correlation} is running {observed} rather than {pid}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Fails if the bus ever reports `session` as over, and returns what it does
/// say after `patience`.
///
/// The waiting is the measurement: a session is reaped within a look or two of
/// the process table, so a bus that has said nothing for several of them was
/// never going to.
fn never_over(bus: &Bus, session: &str, patience: Duration) -> SessionStatus {
    let deadline = Instant::now() + patience;
    loop {
        let status = session_of(&bus.snapshot(), session).status;
        assert_ne!(
            status,
            SessionStatus::Done,
            "{session} was ended by something that never saw its process"
        );
        if Instant::now() >= deadline {
            return status;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_session_whose_process_vanished_is_over_without_anything_having_said_so() {
    let tree = one_agent_running();
    let bus = Bus::watching(&tree.root());
    bus.wait_for_foreground("w9:p3");
    works(&bus, "claude", Some("w9:p3"));
    bus.wait_for(CLAUDE_SESSION, SessionStatus::Working);
    let mut subscriber = bus.subscribe();
    subscriber.snapshot();

    // What being killed looks like from outside: the process is simply not
    // there the next time anything looks.
    tree.remove(200);

    bus.wait_for(CLAUDE_SESSION, SessionStatus::Done);
    while let Some(line) = subscriber.quiet_for(Duration::from_millis(500)) {
        if let StreamLine::Event(event) = line {
            assert_ne!(
                event.kind,
                Kind::SessionEnd,
                "the bus invented an event to end a session with"
            );
        }
    }
}

#[test]
fn a_session_that_was_only_suspended_goes_stale_rather_than_over() {
    let tree = one_agent_running();
    // Short enough that the difference between "we lost it" and "it is over"
    // is a few seconds of test rather than the two minutes a bus defaults to.
    let bus = Bus::watching_with(&tree.root(), &["--stale-secs", "1"]);
    bus.wait_for_foreground("w9:p3");
    works(&bus, "claude", Some("w9:p3"));

    // What Ctrl-Z looks like from outside: the shell has its terminal back and
    // the process it was running is still in the table.
    tree.write(&shell(100, "w9:p3", 100));

    bus.wait_for(CLAUDE_SESSION, SessionStatus::Stale);
    assert_eq!(
        never_over(&bus, CLAUDE_SESSION, SILENCE),
        SessionStatus::Stale,
        "a suspended agent is alive and not talking, which is what stale means"
    );
}

#[test]
fn a_session_nothing_was_ever_seen_running_for_is_never_reaped() {
    let tree = one_agent_running();
    let bus = Bus::watching(&tree.root());
    bus.wait_for_foreground("w9:p3");
    // A terminal the process table says nothing about: an agent under a nested
    // multiplexer, started from a script, or on the other side of something
    // this daemon cannot see into.
    works(&bus, "claude", Some("w9:p4"));
    bus.wait_for(CLAUDE_SESSION, SessionStatus::Working);

    tree.remove(200);
    tree.remove(100);

    assert_eq!(
        never_over(&bus, CLAUDE_SESSION, SILENCE),
        SessionStatus::Working
    );
}

#[test]
fn a_session_that_named_no_terminal_is_never_reaped() {
    let tree = one_agent_running();
    let bus = Bus::watching(&tree.root());
    bus.wait_for_foreground("w9:p3");
    works(&bus, "claude", None);
    bus.wait_for(CLAUDE_SESSION, SessionStatus::Working);

    tree.remove(200);

    assert_eq!(
        never_over(&bus, CLAUDE_SESSION, SILENCE),
        SessionStatus::Working
    );
}

#[test]
fn every_session_speaking_from_one_process_ends_when_it_does() {
    let tree = one_agent_running();
    let bus = Bus::watching(&tree.root());
    bus.wait_for_foreground("w9:p3");
    // One terminal, two sessions: whatever is in front of it is the process
    // both of them are speaking from.
    works(&bus, "claude", Some("w9:p3"));
    works(&bus, "codex", Some("w9:p3"));
    bus.wait_for(CLAUDE_SESSION, SessionStatus::Working);
    bus.wait_for(CODEX_SESSION, SessionStatus::Working);

    tree.remove(200);

    bus.wait_for(CLAUDE_SESSION, SessionStatus::Done);
    bus.wait_for(CODEX_SESSION, SessionStatus::Done);
}

#[test]
fn an_observation_of_a_terminal_ends_with_the_process_it_was_watching() {
    let tree = one_agent_running();
    let bus = Bus::watching(&tree.root());
    bus.wait_for_foreground("w9:p3");
    // Nobody has to notice the process died for this to be cleaned up, which is
    // the point: an observer that was killed along with what it was watching
    // never gets to say anything either.
    bus.observe(br#"{"kind": "tool_start", "correlation": "w9:p3"}"#);
    bus.wait_for("observed:w9:p3", SessionStatus::Working);

    tree.remove(200);

    bus.wait_for("observed:w9:p3", SessionStatus::Done);
}

#[test]
fn a_session_ends_with_the_process_it_last_spoke_from() {
    let tree = one_agent_running();
    let bus = Bus::watching(&tree.root());
    wait_for_pid(&bus, "w9:p3", 200);
    works(&bus, "claude", Some("w9:p3"));

    // The agent was left and started again, so the terminal is running a
    // different process, and the session says so by speaking from it.
    tree.write(&running(300, "claude", vec!["claude", "--resume"]));
    tree.write(&shell(100, "w9:p3", 300));
    wait_for_pid(&bus, "w9:p3", 300);
    works(&bus, "claude", Some("w9:p3"));

    tree.remove(200);
    assert_eq!(
        never_over(&bus, CLAUDE_SESSION, SILENCE),
        SessionStatus::Working,
        "the process the session had already left ended it"
    );

    tree.remove(300);

    bus.wait_for(CLAUDE_SESSION, SessionStatus::Done);
}
