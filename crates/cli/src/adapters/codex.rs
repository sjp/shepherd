//! Codex CLI's hook payloads, normalized.
//!
//! Codex hands its hooks a JSON object on stdin, the same way Claude does, and
//! names the event in the same `hook_event_name` field. The fields read here are
//! the ones its hook surface documents as common — `session_id`, `cwd`,
//! `hook_event_name` — plus `tool_name` on the events that are about a tool
//! call. As everywhere else, each is optional on read: a payload is somebody
//! else's schema and it moves, so a field that is missing or has changed type
//! degrades the event rather than losing it.
//!
//! Two things are worth stating about where this mapping differs from Claude's,
//! because both are cases where Codex is the better-behaved of the two.
//!
//! **The block is asked for outright.** Codex has a `PermissionRequest` event
//! that means exactly what it says, so nothing here has to discriminate a
//! general-purpose notification into "waiting on a person" and "not". Leaving
//! the block is still inferred, by the status fold, from the activity that
//! follows — that gap is the agent's, not this adapter's.
//!
//! **Compaction is reported from both sides.** Codex fires before and after, and
//! the two are one kind with a `phase` in the detail rather than two kinds: what
//! happened is the same thing, and a subscriber that only cares that a
//! compaction occurred should not have to know there are two spellings of it.

use agentbus_protocol::{Kind, UnstampedEvent};
use serde_json::Value;

use super::{agent, field, string, tool};

/// Normalizes one Codex CLI hook payload.
///
/// `None` means the payload produces no event: it is not an object, it carries
/// no session identity, or it names an event this bus does not model. Callers
/// treat all three the same way — send nothing, and say nothing about it.
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
        "PreToolUse" => (Kind::ToolStart, tool(payload)),
        "PostToolUse" => (Kind::ToolEnd, tool(payload)),
        // The tool being asked about is the whole of what a person needs in
        // order to answer, so it travels with the block when the payload names
        // one.
        "PermissionRequest" => (Kind::Blocked, tool(payload)),
        "SubagentStart" => (Kind::SubagentStart, None),
        "SubagentStop" => (Kind::SubagentEnd, None),
        "PreCompact" => (Kind::Compact, Some(field("phase", "pre"))),
        "PostCompact" => (Kind::Compact, Some(field("phase", "post"))),
        _ => return None,
    };

    let mut event = UnstampedEvent::new(agent("codex"), session, kind).with_raw(raw.clone());
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
