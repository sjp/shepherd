//! The hook-mapping dialect: what a mapping may say, and what makes one
//! unacceptable.
//!
//! A hook-mapping manifest describes how one agent's hook payloads become
//! normalized events: which field of the payload names the event, where the
//! session id and working directory are to be found, and what each event name
//! means. It is the same bargain the screen dialect strikes — the engine knows
//! *how* to read a payload, the manifest knows *what* the payload says — so a
//! harness nobody has heard of is supported by writing a file rather than a
//! module.
//!
//! # Deliberately tiny
//!
//! Field lookup, string equality, a constant, and one named transform. There
//! are no regexes, no expressions and no paths into nested values, and that is
//! a decision rather than an omission: this dialect is interpreted inside
//! somebody's coding agent while they wait, so everything it can ask for has to
//! be something that can be answered by one hash lookup. New expressive power
//! arrives by growing the closed sets — a transform, a kind — behind an engine
//! version, which is also why unknown keys are errors here rather than ignored:
//! a manifest leaning on a key this engine never heard of should be told so
//! instead of quietly mapping less than its author expected.
//!
//! # What bounds one
//!
//! Nothing in this file counts rules or nests, because there is nothing here
//! that nests and nothing that is compiled. A mapping is a flat list of event
//! names looked up by string equality, so the work of reading a payload is
//! linear in the size of the file it came from — and the size of that file is
//! already capped by whoever read it off disk.

use std::collections::HashSet;

use agentbus_protocol::{Agent, InvalidAgent, Kind};
use serde::Deserialize;

use crate::version::ManifestVersion;

/// The dialect this engine speaks.
///
/// A manifest may declare the lowest engine it needs; anything above this is
/// refused rather than half-understood.
///
/// Two, since a mapping may leave out the field naming the event, for the
/// agents whose payload does not carry one. See [`PayloadFields::event`].
pub const HOOKS_ENGINE_VERSION: u32 = 2;

/// One agent's hook-mapping manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookManifest {
    /// The agent this manifest describes, and the agent id its events are
    /// emitted under.
    pub id: String,
    /// The manifest's own version, used to decide whether a fetched copy is
    /// newer than the one already held.
    #[serde(default)]
    pub version: Option<ManifestVersion>,
    /// The lowest engine version that can honour everything below.
    #[serde(default)]
    pub min_engine_version: Option<u32>,
    /// When the manifest was last edited, in whatever form its author wrote it.
    /// Carried verbatim and never parsed: nothing here needs a calendar.
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Other names this agent goes by.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Where the parts of the envelope live in the payload.
    pub payload: PayloadFields,
    /// What each event name means, in the order they were written.
    #[serde(default)]
    pub events: Vec<EventMapping>,
}

/// Where the fields every event needs are found in a payload.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadFields {
    /// The field whose string value names the event.
    ///
    /// Left out by a mapping for an agent whose payload does not name its own
    /// event — one payload shape per event, with whoever registered the hook
    /// expected to know which one they registered. Such a payload can only be
    /// read by a caller that says which event it is about, and one that does
    /// not say produces nothing at all. Naming a field is therefore the normal
    /// case and leaving it out is a claim about the agent, which is why it has
    /// to be written down rather than inferred from a payload that happens to
    /// be missing it.
    #[serde(default)]
    pub event: Option<String>,
    /// Fields the session id may be under, best first. The first one holding a
    /// non-empty string answers.
    ///
    /// A list rather than a single name because a payload's own spelling is not
    /// this program's to control: an agent that renames the field, or spells it
    /// two ways across its event set, costs a second entry here rather than
    /// every event in a session.
    pub session: Vec<String>,
    /// Fields the working directory may be under, read the same way. A payload
    /// that names none simply reports no directory.
    #[serde(default)]
    pub cwd: Vec<String>,
}

