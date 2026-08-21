//! Replaying recorded hook payloads through the mappings that ship in the
//! library.
//!
//! The payloads in `tests/fixtures/hooks` stand in for what arrives on a hook's
//! stdin — or, where an agent has no hooks, for what the installed plugin
//! writes there — and the table below pins what each of them must normalize to.
//! The table and the directories must agree in both directions: a payload
//! nobody has said what to expect from is as much a hole as an expectation with
//! no payload behind it.
//!
//! Nothing here reaches for a mapping by hand. The store is asked what one
//! agent's payload means, which is the same question the emit path asks, so
//! what these tests pin is the answer a machine with nothing installed on it
//! gives.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use agentbus_detect::{ManifestStore, StorePaths};
use agentbus_protocol::{Kind, Source};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

/// What one recorded payload must normalize to.
struct Expected {
    /// The agent whose directory the payload sits in, which is the name the
    /// mapping is looked up by.
    agent: &'static str,
    /// The payload's file name, without its extension.
    stem: &'static str,
    /// The normalized kind, or nothing where the payload deliberately produces
    /// no event at all.
    kind: Option<Kind>,
    /// The detail, built when the assertion runs because a JSON value cannot be
    /// a constant. `None` where the mapping carries no extras.
    detail: Option<fn() -> Value>,
}

