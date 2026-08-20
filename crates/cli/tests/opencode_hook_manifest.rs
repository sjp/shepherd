//! What OpenCode's mapping decides that no other agent's does.
//!
//! Every recorded payload of every agent is replayed against its expected kind
//! and detail in `hook_mapping.rs`, and the envelope questions that are the same
//! everywhere — no session, no working directory, an event nothing maps, a
//! payload that is not an object — are asked there once for all three agents.
//! What is left is this agent's own, and it is asked here: two spellings of a
//! session, a start of a turn that is deliberately absent, and both sides of a
//! block.
//!
//! This agent has no hook commands. It loads plugins, hands each one an event
//! object and drops whatever the handler returns, so the payloads replayed here
//! are the ones the plugin this program installs writes: the event's `type`, the
//! directory the plugin was loaded for, and the event's own properties spread
//! out beside them.
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
const AGENT: &str = "opencode";

/// A store reading nothing but the mappings that ship inside the library, under
/// a home directory of its own so that whatever the machine running the tests
/// keeps in its own manifest directories cannot answer here.
fn bundled() -> (TempDir, ManifestStore) {
    let home = TempDir::new().expect("a temporary directory");
    let store = ManifestStore::open(StorePaths::rooted(home.path()));
    (home, store)
}

/// The directory this agent's payloads live in.
fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks")
        .join(AGENT)
}

/// One recorded payload, parsed.
fn fixture(stem: &str) -> Value {
    let path = dir().join(format!("{stem}.json"));
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
fn nothing_reports_the_start_of_a_turn_because_the_agent_has_no_such_event() {
    // There is no user-prompt event here and nothing invents one. The bus does
    // not put an event on the wire that no agent produced, and the client that
    // would have to invent it is a one-shot process that sees a single event
    // and nothing before it — so it could only guess.
    let (_home, store) = bundled();
    let mut replayed = 0;
    for entry in fs::read_dir(dir()).expect("the fixture directory") {
        let path = entry.expect("a directory entry").path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        replayed += 1;
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let text = fs::read_to_string(&path).expect("a payload");
        let raw: Value = serde_json::from_str(&text).expect("a payload is JSON");
        if let Some(event) = normalized(&store, &raw) {
            assert_ne!(event.kind, Kind::TurnStart, "{stem}");
        }
    }
    assert!(replayed > 0, "no payloads in {}", dir().display());
}

#[test]
fn a_session_identified_the_other_way_round_is_still_a_session() {
    // The properties of one of this agent's events are its own and this program
    // does not control them, so both spellings are accepted: it costs a
    // comparison and saves every event in a session from being dropped over a
    // convention that changed.
    let (_home, store) = bundled();
    let recorded = fixture("session.created");
    let session = recorded["sessionID"]
        .as_str()
        .expect("a recorded session id")
        .to_owned();

    let mut raw = without(&recorded, "sessionID");
    raw["session_id"] = json!(session);

    let event = normalized(&store, &raw).expect("an event");
    assert_eq!(event.session, recorded["sessionID"].as_str().unwrap());
}

#[test]
fn a_tool_event_without_a_tool_name_still_reports_the_call() {
    // The call happened whether or not the payload says what was called, and a
    // dropped event would leave a session looking idle in the middle of one.
    let (_home, store) = bundled();
    let raw = without(&fixture("tool.execute.before"), "tool");
    let event = normalized(&store, &raw).expect("an event");
    assert_eq!(event.kind, Kind::ToolStart);
    assert_eq!(event.detail, None);
}

#[test]
fn both_sides_of_a_block_come_from_the_agent_rather_than_being_inferred() {
    // This is the only supported agent that says when a permission was answered
    // as well as when one was asked for, which makes it the honest case the
    // other two are measured against: their unblock is inferred by the status
    // fold from the activity that follows.
    let (_home, store) = bundled();
    let asked = normalized(&store, &fixture("permission.updated")).expect("an event");
    let answered = normalized(&store, &fixture("permission.replied")).expect("an event");

    assert_eq!(asked.kind, Kind::Blocked);
    assert!(asked.detail.expect("a named tool").contains_key("tool"));
    assert_eq!(answered.kind, Kind::Unblocked);

    // The block still stands when the payload does not say what it is about.
    let raw = without(&fixture("permission.updated"), "tool");
    let event = normalized(&store, &raw).expect("an event");
    assert_eq!(event.kind, Kind::Blocked);
    assert_eq!(event.detail, None);
}

#[test]
fn the_events_this_agent_has_and_the_bus_does_not_model_produce_nothing() {
    // Message updates, file edits and a file watcher, none of which any other
    // agent has a word for. The set of kinds is closed so that a subscriber can
    // be written against all of them; giving these ones a kind would make the
    // envelope a claim about agents that cannot make it.
    let (_home, store) = bundled();
    let mut raw = fixture("session.created");
    for unmapped in [
        "message.updated",
        "message.removed",
        "message.part.updated",
        "file.edited",
        "file.watcher.updated",
    ] {
        raw["type"] = json!(unmapped);
        assert!(normalized(&store, &raw).is_none(), "{unmapped}");
    }
}
