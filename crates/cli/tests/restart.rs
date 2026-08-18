//! Killing a subscriber and starting another one.
//!
//! This is the thing the split between the bus and the things that watch it buys,
//! and so it is a standing regression test rather than a one-off: whatever is
//! watching may be restarted at any moment, and the session it was watching does
//! not stop or restart with it. What the new subscriber reads first has to be
//! current state — including everything that happened while nothing was
//! listening — with no gap in the stream that follows and no session reported
//! twice for having been seen twice.
//!
//! It runs once for each way of subscribing, because the bus is not told which
//! of them reconnected and must not behave as though it knew.

mod common;

use agentbus_protocol::SessionStatus;
use common::{Bus, Subscriber};

/// The agent whose recorded payloads this scenario is scripted from.
const AGENT: &str = "claude";

/// One way of subscribing, and what to call it when it is the one that failed.
type Way = (&'static str, fn(&Bus) -> Subscriber);

/// The two ways to subscribe: a program reading the socket, and a command whose
/// stdout somebody is reading.
const WAYS: [Way; 2] = [
    ("the socket", Bus::attach),
    ("agentbus subscribe", Bus::subscribe),
];

#[test]
fn a_subscriber_that_comes_back_is_told_everything_it_missed() {
    for (way, connect) in WAYS {
        let bus = Bus::start();
        let mut watching = connect(&bus);
        watching.snapshot();

        // A session that is under way, and watched while it gets there.
        for payload in ["SessionStart", "UserPromptSubmit"] {
            bus.emit(AGENT, None, &common::payload(AGENT, payload));
        }
        watching.event("the session starting");
        let last_seen = watching.event("the turn starting");
        let session = last_seen.session.clone();

        // Whatever was watching goes away, and the session carries on without it.
        watching.close();
        for payload in ["PreToolUse", "Notification"] {
            bus.emit(AGENT, None, &common::payload(AGENT, payload));
        }
        bus.wait_for(&session, SessionStatus::Blocked);

        let mut again = connect(&bus);
        let snapshot = again.snapshot();

        assert!(
            snapshot.seq > last_seen.seq,
            "{way}: the snapshot is from before the events it missed: {snapshot:?}"
        );
        assert_eq!(
            snapshot.sessions.len(),
            1,
            "{way}: one session came back as {} rows: {:?}",
            snapshot.sessions.len(),
            snapshot.sessions
        );
        // The state it arrived in while nobody was watching, not the state it was
        // in when the last subscriber left.
        let entry = common::session_of(&snapshot, &session);
        assert_eq!(
            entry.status,
            SessionStatus::Blocked,
            "{way}: it came back to {}",
            entry.status
        );

        // And the stream is a continuation of that snapshot rather than a
        // replay of anything before it.
        bus.emit(AGENT, None, &common::payload(AGENT, "Stop"));
        let next = again.event("the turn ending");
        assert_eq!(next.session, session, "{way}");
        assert!(
            next.seq > snapshot.seq,
            "{way}: {next:?} does not follow {snapshot:?}"
        );
        assert_eq!(
            common::session_of(&bus.snapshot(), &session).status,
            SessionStatus::Idle,
            "{way}"
        );
    }
}
