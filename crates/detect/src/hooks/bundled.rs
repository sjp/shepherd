//! The hook mappings that ship inside this library.
//!
//! The same floor the screen family has: a machine with nothing installed on it
//! still knows how to read the payloads of the agents this library was built
//! knowing about, and a copy found on disk or fetched later shadows the bundled
//! one for that agent.
//!
//! Each entry is keyed by the `id` its manifest declares, never by the file it
//! came from, because the id is what a caller asks for.

/// Every bundled hook mapping, as (declared id, TOML source).
///
/// Sorted by id so that the list reads the way a listing of it prints. The
/// contents are not parsed here: a mapping is validated when it is loaded, and
/// a test in this module proves the bundled ones survive that. An agent no
/// mapping describes normalizes to nothing, which is the same answer an
/// unmapped event gets.
pub(crate) const BUNDLED_HOOKS: &[(&str, &str)] = &[
    (
        "antigravity",
        include_str!("../../manifests/hooks/antigravity.toml"),
    ),
    ("claude", include_str!("../../manifests/hooks/claude.toml")),
    ("codex", include_str!("../../manifests/hooks/codex.toml")),
    ("cursor", include_str!("../../manifests/hooks/cursor.toml")),
    ("devin", include_str!("../../manifests/hooks/devin.toml")),
    ("droid", include_str!("../../manifests/hooks/droid.toml")),
    (
        "github-copilot",
        include_str!("../../manifests/hooks/github-copilot.toml"),
    ),
    ("grok", include_str!("../../manifests/hooks/grok.toml")),
    ("hermes", include_str!("../../manifests/hooks/hermes.toml")),
    ("kilo", include_str!("../../manifests/hooks/kilo.toml")),
    ("kimi", include_str!("../../manifests/hooks/kimi.toml")),
    (
        "mastracode",
        include_str!("../../manifests/hooks/mastracode.toml"),
    ),
    ("omp", include_str!("../../manifests/hooks/omp.toml")),
    (
        "opencode",
        include_str!("../../manifests/hooks/opencode.toml"),
    ),
    ("pi", include_str!("../../manifests/hooks/pi.toml")),
    (
        "qodercli",
        include_str!("../../manifests/hooks/qodercli.toml"),
    ),
    ("qwen", include_str!("../../manifests/hooks/qwen.toml")),
];

