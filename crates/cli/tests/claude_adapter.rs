//! Replaying Claude Code hook payloads through the adapter.
//!
//! The payloads in `tests/fixtures/hooks/claude` stand in for what arrives on a
//! hook's stdin, and the table below pins what each of them must normalize to.
//! The table and the directory must agree in both directions: a payload nobody
//! has said what to expect from is as much a hole as an expectation with no
//! payload behind it.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use agentbus_cli::adapters::claude;
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
    /// The normalized kind.
    kind: Kind,
    /// The detail, built when the assertion runs because a JSON value cannot be
    /// a constant. `None` where the mapping carries no extras.
    detail: Option<fn() -> Value>,
}

/// What each payload in the directory normalizes to.
const EXPECTED: &[Expected] = &[
    Expected {
        stem: "SessionStart",
        kind: Kind::SessionStart,
        detail: None,
    },
    Expected {
        stem: "SessionEnd",
        kind: Kind::SessionEnd,
        detail: None,
    },
    Expected {
        stem: "UserPromptSubmit",
        kind: Kind::TurnStart,
        detail: None,
    },
    Expected {
        stem: "Stop",
        kind: Kind::TurnEnd,
        detail: None,
    },
    Expected {
        stem: "PreToolUse",
        kind: Kind::ToolStart,
        detail: Some(|| json!({"tool": "Bash"})),
    },
    Expected {
        stem: "PostToolUse",
        kind: Kind::ToolEnd,
        detail: Some(|| json!({"tool": "Bash"})),
    },
    Expected {
        stem: "Notification",
        kind: Kind::Blocked,
        detail: Some(|| json!({"notification_type": "permission_prompt"})),
    },
    Expected {
        stem: "StopFailure",
        kind: Kind::Error,
        detail: Some(
            || json!({"message": "the model connection was reset before the turn finished"}),
        ),
    },
    Expected {
        stem: "SubagentStart",
        kind: Kind::SubagentStart,
        detail: Some(|| {
            json!({"agent_id": "1a7d33c9-58f0-4b2e-9d64-c0a7e5b41f22",
                               "agent_type": "general-purpose"})
        }),
    },
    Expected {
        stem: "SubagentStop",
        kind: Kind::SubagentEnd,
        detail: Some(|| {
            json!({"agent_id": "1a7d33c9-58f0-4b2e-9d64-c0a7e5b41f22",
                               "agent_type": "general-purpose"})
        }),
    },
    Expected {
        stem: "PreCompact",
        kind: Kind::Compact,
        detail: Some(|| json!({"phase": "pre"})),
    },
];

/// The directory the payloads live in.
fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks/claude")
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
        let event = claude::normalize(&raw).unwrap_or_else(|| panic!("{stem} produced no event"));

        assert_eq!(&event.kind, kind, "{stem}");
        assert_eq!(
            event.detail.clone().map(Value::Object),
            detail.map(|build| build()),
            "{stem}"
        );
        assert_eq!(event.agent, agent("claude"), "{stem}");
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
fn only_the_blocking_notification_types_report_a_block() {
    let mut raw = fixture("Notification");
    for blocking in claude::BLOCKING_NOTIFICATION_TYPES {
        raw["notification_type"] = json!(blocking);
        let event = claude::normalize(&raw).unwrap_or_else(|| panic!("{blocking} produced none"));
        assert_eq!(event.kind, Kind::Blocked);
        assert_eq!(event.detail.unwrap()["notification_type"], json!(blocking));
    }

    // Idleness is not somebody being asked for a decision; the end of a turn
    // already reports it.
    for quiet in ["idle_prompt", "something_invented_later"] {
        raw["notification_type"] = json!(quiet);
        assert!(claude::normalize(&raw).is_none(), "{quiet}");
    }

    raw.as_object_mut().unwrap().remove("notification_type");
    assert!(claude::normalize(&raw).is_none());
}

#[test]
fn a_failed_tool_call_is_still_the_end_of_a_tool_call() {
    let mut raw = fixture("PostToolUse");
    for reported in [json!(true), json!("permission denied"), json!({"code": 1})] {
        raw["tool_error"] = reported.clone();
        let event = claude::normalize(&raw).expect("an event");
        assert_eq!(event.kind, Kind::ToolEnd);
        assert_eq!(
            event.detail.unwrap(),
            *json!({"tool": "Bash", "error": true}).as_object().unwrap(),
            "{reported}"
        );
    }

    // The shapes that conventionally mean "nothing went wrong".
    for quiet in [json!(null), json!(false), json!("")] {
        raw["tool_error"] = quiet.clone();
        let event = claude::normalize(&raw).expect("an event");
        assert_eq!(
            event.detail.unwrap(),
            *json!({"tool": "Bash"}).as_object().unwrap(),
            "{quiet}"
        );
    }
}

#[test]
fn a_tool_event_without_a_tool_name_still_reports_the_call() {
    let mut raw = fixture("PreToolUse");
    raw.as_object_mut().unwrap().remove("tool_name");
    let event = claude::normalize(&raw).expect("an event");
    assert_eq!(event.kind, Kind::ToolStart);
    assert_eq!(event.detail, None);
}

#[test]
fn a_payload_with_no_session_identity_produces_nothing() {
    let mut raw = fixture("SessionStart");
    raw.as_object_mut().unwrap().remove("session_id");
    assert!(claude::normalize(&raw).is_none());

    raw["session_id"] = json!(42);
    assert!(claude::normalize(&raw).is_none());
}

#[test]
fn a_payload_that_is_not_an_object_produces_nothing() {
    for raw in [
        json!(null),
        json!("SessionStart"),
        json!([1, 2, 3]),
        json!(7),
    ] {
        assert!(claude::normalize(&raw).is_none(), "{raw}");
    }
}

#[test]
fn events_the_bus_does_not_model_produce_nothing() {
    let mut raw = fixture("SessionStart");
    for unmapped in [
        "FileChanged",
        "MessageDisplay",
        "ConfigChange",
        "WorktreeCreate",
        "SomethingAddedNextRelease",
    ] {
        raw["hook_event_name"] = json!(unmapped);
        assert!(claude::normalize(&raw).is_none(), "{unmapped}");
    }

    raw.as_object_mut().unwrap().remove("hook_event_name");
    assert!(claude::normalize(&raw).is_none());
}

#[test]
fn a_payload_without_a_working_directory_still_produces_an_event() {
    let mut raw = fixture("Stop");
    raw.as_object_mut().unwrap().remove("cwd");
    let event = claude::normalize(&raw).expect("an event");
    assert_eq!(event.kind, Kind::TurnEnd);
    assert_eq!(event.cwd, None);
}