/// What one event name means.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventMapping {
    /// The value of the event field this entry answers to, matched by exact
    /// string equality. Unique within its manifest.
    pub name: String,
    /// The normalized kind the event becomes.
    pub kind: Kind,
    /// A condition on the payload that must hold for this entry to apply. An
    /// entry whose condition fails maps to nothing at all.
    #[serde(default)]
    pub when: Option<Condition>,
    /// What travels beside the event as normalized extras.
    #[serde(default)]
    pub detail: Vec<Projection>,
}

/// A condition on one field of the payload.
///
/// Exactly one of [`equals`](Self::equals) and [`one_of`](Self::one_of) is
/// written; which of the two a manifest reaches for is a matter of how many
/// values it is naming, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    /// The field to look at.
    pub field: String,
    /// The one value that satisfies the condition.
    #[serde(default)]
    pub equals: Option<String>,
    /// The set of values that satisfy it.
    #[serde(default, rename = "in")]
    pub one_of: Option<Vec<String>>,
}

impl Condition {
    /// Whether `value`, the payload's value for the named field, satisfies this
    /// condition.
    ///
    /// A field that is absent or is not a string satisfies nothing. Comparing a
    /// number or an object against a written value would mean deciding how each
    /// of them spells itself, and a dialect that only ever names strings has no
    /// business making that decision on an author's behalf.
    pub(crate) fn holds(&self, value: Option<&serde_json::Value>) -> bool {
        let Some(found) = value.and_then(serde_json::Value::as_str) else {
            return false;
        };
        if let Some(expected) = &self.equals {
            return found == expected;
        }
        match &self.one_of {
            Some(expected) => expected.iter().any(|expected| expected == found),
            // A validated condition names one of the two; this arm is only ever
            // reached by a caller that built one by hand and skipped validation.
            None => false,
        }
    }
}

/// One entry in an event's detail: a key, and where its value comes from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Projection {
    /// The key this entry writes in the detail.
    pub field: String,
    /// The payload field to take the value from, verbatim.
    #[serde(default)]
    pub from: Option<String>,
    /// A constant value, written here rather than read from the payload.
    #[serde(default)]
    pub value: Option<String>,
    /// What to do with the value read from the payload, instead of copying it.
    #[serde(default)]
    pub transform: Option<Transform>,
}

/// The closed set of things a projection can do to a value it read.
///
/// One member, and growing it is an engine-version decision. The alternative —
/// an expression language — is how a data format becomes a program, and a
/// program is exactly what this family exists not to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transform {
    /// Report whether the field says anything: `true` when the payload holds a
    /// value that is not null, `false` or the empty string, and nothing at all
    /// otherwise.
    ///
    /// This is the shape a payload uses when it reports something that either
    /// happened or did not, and each author picks their own spelling for it —
    /// a flag, a message, a structured value. What a consumer wants to know is
    /// which of the two it was.
    NonEmpty,
}

