//! Claude Code's hook payloads, normalized.
//!
//! Claude passes a JSON object on the hook command's stdin. The fields this
//! adapter reads are the ones its hook documentation names as common —
//! `session_id`, `cwd`, `hook_event_name` — plus a handful of per-event extras:
//! `tool_name` and `tool_error` on the tool events, `notification_type` on
//! `Notification`, `agent_id` and `agent_type` on the subagent events. Every one
//! of them is treated as optional on read. A hook payload is somebody else's
//! schema and it moves; a missing field must degrade the event, never lose it.
//!
//! Two decisions are worth stating outright, because both are places where the
//! honest mapping is smaller than the tempting one.
//!
//! **Events with no counterpart elsewhere produce no event at all.** Claude
//! emits more than the bus's kinds cover — file changes, message display,
//! configuration changes, worktree creation. The set of kinds is closed on
//! purpose, so that a subscriber can be written against all of them; inventing a
//! kind that only one agent could ever emit would make the envelope a lie about
//! what it describes. Such payloads map to `None` here, and the shipped hook
//! configuration simply does not ask Claude for them.
//!
//! **`Notification` is discriminated, not trusted.** Claude has no dedicated
//! "waiting for permission" hook. `Notification` is the closest thing and it is
//! broader than that, so only the notification types in
//! [`BLOCKING_NOTIFICATION_TYPES`] become `blocked`; the rest produce nothing.
//! This is the weakest mapping in the adapter and the one most likely to need
//! revising against sessions in the wild, which is why the set is one const and
//! nothing else in the file mentions a notification type by name.

use agentbus_protocol::{Agent, Kind, UnstampedEvent};
use serde_json::{Map, Value};

use super::string;

/// The `notification_type` values that mean "this session is waiting on a
/// person", and so are the ones reported as `blocked`.
///
/// Everything else Claude notifies about — idleness above all, which the `Stop`
/// hook already reports as the end of a turn — is not somebody being asked for a
/// decision, and reporting it as `blocked` would cry wolf in the one place the
/// bus is expected to be precise. Revising this list is the whole of revising
/// the mapping.
pub const BLOCKING_NOTIFICATION_TYPES: &[&str] = &["permission_prompt", "elicitation_dialog"];

/// Normalizes one Claude Code hook payload.
///
/// `None` means the payload produces no event: it is not an object, it carries
/// no session identity, or it is one of the events this bus deliberately does
/// not model. Callers treat all three the same way — send nothing, and say
/// nothing about it.
pub fn normalize(raw: &Value) -> Option<UnstampedEvent> {
    let payload = raw.as_object()?;

    // Without a session there is nothing an event could be attributed to, and a
    // session-less event would land in the table as a phantom of its own.
    let session = string(payload, "session_id")?;

    let (kind, detail) = match string(payload, "hook_event_name")? {
        "SessionStart" => (Kind::SessionStart, None),
        "SessionEnd" => (Kind::SessionEnd, None),
        "UserPromptSubmit" => (Kind::TurnStart, None),
        "Stop" => (Kind::TurnEnd, None),
        "PreToolUse" => (Kind::ToolStart, tool(payload, false)),
        "PostToolUse" => (
            Kind::ToolEnd,
            // A failed tool call is still the end of a tool call: the status
            // fold reads it as activity either way, and the failure is an extra
            // rather than a different kind.
            tool(payload, reports_error(payload.get("tool_error"))),
        ),
        "Notification" => {
            let notification_type = string(payload, "notification_type")?;
            if !BLOCKING_NOTIFICATION_TYPES.contains(&notification_type) {
                return None;
            }
            (Kind::Blocked, copied(payload, &["notification_type"]))
        }
        "StopFailure" => (Kind::Error, copied(payload, &["message"])),
        "SubagentStart" => (Kind::SubagentStart, copied(payload, SUBAGENT_FIELDS)),
        "SubagentStop" => (Kind::SubagentEnd, copied(payload, SUBAGENT_FIELDS)),
        // The name says which side of the compaction this is, so the detail says
        // so too: an agent that grows a matching hook for the far side would
        // otherwise be indistinguishable in the stream.
        "PreCompact" => (Kind::Compact, Some(field("phase", "pre"))),
        _ => return None,
    };

    let mut event = UnstampedEvent::new(Agent::Claude, session, kind).with_raw(raw.clone());
    if let Some(cwd) = string(payload, "cwd") {
        event = event.with_cwd(cwd);
    }
    if let Some(detail) = detail {
        event = event.with_detail(detail);
    }
    // `correlation` is deliberately absent: it comes from the environment the
    // hook was run in, which is the caller's to read, not this function's.
    Some(event)
}

/// The fields a subagent event carries about the subagent it is reporting on.
const SUBAGENT_FIELDS: &[&str] = &["agent_id", "agent_type"];

/// The detail for a tool event: which tool, and whether it failed.
///
/// Nothing else about the call travels here. Three agents' notions of a tool
/// call diverge in every direction, and the whole payload is already carried
/// verbatim for anyone who needs more than the name.
fn tool(payload: &Map<String, Value>, failed: bool) -> Option<Map<String, Value>> {
    let mut detail = Map::new();
    if let Some(name) = string(payload, "tool_name") {
        detail.insert("tool".to_owned(), Value::from(name));
    }
    if failed {
        detail.insert("error".to_owned(), Value::Bool(true));
    }
    (!detail.is_empty()).then_some(detail)
}

/// Whether a `tool_error` field says the call actually failed.
///
/// Hooks report failure in whatever shape their author reached for — a flag, a
/// message, a structured error — so anything present and non-empty counts, and
/// the values that conventionally mean "no error" do not.
fn reports_error(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(message)) => !message.is_empty(),
        Some(_) => true,
    }
}

/// The named fields of `payload` that are present, verbatim, or `None` if none
/// of them are.
fn copied(payload: &Map<String, Value>, keys: &[&str]) -> Option<Map<String, Value>> {
    let detail: Map<String, Value> = keys
        .iter()
        .filter_map(|key| {
            payload
                .get(*key)
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .collect();
    (!detail.is_empty()).then_some(detail)
}

/// A detail map holding one field.
fn field(key: &str, value: &str) -> Map<String, Value> {
    let mut detail = Map::new();
    detail.insert(key.to_owned(), Value::from(value));
    detail
}