/// What each payload in each directory normalizes to.
const EXPECTED: &[Expected] = &[
    Expected {
        agent: "claude",
        stem: "SessionStart",
        kind: Some(Kind::SessionStart),
        detail: None,
    },
    Expected {
        agent: "claude",
        stem: "SessionEnd",
        kind: Some(Kind::SessionEnd),
        detail: None,
    },
    Expected {
        agent: "claude",
        stem: "UserPromptSubmit",
        kind: Some(Kind::TurnStart),
        detail: None,
    },
    Expected {
        agent: "claude",
        stem: "Stop",
        kind: Some(Kind::TurnEnd),
        detail: None,
    },
    Expected {
        agent: "claude",
        stem: "PreToolUse",
        kind: Some(Kind::ToolStart),
        detail: Some(|| json!({"tool": "Bash"})),
    },
    // The three shapes a report of a tool failure arrives in: absent, empty,
    // and a message. Only the last of them is a failure.
    Expected {
        agent: "claude",
        stem: "PostToolUse",
        kind: Some(Kind::ToolEnd),
        detail: Some(|| json!({"tool": "Bash"})),
    },
    Expected {
        agent: "claude",
        stem: "PostToolUse-error-empty",
        kind: Some(Kind::ToolEnd),
        detail: Some(|| json!({"tool": "Bash"})),
    },
    Expected {
        agent: "claude",
        stem: "PostToolUse-error-message",
        kind: Some(Kind::ToolEnd),
        detail: Some(|| json!({"tool": "Bash", "error": true})),
    },
    // Both sides of the narrowest decision in the corpus: the notification
    // types that mean a person is being asked something, and one that does not.
    Expected {
        agent: "claude",
        stem: "Notification",
        kind: Some(Kind::Blocked),
        detail: Some(|| json!({"notification_type": "permission_prompt"})),
    },
    Expected {
        agent: "claude",
        stem: "Notification-elicitation",
        kind: Some(Kind::Blocked),
        detail: Some(|| json!({"notification_type": "elicitation_dialog"})),
    },
    Expected {
        agent: "claude",
        stem: "Notification-idle",
        kind: None,
        detail: None,
    },
    Expected {
        agent: "claude",
        stem: "StopFailure",
        kind: Some(Kind::Error),
        detail: Some(
            || json!({"message": "the model connection was reset before the turn finished"}),
        ),
    },
    Expected {
        agent: "claude",
        stem: "SubagentStart",
        kind: Some(Kind::SubagentStart),
        detail: Some(|| {
            json!({"agent_id": "1a7d33c9-58f0-4b2e-9d64-c0a7e5b41f22",
                   "agent_type": "general-purpose"})
        }),
    },
    Expected {
        agent: "claude",
        stem: "SubagentStop",
        kind: Some(Kind::SubagentEnd),
        detail: Some(|| {
            json!({"agent_id": "1a7d33c9-58f0-4b2e-9d64-c0a7e5b41f22",
                   "agent_type": "general-purpose"})
        }),
    },
    Expected {
        agent: "claude",
        stem: "PreCompact",
        kind: Some(Kind::Compact),
        detail: Some(|| json!({"phase": "pre"})),
    },
    // An event this agent has and the bus does not model.
    Expected {
        agent: "claude",
        stem: "FileChanged",
        kind: None,
        detail: None,
    },
    Expected {
        agent: "codex",
        stem: "SessionStart",
        kind: Some(Kind::SessionStart),
        detail: None,
    },
    Expected {
        agent: "codex",
        stem: "SessionEnd",
        kind: Some(Kind::SessionEnd),
        detail: None,
    },
    Expected {
        agent: "codex",
        stem: "UserPromptSubmit",
        kind: Some(Kind::TurnStart),
        detail: None,
    },
    Expected {
        agent: "codex",
        stem: "Stop",
        kind: Some(Kind::TurnEnd),
        detail: None,
    },
    Expected {
        agent: "codex",
        stem: "PreToolUse",
        kind: Some(Kind::ToolStart),
        detail: Some(|| json!({"tool": "shell"})),
    },
    Expected {
        agent: "codex",
        stem: "PostToolUse",
        kind: Some(Kind::ToolEnd),
        detail: Some(|| json!({"tool": "shell"})),
    },
    Expected {
        agent: "codex",
        stem: "PermissionRequest",
        kind: Some(Kind::Blocked),
        detail: Some(|| json!({"tool": "shell"})),
    },
    Expected {
        agent: "codex",
        stem: "SubagentStart",
        kind: Some(Kind::SubagentStart),
        detail: None,
    },
    Expected {
        agent: "codex",
        stem: "SubagentStop",
        kind: Some(Kind::SubagentEnd),
        detail: None,
    },
    Expected {
        agent: "codex",
        stem: "PreCompact",
        kind: Some(Kind::Compact),
        detail: Some(|| json!({"phase": "pre"})),
    },
    Expected {
        agent: "codex",
        stem: "PostCompact",
        kind: Some(Kind::Compact),
        detail: Some(|| json!({"phase": "post"})),
    },
    // The event another agent's mapping answers to and this one does not: an
    // answer here would be reporting an agent as sending something it never
    // sends.
    Expected {
        agent: "codex",
        stem: "Notification",
        kind: None,
        detail: None,
    },
    Expected {
        agent: "opencode",
        stem: "session.created",
        kind: Some(Kind::SessionStart),
        detail: None,
    },
    // Not one of the agent's own events: the plugin its terminal interface
    // loads writes this when the user opens a session, which is the only thing
    // that interface knows and the events do not.
    Expected {
        agent: "opencode",
        stem: "tui.session.selected",
        kind: Some(Kind::SessionStart),
        detail: None,
    },
    Expected {
        agent: "opencode",
        stem: "session.deleted",
        kind: Some(Kind::SessionEnd),
        detail: None,
    },
    Expected {
        agent: "opencode",
        stem: "session.idle",
        kind: Some(Kind::TurnEnd),
        detail: None,
    },
    // The same event with the session spelled the other way round.
    Expected {
        agent: "opencode",
        stem: "session.idle-session_id",
        kind: Some(Kind::TurnEnd),
        detail: None,
    },
    Expected {
        agent: "opencode",
        stem: "session.compacted",
        kind: Some(Kind::Compact),
        detail: None,
    },
    Expected {
        agent: "opencode",
        stem: "session.error",
        kind: Some(Kind::Error),
        detail: None,
    },
    Expected {
        agent: "opencode",
        stem: "tool.execute.before",
        kind: Some(Kind::ToolStart),
        detail: Some(|| json!({"tool": "bash"})),
    },
    Expected {
        agent: "opencode",
        stem: "tool.execute.after",
        kind: Some(Kind::ToolEnd),
        detail: Some(|| json!({"tool": "bash"})),
    },
    Expected {
        agent: "opencode",
        stem: "permission.updated",
        kind: Some(Kind::Blocked),
        detail: Some(|| json!({"tool": "edit"})),
    },
    Expected {
        agent: "opencode",
        stem: "permission.replied",
        kind: Some(Kind::Unblocked),
        detail: None,
    },
    Expected {
        agent: "opencode",
        stem: "message.updated",
        kind: None,
        detail: None,
    },
];

