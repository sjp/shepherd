//! Replaying OpenCode plugin payloads through the adapter.
//!
//! The payloads in `tests/fixtures/hooks/opencode` stand in for what the
//! installed plugin writes to the emit command's stdin, and the table below pins
//! what each of them must normalize to. The table and the directory must agree
//! in both directions: a payload nobody has said what to expect from is as much
//! a hole as an expectation with no payload behind it.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use agentbus_cli::adapters::opencode;
use agentbus_protocol::{Agent, Kind, Source};
use serde_json::{Value, json};

/// Builds an agent id from a literal, which is what every one of these is.
fn agent(name: &str) -> Agent {
    Agent::new(name).expect("a test's own agent id is a valid one")
}

/// The event a fixture must produce.
struct Expected {
    /// The fixture's file name, without its extension.
    stem: &'static str,
    /// The normalized kind, or nothing where the payload deliberately produces
    /// no event at all.
    kind: Option<Kind>,
    /// The detail, built when the assertion runs because a JSON value cannot be
    /// a constant. `None` where the mapping carries no extras.
    detail: Option<fn() -> Value>,
}

/// What each payload in the directory normalizes to.
const EXPECTED: &[Expected] = &[
    Expected {
        stem: "session.created",
        kind: Some(Kind::SessionStart),
        detail: None,
    },
    Expected {
        stem: "session.deleted",
        kind: Some(Kind::SessionEnd),
        detail: None,
    },
    Expected {
        stem: "session.idle",
        kind: Some(Kind::TurnEnd),
        detail: None,
    },
    // The same event with the session spelled the other way round.
    Expected {
        stem: "session.idle-session_id",
        kind: Some(Kind::TurnEnd),
        detail: None,
    },
    Expected {
        stem: "session.compacted",
        kind: Some(Kind::Compact),
        detail: None,
    },
    Expected {
        stem: "session.error",
        kind: Some(Kind::Error),
        detail: None,
    },
    Expected {
        stem: "tool.execute.before",
        kind: Some(Kind::ToolStart),
        detail: Some(|| json!({"tool": "bash"})),
    },
    Expected {
        stem: "tool.execute.after",
        kind: Some(Kind::ToolEnd),
        detail: Some(|| json!({"tool": "bash"})),
    },
    Expected {
        stem: "permission.updated",
        kind: Some(Kind::Blocked),
        detail: Some(|| json!({"tool": "edit"})),
    },
    Expected {
        stem: "permission.replied",
        kind: Some(Kind::Unblocked),
        detail: None,
    },
    // One of the events this agent reports that no other agent has a word for.
    Expected {
        stem: "message.updated",
        kind: None,
        detail: None,
    },
];

/// The directory the payloads live in.
fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks/opencode")
}

