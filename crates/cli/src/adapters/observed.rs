//! An observation of a terminal, normalized by whoever made it.
//!
//! A coding agent's hook payload is read by a mapping manifest, which says what
//! that agent's events mean. This reads the other kind of payload the client
//! accepts: fields already in the bus's own vocabulary, sent by a program that
//! was watching a terminal and formed an opinion about what was happening in
//! it — which is why it is code and not a manifest, since a manifest here would
//! only describe this crate to itself. A screen reader, a multiplexer
//! plugin, a script tailing a log, a person with a shell script — this module
//! never learns which, and nothing downstream of it learns that anything was
//! watched at all. It reads a payload; that is the whole interface.
//!
//! Two fields are required and everything else degrades:
//!
//! - `kind` must be one this build knows. An observation is a guess, and a guess
//!   spelled in a vocabulary the receiver does not share is not worth carrying.
//! - `correlation` must be there, because it is the only thing that says what
//!   was observed. An observation nobody can attribute would land in the table
//!   as a session that never existed, so it is dropped instead.
//!
//! An `agent`, if the payload names one, must be usable as an identifier; an
//! observation that names an unusable one is dropped rather than silently
//! relabelled, since relabelling would attribute somebody's session to the wrong
//! agent. Omitting the field is always fine and is the ordinary case.
//!
//! `source` on the way out is always `observed`, whatever the payload said.
//! Provenance is a promise made to the receiver — an inferred status presented
//! as an authoritative one is how trust in the whole stream dies — and a payload
//! allowed to set its own would let anything at all claim to be an agent
//! speaking for itself.

use agentbus_protocol::{Agent, Kind, Source, UnstampedEvent, observed_session_id};
use serde_json::{Map, Value};

/// The value of a string field, if it is there and is a string.
///
/// Every field read below is optional on read, whatever the sender's own schema
/// promises: a payload arrives from another program and that program moves, so a
/// field that is missing or has changed type must degrade the event rather than
/// lose it.
fn string<'a>(payload: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    payload.get(key)?.as_str()
}

/// Normalizes one observation.
///
/// `None` means the payload is not an observation: it is not an object, it names
/// a kind that is not in the closed set, or it says nothing about what was
/// observed.
pub fn normalize(raw: &Value) -> Option<UnstampedEvent> {
    let payload = raw.as_object()?;

    let kind = Kind::from(string(payload, "kind")?);
    if !kind.is_known() {
        return None;
    }
    let correlation = string(payload, "correlation").filter(|slot| !slot.is_empty())?;

    // An observer knows which terminal it was watching and rarely anything more,
    // so both of the fields that make up a session's identity have a fallback:
    // the agent it could not identify, and the id no agent told it. Synthesizing
    // the session from the correlation is what keeps two observations of one
    // terminal from being two sessions.
    let agent = match string(payload, "agent") {
        Some(named) => Agent::new(named).ok()?,
        None => Agent::unknown(),
    };
    let session = string(payload, "session")
        .map(str::to_owned)
        .unwrap_or_else(|| observed_session_id(correlation));

    let mut event = UnstampedEvent::new(agent, session, kind)
        .with_source(Source::Observed)
        .with_correlation(correlation);
    if let Some(cwd) = string(payload, "cwd") {
        event = event.with_cwd(cwd);
    }
    if let Some(detail) = payload.get("detail").and_then(Value::as_object) {
        event = event.with_detail(detail.clone());
    }
    // The payload is not carried verbatim the way a hook's is: it *is* the
    // normalized event, so keeping a copy would be keeping the same fields
    // twice. An observer with something to show its work with says so here.
    if let Some(evidence) = payload.get("raw") {
        event = event.with_raw(evidence.clone());
    }
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Builds an agent id from a literal, which is what every one of these is.
    fn agent(name: &str) -> Agent {
        Agent::new(name).expect("a test's own agent id is a valid one")
    }

    /// The shape this path documents, in full.
    fn documented() -> Value {
        json!({
            "kind": "blocked",
            "correlation": "w9:p3",
            "agent": "unknown",
            "cwd": "/x",
            "detail": {"confidence": "low"},
        })
    }

    #[test]
    fn the_documented_payload_normalizes() {
        let event = normalize(&documented()).expect("that should have been an event");
        assert_eq!(event.agent, Agent::unknown());
        assert_eq!(event.session, "observed:w9:p3");
        assert_eq!(event.kind, Kind::Blocked);
        assert_eq!(event.source, Source::Observed);
        assert_eq!(event.correlation.as_deref(), Some("w9:p3"));
        assert_eq!(event.cwd.as_deref(), Some("/x"));
        assert_eq!(event.detail.as_ref().unwrap()["confidence"], json!("low"));
        assert!(event.raw.is_none());
    }

    #[test]
    fn the_smallest_observation_is_a_kind_and_what_it_is_about() {
        let event = normalize(&json!({"kind": "turn_end", "correlation": "anything"}))
            .expect("that should have been an event");
        assert_eq!(event.agent, Agent::unknown());
        assert_eq!(event.session, "observed:anything");
        assert_eq!(event.cwd, None);
        assert_eq!(event.detail, None);
    }

    #[test]
    fn an_observation_that_names_an_agent_and_a_session_is_believed() {
        let event = normalize(&json!({
            "kind": "blocked", "correlation": "w9:p3",
            "agent": "claude", "session": "abc123",
        }))
        .expect("that should have been an event");
        assert_eq!(event.agent, agent("claude"));
        assert_eq!(event.session, "abc123");
        // Still an observation. Naming the session an agent reported does not
        // make the claim the agent's.
        assert_eq!(event.source, Source::Observed);
    }

    #[test]
    fn an_observation_nobody_can_attribute_is_dropped() {
        assert!(normalize(&json!({"kind": "blocked"})).is_none());
        assert!(normalize(&json!({"kind": "blocked", "correlation": ""})).is_none());
        assert!(normalize(&json!({"kind": "blocked", "correlation": 3})).is_none());
    }

    #[test]
    fn a_kind_outside_the_closed_set_is_dropped() {
        assert!(normalize(&json!({"kind": "hungry", "correlation": "w9:p3"})).is_none());
        assert!(normalize(&json!({"correlation": "w9:p3"})).is_none());
    }

    #[test]
    fn the_payload_cannot_claim_to_be_a_hook() {
        let event = normalize(&json!({
            "kind": "blocked", "correlation": "w9:p3", "source": "hook",
        }))
        .expect("that should have been an event");
        assert_eq!(event.source, Source::Observed);
    }

    #[test]
    fn an_observer_may_show_its_working() {
        let event = normalize(&json!({
            "kind": "blocked", "correlation": "w9:p3",
            "raw": {"matched": "Do you want to proceed?"},
        }))
        .expect("that should have been an event");
        assert_eq!(
            event.raw.as_ref().unwrap()["matched"],
            json!("Do you want to proceed?")
        );
    }

    #[test]
    fn what_is_not_an_object_is_not_an_observation() {
        for payload in [json!(null), json!("blocked"), json!([1, 2]), json!(7)] {
            assert!(normalize(&payload).is_none());
        }
    }

    #[test]
    fn every_kind_in_the_closed_set_is_accepted() {
        for kind in Kind::ALL {
            let payload = json!({"kind": kind.as_str(), "correlation": "w9:p3"});
            assert_eq!(normalize(&payload).unwrap().kind, kind);
        }
    }
}
