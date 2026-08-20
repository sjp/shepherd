//! Turning an agent's hook payload into a normalized event, from data.
//!
//! Every harness that reports its own lifecycle does it the same way and says
//! it differently: a JSON object arrives, one of its fields names what
//! happened, and the rest is that harness's own vocabulary. What varies between
//! harnesses is only the spelling — which field names the event, where the
//! session id lives, what `PreToolUse` is called this year — and spelling is
//! the kind of thing that belongs in a file rather than in a module.
//!
//! So this is the whole engine: look up the field the manifest names, find the
//! entry that answers to its value, check the entry's condition, read the
//! session and directory from the first field that has them, and project a
//! handful of extras. No regexes, no expressions, no paths into nested values.
//! Supporting a new harness is writing TOML.
//!
//! # Nothing to say is not a failure
//!
//! A payload that names an event this manifest does not map, or that fails an
//! entry's condition, or that carries no session at all, produces no event.
//! Agents emit far more than the bus normalizes, so this is the ordinary case,
//! not an error: the caller sends nothing, and says nothing about it.
//!
//! # Purity, and why it matters here
//!
//! Normalizing is a pure function of a manifest and a payload — no clock, no
//! environment, no socket. That is what lets a mapping be proved by replaying
//! captured payloads, and it is what keeps the part that decides what an event
//! *means* separate from the part that has to survive contact with a
//! filesystem. It also keeps the cost honest: this runs inside somebody's
//! coding agent while they wait for it.

pub(crate) mod bundled;
pub mod schema;

use agentbus_protocol::{Agent, UnstampedEvent};
use serde_json::{Map, Value};

use schema::{HookManifest, Projection, Transform};

/// One agent's mapping, ready to read payloads with.
///
/// There is nothing here to compile in the sense the screen family means it —
/// no matchers to build — so what this adds to the manifest is the one thing
/// worth deciding once: the agent id every event it produces is attributed to,
/// validated as an envelope id at load and never rebuilt per payload.
#[derive(Debug, Clone)]
pub struct CompiledHookManifest {
    /// The id events are emitted under.
    agent: Agent,
    /// The mapping as its author wrote it.
    manifest: HookManifest,
}

impl CompiledHookManifest {
    /// Prepares a validated manifest for use.
    ///
    /// # Panics
    ///
    /// If the manifest's id is not a usable agent id. Validation refuses such a
    /// manifest, so reaching this means a caller built one by hand and skipped
    /// [`HookManifest::validate`].
    pub fn compile(manifest: HookManifest) -> Self {
        let agent = Agent::new(manifest.id.trim())
            .expect("a validated manifest's id is usable as an agent id");
        Self { agent, manifest }
    }

    /// The mapping this was built from.
    pub fn manifest(&self) -> &HookManifest {
        &self.manifest
    }

    /// The agent id events are emitted under.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Normalizes one payload, or reports that there is nothing to say about
    /// it.
    ///
    /// `None` covers four things a caller treats identically: the payload is
    /// not an object, it names an event this manifest does not map, the entry's
    /// condition does not hold, or it carries no session for the event to
    /// belong to.
    ///
    /// The payload always travels along verbatim. Everything the mapping did
    /// not think to normalize is still in there, which is what makes a gap in
    /// the dialect an inconvenience rather than a loss.
    pub fn normalize(&self, payload: &Value) -> Option<UnstampedEvent> {
        let object = payload.as_object()?;

        let name = string(object, &self.manifest.payload.event)?;
        let mapping = self
            .manifest
            .events
            .iter()
            .find(|mapping| mapping.name == name)?;

        // A condition that does not hold drops the payload rather than looking
        // for another entry to try: names are unique, so there is no other
        // entry, and a mapping where there could be would make the order its
        // author happened to write things in load-bearing.
        if let Some(when) = &mapping.when
            && !when.holds(object.get(&when.field))
        {
            return None;
        }

        // Without a session there is nothing an event could be attributed to,
        // and a session-less event would land in a receiver's table as a
        // phantom of its own.
        let session = first_present(object, &self.manifest.payload.session)?;

        let mut event = UnstampedEvent::new(self.agent.clone(), session, mapping.kind.clone())
            .with_raw(payload.clone());
        if let Some(cwd) = first_present(object, &self.manifest.payload.cwd) {
            event = event.with_cwd(cwd);
        }
        let detail = project(object, &mapping.detail);
        if !detail.is_empty() {
            event = event.with_detail(detail);
        }
        // `correlation` is deliberately absent: it comes from the environment
        // the hook was run in, which is the caller's to read, not this
        // function's.
        Some(event)
    }
}

