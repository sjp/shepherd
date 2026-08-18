//! Turning a payload from outside into the bus's envelope.
//!
//! There is one module per shape of payload this client can be handed — one per
//! agent whose hooks it understands, plus [`observed`] for the fields a program
//! that watched a terminal sends instead — and each is a single pure function
//! from a `serde_json::Value` to an optional unstamped event. Purity is the
//! point: these functions run inside somebody's coding agent, on the emit path,
//! where the budget is a few milliseconds and a panic is a bug in their editor.
//! Having no I/O to do — no clock, no environment, no socket — is what makes
//! them testable by replaying captured payloads, and what keeps the risky part
//! of the emit path (deciding what an event *means*) separate from the part that
//! has to survive contact with the filesystem.
//!
//! A payload that produces no event yields `None` rather than an error. Agents
//! emit far more than the bus normalizes, and "nothing to say about this one" is
//! the ordinary case, not a failure.

pub mod claude;
pub mod observed;

use serde_json::{Map, Value};

/// The value of a string field, if it is there and is a string.
///
/// Every field any of these modules reads is optional on read, whatever the
/// payload's own schema promises: a payload is somebody else's schema and it
/// moves, so a field that is missing or has changed type must degrade the event
/// rather than lose it.
fn string<'a>(payload: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    payload.get(key)?.as_str()
}