impl HookManifest {
    /// Parses and validates one manifest.
    pub fn parse(content: &str) -> Result<Self, HookManifestError> {
        let manifest: Self =
            toml::from_str(content).map_err(|error| HookManifestError::Syntax {
                message: error.to_string(),
            })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Checks everything [`HookManifest::parse`] checks after the TOML has been
    /// read, for a manifest that came from somewhere other than TOML.
    pub fn validate(&self) -> Result<(), HookManifestError> {
        // The id is what every event this manifest produces is attributed to,
        // so an id the envelope would refuse is caught here rather than at the
        // moment a payload arrives: a mapping that could only ever throw its
        // events away is a broken manifest, not a quiet one.
        if let Err(error) = Agent::new(self.id.trim()) {
            return Err(self.fault(HookFault::UnusableId { error }));
        }
        match self.min_engine_version {
            Some(required) if required > HOOKS_ENGINE_VERSION => {
                return Err(self.fault(HookFault::EngineTooNew {
                    required,
                    engine: HOOKS_ENGINE_VERSION,
                }));
            }
            _ => {}
        }

        if self
            .payload
            .event
            .as_ref()
            .is_some_and(|field| field.trim().is_empty())
        {
            return Err(self.fault(HookFault::EmptyFieldName { field: "event" }));
        }
        if self.payload.session.is_empty() {
            return Err(self.fault(HookFault::NoSessionField));
        }
        for (field, names) in [
            ("session", &self.payload.session),
            ("cwd", &self.payload.cwd),
        ] {
            if names.iter().any(|name| name.trim().is_empty()) {
                return Err(self.fault(HookFault::EmptyFieldName { field }));
            }
        }

        if self.events.is_empty() {
            return Err(self.fault(HookFault::NoEvents));
        }
        let mut seen = HashSet::with_capacity(self.events.len());
        for (index, event) in self.events.iter().enumerate() {
            if event.name.is_empty() {
                return Err(self.fault(HookFault::EmptyEventName { index }));
            }
            if !seen.insert(event.name.as_str()) {
                return Err(self.event_fault(event, HookFault::DuplicateEventName));
            }
            self.validate_event(event)?;
        }
        Ok(())
    }

    fn validate_event(&self, event: &EventMapping) -> Result<(), HookManifestError> {
        // An unknown kind is refused rather than carried: the set is closed so
        // that a subscriber can be written against all of it, and a manifest
        // that needs a kind this engine has never heard of is asking for an
        // engine it does not have.
        if !event.kind.is_known() {
            return Err(self.event_fault(
                event,
                HookFault::UnknownKind {
                    kind: event.kind.to_string(),
                },
            ));
        }

        if let Some(when) = &event.when {
            if when.field.trim().is_empty() {
                return Err(self.event_fault(event, HookFault::EmptyFieldName { field: "when" }));
            }
            match (&when.equals, &when.one_of) {
                (Some(_), Some(_)) => {
                    return Err(self.event_fault(event, HookFault::ConditionOverSpecified));
                }
                (None, None) => {
                    return Err(self.event_fault(event, HookFault::ConditionSaysNothing));
                }
                (None, Some(values)) if values.is_empty() => {
                    // A set nothing can be in is a rule that always fails,
                    // which is a longer way of deleting the entry.
                    return Err(self.event_fault(event, HookFault::ConditionSaysNothing));
                }
                _ => {}
            }
        }

        let mut keys = HashSet::with_capacity(event.detail.len());
        for projection in &event.detail {
            if projection.field.trim().is_empty() {
                return Err(self.event_fault(event, HookFault::EmptyFieldName { field: "detail" }));
            }
            if !keys.insert(projection.field.as_str()) {
                return Err(self.event_fault(
                    event,
                    HookFault::DuplicateDetailField {
                        field: projection.field.clone(),
                    },
                ));
            }
            match (&projection.from, &projection.value) {
                (Some(_), Some(_)) => {
                    return Err(self.event_fault(
                        event,
                        HookFault::ProjectionOverSpecified {
                            field: projection.field.clone(),
                        },
                    ));
                }
                (None, None) => {
                    return Err(self.event_fault(
                        event,
                        HookFault::ProjectionSaysNothing {
                            field: projection.field.clone(),
                        },
                    ));
                }
                (Some(from), None) if from.trim().is_empty() => {
                    return Err(
                        self.event_fault(event, HookFault::EmptyFieldName { field: "from" })
                    );
                }
                _ => {}
            }
            if projection.transform.is_some() && projection.from.is_none() {
                return Err(self.event_fault(
                    event,
                    HookFault::TransformWithoutSource {
                        field: projection.field.clone(),
                    },
                ));
            }
        }
        Ok(())
    }

    fn fault(&self, fault: HookFault) -> HookManifestError {
        HookManifestError::Manifest {
            manifest: self.id.clone(),
            fault,
        }
    }

    fn event_fault(&self, event: &EventMapping, fault: HookFault) -> HookManifestError {
        HookManifestError::Event {
            manifest: self.id.clone(),
            event: event.name.clone(),
            fault,
        }
    }
}

/// Why a hook-mapping manifest was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HookManifestError {
    /// The bytes were not a manifest at all: broken TOML, a value of the wrong
    /// type, a key this dialect does not have.
    #[error("manifest is not readable: {message}")]
    Syntax {
        /// What the parser said.
        message: String,
    },
    /// The manifest read cleanly but breaks a rule of the dialect.
    #[error("manifest {manifest:?}: {fault}")]
    Manifest {
        /// The manifest's id.
        manifest: String,
        /// What is wrong.
        fault: HookFault,
    },
    /// One event mapping breaks a rule of the dialect.
    #[error("manifest {manifest:?}, event {event:?}: {fault}")]
    Event {
        /// The manifest's id.
        manifest: String,
        /// The event name the entry answers to.
        event: String,
        /// What is wrong.
        fault: HookFault,
    },
}