/// Where one agent's payloads keep the parts every event needs.
struct Envelope {
    /// The agent whose payloads these are.
    agent: &'static str,
    /// The field naming the event.
    event: &'static str,
    /// The fields the session may be under, best first.
    session: &'static [&'static str],
    /// The fields the working directory may be under, read the same way.
    cwd: &'static [&'static str],
}

/// Where each agent's payloads keep them.
///
/// The mappings say the same thing, which is the point: an assertion needs
/// something of its own to compare an answer against, and reading the answer
/// out of the mapping that produced it would assert nothing.
const ENVELOPE: &[Envelope] = &[
    Envelope {
        agent: "claude",
        event: "hook_event_name",
        session: &["session_id"],
        cwd: &["cwd"],
    },
    Envelope {
        agent: "codex",
        event: "hook_event_name",
        session: &["session_id"],
        cwd: &["cwd"],
    },
    Envelope {
        agent: "opencode",
        event: "type",
        session: &["sessionID", "session_id"],
        // The directory the plugin was loaded for is the nearest thing this
        // agent offers to the working directory the others report.
        cwd: &["directory"],
    },
];

/// The kinds each agent's payloads can produce.
///
/// Written out so that a kind an agent can report but nothing here records is a
/// failure rather than a silence: the gaps are the interesting part of this
/// list. Nothing reports the start of a turn for `opencode`, because that agent
/// has no event that means one; nothing reports leaving a block anywhere else,
/// because `opencode` is the only agent that says so itself.
const KINDS: &[(&str, &[Kind])] = &[
    (
        "claude",
        &[
            Kind::SessionStart,
            Kind::SessionEnd,
            Kind::TurnStart,
            Kind::TurnEnd,
            Kind::ToolStart,
            Kind::ToolEnd,
            Kind::Blocked,
            Kind::SubagentStart,
            Kind::SubagentEnd,
            Kind::Compact,
            Kind::Error,
        ],
    ),
    (
        "codex",
        &[
            Kind::SessionStart,
            Kind::SessionEnd,
            Kind::TurnStart,
            Kind::TurnEnd,
            Kind::ToolStart,
            Kind::ToolEnd,
            Kind::Blocked,
            Kind::SubagentStart,
            Kind::SubagentEnd,
            Kind::Compact,
        ],
    ),
    (
        "opencode",
        &[
            Kind::SessionStart,
            Kind::SessionEnd,
            Kind::TurnEnd,
            Kind::ToolStart,
            Kind::ToolEnd,
            Kind::Blocked,
            Kind::Unblocked,
            Kind::Compact,
            Kind::Error,
        ],
    ),
];

/// A store reading nothing but the mappings that ship inside the library.
///
/// The home directory is one of its own, so that whatever the machine running
/// these tests keeps in its own manifest directories cannot answer for an agent
/// here: what is pinned is what a fresh machine does.
fn bundled() -> (TempDir, ManifestStore) {
    let home = TempDir::new().expect("a temporary directory");
    let store = ManifestStore::open(StorePaths::rooted(home.path()));
    (home, store)
}

/// The directory one agent's payloads live in.
fn dir(agent: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks")
        .join(agent)
}

/// One payload, parsed.
fn fixture(agent: &str, stem: &str) -> Value {
    let path = dir(agent).join(format!("{stem}.json"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every agent a payload was recorded for, in the order the table names them.
fn agents() -> Vec<&'static str> {
    let mut agents: Vec<&'static str> = Vec::new();
    for expected in EXPECTED {
        if !agents.contains(&expected.agent) {
            agents.push(expected.agent);
        }
    }
    agents
}

/// Where one agent's payloads keep the envelope.
fn envelope(agent: &str) -> &'static Envelope {
    ENVELOPE
        .iter()
        .find(|envelope| envelope.agent == agent)
        .unwrap_or_else(|| panic!("nothing says where {agent}'s session is"))
}

/// The first of `fields` the payload answers with a non-empty string.
fn first_present<'a>(payload: &'a Value, fields: &[&str]) -> Option<&'a str> {
    fields
        .iter()
        .find_map(|field| payload.get(*field)?.as_str().filter(|it| !it.is_empty()))
}