/// What one payload says, according to one agent's mapping.
///
/// This is the form for a caller holding a mapping of its own. A caller that
/// only knows which agent sent the payload wants
/// [`ManifestStore::normalize_hook`](crate::store::ManifestStore::normalize_hook),
/// which finds the copy in force on the machine first.
pub fn normalize(manifest: &CompiledHookManifest, payload: &Value) -> Option<UnstampedEvent> {
    manifest.normalize(payload)
}

/// The value of a string field, if it is there and is a string.
///
/// Every field a mapping reads is optional on read, whatever the payload's own
/// schema promises: a payload is somebody else's schema and it moves, so a
/// field that is missing or has changed type must degrade the event rather than
/// lose it.
fn string<'a>(payload: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    payload.get(key)?.as_str()
}

/// The first of `keys` the payload answers with a non-empty string.
///
/// A field that is present but blank, or present as something other than a
/// string, is passed over rather than accepted: the alternatives exist because
/// one payload can spell a thing two ways, and stopping at the first spelling
/// that is merely *present* would take the empty one over the real one.
fn first_present<'a>(payload: &'a Map<String, Value>, keys: &[String]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| string(payload, key).filter(|value| !value.is_empty()))
}

/// The normalized extras one event's projections produce.
fn project(payload: &Map<String, Value>, projections: &[Projection]) -> Map<String, Value> {
    let mut detail = Map::new();
    for projection in projections {
        let value = match (&projection.from, &projection.value) {
            // Copied verbatim, whatever shape it is in. Deciding what a
            // payload's own value ought to look like is not this engine's
            // business; a receiver has the field and the raw payload both.
            (Some(from), _) => match projection.transform {
                Some(Transform::NonEmpty) => {
                    says_something(payload.get(from)).then_some(Value::Bool(true))
                }
                None => payload.get(from).cloned(),
            },
            (None, Some(value)) => Some(Value::from(value.as_str())),
            // A validated projection names one of the two.
            (None, None) => None,
        };
        if let Some(value) = value {
            detail.insert(projection.field.clone(), value);
        }
    }
    detail
}

