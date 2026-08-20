//! Replaying Codex CLI hook payloads through the adapter.
//!
//! The payloads in `tests/fixtures/hooks/codex` stand in for what arrives on a
//! hook's stdin, and the table below pins what each of them must normalize to.
//! The table and the directory must agree in both directions: a payload nobody
//! has said what to expect from is as much a hole as an expectation with no
//! payload behind it.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use agentbus_cli::adapters::codex;
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
        stem: "SessionStart",
        kind: Some(Kind::SessionStart),
        detail: None,
    },
    Expected {
        stem: "SessionEnd",
        kind: Some(Kind::SessionEnd),
        detail: None,
    },
    Expected {
        stem: "UserPromptSubmit",
        kind: Some(Kind::TurnStart),
        detail: None,
    },
    Expected {
        stem: "Stop",
        kind: Some(Kind::TurnEnd),
        detail: None,
    },
    Expected {
        stem: "PreToolUse",
        kind: Some(Kind::ToolStart),
        detail: Some(|| json!({"tool": "shell"})),
    },
    Expected {
        stem: "PostToolUse",
        kind: Some(Kind::ToolEnd),
        detail: Some(|| json!({"tool": "shell"})),
    },
    Expected {
        stem: "PermissionRequest",
        kind: Some(Kind::Blocked),
        detail: Some(|| json!({"tool": "shell"})),
    },
    Expected {
        stem: "SubagentStart",
        kind: Some(Kind::SubagentStart),
        detail: None,
    },
    Expected {
        stem: "SubagentStop",
        kind: Some(Kind::SubagentEnd),
        detail: None,
    },
    Expected {
        stem: "PreCompact",
        kind: Some(Kind::Compact),
        detail: Some(|| json!({"phase": "pre"})),
    },
    Expected {
        stem: "PostCompact",
        kind: Some(Kind::Compact),
        detail: Some(|| json!({"phase": "post"})),
    },
    // The event another agent has and this one does not: answering for it would
    // report an agent's hooks as sending something they never send.
    Expected {
        stem: "Notification",
        kind: None,
        detail: None,
    },
];

/// The directory the payloads live in.
fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks/codex")
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
                codex::normalize(&raw).is_none(),
                "{stem} should have produced nothing",
            );
            continue;
        };
        let event = codex::normalize(&raw).unwrap_or_else(|| panic!("{stem} produced no event"));

        assert_eq!(&event.kind, kind, "{stem}");
        assert_eq!(
            event.detail.clone().map(Value::Object),
            detail.map(|build| build()),
            "{stem}"
        );
        assert_eq!(event.agent, agent("codex"), "{stem}");
        assert_eq!(event.source, Source::Hook, "{stem}");
        assert_eq!(event.session, raw["session_id"].as_str().unwrap(), "{stem}");
        assert_eq!(event.cwd.as_deref(), raw["cwd"].as_str(), "{stem}");
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
fn the_two_sides_of_a_compaction_are_one_kind_told_apart_by_the_detail() {
    let before = codex::normalize(&fixture("PreCompact")).expect("an event");
    let after = codex::normalize(&fixture("PostCompact")).expect("an event");

    assert_eq!(before.kind, after.kind);
    assert_ne!(before.detail, after.detail);
}

#[test]
fn a_permission_request_reports_a_block_whether_or_not_it_names_a_tool() {
    let mut raw = fixture("PermissionRequest");
    raw.as_object_mut().unwrap().remove("tool_name");

    let event = codex::normalize(&raw).expect("an event");

    assert_eq!(event.kind, Kind::Blocked);
    assert_eq!(event.detail, None);
}

#[test]
fn a_tool_event_without_a_tool_name_still_reports_the_call() {
    let mut raw = fixture("PreToolUse");
    raw.as_object_mut().unwrap().remove("tool_name");

    let event = codex::normalize(&raw).expect("an event");

    assert_eq!(event.kind, Kind::ToolStart);
    assert_eq!(event.detail, None);
}

#[test]
fn a_payload_with_no_session_identity_produces_nothing() {
    let mut raw = fixture("SessionStart");
    raw.as_object_mut().unwrap().remove("session_id");
    assert!(codex::normalize(&raw).is_none());

    raw["session_id"] = json!(42);
    assert!(codex::normalize(&raw).is_none());
}

#[test]
fn a_payload_that_is_not_an_object_produces_nothing() {
    for raw in [
        json!(null),
        json!({}),
        json!("SessionStart"),
        json!([1, 2, 3]),
        json!(7),
    ] {
        assert!(codex::normalize(&raw).is_none(), "{raw}");
    }
}

#[test]
fn events_the_bus_does_not_model_produce_nothing() {
    let mut raw = fixture("SessionStart");
    for unmapped in [
        // The events another agent has and this one does not: an adapter that
        // answered for them would be reporting an agent's hooks as sending
        // something they never send.
        "Notification",
        "StopFailure",
        "SomethingAddedNextRelease",
    ] {
        raw["hook_event_name"] = json!(unmapped);
        assert!(codex::normalize(&raw).is_none(), "{unmapped}");
    }

    raw.as_object_mut().unwrap().remove("hook_event_name");
    assert!(codex::normalize(&raw).is_none());
}

#[test]
fn a_payload_without_a_working_directory_still_produces_an_event() {
    let mut raw = fixture("Stop");
    raw.as_object_mut().unwrap().remove("cwd");

    let event = codex::normalize(&raw).expect("an event");

    assert_eq!(event.kind, Kind::TurnEnd);
    assert_eq!(event.cwd, None);
}
