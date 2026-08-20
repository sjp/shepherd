//! What Codex CLI's mapping decides that no other agent's does.
//!
//! Every recorded payload of every agent is replayed against its expected kind
//! and detail in `hook_mapping.rs`, and the envelope questions that are the same
//! everywhere — no session, no working directory, an event nothing maps, a
//! payload that is not an object — are asked there once for all three agents.
//! What is left is this agent's own, and it is asked here: the two sides of a
//! compaction, and a block this agent asks for outright rather than being
//! discriminated out of something broader.
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
const AGENT: &str = "codex";

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
fn the_two_sides_of_a_compaction_are_one_kind_told_apart_by_the_detail() {
    // This agent fires before and after. What happened is the same thing both
    // times, and a subscriber that only cares that a compaction occurred should
    // not have to know there are two spellings of it — so the side is in the
    // detail rather than in a kind of its own.
    let (_home, store) = bundled();
    let before = normalized(&store, &fixture("PreCompact")).expect("an event");
    let after = normalized(&store, &fixture("PostCompact")).expect("an event");

    assert_eq!(before.kind, Kind::Compact);
    assert_eq!(before.kind, after.kind);
    assert_ne!(before.detail, after.detail);
}

#[test]
fn a_permission_request_reports_a_block_whether_or_not_it_names_a_tool() {
    // Unlike the agent that has to be discriminated, this one has an event that
    // means exactly what it says. The tool being asked about is the whole of
    // what a person needs in order to answer, so it travels when the payload
    // names one — and the block still stands when it does not.
    let (_home, store) = bundled();
    let named = normalized(&store, &fixture("PermissionRequest")).expect("an event");
    assert_eq!(named.kind, Kind::Blocked);
    assert!(named.detail.expect("a named tool").contains_key("tool"));

    let raw = without(&fixture("PermissionRequest"), "tool_name");
    let event = normalized(&store, &raw).expect("an event");
    assert_eq!(event.kind, Kind::Blocked);
    assert_eq!(event.detail, None);
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
fn the_events_another_agent_has_and_this_one_does_not_produce_nothing() {
    // Answering for these would be reporting this agent's hooks as sending
    // something they never send.
    let (_home, store) = bundled();
    let mut raw = fixture("SessionStart");
    for elsewhere in ["Notification", "StopFailure"] {
        raw["hook_event_name"] = json!(elsewhere);
        assert!(normalized(&store, &raw).is_none(), "{elsewhere}");
    }
}