/// A payload with the named fields taken out of it.
fn without(payload: &Value, fields: &[&str]) -> Value {
    let mut payload = payload.clone();
    let object = payload.as_object_mut().expect("a payload is an object");
    for field in fields {
        object.remove(*field);
    }
    payload
}

/// The detail one payload produces, as a JSON value for comparison.
fn detail(detail: Option<Map<String, Value>>) -> Option<Value> {
    detail.map(Value::Object)
}

#[test]
fn every_fixture_normalizes_to_its_expected_kind_and_detail() {
    let (_home, store) = bundled();
    for expected in EXPECTED {
        let at = format!("{} {}", expected.agent, expected.stem);
        let raw = fixture(expected.agent, expected.stem);
        let normalized = store.normalize_hook(expected.agent, &raw);

        let Some(kind) = &expected.kind else {
            assert!(normalized.is_none(), "{at} should have produced nothing");
            continue;
        };
        let event = normalized.unwrap_or_else(|| panic!("{at} produced no event"));

        assert_eq!(&event.kind, kind, "{at}");
        assert_eq!(
            detail(event.detail.clone()),
            expected.detail.map(|build| build()),
            "{at}",
        );
        assert_eq!(event.agent.as_str(), expected.agent, "{at}");
        assert_eq!(event.source, Source::Hook, "{at}");

        let envelope = envelope(expected.agent);
        assert_eq!(
            Some(event.session.as_str()),
            first_present(&raw, envelope.session),
            "{at}",
        );
        assert_eq!(
            event.cwd.as_deref(),
            first_present(&raw, envelope.cwd),
            "{at}",
        );
        // The environment is the emit client's to read, not the mapping's.
        assert_eq!(event.correlation, None, "{at}");
        // The payload travels verbatim, so that anything the normalized fields
        // do not cover is still reachable downstream.
        assert_eq!(event.raw.as_ref(), Some(&raw), "{at}");
    }
}

