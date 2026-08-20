//! OpenCode's plugin events, normalized.
//!
//! OpenCode has no hook commands. It loads plugins, hands each one an event
//! object, and drops whatever the handler returns — so the payload this reads is
//! not the agent's own, it is the one the plugin this program installs writes:
//! the event's `type`, the directory the plugin was loaded for, and the event's
//! own properties spread out beside them. The plugin selects nothing and renames
//! nothing, which is what puts the whole of that decision here.
//!
//! Three things are worth stating about this mapping, because all three are
//! places where OpenCode's event set and the bus's kinds do not line up.
//!
//! **Both sides of a block are reported.** OpenCode is the only supported agent
//! that says when a permission was answered as well as when one was asked for,
//! so `unblocked` comes from the agent here rather than being inferred from the
//! activity that follows. That makes it the honest case the other two are
//! measured against.
//!
//! **There is no start of a turn.** OpenCode has no user-prompt event, and
//! nothing here invents one. The bus does not put an event on the wire that no
//! agent produced, and the client that would have to invent it is a one-shot
//! process that sees a single event and nothing before it — so it could only
//! guess. Nothing downstream is worse off for the status: a session that has
//! gone idle is reported as working again the moment it calls a tool, because
//! any tool call is activity. What a subscriber does lose is the count: an
//! OpenCode session's turns cannot be counted by counting `turn_start` events,
//! because there will not be any.
//!
//! **The events with no counterpart produce nothing.** OpenCode reports message
//! updates, file edits and a file watcher, none of which any other agent has a
//! word for. The set of kinds is closed so that a subscriber can be written
//! against all of them; giving these ones a kind of their own would make the
//! envelope a claim about agents that cannot make it.

use agentbus_protocol::{Kind, UnstampedEvent};
use serde_json::{Map, Value};

use super::{agent, field, string};

/// Normalizes one payload from the OpenCode plugin.
///
/// `None` means the payload produces no event: it is not an object, it carries
/// no session identity, or it names an event this bus does not model. Callers
/// treat all three the same way — send nothing, and say nothing about it.
pub fn normalize(raw: &Value) -> Option<UnstampedEvent> {
    let payload = raw.as_object()?;

    // Without a session there is nothing an event could be attributed to, and a
    // session-less event would land in the table as a phantom of its own.
    let session = session(payload)?;

    let (kind, detail) = match string(payload, "type")? {
        "session.created" => (Kind::SessionStart, None),
        "session.deleted" => (Kind::SessionEnd, None),
        "session.idle" => (Kind::TurnEnd, None),
        "session.compacted" => (Kind::Compact, None),
        "session.error" => (Kind::Error, None),
        "tool.execute.before" => (Kind::ToolStart, tool(payload)),
        "tool.execute.after" => (Kind::ToolEnd, tool(payload)),
        // Asked and answered. Neither says what the session does next, and
        // neither has to: the status fold reads one as entering the wait and the
        // other as leaving it.
        "permission.updated" => (Kind::Blocked, tool(payload)),
        "permission.replied" => (Kind::Unblocked, None),
        _ => return None,
    };

    let mut event = UnstampedEvent::new(agent("opencode"), session, kind).with_raw(raw.clone());
    // The plugin is loaded for a directory and says which one; that is the
    // nearest thing OpenCode offers to the working directory the other agents
    // report, and it is what a receiver groups a session by.
    if let Some(cwd) = string(payload, "directory") {
        event = event.with_cwd(cwd);
    }
    if let Some(detail) = detail {
        event = event.with_detail(detail);
    }
    // `correlation` is deliberately absent: it comes from the environment the
    // plugin was run in, which is the caller's to read, not this function's.
    Some(event)
}

/// The session an event belongs to, however the payload spells it.
///
/// Two spellings are accepted because the properties of an OpenCode event are
/// its own and this program does not control them; taking both costs a
/// comparison and saves every event in a session from being dropped over a
/// convention that changed.
fn session(payload: &Map<String, Value>) -> Option<&str> {
    string(payload, "sessionID").or_else(|| string(payload, "session_id"))
}

/// The detail naming which tool a tool event is about, if the payload says.
///
/// OpenCode names the tool in a property of its own rather than in the field the
/// hook-driven agents use, which is the only reason this is not the shared
/// helper next door. What travels is the same one field, for the same reason:
/// nothing else about a tool call means the same thing across three agents.
fn tool(payload: &Map<String, Value>) -> Option<Map<String, Value>> {
    Some(field("tool", string(payload, "tool")?))
}