/// One payload, parsed.
fn fixture(stem: &str) -> Value {
    let path = dir().join(format!("{stem}.json"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn every_fixture_maps_to_its_expected_kind_and_detail() {
    for Expected { stem, kind, detail } in EXPECTED {
        let raw = fixture(stem);
        let Some(kind) = kind else {
            assert!(
                opencode::normalize(&raw).is_none(),
                "{stem} should have produced nothing",
            );
            continue;
        };
        let event = opencode::normalize(&raw).unwrap_or_else(|| panic!("{stem} produced no event"));

        assert_eq!(&event.kind, kind, "{stem}");
        assert_eq!(
            event.detail.clone().map(Value::Object),
            detail.map(|build| build()),
            "{stem}"
        );
        assert_eq!(event.agent, agent("opencode"), "{stem}");
        assert_eq!(event.source, Source::Hook, "{stem}");
        // Either spelling of the session identifies the same session.
        let session = raw["sessionID"].as_str().or(raw["session_id"].as_str());
        assert_eq!(Some(event.session.as_str()), session, "{stem}");
        // The directory the plugin was loaded for is the nearest thing OpenCode
        // has to the working directory the other agents report.
        assert_eq!(event.cwd.as_deref(), raw["directory"].as_str(), "{stem}");
        // The environment is the emit client's to read, not the adapter's.
        assert_eq!(event.correlation, None, "{stem}");
        // The payload travels verbatim, so that anything the normalized fields
        // do not cover is still reachable downstream.
        assert_eq!(event.raw.as_ref(), Some(&raw), "{stem}");
    }
}

#[test]
fn the_table_and_the_fixture_directory_describe_the_same_set() {
    let on_disk: BTreeSet<String> = fs::read_dir(dir())
        .expect("the fixture directory")
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    let expected: BTreeSet<String> = EXPECTED
        .iter()
        .map(|expected| expected.stem.to_owned())
        .collect();
    assert_eq!(on_disk, expected);
}

#[test]
fn nothing_reports_the_start_of_a_turn_because_the_agent_has_no_such_event() {
    for Expected { stem, .. } in EXPECTED {
        let Some(event) = opencode::normalize(&fixture(stem)) else {
            continue;
        };
        assert_ne!(event.kind, Kind::TurnStart, "{stem}");
    }
}

#[test]
fn a_session_identified_the_other_way_round_is_still_a_session() {
    let mut raw = fixture("session.created");
    let session = raw["sessionID"].as_str().unwrap().to_owned();
    let payload = raw.as_object_mut().unwrap();
    payload.remove("sessionID");
    payload.insert("session_id".to_owned(), json!(session.clone()));

    let event = opencode::normalize(&raw).expect("an event");

    assert_eq!(event.session, session);
}

#[test]
fn a_payload_with_no_session_identity_produces_nothing() {
    let mut raw = fixture("session.created");
    raw.as_object_mut().unwrap().remove("sessionID");
    assert!(opencode::normalize(&raw).is_none());

    raw["sessionID"] = json!(42);
    assert!(opencode::normalize(&raw).is_none());
}

#[test]
fn a_tool_event_without_a_tool_name_still_reports_the_call() {
    let mut raw = fixture("tool.execute.before");
    raw.as_object_mut().unwrap().remove("tool");

    let event = opencode::normalize(&raw).expect("an event");

    assert_eq!(event.kind, Kind::ToolStart);
    assert_eq!(event.detail, None);
}

#[test]
fn a_permission_request_reports_a_block_whether_or_not_it_names_a_tool() {
    let mut raw = fixture("permission.updated");
    raw.as_object_mut().unwrap().remove("tool");

    let event = opencode::normalize(&raw).expect("an event");

    assert_eq!(event.kind, Kind::Blocked);
    assert_eq!(event.detail, None);
}

#[test]
fn a_payload_that_is_not_an_object_produces_nothing() {
    for raw in [
        json!(null),
        json!({}),
        json!("session.created"),
        json!([1, 2, 3]),
        json!(7),
    ] {
        assert!(opencode::normalize(&raw).is_none(), "{raw}");
    }
}

#[test]
fn events_the_bus_does_not_model_produce_nothing() {
    let mut raw = fixture("session.created");
    for unmapped in [
        // Everything else OpenCode reports. Giving any of them a kind would be
        // inventing one only this agent could ever emit.
        "message.updated",
        "message.removed",
        "message.part.updated",
        "file.edited",
        "file.watcher.updated",
        "something.added.next.release",
    ] {
        raw["type"] = json!(unmapped);
        assert!(opencode::normalize(&raw).is_none(), "{unmapped}");
    }

    raw.as_object_mut().unwrap().remove("type");
    assert!(opencode::normalize(&raw).is_none());
}

#[test]
fn a_payload_without_a_directory_still_produces_an_event() {
    let mut raw = fixture("session.idle");
    raw.as_object_mut().unwrap().remove("directory");

    let event = opencode::normalize(&raw).expect("an event");

    assert_eq!(event.kind, Kind::TurnEnd);
    assert_eq!(event.cwd, None);
}