#[test]
fn the_table_and_the_fixture_directories_describe_the_same_set() {
    for agent in agents() {
        let on_disk: BTreeSet<String> = fs::read_dir(dir(agent))
            .expect("the fixture directory")
            .map(|entry| entry.expect("a directory entry").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        let pinned: BTreeSet<String> = EXPECTED
            .iter()
            .filter(|expected| expected.agent == agent)
            .map(|expected| expected.stem.to_owned())
            .collect();
        assert_eq!(on_disk, pinned, "{agent}");
    }
}

#[test]
fn every_kind_an_agent_can_report_has_a_payload_behind_it() {
    for (agent, kinds) in KINDS {
        let recorded: BTreeSet<&str> = EXPECTED
            .iter()
            .filter(|expected| expected.agent == *agent)
            .filter_map(|expected| expected.kind.as_ref())
            .map(Kind::as_str)
            .collect();
        let claimed: BTreeSet<&str> = kinds.iter().map(Kind::as_str).collect();
        assert_eq!(recorded, claimed, "{agent}");
    }
    assert_eq!(
        agents(),
        KINDS.iter().map(|(agent, _)| *agent).collect::<Vec<_>>(),
    );
}

#[test]
fn every_agent_records_a_payload_that_produces_nothing() {
    for agent in agents() {
        assert!(
            EXPECTED
                .iter()
                .any(|expected| expected.agent == agent && expected.kind.is_none()),
            "{agent} records nothing that produces nothing",
        );
    }
}

#[test]
fn the_narrowest_decision_is_pinned_from_both_sides() {
    // The one place a mapping decides what an agent meant rather than reading
    // what it said: a general-purpose notification is a block only for the
    // types that mean somebody is being asked something. Both answers are
    // recorded, and the ones that count are told apart by what they say they
    // are rather than by which file they came from.
    let (_home, store) = bundled();
    let mut blocking: BTreeMap<String, Kind> = BTreeMap::new();
    let mut quiet = 0;

    for expected in EXPECTED
        .iter()
        .filter(|expected| expected.agent == "claude")
    {
        let raw = fixture("claude", expected.stem);
        if raw["hook_event_name"] != json!("Notification") {
            continue;
        }
        let notification_type = raw["notification_type"]
            .as_str()
            .expect("a notification says what type it is")
            .to_owned();
        match store.normalize_hook("claude", &raw) {
            Some(event) => {
                assert_eq!(
                    event
                        .detail
                        .as_ref()
                        .and_then(|detail| detail.get("notification_type").and_then(Value::as_str)),
                    Some(notification_type.as_str()),
                    "a block should say what it was asked about",
                );
                blocking.insert(notification_type, event.kind);
            }
            None => quiet += 1,
        }
    }

    assert!(
        blocking.len() >= 2,
        "only {} notification type(s) recorded as a block",
        blocking.len(),
    );
    assert!(blocking.values().all(|kind| *kind == Kind::Blocked));
    assert!(quiet > 0, "no notification recorded as producing nothing");
}

#[test]
fn a_payload_with_no_session_identity_produces_nothing() {
    let (_home, store) = bundled();
    for agent in agents() {
        let session = envelope(agent).session;
        let raw = a_payload_of(agent);
        assert!(
            store
                .normalize_hook(agent, &without(&raw, session))
                .is_none(),
            "{agent} with no session",
        );

        // A session field that is there but is not a string is no session
        // either: what travels on the wire is a string.
        let mut wrong = raw.clone();
        for field in session {
            wrong[*field] = json!(42);
        }
        assert!(
            store.normalize_hook(agent, &wrong).is_none(),
            "{agent} with a session that is not a string",
        );
    }
}

#[test]
fn a_payload_without_a_working_directory_still_produces_an_event() {
    let (_home, store) = bundled();
    for agent in agents() {
        let raw = without(&a_payload_of(agent), envelope(agent).cwd);
        let event = store
            .normalize_hook(agent, &raw)
            .unwrap_or_else(|| panic!("{agent} produced no event"));
        assert_eq!(event.cwd, None, "{agent}");
    }
}

#[test]
fn an_event_no_mapping_maps_produces_nothing() {
    let (_home, store) = bundled();
    for agent in agents() {
        let raw = a_payload_of(agent);
        let field = envelope(agent).event;
        for unmapped in ["", "SomethingAddedNextRelease", "session.forked"] {
            let mut renamed = raw.clone();
            renamed[field] = json!(unmapped);
            assert!(
                store.normalize_hook(agent, &renamed).is_none(),
                "{agent} {unmapped:?}",
            );
        }
        assert!(
            store
                .normalize_hook(agent, &without(&raw, &[field]))
                .is_none()
        );
    }
}

#[test]
fn a_payload_that_is_not_an_object_produces_nothing() {
    let (_home, store) = bundled();
    for agent in agents() {
        for raw in [
            json!(null),
            json!("SessionStart"),
            json!([1, 2, 3]),
            json!(7),
        ] {
            assert!(store.normalize_hook(agent, &raw).is_none(), "{agent} {raw}");
        }
    }
}

#[test]
fn an_agent_no_mapping_describes_normalizes_nothing() {
    let (_home, store) = bundled();
    let raw = a_payload_of("claude");
    assert!(
        store
            .normalize_hook("an-agent-nobody-shipped", &raw)
            .is_none()
    );
}

/// One payload of an agent's that produces an event, for the tests that are
/// about the envelope rather than about a particular event.
fn a_payload_of(agent: &str) -> Value {
    let expected = EXPECTED
        .iter()
        .find(|expected| expected.agent == agent && expected.kind.is_some())
        .unwrap_or_else(|| panic!("no payload of {agent}'s produces an event"));
    fixture(agent, expected.stem)
}
