//! What Claude Code's mapping decides that no other agent's does.
//!
//! Every recorded payload of every agent is replayed against its expected kind
//! and detail in `hook_mapping.rs`, and the envelope questions that are the same
//! everywhere — no session, no working directory, an event nothing maps, a
//! payload that is not an object — are asked there once for all three agents.
//! What is left is this agent's own, and it is asked here: the discrimination
//! that turns a general-purpose notification into a block, and the reading of a
//! field only this agent fills.
//!
//! Nothing reaches for a mapping by hand. The store is asked what one payload
//! means, which is the same question the emit path asks, so what these tests pin
//! is the answer a machine with nothing installed on it gives.

use std::fs;
use std::path::PathBuf;

use agentbus_detect::{ManifestStore, StorePaths};
use agentbus_protocol::{Kind, UnstampedEvent};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The agent whose mapping this file is about.
const AGENT: &str = "claude";

/// The notification types that mean somebody is being asked for a decision.
///
/// Written out here rather than read from the mapping that produced the answer,
/// because reading it from there would assert nothing. Revising this list and
/// the mapping's together is the whole of revising the narrowest decision in the
/// mapping — the one place it says what the agent *meant* rather than repeating
/// what the agent said.
const BLOCKING: &[&str] = &["permission_prompt", "elicitation_dialog"];

/// Everything else the agent notifies about. Idleness above all, which the end
/// of a turn already reports, and whatever gets invented next.
const QUIET: &[&str] = &["idle_prompt", "something_invented_later"];

/// A store reading nothing but the mappings that ship inside the library, under
/// a home directory of its own so that whatever the machine running the tests
/// keeps in its own manifest directories cannot answer here.
fn bundled() -> (TempDir, ManifestStore) {
    let home = TempDir::new().expect("a temporary directory");
    let store = ManifestStore::open(StorePaths::rooted(home.path()));
    (home, store)
}

/// One recorded payload, parsed.
fn fixture(stem: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks")
        .join(AGENT)
        .join(format!("{stem}.json"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// What the bundled mapping makes of one payload.
fn normalized(store: &ManifestStore, raw: &Value) -> Option<UnstampedEvent> {
    store.normalize_hook(AGENT, raw)
}

/// A payload with one field taken out of it.
fn without(payload: &Value, field: &str) -> Value {
    let mut payload = payload.clone();
    payload.as_object_mut().expect("an object").remove(field);
    payload
}

#[test]
fn only_the_notification_types_that_mean_a_question_report_a_block() {
    let (_home, store) = bundled();
    let mut raw = fixture("Notification");

    for blocking in BLOCKING {
        raw["notification_type"] = json!(blocking);
        let event = normalized(&store, &raw).unwrap_or_else(|| panic!("{blocking} produced none"));
        assert_eq!(event.kind, Kind::Blocked, "{blocking}");
        // What the block was about travels with it, because a person deciding
        // which session to go to wants to know what it is asking.
        assert_eq!(
            event.detail.unwrap()["notification_type"],
            json!(blocking),
            "{blocking}",
        );
    }

    for quiet in QUIET {
        raw["notification_type"] = json!(quiet);
        assert!(normalized(&store, &raw).is_none(), "{quiet}");
    }

    // A notification that does not say what it is is not one of the ones that
    // count, so it is passed over rather than guessed at.
    assert!(normalized(&store, &without(&raw, "notification_type")).is_none());
}

#[test]
fn a_failed_tool_call_is_still_the_end_of_a_tool_call() {
    // This agent is the only one here that says whether a call went wrong, and
    // it says so in whatever shape the hook's author reached for. Anything
    // present and non-empty is a failure; the shapes that conventionally mean
    // "nothing went wrong" are not. Either way the kind is the same: the status
    // fold reads it as activity, and the failure is an extra beside the name.
    let (_home, store) = bundled();
    let mut raw = fixture("PostToolUse");

    for reported in [json!(true), json!("permission denied"), json!({"code": 1})] {
        raw["tool_error"] = reported.clone();
        let event = normalized(&store, &raw).expect("an event");
        assert_eq!(event.kind, Kind::ToolEnd, "{reported}");
        assert_eq!(
            Value::Object(event.detail.unwrap()),
            json!({"tool": "Bash", "error": true}),
            "{reported}",
        );
    }

    for quiet in [json!(null), json!(false), json!("")] {
        raw["tool_error"] = quiet.clone();
        let event = normalized(&store, &raw).expect("an event");
        assert_eq!(event.kind, Kind::ToolEnd, "{quiet}");
        assert_eq!(
            Value::Object(event.detail.unwrap()),
            json!({"tool": "Bash"}),
            "{quiet}",
        );
    }

    let event = normalized(&store, &without(&raw, "tool_error")).expect("an event");
    assert_eq!(
        Value::Object(event.detail.unwrap()),
        json!({"tool": "Bash"}),
    );
}

#[test]
fn a_tool_event_without_a_tool_name_still_reports_the_call() {
    // The call happened whether or not the payload says what was called, and a
    // dropped event would leave a session looking idle in the middle of one.
    let (_home, store) = bundled();
    let raw = without(&fixture("PreToolUse"), "tool_name");
    let event = normalized(&store, &raw).expect("an event");
    assert_eq!(event.kind, Kind::ToolStart);
    assert_eq!(event.detail, None);
}

#[test]
fn the_events_this_agent_has_and_the_bus_does_not_model_produce_nothing() {
    // Real events of this agent's, none of which any other agent has a word
    // for. The set of kinds is closed so that a subscriber can be written
    // against all of them; giving these ones a kind would make the envelope a
    // claim about agents that cannot make it.
    let (_home, store) = bundled();
    let mut raw = fixture("SessionStart");
    for unmapped in [
        "FileChanged",
        "MessageDisplay",
        "ConfigChange",
        "WorktreeCreate",
    ] {
        raw["hook_event_name"] = json!(unmapped);
        assert!(normalized(&store, &raw).is_none(), "{unmapped}");
    }
}