/// Whether a field says anything, for the transform of the same name.
///
/// Payloads report a thing that either happened or did not in whatever shape
/// their author reached for — a flag, a message, a structured value — so
/// anything present and not empty counts, and the values that conventionally
/// mean "nothing here" do not.
fn says_something(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use agentbus_protocol::{Kind, Source};
    use serde_json::json;

    /// A mapping around whatever the test wants to say, over the ordinary
    /// envelope fields.
    fn mapping(events: &str) -> CompiledHookManifest {
        compile(&format!(
            "id = \"agent\"\n\n[payload]\nevent = \"hook_event_name\"\n\
             session = [\"session_id\"]\ncwd = [\"cwd\"]\n\n{events}",
        ))
    }

    fn compile(content: &str) -> CompiledHookManifest {
        CompiledHookManifest::compile(HookManifest::parse(content).expect("a valid mapping"))
    }

    /// The detail of the event a payload produces, or nothing.
    fn detail_of(manifest: &CompiledHookManifest, payload: &Value) -> Option<Map<String, Value>> {
        manifest.normalize(payload).expect("an event").detail
    }

    #[test]
    fn maps_an_event_name_to_a_kind() {
        let manifest = mapping("[[events]]\nname = \"Stop\"\nkind = \"turn_end\"\n");
        let event = manifest
            .normalize(&json!({"hook_event_name": "Stop", "session_id": "s1", "cwd": "/work"}))
            .expect("an event");

        assert_eq!(event.agent.as_str(), "agent");
        assert_eq!(event.session, "s1");
        assert_eq!(event.kind, Kind::TurnEnd);
        assert_eq!(event.source, Source::Hook);
        assert_eq!(event.cwd.as_deref(), Some("/work"));
        assert!(event.detail.is_none());
        assert!(event.correlation.is_none());
    }

    #[test]
    fn carries_the_payload_verbatim() {
        let manifest = mapping("[[events]]\nname = \"Stop\"\nkind = \"turn_end\"\n");
        let payload = json!({
            "hook_event_name": "Stop",
            "session_id": "s1",
            "something_nobody_mapped": {"nested": [1, 2, 3]},
        });
        let event = manifest.normalize(&payload).expect("an event");
        assert_eq!(event.raw.as_ref(), Some(&payload));
    }

    #[test]
    fn says_nothing_about_an_event_it_does_not_map() {
        let manifest = mapping("[[events]]\nname = \"Stop\"\nkind = \"turn_end\"\n");
        assert!(
            manifest
                .normalize(&json!({"hook_event_name": "FileChanged", "session_id": "s1"}))
                .is_none()
        );
    }

    #[test]
    fn says_nothing_about_a_payload_it_cannot_read() {
        let manifest = mapping("[[events]]\nname = \"Stop\"\nkind = \"turn_end\"\n");
        for payload in [
            // Not an object at all.
            json!("Stop"),
            json!([{"hook_event_name": "Stop", "session_id": "s1"}]),
            // The field that names the event is missing, or is not a string.
            json!({"session_id": "s1"}),
            json!({"hook_event_name": 7, "session_id": "s1"}),
        ] {
            assert!(
                manifest.normalize(&payload).is_none(),
                "{payload} should have produced nothing",
            );
        }
    }

    #[test]
    fn refuses_a_payload_with_no_session() {
        let manifest = mapping("[[events]]\nname = \"Stop\"\nkind = \"turn_end\"\n");
        for payload in [
            json!({"hook_event_name": "Stop"}),
            json!({"hook_event_name": "Stop", "session_id": ""}),
            json!({"hook_event_name": "Stop", "session_id": 12}),
            json!({"hook_event_name": "Stop", "session_id": null}),
        ] {
            assert!(
                manifest.normalize(&payload).is_none(),
                "{payload} should have produced nothing",
            );
        }
    }

    #[test]
    fn takes_the_first_alternative_that_answers() {
        let manifest = compile(
            "id = \"agent\"\n\n[payload]\nevent = \"type\"\n\
             session = [\"sessionID\", \"session_id\"]\ncwd = [\"directory\", \"cwd\"]\n\n\
             [[events]]\nname = \"tool\"\nkind = \"tool_start\"\n",
        );

        let both = manifest
            .normalize(&json!({
                "type": "tool",
                "sessionID": "first",
                "session_id": "second",
                "directory": "/first",
                "cwd": "/second",
            }))
            .expect("an event");
        assert_eq!(both.session, "first");
        assert_eq!(both.cwd.as_deref(), Some("/first"));

        // A blank or wrongly-typed first alternative is passed over, not taken.
        let fallback = manifest
            .normalize(&json!({
                "type": "tool",
                "sessionID": "",
                "session_id": "second",
                "directory": 7,
                "cwd": "/second",
            }))
            .expect("an event");
        assert_eq!(fallback.session, "second");
        assert_eq!(fallback.cwd.as_deref(), Some("/second"));
    }

    #[test]
    fn a_payload_with_no_directory_simply_reports_none() {
        let manifest = mapping("[[events]]\nname = \"Stop\"\nkind = \"turn_end\"\n");
        let event = manifest
            .normalize(&json!({"hook_event_name": "Stop", "session_id": "s1"}))
            .expect("an event");
        assert!(event.cwd.is_none());
    }

    #[test]
    fn a_mapping_that_names_no_directory_field_never_reports_one() {
        let manifest = compile(
            "id = \"agent\"\n\n[payload]\nevent = \"type\"\nsession = [\"session_id\"]\n\n\
             [[events]]\nname = \"tool\"\nkind = \"tool_start\"\n",
        );
        let event = manifest
            .normalize(&json!({"type": "tool", "session_id": "s1", "cwd": "/work"}))
            .expect("an event");
        assert!(event.cwd.is_none());
    }

    #[test]
    fn a_condition_admits_the_values_it_names_and_drops_the_rest() {
        let manifest = mapping(
            "[[events]]\nname = \"Notification\"\nkind = \"blocked\"\n\
             when = { field = \"notification_type\", in = [\"permission\", \"elicitation\"] }\n",
        );

        for admitted in ["permission", "elicitation"] {
            let event = manifest
                .normalize(&json!({
                    "hook_event_name": "Notification",
                    "session_id": "s1",
                    "notification_type": admitted,
                }))
                .expect("an event");
            assert_eq!(event.kind, Kind::Blocked);
        }

        for dropped in [
            json!({"hook_event_name": "Notification", "session_id": "s1", "notification_type": "idle"}),
            // The field the condition names is absent, or is not a string.
            json!({"hook_event_name": "Notification", "session_id": "s1"}),
            json!({"hook_event_name": "Notification", "session_id": "s1", "notification_type": true}),
        ] {
            assert!(
                manifest.normalize(&dropped).is_none(),
                "{dropped} should have produced nothing",
            );
        }
    }

    #[test]
    fn a_condition_on_one_value_reads_the_same_way() {
        let manifest = mapping(
            "[[events]]\nname = \"Notification\"\nkind = \"blocked\"\n\
             when = { field = \"notification_type\", equals = \"permission\" }\n",
        );
        assert!(
            manifest
                .normalize(&json!({
                    "hook_event_name": "Notification",
                    "session_id": "s1",
                    "notification_type": "permission",
                }))
                .is_some()
        );
        assert!(
            manifest
                .normalize(&json!({
                    "hook_event_name": "Notification",
                    "session_id": "s1",
                    "notification_type": "permission_prompt",
                }))
                .is_none(),
            "equality is exact, not a prefix",
        );
    }

    #[test]
    fn copies_a_field_verbatim_whatever_shape_it_is_in() {
        let manifest = mapping(
            "[[events]]\nname = \"PreToolUse\"\nkind = \"tool_start\"\n\
             detail = [{ field = \"tool\", from = \"tool_name\" }]\n",
        );

        let structured = json!({"name": "Bash", "input": {"command": "ls"}});
        let detail = detail_of(
            &manifest,
            &json!({
                "hook_event_name": "PreToolUse",
                "session_id": "s1",
                "tool_name": structured,
            }),
        )
        .expect("a detail");
        assert_eq!(detail.get("tool"), Some(&structured));

        // An absent source omits its entry, which leaves nothing to report.
        assert!(
            detail_of(
                &manifest,
                &json!({"hook_event_name": "PreToolUse", "session_id": "s1"}),
            )
            .is_none(),
        );
    }

    #[test]
    fn writes_a_constant_where_the_payload_has_nothing_to_say() {
        let manifest = mapping(
            "[[events]]\nname = \"PreCompact\"\nkind = \"compact\"\n\
             detail = [{ field = \"phase\", value = \"pre\" }]\n",
        );
        let detail = detail_of(
            &manifest,
            &json!({"hook_event_name": "PreCompact", "session_id": "s1"}),
        )
        .expect("a detail");
        assert_eq!(detail.get("phase"), Some(&json!("pre")));
    }

    #[test]
    fn reports_whether_a_field_says_anything() {
        let manifest = mapping(
            "[[events]]\nname = \"PostToolUse\"\nkind = \"tool_end\"\n\
             detail = [{ field = \"error\", from = \"tool_error\", transform = \"nonempty\" }]\n",
        );
        let detail_for = |value: Option<Value>| {
            let mut payload = json!({"hook_event_name": "PostToolUse", "session_id": "s1"});
            if let Some(value) = value {
                payload["tool_error"] = value;
            }
            detail_of(&manifest, &payload)
        };

        for nothing in [None, Some(json!(null)), Some(json!(false)), Some(json!(""))] {
            assert!(
                detail_for(nothing.clone()).is_none(),
                "{nothing:?} should have said nothing",
            );
        }

        for something in [
            json!(true),
            json!("the tool exploded"),
            // A number is a value the payload holds, and zero is a number.
            json!(0),
            json!({"code": 2}),
            json!([]),
        ] {
            let detail = detail_for(Some(something.clone())).expect("a detail");
            assert_eq!(
                detail.get("error"),
                Some(&json!(true)),
                "{something} should have counted",
            );
        }
    }

    #[test]
    fn an_empty_detail_is_no_detail_at_all() {
        let manifest = mapping(
            "[[events]]\nname = \"PostToolUse\"\nkind = \"tool_end\"\n\
             detail = [\n  { field = \"tool\", from = \"tool_name\" },\n  \
             { field = \"error\", from = \"tool_error\", transform = \"nonempty\" },\n]\n",
        );

        let event = manifest
            .normalize(&json!({"hook_event_name": "PostToolUse", "session_id": "s1"}))
            .expect("an event");
        assert!(event.detail.is_none());

        // One entry answering is enough for a detail to exist.
        let partial = detail_of(
            &manifest,
            &json!({"hook_event_name": "PostToolUse", "session_id": "s1", "tool_name": "Bash"}),
        )
        .expect("a detail");
        assert_eq!(partial.len(), 1);
        assert_eq!(partial.get("tool"), Some(&json!("Bash")));
    }

    #[test]
    fn every_event_of_a_mapping_answers_to_its_own_name() {
        let manifest = mapping(
            "[[events]]\nname = \"SessionStart\"\nkind = \"session_start\"\n\n\
             [[events]]\nname = \"UserPromptSubmit\"\nkind = \"turn_start\"\n\n\
             [[events]]\nname = \"SessionEnd\"\nkind = \"session_end\"\n",
        );
        for (name, kind) in [
            ("SessionStart", Kind::SessionStart),
            ("UserPromptSubmit", Kind::TurnStart),
            ("SessionEnd", Kind::SessionEnd),
        ] {
            let event = manifest
                .normalize(&json!({"hook_event_name": name, "session_id": "s1"}))
                .expect("an event");
            assert_eq!(event.kind, kind);
        }
    }

    #[test]
    fn matching_is_exact() {
        let manifest = mapping("[[events]]\nname = \"Stop\"\nkind = \"turn_end\"\n");
        for spelling in ["stop", "STOP", " Stop", "Stop "] {
            assert!(
                manifest
                    .normalize(&json!({"hook_event_name": spelling, "session_id": "s1"}))
                    .is_none(),
                "{spelling:?} is not the name the mapping wrote",
            );
        }
    }

    #[test]
    fn the_free_function_and_the_method_agree() {
        let manifest = mapping("[[events]]\nname = \"Stop\"\nkind = \"turn_end\"\n");
        let payload = json!({"hook_event_name": "Stop", "session_id": "s1"});
        assert_eq!(normalize(&manifest, &payload), manifest.normalize(&payload));
    }
}