/// The specific thing that is wrong with a hook-mapping manifest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HookFault {
    /// The id could not be used as an agent id, so no event it produced could
    /// be put on the wire.
    #[error("id is unusable: {error}")]
    UnusableId {
        /// Why the envelope refused it.
        error: InvalidAgent,
    },
    /// The manifest needs an engine newer than this one.
    #[error("needs engine version {required}, this engine speaks {engine}")]
    EngineTooNew {
        /// The version the manifest asked for.
        required: u32,
        /// The version this engine implements.
        engine: u32,
    },
    /// A field name was blank, which could never name a field.
    #[error("{field} names an empty field")]
    EmptyFieldName {
        /// Which setting the blank name was in.
        field: &'static str,
    },
    /// A payload with no session field can only ever produce events that belong
    /// to nothing.
    #[error("payload.session must name at least one field")]
    NoSessionField,
    /// A manifest with no events maps nothing.
    #[error("must map at least one event")]
    NoEvents,
    /// An event with no name could never be matched, since matching is by exact
    /// equality with what the payload says.
    #[error("the event at position {index} has an empty name")]
    EmptyEventName {
        /// Where the entry sits in the file, counting from zero.
        index: usize,
    },
    /// Two entries answer to one event name, so which of them applies would
    /// depend on the order they happen to be written in.
    #[error("duplicate event name")]
    DuplicateEventName,
    /// The kind is not one this engine knows.
    #[error("kind {kind:?} is not one this engine knows")]
    UnknownKind {
        /// The kind the manifest asked for.
        kind: String,
    },
    /// A condition said two things at once.
    #[error("when names both equals and in")]
    ConditionOverSpecified,
    /// A condition said nothing that could ever hold.
    #[error("when names neither equals nor a non-empty in")]
    ConditionSaysNothing,
    /// Two projections write the same detail key, so one would silently replace
    /// the other.
    #[error("two detail entries write {field:?}")]
    DuplicateDetailField {
        /// The key both entries wrote.
        field: String,
    },
    /// A projection said two things at once.
    #[error("detail entry {field:?} names both from and value")]
    ProjectionOverSpecified {
        /// The detail key of the offending entry.
        field: String,
    },
    /// A projection named no source at all.
    #[error("detail entry {field:?} names neither from nor value")]
    ProjectionSaysNothing {
        /// The detail key of the offending entry.
        field: String,
    },
    /// A transform was asked for where there is nothing to transform.
    #[error("detail entry {field:?} has a transform but no from")]
    TransformWithoutSource {
        /// The detail key of the offending entry.
        field: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest around whatever the test wants to say about one event.
    fn manifest_with(event_body: &str) -> String {
        format!(
            "id = \"agent\"\n\n[payload]\nevent = \"hook_event_name\"\nsession = [\"session_id\"]\n\n\
             [[events]]\nname = \"Something\"\n{event_body}\n",
        )
    }

    fn parse(content: &str) -> Result<HookManifest, HookManifestError> {
        HookManifest::parse(content)
    }

    fn fault(content: &str) -> HookFault {
        match parse(content).expect_err("should have been refused") {
            HookManifestError::Manifest { fault, .. } | HookManifestError::Event { fault, .. } => {
                fault
            }
            HookManifestError::Syntax { message } => {
                panic!("expected a dialect fault, got a syntax error: {message}")
            }
        }
    }

    #[test]
    fn reads_every_field_of_the_dialect() {
        let manifest = parse(
            r#"
id = "agent"
version = "2026.08.20.1"
min_engine_version = 1
updated_at = "2026-08-20"
aliases = ["other-name"]

[payload]
event = "hook_event_name"
session = ["session_id", "sessionID"]
cwd = ["cwd", "directory"]

[[events]]
name = "PreToolUse"
kind = "tool_start"
detail = [{ field = "tool", from = "tool_name" }]

[[events]]
name = "Notification"
kind = "blocked"
when = { field = "notification_type", in = ["permission", "elicitation"] }
detail = [
  { field = "phase", value = "pre" },
  { field = "error", from = "tool_error", transform = "nonempty" },
]
"#,
        )
        .expect("a manifest using everything the dialect has");

        assert_eq!(manifest.id, "agent");
        assert_eq!(
            manifest
                .version
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("2026.08.20.1")
        );
        assert_eq!(manifest.min_engine_version, Some(1));
        assert_eq!(manifest.updated_at.as_deref(), Some("2026-08-20"));
        assert_eq!(manifest.aliases, ["other-name"]);
        assert_eq!(manifest.payload.event.as_deref(), Some("hook_event_name"));
        assert_eq!(manifest.payload.session, ["session_id", "sessionID"]);
        assert_eq!(manifest.payload.cwd, ["cwd", "directory"]);
        assert_eq!(manifest.events.len(), 2);

        let tool = &manifest.events[0];
        assert_eq!(tool.kind, Kind::ToolStart);
        assert!(tool.when.is_none());
        assert_eq!(tool.detail[0].from.as_deref(), Some("tool_name"));

        let notification = &manifest.events[1];
        let when = notification.when.as_ref().expect("a condition");
        assert_eq!(when.field, "notification_type");
        assert_eq!(
            when.one_of.as_deref(),
            Some(&["permission".to_owned(), "elicitation".to_owned()][..])
        );
        assert_eq!(notification.detail[0].value.as_deref(), Some("pre"));
        assert_eq!(notification.detail[1].transform, Some(Transform::NonEmpty));
    }

    #[test]
    fn the_optional_parts_are_optional() {
        let manifest = parse(
            r#"
id = "agent"

[payload]
event = "type"
session = ["session_id"]

[[events]]
name = "Something"
kind = "error"
"#,
        )
        .expect("the smallest legal manifest");
        assert!(manifest.version.is_none());
        assert!(manifest.payload.cwd.is_empty());
        assert!(manifest.events[0].detail.is_empty());
    }

    #[test]
    fn rejects_an_unknown_key_at_every_level() {
        let cases = [
            // Top level.
            "id = \"agent\"\nsurprise = 1\n\n[payload]\nevent = \"e\"\nsession = [\"s\"]\n\n\
             [[events]]\nname = \"n\"\nkind = \"error\"\n",
            // The payload table.
            "id = \"agent\"\n\n[payload]\nevent = \"e\"\nsession = [\"s\"]\nsurprise = 1\n\n\
             [[events]]\nname = \"n\"\nkind = \"error\"\n",
            // An event.
            &manifest_with("kind = \"error\"\nsurprise = 1"),
            // A condition.
            &manifest_with(
                "kind = \"error\"\nwhen = { field = \"f\", equals = \"v\", surprise = 1 }",
            ),
            // A projection.
            &manifest_with(
                "kind = \"error\"\ndetail = [{ field = \"f\", value = \"v\", surprise = 1 }]",
            ),
        ];
        for case in cases {
            assert!(
                matches!(parse(case), Err(HookManifestError::Syntax { .. })),
                "an unknown key should have been refused: {case}",
            );
        }
    }

    #[test]
    fn requires_the_parts_an_event_cannot_do_without() {
        let missing_payload = "id = \"agent\"\n\n[[events]]\nname = \"n\"\nkind = \"error\"\n";
        assert!(matches!(
            parse(missing_payload),
            Err(HookManifestError::Syntax { .. })
        ));

        let missing_session = "id = \"agent\"\n\n[payload]\nevent = \"e\"\n\n\
                               [[events]]\nname = \"n\"\nkind = \"error\"\n";
        assert!(matches!(
            parse(missing_session),
            Err(HookManifestError::Syntax { .. })
        ));

        let no_session_alternatives = "id = \"agent\"\n\n[payload]\nevent = \"e\"\nsession = []\n\n\
                                       [[events]]\nname = \"n\"\nkind = \"error\"\n";
        assert_eq!(fault(no_session_alternatives), HookFault::NoSessionField);

        let no_events = "id = \"agent\"\n\n[payload]\nevent = \"e\"\nsession = [\"s\"]\n";
        assert_eq!(fault(no_events), HookFault::NoEvents);
    }

    #[test]
    fn rejects_a_blank_name_wherever_one_appears() {
        let blank_id = "id = \" \"\n\n[payload]\nevent = \"e\"\nsession = [\"s\"]\n\n\
                        [[events]]\nname = \"n\"\nkind = \"error\"\n";
        assert!(matches!(fault(blank_id), HookFault::UnusableId { .. }));

        let blank_event_field = "id = \"agent\"\n\n[payload]\nevent = \"\"\nsession = [\"s\"]\n\n\
                                 [[events]]\nname = \"n\"\nkind = \"error\"\n";
        assert_eq!(
            fault(blank_event_field),
            HookFault::EmptyFieldName { field: "event" }
        );

        let blank_alternative = "id = \"agent\"\n\n[payload]\nevent = \"e\"\nsession = [\"s\", \" \"]\n\n\
                                 [[events]]\nname = \"n\"\nkind = \"error\"\n";
        assert_eq!(
            fault(blank_alternative),
            HookFault::EmptyFieldName { field: "session" }
        );

        let blank_cwd = "id = \"agent\"\n\n[payload]\nevent = \"e\"\nsession = [\"s\"]\ncwd = [\"\"]\n\n\
                         [[events]]\nname = \"n\"\nkind = \"error\"\n";
        assert_eq!(fault(blank_cwd), HookFault::EmptyFieldName { field: "cwd" });

        let blank_event_name = "id = \"agent\"\n\n[payload]\nevent = \"e\"\nsession = [\"s\"]\n\n\
                                [[events]]\nname = \"\"\nkind = \"error\"\n";
        assert_eq!(
            fault(blank_event_name),
            HookFault::EmptyEventName { index: 0 }
        );

        assert_eq!(
            fault(&manifest_with(
                "kind = \"error\"\nwhen = { field = \" \", equals = \"v\" }"
            )),
            HookFault::EmptyFieldName { field: "when" }
        );
        assert_eq!(
            fault(&manifest_with(
                "kind = \"error\"\ndetail = [{ field = \"\", value = \"v\" }]"
            )),
            HookFault::EmptyFieldName { field: "detail" }
        );
        assert_eq!(
            fault(&manifest_with(
                "kind = \"error\"\ndetail = [{ field = \"f\", from = \" \" }]"
            )),
            HookFault::EmptyFieldName { field: "from" }
        );
    }

    #[test]
    fn rejects_an_id_the_envelope_could_not_carry() {
        let spaced = "id = \"two words\"\n\n[payload]\nevent = \"e\"\nsession = [\"s\"]\n\n\
                      [[events]]\nname = \"n\"\nkind = \"error\"\n";
        assert!(matches!(fault(spaced), HookFault::UnusableId { .. }));
    }

    #[test]
    fn rejects_two_entries_for_one_event_name() {
        let duplicate = "id = \"agent\"\n\n[payload]\nevent = \"e\"\nsession = [\"s\"]\n\n\
                         [[events]]\nname = \"Same\"\nkind = \"error\"\n\n\
                         [[events]]\nname = \"Same\"\nkind = \"blocked\"\n";
        assert_eq!(fault(duplicate), HookFault::DuplicateEventName);
    }

    #[test]
    fn matching_by_exact_equality_makes_two_spellings_two_events() {
        // The names differ only in case, and case is not something the engine
        // is entitled to fold away when it is matching somebody else's schema.
        let both = "id = \"agent\"\n\n[payload]\nevent = \"e\"\nsession = [\"s\"]\n\n\
                    [[events]]\nname = \"Same\"\nkind = \"error\"\n\n\
                    [[events]]\nname = \"same\"\nkind = \"blocked\"\n";
        assert!(parse(both).is_ok());
    }

    #[test]
    fn rejects_a_kind_this_engine_does_not_know() {
        assert_eq!(
            fault(&manifest_with("kind = \"telepathy\"")),
            HookFault::UnknownKind {
                kind: "telepathy".to_owned()
            }
        );
    }

    #[test]
    fn accepts_every_kind_in_the_closed_set() {
        for kind in Kind::ALL {
            let content = manifest_with(&format!("kind = \"{kind}\""));
            assert!(
                parse(&content).is_ok(),
                "kind {kind:?} should have been accepted",
            );
        }
    }

    #[test]
    fn rejects_a_condition_that_says_two_things_or_nothing() {
        assert_eq!(
            fault(&manifest_with(
                "kind = \"error\"\nwhen = { field = \"f\", equals = \"v\", in = [\"v\"] }"
            )),
            HookFault::ConditionOverSpecified
        );
        assert_eq!(
            fault(&manifest_with("kind = \"error\"\nwhen = { field = \"f\" }")),
            HookFault::ConditionSaysNothing
        );
        assert_eq!(
            fault(&manifest_with(
                "kind = \"error\"\nwhen = { field = \"f\", in = [] }"
            )),
            HookFault::ConditionSaysNothing
        );
    }

    #[test]
    fn rejects_a_projection_that_says_two_things_or_nothing() {
        assert_eq!(
            fault(&manifest_with(
                "kind = \"error\"\ndetail = [{ field = \"f\", from = \"a\", value = \"b\" }]"
            )),
            HookFault::ProjectionOverSpecified {
                field: "f".to_owned()
            }
        );
        assert_eq!(
            fault(&manifest_with(
                "kind = \"error\"\ndetail = [{ field = \"f\" }]"
            )),
            HookFault::ProjectionSaysNothing {
                field: "f".to_owned()
            }
        );
    }

    #[test]
    fn rejects_a_transform_with_nothing_to_transform() {
        assert_eq!(
            fault(&manifest_with(
                "kind = \"error\"\ndetail = [{ field = \"f\", value = \"v\", transform = \"nonempty\" }]"
            )),
            HookFault::TransformWithoutSource {
                field: "f".to_owned()
            }
        );
    }

    #[test]
    fn rejects_a_transform_this_engine_does_not_have() {
        assert!(matches!(
            parse(&manifest_with(
                "kind = \"error\"\ndetail = [{ field = \"f\", from = \"a\", transform = \"uppercase\" }]"
            )),
            Err(HookManifestError::Syntax { .. })
        ));
    }

    #[test]
    fn rejects_two_projections_writing_one_key() {
        assert_eq!(
            fault(&manifest_with(
                "kind = \"error\"\ndetail = [{ field = \"f\", value = \"a\" }, { field = \"f\", value = \"b\" }]"
            )),
            HookFault::DuplicateDetailField {
                field: "f".to_owned()
            }
        );
    }

    #[test]
    fn refuses_an_engine_newer_than_this_one() {
        let content = format!(
            "id = \"agent\"\nmin_engine_version = {}\n\n[payload]\nevent = \"e\"\nsession = [\"s\"]\n\n\
             [[events]]\nname = \"n\"\nkind = \"error\"\n",
            HOOKS_ENGINE_VERSION + 1,
        );
        assert_eq!(
            fault(&content),
            HookFault::EngineTooNew {
                required: HOOKS_ENGINE_VERSION + 1,
                engine: HOOKS_ENGINE_VERSION,
            }
        );
    }

    #[test]
    fn names_the_manifest_and_the_event_in_the_message() {
        let error = parse(&manifest_with("kind = \"telepathy\"")).expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("\"agent\""), "{message}");
        assert!(message.contains("\"Something\""), "{message}");
    }
}
