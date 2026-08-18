//! Recorded hook payloads, replayed through the whole bus.
//!
//! This is how the pipeline stays tested without installing a coding agent on a
//! build machine: the payloads next door were captured from the agents once, and
//! every run pushes them through the real client, a real daemon and a real
//! subscriber, checking the status the bus arrives at after each one. Nothing
//! here names an agent — the recordings are discovered — so a new agent's
//! directory joins the replay without this file being touched.

mod common;

use std::time::{Duration, Instant};

use common::Bus;

/// The correlation the payloads are emitted with. Opaque to everything it passes
/// through, and here only because a hook installed in a real terminal always has
/// one.
const PANE: &str = "w1:p1";

/// How long an event may take to travel from the start of the emitting process
/// to a subscriber.
///
/// The promise is that a session appears within a second of the agent reporting
/// something. Half of that is the budget the tests hold the pipeline to, so that
/// a machine having a bad day still fails honestly rather than at the edge of
/// the promise.
const BUDGET: Duration = Duration::from_millis(500);

#[test]
fn every_recorded_session_reaches_the_statuses_it_was_recorded_with() {
    for recording in common::recordings() {
        let bus = Bus::start();
        let mut subscriber = bus.attach();
        subscriber.snapshot();

        for step in &recording.steps {
            bus.emit(&recording.agent, Some(PANE), &step.read());
            // The daemon folds an event before it publishes it, so a step that
            // has reached the stream has already reached the snapshot: what
            // follows is an assertion about the fold, never a race with it.
            let event = subscriber.event(&format!("{} {}", recording.agent, step.name()));
            let Some(expected) = step.expected else {
                continue;
            };
            let snapshot = bus.snapshot();
            let session = common::session_of(&snapshot, &event.session);
            assert_eq!(
                session.status,
                expected,
                "{} {} left the session {} instead",
                recording.agent,
                step.name(),
                session.status
            );
        }
    }
}

#[test]
fn a_hook_event_reaches_a_subscriber_while_someone_is_still_looking() {
    let recording = common::recordings().swap_remove(0);
    let step = recording.steps.first().expect("a recording with no steps");
    let bus = Bus::start();
    let mut subscriber = bus.attach();
    subscriber.snapshot();

    // Timed from before the process exists, because the process starting is part
    // of what a hook costs its agent and part of what the promise covers.
    let started = Instant::now();
    bus.emit(&recording.agent, Some(PANE), &step.read());
    let event = subscriber.event(&step.name());
    let took = started.elapsed();

    assert_eq!(event.correlation.as_deref(), Some(PANE));
    assert!(
        took < BUDGET,
        "{} took {took:?} to reach a subscriber",
        step.name()
    );
}