/// The bundled mappings.
///
/// A function rather than the constant itself so that callers outside this
/// module never grow a habit of indexing into a fixed list.
pub(crate) fn bundled_hook_manifests() -> &'static [(&'static str, &'static str)] {
    BUNDLED_HOOKS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::CompiledHookManifest;
    use crate::hooks::schema::{HOOKS_ENGINE_VERSION, HookManifest};
    use crate::store::MAX_MANIFEST_BYTES;
    use agentbus_protocol::Kind;
    use serde_json::Value;
    use std::collections::HashSet;

    /// How many agents the bundled mappings cover. Asserted rather than derived
    /// so that a mapping dropped from the list has to be an explicit decision.
    const BUNDLED_COUNT: usize = 17;

    #[test]
    fn every_bundled_manifest_loads_cleanly_under_the_key_it_is_filed_by() {
        assert_eq!(bundled_hook_manifests().len(), BUNDLED_COUNT);

        let mut seen = HashSet::new();
        for (id, content) in bundled_hook_manifests() {
            let manifest = HookManifest::parse(content)
                .unwrap_or_else(|error| panic!("bundled mapping {id:?} is not loadable: {error}"));
            assert_eq!(
                &manifest.id, id,
                "registry key {id:?} does not match the id the manifest declares",
            );
            assert!(seen.insert(*id), "duplicate registry key {id:?}");
            let required = manifest.min_engine_version.unwrap_or(1);
            assert!(
                required <= HOOKS_ENGINE_VERSION,
                "bundled mapping {id:?} asks for engine {required}, this engine speaks \
                 {HOOKS_ENGINE_VERSION}",
            );
        }
    }

    #[test]
    fn every_bundled_manifest_is_readable_where_a_copy_of_it_would_be() {
        // A bundled mapping big enough that the same file on disk would be
        // refused is a mapping nobody could override or update, since every
        // tier above the bundled one is read through that cap.
        for (id, content) in bundled_hook_manifests() {
            assert!(
                content.len() as u64 <= MAX_MANIFEST_BYTES,
                "bundled mapping {id:?} is {} bytes, over the {MAX_MANIFEST_BYTES}-byte limit \
                 every other tier is read under",
                content.len(),
            );
        }
    }

    /// A payload as its agent delivers it, and what the mapping has to make of
    /// it.
    ///
    /// One per bundled mapping, checked exhaustively below, because a mapping
    /// that parses is not a mapping that works: every field name in it is a
    /// claim about somebody else's payload, and the only way to hold a claim
    /// like that to account is to hand it the payload and look at what comes
    /// back. The session and the directory are spelled differently by every
    /// agent here, which is exactly the thing the mapping exists to absorb.
    ///
    /// The third column is the directory the mapping is expected to report.
    /// Almost every agent names one; the two that do not name a list of working
    /// roots instead, which is not something a directory can be read out of, and
    /// a mapping that claimed otherwise would report a key that is silently
    /// always absent.
    const SESSION_STARTS: &[(&str, &str, Option<&str>)] = &[
        (
            "antigravity",
            r#"{"conversationId": "s1", "workspacePaths": ["/w"]}"#,
            None,
        ),
        (
            "claude",
            r#"{"hook_event_name": "SessionStart", "session_id": "s1", "cwd": "/w"}"#,
            Some("/w"),
        ),
        (
            "codex",
            r#"{"hook_event_name": "SessionStart", "session_id": "s1", "cwd": "/w"}"#,
            Some("/w"),
        ),
        (
            "cursor",
            r#"{"hook_event_name": "sessionStart", "session_id": "s1",                "conversation_id": "c1", "workspace_roots": ["/w"]}"#,
            None,
        ),
        (
            "devin",
            r#"{"hook_event_name": "SessionStart", "session_id": "s1", "cwd": "/w"}"#,
            Some("/w"),
        ),
        (
            "droid",
            r#"{"hook_event_name": "SessionStart", "session_id": "s1", "cwd": "/w"}"#,
            Some("/w"),
        ),
        (
            "github-copilot",
            r#"{"hook_event_name": "SessionStart", "session_id": "s1", "cwd": "/w",                "source": "startup"}"#,
            Some("/w"),
        ),
        (
            "grok",
            r#"{"hookEventName": "SessionStart", "sessionId": "s1", "cwd": "/w"}"#,
            Some("/w"),
        ),
        (
            "hermes",
            r#"{"event": "on_session_start", "session_id": "s1", "platform": "tui",                "cwd": "/w"}"#,
            Some("/w"),
        ),
        (
            "kilo",
            r#"{"type": "session.created", "sessionID": "s1", "directory": "/w"}"#,
            Some("/w"),
        ),
        (
            "kimi",
            r#"{"hook_event_name": "SessionStart", "session_id": "s1", "cwd": "/w",                "session_title": "a session", "client_type": "kimi_code_cli"}"#,
            Some("/w"),
        ),
        (
            "mastracode",
            r#"{"hook_event_name": "SessionStart", "session_id": "s1", "cwd": "/w"}"#,
            Some("/w"),
        ),
        (
            "omp",
            r#"{"event": "session_start", "session_id": "s1", "has_ui": true,                "cwd": "/w"}"#,
            Some("/w"),
        ),
        (
            "opencode",
            r#"{"type": "session.created", "sessionID": "s1", "directory": "/w"}"#,
            Some("/w"),
        ),
        (
            "pi",
            r#"{"event": "session_start", "session_id": "s1", "mode": "tui",                "cwd": "/w"}"#,
            Some("/w"),
        ),
        (
            "qodercli",
            r#"{"hook_event_name": "SessionStart", "session_id": "s1", "cwd": "/w"}"#,
            Some("/w"),
        ),
        (
            "qwen",
            r#"{"hook_event_name": "SessionStart", "session_id": "s1", "cwd": "/w",                "source": "resume"}"#,
            Some("/w"),
        ),
    ];

    /// The agents whose payload does not name its own event, and the name
    /// whoever runs the hook has to supply for it.
    ///
    /// One shape per event and no field saying which, so the mapping names no
    /// event field and the name comes from the caller. Everything not listed
    /// here says so in its payload and is read without help.
    const NAMED_BY_CALLER: &[(&str, &str)] = &[("antigravity", "PreInvocation")];

    /// What the caller has to say about `id`'s payload, if anything.
    fn named(id: &str) -> Option<&'static str> {
        NAMED_BY_CALLER
            .iter()
            .find(|(agent, _)| *agent == id)
            .map(|(_, event)| *event)
    }

    /// The mapping filed under `id`, ready to read a payload with.
    fn compiled(id: &str) -> CompiledHookManifest {
        let (_, content) = bundled_hook_manifests()
            .iter()
            .find(|(name, _)| *name == id)
            .unwrap_or_else(|| panic!("no bundled mapping for {id:?}"));
        let manifest = HookManifest::parse(content).expect("loadable");
        manifest.validate().expect("valid");
        CompiledHookManifest::compile(manifest)
    }

    #[test]
    fn every_bundled_manifest_turns_its_agents_session_start_into_a_session_beginning() {
        let covered: Vec<&str> = SESSION_STARTS.iter().map(|(id, _, _)| *id).collect();
        let bundled: Vec<&str> = bundled_hook_manifests().iter().map(|(id, _)| *id).collect();
        assert_eq!(covered, bundled, "every mapping needs a payload to answer");

        for (id, payload, cwd) in SESSION_STARTS {
            let payload: Value = serde_json::from_str(payload).expect("a payload");
            let event = compiled(id)
                .normalize_named(&payload, named(id))
                .unwrap_or_else(|| panic!("{id} said nothing about a session starting"));

            assert_eq!(event.agent.as_str(), *id);
            assert_eq!(event.kind, Kind::SessionStart, "{id}");
            assert_eq!(event.session, "s1", "{id}");
            assert_eq!(event.cwd.as_deref(), *cwd, "{id}");
            assert_eq!(event.raw.as_ref(), Some(&payload), "{id}");
        }
    }

    #[test]
    fn the_agent_that_names_a_conversation_rather_than_a_session_is_still_understood() {
        let payload: Value =
            serde_json::from_str(r#"{"hook_event_name": "sessionStart", "conversation_id": "c1"}"#)
                .expect("a payload");

        let event = compiled("cursor").normalize(&payload).expect("an event");

        assert_eq!(event.session, "c1");
    }

    #[test]
    fn an_agent_that_says_why_a_session_began_has_it_reported() {
        let payload: Value = serde_json::from_str(
            r#"{"hook_event_name": "SessionStart", "session_id": "s1", "source": "resume"}"#,
        )
        .expect("a payload");

        let event = compiled("qwen").normalize(&payload).expect("an event");

        assert_eq!(
            event.detail.expect("a detail")["source"],
            Value::from("resume"),
        );
    }

    #[test]
    fn the_agent_registered_for_more_than_one_event_maps_all_of_them() {
        let manifest = compiled("devin");
        let expected = [
            ("SessionStart", Kind::SessionStart),
            ("UserPromptSubmit", Kind::TurnStart),
            ("PreToolUse", Kind::ToolStart),
            ("PostToolUse", Kind::ToolEnd),
            ("PermissionRequest", Kind::Blocked),
            ("Stop", Kind::TurnEnd),
        ];

        for (name, kind) in expected {
            let payload: Value = serde_json::from_str(&format!(
                r#"{{"hook_event_name": "{name}", "sessionId": "s1"}}"#
            ))
            .expect("a payload");

            let event = manifest
                .normalize(&payload)
                .unwrap_or_else(|| panic!("{name} produced nothing"));
            assert_eq!(event.kind, kind, "{name}");
            assert_eq!(
                event.session, "s1",
                "{name} should read the second spelling of the session",
            );
        }
    }

    #[test]
    fn an_agent_whose_payload_names_no_event_is_read_only_when_the_caller_names_one() {
        for (id, event) in NAMED_BY_CALLER {
            let (_, payload, _) = SESSION_STARTS
                .iter()
                .find(|(agent, _, _)| agent == id)
                .unwrap_or_else(|| panic!("no payload for {id:?}"));
            let payload: Value = serde_json::from_str(payload).expect("a payload");
            let manifest = compiled(id);

            assert!(
                manifest.normalize(&payload).is_none(),
                "{id} was read without being told which event this is",
            );
            assert_eq!(
                manifest
                    .normalize_named(&payload, Some(event))
                    .expect("an event")
                    .kind,
                Kind::SessionStart,
            );
            assert!(
                manifest
                    .normalize_named(&payload, Some("SomethingElse"))
                    .is_none(),
                "{id} answered to an event it does not map",
            );
        }
    }

    #[test]
    fn the_agent_whose_plugin_reports_every_interface_is_read_only_where_a_person_is_at_one() {
        let manifest = compiled("hermes");
        let payload = |event: &str, platform: &str| -> Value {
            serde_json::from_str(&format!(
                r#"{{"event": "{event}", "session_id": "s1", "platform": "{platform}",
                     "cwd": "/w"}}"#
            ))
            .expect("a payload")
        };
        let mapped = [
            ("on_session_start", Kind::SessionStart),
            ("on_session_reset", Kind::SessionStart),
            ("pre_llm_call", Kind::TurnStart),
        ];

        for (name, kind) in mapped {
            let event = manifest
                .normalize(&payload(name, "tui"))
                .unwrap_or_else(|| panic!("{name} produced nothing"));
            assert_eq!(event.kind, kind, "{name}");
            assert_eq!(event.session, "s1", "{name}");
            assert_eq!(event.cwd.as_deref(), Some("/w"), "{name}");

            // The same callback, for a session the agent is running underneath
            // the one somebody opened. The plugin forwards it either way; this
            // is where it stops.
            assert!(
                manifest.normalize(&payload(name, "subagent")).is_none(),
                "{name} was reported for a session nobody is sitting at",
            );
        }
    }

    #[test]
    fn the_agent_whose_extension_reports_the_interface_by_name_is_read_only_at_a_terminal() {
        let manifest = compiled("pi");
        let payload = |event: &str, mode: &str| -> Value {
            serde_json::from_str(&format!(
                r#"{{"event": "{event}", "session_id": "s1", "mode": "{mode}",
                     "cwd": "/w"}}"#
            ))
            .expect("a payload")
        };
        let mapped = [
            ("session_start", Kind::SessionStart),
            ("agent_start", Kind::TurnStart),
            ("agent_settled", Kind::TurnEnd),
        ];

        for (name, kind) in mapped {
            let event = manifest
                .normalize(&payload(name, "tui"))
                .unwrap_or_else(|| panic!("{name} produced nothing"));
            assert_eq!(event.kind, kind, "{name}");
            assert_eq!(event.session, "s1", "{name}");
            assert_eq!(event.cwd.as_deref(), Some("/w"), "{name}");

            // The same event, in a mode the agent is driven through rather than
            // typed at. The extension forwards it either way; this is where it
            // stops.
            assert!(
                manifest.normalize(&payload(name, "rpc")).is_none(),
                "{name} was reported for a session nobody is sitting at",
            );
        }
    }

    #[test]
    fn the_agent_that_shares_that_extension_layout_maps_its_own_wider_event_set() {
        let manifest = compiled("omp");
        let expected = [
            ("session_start", Kind::SessionStart),
            ("session_switch", Kind::SessionStart),
            ("session_shutdown", Kind::SessionEnd),
            ("agent_start", Kind::TurnStart),
            ("agent_end", Kind::TurnEnd),
            ("tool_execution_start", Kind::ToolStart),
            ("tool_execution_end", Kind::ToolEnd),
            ("tool_approval_requested", Kind::Blocked),
            ("tool_approval_resolved", Kind::Unblocked),
        ];

        for (name, kind) in expected {
            let payload: Value = serde_json::from_str(&format!(
                r#"{{"event": "{name}", "session_id": "s1", "has_ui": true,
                     "tool": "bash", "cwd": "/w"}}"#
            ))
            .expect("a payload");

            let event = manifest
                .normalize(&payload)
                .unwrap_or_else(|| panic!("{name} produced nothing"));
            assert_eq!(event.kind, kind, "{name}");
            assert_eq!(event.session, "s1", "{name}");
            assert_eq!(event.cwd.as_deref(), Some("/w"), "{name}");
        }

        let payload: Value = serde_json::from_str(
            r#"{"event": "tool_execution_start", "session_id": "s1", "tool": "bash"}"#,
        )
        .expect("a payload");
        assert_eq!(
            manifest
                .normalize(&payload)
                .expect("an event")
                .detail
                .expect("a detail")["tool"],
            Value::from("bash"),
        );
    }

    #[test]
    fn the_callback_an_installed_plugin_names_for_itself_is_what_starts_a_turn() {
        let manifest = compiled("kilo");
        let payload: Value = serde_json::from_str(
            r#"{"type": "chat.message", "sessionID": "s1", "directory": "/w"}"#,
        )
        .expect("a payload");

        let event = manifest.normalize(&payload).expect("an event");

        assert_eq!(event.kind, Kind::TurnStart);
        assert_eq!(event.session, "s1");
        assert_eq!(event.cwd.as_deref(), Some("/w"));
    }

    #[test]
    fn an_agent_that_says_when_it_stopped_waiting_has_both_halves_mapped() {
        let manifest = compiled("kilo");
        let expected = [
            ("permission.asked", Kind::Blocked),
            ("permission.replied", Kind::Unblocked),
            ("question.asked", Kind::Blocked),
            ("question.replied", Kind::Unblocked),
            ("question.rejected", Kind::Unblocked),
        ];

        for (name, kind) in expected {
            let payload: Value =
                serde_json::from_str(&format!(r#"{{"type": "{name}", "sessionID": "s1"}}"#))
                    .expect("a payload");

            let event = manifest
                .normalize(&payload)
                .unwrap_or_else(|| panic!("{name} produced nothing"));
            assert_eq!(event.kind, kind, "{name}");
        }
    }

    #[test]
    fn a_payload_that_names_its_own_event_is_believed_over_the_caller() {
        let payload: Value =
            serde_json::from_str(r#"{"hookEventName": "SessionStart", "sessionId": "s1"}"#)
                .expect("a payload");

        let event = compiled("grok")
            .normalize_named(&payload, Some("Stop"))
            .expect("an event");

        assert_eq!(event.kind, Kind::SessionStart);
    }

    #[test]
    fn every_bundled_manifest_maps_something() {
        // A mapping with no events parses and answers nothing, which is a
        // slower way of not shipping it.
        for (id, content) in bundled_hook_manifests() {
            let manifest = HookManifest::parse(content).expect("loadable");
            assert!(
                !manifest.events.is_empty(),
                "bundled mapping {id:?} maps no events",
            );
        }
    }
}
