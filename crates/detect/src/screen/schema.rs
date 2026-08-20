//! The screen manifest dialect: what a manifest may say, and what makes one
//! unacceptable.
//!
//! A screen manifest describes how one agent's terminal UI reads in each of its
//! states. It is data, not code: the engine knows *how* to match, and the
//! manifest knows *what* to look for, so a UI change is answered by shipping a
//! file rather than a release.
//!
//! Two properties are load-bearing here. Unknown keys are **errors**, not
//! ignored: new expressive power arrives by raising the engine version, and a
//! manifest that leans on a key this engine never heard of should be told so
//! rather than quietly matching less than its author expected. And every limit
//! below is enforced at load time, because a manifest can arrive from a
//! network, and the cost of evaluating one must be bounded before it is ever
//! run against a screen.

use std::collections::HashSet;
use std::fmt;

use serde::Deserialize;

use crate::screen::region::{DEFAULT_REGION, RegionSpec, UnknownRegion};
use crate::version::ManifestVersion;

/// The dialect this engine speaks.
///
/// A manifest may declare the lowest engine it needs; anything above this is
/// refused rather than half-understood.
pub const SCREEN_ENGINE_VERSION: u32 = 3;

/// The most rules one manifest may hold.
const MAX_RULES: usize = 128;
/// The deepest a gate may nest below its rule.
const MAX_GATE_DEPTH: usize = 8;
/// The most gates one manifest may hold, counting each rule's own matcher set.
const MAX_GATES: usize = 512;
/// The most matchers one gate may name directly.
const MAX_MATCHERS_PER_GATE: usize = 32;
/// The most matchers one manifest may hold.
const MAX_MATCHERS: usize = 1024;
/// The longest a single matcher string may be, in characters.
const MAX_MATCHER_CHARS: usize = 512;

/// What a screen says the agent is doing.
///
/// Private to this crate's screen reading, and deliberately not the vocabulary
/// any wire format uses: a screen is evidence, and what a consumer does with
/// that evidence is its own business.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenState {
    /// Waiting for a human, with nothing outstanding.
    Idle,
    /// Busy on its own.
    Working,
    /// Waiting for a human who has not been asked yet — an approval prompt, a
    /// question, an error that stops progress.
    Blocked,
    /// The screen says nothing either way.
    Unknown,
}

impl fmt::Display for ScreenState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Unknown => "unknown",
        })
    }
}

/// One agent's screen manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenManifest {
    /// The agent this manifest describes.
    pub id: String,
    /// The manifest's own version, used to decide whether a fetched copy is
    /// newer than the one already held.
    #[serde(default)]
    pub version: Option<ManifestVersion>,
    /// The lowest engine version that can honour every rule below.
    #[serde(default)]
    pub min_engine_version: Option<u32>,
    /// When the manifest was last edited, in whatever form its author wrote it.
    /// Carried verbatim and never parsed: nothing here needs a calendar.
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Other names this agent goes by.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// How the agent is recognized from a process. An absent table and an empty
    /// one mean the same thing: this manifest identifies nothing by itself.
    #[serde(default)]
    pub identify: Identify,
    /// The rules, in the order they were written — which is also the order that
    /// breaks ties between equal priorities.
    #[serde(default)]
    pub rules: Vec<Rule>,
}

/// How an agent is recognized from the process running it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identify {
    /// Executable names that are this agent.
    #[serde(default)]
    pub names: Vec<String>,
    /// Interpreters and launchers to look through — an agent shipped as a
    /// script is running as its interpreter, and the interpreter's name says
    /// nothing.
    #[serde(default)]
    pub wrappers: Vec<String>,
}

/// One rule: a claim about the screen, and what it means if it holds.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Names the rule in diagnostics. Unique within its manifest.
    pub id: String,
    /// What the screen says when this rule matches. A rule with no state
    /// contributes its flags and nothing else.
    #[serde(default)]
    pub state: Option<ScreenState>,
    /// Highest priority wins; ties go to whichever rule was written first.
    #[serde(default)]
    pub priority: i32,
    /// Which slice of the evidence the matchers see.
    #[serde(default = "default_region")]
    pub region: String,
    /// Marks a rule that matched live chrome saying the agent is idle, rather
    /// than text that merely mentions it.
    #[serde(default)]
    pub visible_idle: bool,
    /// Marks a rule that matched live chrome saying the agent is blocked.
    #[serde(default)]
    pub visible_blocker: bool,
    /// Marks a rule that matched live chrome saying the agent is working.
    #[serde(default)]
    pub visible_working: bool,
    /// Marks a screen that says nothing about the present — a transcript being
    /// reviewed, a scrolled-back history — so that a reader keeps whatever it
    /// believed before instead of adopting what it can see.
    #[serde(default)]
    pub skip_state_update: bool,
    /// Nested gates that must all hold.
    #[serde(default)]
    pub all: Vec<Gate>,
    /// Nested gates of which at least one must hold.
    #[serde(default)]
    pub any: Vec<Gate>,
    /// Nested gates none of which may hold.
    #[serde(default)]
    pub not: Vec<Gate>,
    /// Case-insensitive substrings of the region.
    #[serde(default)]
    pub contains: Vec<String>,
    /// Patterns matched against the region as a whole.
    #[serde(default)]
    pub regex: Vec<String>,
    /// Patterns matched against each line of the region on its own.
    #[serde(default)]
    pub line_regex: Vec<String>,
}

/// A group of matchers, combined with other groups.
///
/// A gate holds exactly what a rule holds minus the rule's verdict, and nests
/// without limit other than the depth this module enforces.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    /// Nested gates that must all hold.
    #[serde(default)]
    pub all: Vec<Gate>,
    /// Nested gates of which at least one must hold.
    #[serde(default)]
    pub any: Vec<Gate>,
    /// Nested gates none of which may hold.
    #[serde(default)]
    pub not: Vec<Gate>,
    /// Case-insensitive substrings of the region.
    #[serde(default)]
    pub contains: Vec<String>,
    /// Patterns matched against the region as a whole.
    #[serde(default)]
    pub regex: Vec<String>,
    /// Patterns matched against each line of the region on its own.
    #[serde(default)]
    pub line_regex: Vec<String>,
}

/// A borrowed view of one gate's contents.
///
/// A rule and a gate carry the same six fields, and everything that walks them
/// wants to treat them alike; this is that shape, without copying either.
#[derive(Debug, Clone, Copy)]
pub struct GateView<'a> {
    /// Nested gates that must all hold.
    pub all: &'a [Gate],
    /// Nested gates of which at least one must hold.
    pub any: &'a [Gate],
    /// Nested gates none of which may hold.
    pub not: &'a [Gate],
    /// Case-insensitive substrings of the region.
    pub contains: &'a [String],
    /// Patterns matched against the region as a whole.
    pub regex: &'a [String],
    /// Patterns matched against each line of the region on its own.
    pub line_regex: &'a [String],
}

impl<'a> GateView<'a> {
    /// Whether the gate can be satisfied by evidence rather than only by the
    /// absence of it.
    fn has_positive_matcher(&self) -> bool {
        !self.contains.is_empty()
            || !self.regex.is_empty()
            || !self.line_regex.is_empty()
            || !self.all.is_empty()
            || !self.any.is_empty()
    }

    /// Whether the gate says anything at all.
    fn is_empty(&self) -> bool {
        !self.has_positive_matcher() && self.not.is_empty()
    }

    /// How many matchers the gate names directly, ignoring nested gates.
    fn direct_matchers(&self) -> usize {
        self.contains.len() + self.regex.len() + self.line_regex.len()
    }

    /// The matcher strings the gate names directly.
    fn matcher_strings(&self) -> impl Iterator<Item = &'a String> {
        self.contains
            .iter()
            .chain(self.regex.iter())
            .chain(self.line_regex.iter())
    }
}

impl Gate {
    /// The gate's contents, in the shape everything that walks gates uses.
    pub fn view(&self) -> GateView<'_> {
        GateView {
            all: &self.all,
            any: &self.any,
            not: &self.not,
            contains: &self.contains,
            regex: &self.regex,
            line_regex: &self.line_regex,
        }
    }
}

impl Rule {
    /// The rule's own matchers, which combine exactly as a gate's do.
    pub fn gate(&self) -> GateView<'_> {
        GateView {
            all: &self.all,
            any: &self.any,
            not: &self.not,
            contains: &self.contains,
            regex: &self.regex,
            line_regex: &self.line_regex,
        }
    }

    /// The region this rule looks at.
    ///
    /// Parsing here rather than at load time keeps the manifest a faithful
    /// record of what its author wrote; a validated manifest never returns an
    /// error from this.
    pub fn region_spec(&self) -> Result<RegionSpec, UnknownRegion> {
        RegionSpec::parse(&self.region)
    }
}

fn default_region() -> String {
    DEFAULT_REGION.to_owned()
}

impl ScreenManifest {
    /// Parses and validates one manifest.
    pub fn parse(content: &str) -> Result<Self, ScreenManifestError> {
        let manifest: Self =
            toml::from_str(content).map_err(|error| ScreenManifestError::Syntax {
                message: error.to_string(),
            })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Checks everything [`ScreenManifest::parse`] checks after the TOML has
    /// been read, for a manifest that came from somewhere other than TOML.
    pub fn validate(&self) -> Result<(), ScreenManifestError> {
        if self.id.trim().is_empty() {
            return Err(self.fault(ManifestFault::EmptyId));
        }
        match self.min_engine_version {
            Some(required) if required > SCREEN_ENGINE_VERSION => {
                return Err(self.fault(ManifestFault::EngineTooNew {
                    required,
                    engine: SCREEN_ENGINE_VERSION,
                }));
            }
            _ => {}
        }
        for (field, entries) in [
            ("names", &self.identify.names),
            ("wrappers", &self.identify.wrappers),
        ] {
            if entries.iter().any(|entry| entry.trim().is_empty()) {
                return Err(self.fault(ManifestFault::EmptyIdentifyEntry { field }));
            }
        }
        if self.rules.is_empty() {
            return Err(self.fault(ManifestFault::NoRules));
        }
        if self.rules.len() > MAX_RULES {
            return Err(self.fault(ManifestFault::TooManyRules {
                count: self.rules.len(),
                limit: MAX_RULES,
            }));
        }

        let mut seen = HashSet::with_capacity(self.rules.len());
        let mut budget = Budget::default();
        for (index, rule) in self.rules.iter().enumerate() {
            if rule.id.trim().is_empty() {
                return Err(self.fault(ManifestFault::EmptyRuleId { index }));
            }
            if !seen.insert(rule.id.trim()) {
                return Err(self.rule_fault(rule, ManifestFault::DuplicateRuleId));
            }
            self.validate_rule(rule, &mut budget)?;
        }
        Ok(())
    }

    fn validate_rule(&self, rule: &Rule, budget: &mut Budget) -> Result<(), ScreenManifestError> {
        if rule.skip_state_update {
            if rule.state != Some(ScreenState::Unknown) {
                return Err(self.rule_fault(rule, ManifestFault::SkipStateUpdateNeedsUnknown));
            }
            if rule.visible_idle || rule.visible_blocker || rule.visible_working {
                return Err(self.rule_fault(rule, ManifestFault::SkipStateUpdateWithVisibleFlag));
            }
        }

        let region = rule
            .region_spec()
            .map_err(|error| self.rule_fault(rule, ManifestFault::UnknownRegion(error)))?;
        let required = region.min_engine_version();
        match self.min_engine_version {
            Some(declared) if declared < required => {
                return Err(self.rule_fault(
                    rule,
                    ManifestFault::RegionNeedsNewerEngine {
                        region: region.to_string(),
                        required,
                        declared,
                    },
                ));
            }
            _ => {}
        }

        validate_gate(rule.gate(), 0, budget).map_err(|fault| self.rule_fault(rule, fault))
    }

    fn fault(&self, fault: ManifestFault) -> ScreenManifestError {
        ScreenManifestError::Manifest {
            manifest: self.id.clone(),
            fault,
        }
    }

    fn rule_fault(&self, rule: &Rule, fault: ManifestFault) -> ScreenManifestError {
        ScreenManifestError::Rule {
            manifest: self.id.clone(),
            rule: rule.id.clone(),
            fault,
        }
    }
}

/// What a manifest has spent of the limits that apply to it as a whole.
#[derive(Debug, Default)]
struct Budget {
    gates: usize,
    matchers: usize,
}

/// Walks one gate and everything below it.
///
/// `depth` is how far below its rule the gate sits; a rule's own matchers are
/// depth zero.
fn validate_gate(
    gate: GateView<'_>,
    depth: usize,
    budget: &mut Budget,
) -> Result<(), ManifestFault> {
    if depth > MAX_GATE_DEPTH {
        return Err(ManifestFault::GateTooDeep {
            limit: MAX_GATE_DEPTH,
        });
    }
    budget.gates += 1;
    if budget.gates > MAX_GATES {
        return Err(ManifestFault::TooManyGates { limit: MAX_GATES });
    }

    let direct = gate.direct_matchers();
    if direct > MAX_MATCHERS_PER_GATE {
        return Err(ManifestFault::TooManyMatchersInGate {
            count: direct,
            limit: MAX_MATCHERS_PER_GATE,
        });
    }
    budget.matchers += direct;
    if budget.matchers > MAX_MATCHERS {
        return Err(ManifestFault::TooManyMatchers {
            limit: MAX_MATCHERS,
        });
    }
    for matcher in gate.matcher_strings() {
        let length = matcher.chars().count();
        if length > MAX_MATCHER_CHARS {
            return Err(ManifestFault::MatcherTooLong {
                length,
                limit: MAX_MATCHER_CHARS,
            });
        }
    }

    if gate.is_empty() {
        return Err(ManifestFault::EmptyGate);
    }
    if !gate.has_positive_matcher() {
        return Err(ManifestFault::GateWithoutPositiveMatcher);
    }

    for (field, patterns) in [("regex", gate.regex), ("line_regex", gate.line_regex)] {
        for pattern in patterns {
            regex::Regex::new(pattern).map_err(|error| ManifestFault::InvalidPattern {
                field,
                pattern: pattern.clone(),
                message: error.to_string(),
            })?;
        }
    }

    for nested in gate.all.iter().chain(gate.any).chain(gate.not) {
        validate_gate(nested.view(), depth + 1, budget)?;
    }
    Ok(())
}

/// Why a screen manifest was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScreenManifestError {
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
        fault: ManifestFault,
    },
    /// One rule breaks a rule of the dialect.
    #[error("manifest {manifest:?}, rule {rule:?}: {fault}")]
    Rule {
        /// The manifest's id.
        manifest: String,
        /// The rule's id.
        rule: String,
        /// What is wrong.
        fault: ManifestFault,
    },
}

/// The specific thing that is wrong with a manifest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestFault {
    /// The manifest does not say which agent it describes.
    #[error("id must not be empty")]
    EmptyId,
    /// The manifest needs an engine newer than this one.
    #[error("needs engine version {required}, this engine speaks {engine}")]
    EngineTooNew {
        /// The version the manifest asked for.
        required: u32,
        /// The version this engine implements.
        engine: u32,
    },
    /// An entry in `[identify]` was blank, which could never identify anything.
    #[error("identify.{field} contains an empty entry")]
    EmptyIdentifyEntry {
        /// Which list the blank entry was in.
        field: &'static str,
    },
    /// A manifest with no rules can only ever answer nothing.
    #[error("must contain at least one rule")]
    NoRules,
    /// More rules than the engine will evaluate.
    #[error("contains {count} rules, the limit is {limit}")]
    TooManyRules {
        /// How many rules were found.
        count: usize,
        /// The limit.
        limit: usize,
    },
    /// A rule without an id could not be named in diagnostics.
    #[error("the rule at position {index} has an empty id")]
    EmptyRuleId {
        /// Where the rule sits in the file, counting from zero.
        index: usize,
    },
    /// Two rules share an id, so no diagnostic could tell them apart.
    #[error("duplicate rule id")]
    DuplicateRuleId,
    /// The rule looks at a region this engine does not have.
    #[error(transparent)]
    UnknownRegion(#[from] UnknownRegion),
    /// The rule uses a region younger than the engine floor the manifest
    /// declared, so an engine that believed the floor would read the region as
    /// empty and the rule would silently never match.
    #[error(
        "region {region} needs engine version {required}, but the manifest declares {declared}"
    )]
    RegionNeedsNewerEngine {
        /// The region as this engine understands it.
        region: String,
        /// The engine version the region needs.
        required: u32,
        /// The floor the manifest declared.
        declared: u32,
    },
    /// Skipping the state update is only meaningful for a screen that says
    /// nothing about the present.
    #[error("skip_state_update requires state = \"unknown\"")]
    SkipStateUpdateNeedsUnknown,
    /// A screen cannot both say nothing and show live chrome.
    #[error("skip_state_update cannot be combined with a visible_* flag")]
    SkipStateUpdateWithVisibleFlag,
    /// Gates nested deeper than the engine will walk.
    #[error("gates nest deeper than the limit of {limit}")]
    GateTooDeep {
        /// The limit.
        limit: usize,
    },
    /// More gates than the engine will evaluate.
    #[error("the manifest exceeds the limit of {limit} gates")]
    TooManyGates {
        /// The limit.
        limit: usize,
    },
    /// One gate names more matchers than the engine will evaluate.
    #[error("a gate has {count} matchers, the limit is {limit}")]
    TooManyMatchersInGate {
        /// How many matchers the gate named.
        count: usize,
        /// The limit.
        limit: usize,
    },
    /// More matchers than the engine will evaluate.
    #[error("the manifest exceeds the limit of {limit} matchers")]
    TooManyMatchers {
        /// The limit.
        limit: usize,
    },
    /// A matcher string longer than the engine will hold.
    #[error("a matcher is {length} characters, the limit is {limit}")]
    MatcherTooLong {
        /// How long the matcher was, in characters.
        length: usize,
        /// The limit.
        limit: usize,
    },
    /// A gate with nothing in it would be true for every screen.
    #[error("a gate is empty")]
    EmptyGate,
    /// A gate that only says what must be absent would be true for every screen
    /// that happens not to contain it, which is not evidence of anything.
    #[error("a gate has no positive matcher")]
    GateWithoutPositiveMatcher,
    /// A pattern the regex engine could not compile.
    #[error("{field} pattern {pattern:?} does not compile: {message}")]
    InvalidPattern {
        /// Which matcher list the pattern was in.
        field: &'static str,
        /// The pattern.
        pattern: String,
        /// What the regex engine said.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest with one rule, wrapped around whatever the test wants to say.
    fn manifest_with(rule_body: &str) -> String {
        format!("id = \"agent\"\n\n[[rules]]\nid = \"rule\"\n{rule_body}\n")
    }

    fn parse(content: &str) -> Result<ScreenManifest, ScreenManifestError> {
        ScreenManifest::parse(content)
    }

    fn fault(content: &str) -> ManifestFault {
        match parse(content).expect_err("should have been rejected") {
            ScreenManifestError::Manifest { fault, .. }
            | ScreenManifestError::Rule { fault, .. } => fault,
            ScreenManifestError::Syntax { message } => {
                panic!("expected a validation failure, got a syntax error: {message}")
            }
        }
    }

    fn syntax_error(content: &str) -> String {
        match parse(content).expect_err("should have been rejected") {
            ScreenManifestError::Syntax { message } => message,
            other => panic!("expected a syntax error, got {other}"),
        }
    }

    #[test]
    fn parses_a_minimal_manifest() {
        let manifest =
            parse("id = \"agent\"\n\n[[rules]]\nid = \"waiting\"\ncontains = [\"› \"]\n")
                .expect("parses");
        assert_eq!(manifest.id, "agent");
        assert_eq!(manifest.rules.len(), 1);
        assert_eq!(manifest.rules[0].contains, ["› "]);
    }

    #[test]
    fn every_field_round_trips() {
        let manifest = parse(
            r#"
id = "agent"
version = "2026.06.11.1"
min_engine_version = 3
updated_at = "yesterday, in a hurry"
aliases = ["agent-cli", "Agent CLI"]

[identify]
names = ["agent"]
wrappers = ["node", "bun"]

[[rules]]
id = "approval"
state = "blocked"
priority = 90
region = "bottom_non_empty_lines(8)"
visible_idle = false
visible_blocker = true
visible_working = false
skip_state_update = false
contains = ["Do you want to proceed?"]
regex = ["(?i)allow .* to run"]
line_regex = ["^\\s*❯\\s+1\\."]

[[rules.all]]
contains = ["esc to interrupt"]

[[rules.any]]
contains = ["yes", "no"]

[[rules.not]]
contains = ["transcript"]

[[rules]]
id = "transcript"
state = "unknown"
skip_state_update = true
contains = ["showing detailed transcript"]
"#,
        )
        .expect("parses");

        assert_eq!(manifest.id, "agent");
        assert_eq!(
            manifest.version.as_ref().map(ManifestVersion::as_str),
            Some("2026.06.11.1")
        );
        assert_eq!(manifest.min_engine_version, Some(3));
        assert_eq!(
            manifest.updated_at.as_deref(),
            Some("yesterday, in a hurry")
        );
        assert_eq!(manifest.aliases, ["agent-cli", "Agent CLI"]);
        assert_eq!(manifest.identify.names, ["agent"]);
        assert_eq!(manifest.identify.wrappers, ["node", "bun"]);

        let rule = &manifest.rules[0];
        assert_eq!(rule.id, "approval");
        assert_eq!(rule.state, Some(ScreenState::Blocked));
        assert_eq!(rule.priority, 90);
        assert_eq!(rule.region, "bottom_non_empty_lines(8)");
        assert_eq!(
            rule.region_spec(),
            Ok(RegionSpec::BottomNonEmptyLines(8)),
            "the region parses to what it says"
        );
        assert!(!rule.visible_idle);
        assert!(rule.visible_blocker);
        assert!(!rule.visible_working);
        assert!(!rule.skip_state_update);
        assert_eq!(rule.contains, ["Do you want to proceed?"]);
        assert_eq!(rule.regex, ["(?i)allow .* to run"]);
        assert_eq!(rule.line_regex, ["^\\s*❯\\s+1\\."]);
        assert_eq!(rule.all[0].contains, ["esc to interrupt"]);
        assert_eq!(rule.any[0].contains, ["yes", "no"]);
        assert_eq!(rule.not[0].contains, ["transcript"]);

        let view = rule.gate();
        assert_eq!(view.all.len(), 1);
        assert_eq!(view.any.len(), 1);
        assert_eq!(view.not.len(), 1);
        assert_eq!(view.contains, rule.contains);

        let transcript = &manifest.rules[1];
        assert_eq!(transcript.state, Some(ScreenState::Unknown));
        assert!(transcript.skip_state_update);
    }

    #[test]
    fn omitted_fields_take_their_documented_defaults() {
        let manifest = parse(&manifest_with("contains = [\"x\"]")).expect("parses");
        assert_eq!(manifest.version, None);
        assert_eq!(manifest.min_engine_version, None);
        assert_eq!(manifest.updated_at, None);
        assert!(manifest.aliases.is_empty());
        assert_eq!(manifest.identify, Identify::default());

        let rule = &manifest.rules[0];
        assert_eq!(rule.state, None);
        assert_eq!(rule.priority, 0);
        assert_eq!(rule.region, DEFAULT_REGION);
        assert!(!rule.visible_idle && !rule.visible_blocker && !rule.visible_working);
        assert!(!rule.skip_state_update);
        assert!(rule.all.is_empty() && rule.any.is_empty() && rule.not.is_empty());
        assert!(rule.regex.is_empty() && rule.line_regex.is_empty());
    }

    #[test]
    fn an_unknown_key_is_an_error_at_every_level() {
        for content in [
            "id = \"agent\"\nflavour = \"vanilla\"\n\n[[rules]]\nid = \"r\"\ncontains = [\"x\"]\n",
            &manifest_with("flavour = \"vanilla\"\ncontains = [\"x\"]"),
            &manifest_with(
                "contains = [\"x\"]\n\n[[rules.all]]\nflavour = \"vanilla\"\ncontains = [\"y\"]",
            ),
            "id = \"agent\"\n\n[identify]\nflavour = \"vanilla\"\n\n[[rules]]\nid = \"r\"\ncontains = [\"x\"]\n",
        ] {
            let message = syntax_error(content);
            assert!(
                message.contains("flavour"),
                "the error should name the unknown key, said: {message}"
            );
        }
    }

    #[test]
    fn a_missing_id_is_an_error() {
        assert!(syntax_error("[[rules]]\nid = \"r\"\ncontains = [\"x\"]\n").contains("id"));
        assert!(syntax_error("id = \"agent\"\n\n[[rules]]\ncontains = [\"x\"]\n").contains("id"));
    }

    #[test]
    fn an_empty_id_is_an_error() {
        assert_eq!(
            fault("id = \"  \"\n\n[[rules]]\nid = \"r\"\ncontains = [\"x\"]\n"),
            ManifestFault::EmptyId
        );
        assert_eq!(
            fault("id = \"agent\"\n\n[[rules]]\nid = \"\"\ncontains = [\"x\"]\n"),
            ManifestFault::EmptyRuleId { index: 0 }
        );
    }

    #[test]
    fn state_is_one_of_four_words() {
        for state in ["idle", "working", "blocked", "unknown"] {
            let content = manifest_with(&format!("state = \"{state}\"\ncontains = [\"x\"]"));
            assert!(parse(&content).is_ok(), "{state} should be accepted");
        }
        for state in ["busy", "Idle", "waiting", ""] {
            let content = manifest_with(&format!("state = \"{state}\"\ncontains = [\"x\"]"));
            assert!(parse(&content).is_err(), "{state:?} should be rejected");
        }
    }

    #[test]
    fn a_manifest_needs_a_rule() {
        assert_eq!(fault("id = \"agent\"\n"), ManifestFault::NoRules);
        assert_eq!(
            fault("id = \"agent\"\nrules = []\n"),
            ManifestFault::NoRules
        );
    }

    #[test]
    fn rule_ids_are_unique() {
        let content = "id = \"agent\"\n\n[[rules]]\nid = \"r\"\ncontains = [\"x\"]\n\n[[rules]]\nid = \"r\"\ncontains = [\"y\"]\n";
        assert_eq!(fault(content), ManifestFault::DuplicateRuleId);
    }

    #[test]
    fn errors_name_the_manifest_and_the_rule() {
        let content =
            "id = \"agent\"\n\n[[rules]]\nid = \"r\"\nregion = \"nowhere\"\ncontains = [\"x\"]\n";
        let message = parse(content).expect_err("rejected").to_string();
        assert!(message.contains("agent"), "said: {message}");
        assert!(message.contains("\"r\""), "said: {message}");
        assert!(message.contains("nowhere"), "said: {message}");
    }

    fn rules(count: usize) -> String {
        let mut content = String::from("id = \"agent\"\n");
        for index in 0..count {
            content.push_str(&format!(
                "\n[[rules]]\nid = \"r{index}\"\ncontains = [\"x\"]\n"
            ));
        }
        content
    }

    #[test]
    fn rule_count_is_capped() {
        assert!(parse(&rules(MAX_RULES)).is_ok());
        assert_eq!(
            fault(&rules(MAX_RULES + 1)),
            ManifestFault::TooManyRules {
                count: MAX_RULES + 1,
                limit: MAX_RULES,
            }
        );
    }

    /// A rule whose gates nest `depth` levels below the rule itself.
    fn nested(depth: usize) -> String {
        let mut gate = String::from("{ contains = [\"x\"] }");
        for _ in 0..depth {
            gate = format!("{{ all = [{gate}] }}");
        }
        manifest_with(&format!("all = [{gate}]"))
    }

    #[test]
    fn gate_depth_is_capped() {
        // A rule's own matchers are depth zero, so `nested(n)` puts its
        // innermost gate at depth n + 1.
        assert!(
            parse(&nested(MAX_GATE_DEPTH - 1)).is_ok(),
            "a gate at depth {MAX_GATE_DEPTH} is allowed"
        );
        assert_eq!(
            fault(&nested(MAX_GATE_DEPTH)),
            ManifestFault::GateTooDeep {
                limit: MAX_GATE_DEPTH
            },
            "one deeper is not"
        );
    }

    /// A rule with `count` sibling gates, which with the rule's own gate makes
    /// `count + 1`.
    fn sibling_gates(count: usize) -> String {
        let gates = vec!["{ contains = [\"x\"] }"; count].join(", ");
        manifest_with(&format!("any = [{gates}]"))
    }

    #[test]
    fn gate_count_is_capped() {
        assert!(parse(&sibling_gates(MAX_GATES - 1)).is_ok());
        assert_eq!(
            fault(&sibling_gates(MAX_GATES)),
            ManifestFault::TooManyGates { limit: MAX_GATES }
        );
    }

    fn matchers_in_one_gate(count: usize) -> String {
        let matchers = (0..count)
            .map(|index| format!("\"m{index}\""))
            .collect::<Vec<_>>()
            .join(", ");
        manifest_with(&format!("contains = [{matchers}]"))
    }

    #[test]
    fn matchers_per_gate_are_capped() {
        assert!(parse(&matchers_in_one_gate(MAX_MATCHERS_PER_GATE)).is_ok());
        assert_eq!(
            fault(&matchers_in_one_gate(MAX_MATCHERS_PER_GATE + 1)),
            ManifestFault::TooManyMatchersInGate {
                count: MAX_MATCHERS_PER_GATE + 1,
                limit: MAX_MATCHERS_PER_GATE,
            }
        );
    }

    /// A manifest holding `total` matchers, spread over as many rules as the
    /// per-gate limit needs.
    fn matchers_in_total(total: usize) -> String {
        let mut content = String::from("id = \"agent\"\n");
        let mut written = 0;
        let mut index = 0;
        while written < total {
            let here = MAX_MATCHERS_PER_GATE.min(total - written);
            let matchers = (0..here)
                .map(|matcher| format!("\"m{index}_{matcher}\""))
                .collect::<Vec<_>>()
                .join(", ");
            content.push_str(&format!(
                "\n[[rules]]\nid = \"r{index}\"\ncontains = [{matchers}]\n"
            ));
            written += here;
            index += 1;
        }
        content
    }

    #[test]
    fn total_matchers_are_capped() {
        assert!(parse(&matchers_in_total(MAX_MATCHERS)).is_ok());
        assert_eq!(
            fault(&matchers_in_total(MAX_MATCHERS + 1)),
            ManifestFault::TooManyMatchers {
                limit: MAX_MATCHERS
            }
        );
    }

    #[test]
    fn matcher_length_is_capped_in_characters() {
        let fits = "a".repeat(MAX_MATCHER_CHARS);
        assert!(parse(&manifest_with(&format!("contains = [\"{fits}\"]"))).is_ok());

        let wide = "✓".repeat(MAX_MATCHER_CHARS);
        assert!(
            parse(&manifest_with(&format!("contains = [\"{wide}\"]"))).is_ok(),
            "the limit counts characters, not bytes"
        );

        let long = "a".repeat(MAX_MATCHER_CHARS + 1);
        assert_eq!(
            fault(&manifest_with(&format!("contains = [\"{long}\"]"))),
            ManifestFault::MatcherTooLong {
                length: MAX_MATCHER_CHARS + 1,
                limit: MAX_MATCHER_CHARS,
            }
        );
        assert!(matches!(
            fault(&manifest_with(&format!("regex = [\"{long}\"]"))),
            ManifestFault::MatcherTooLong { .. }
        ));
    }

    #[test]
    fn patterns_must_compile() {
        assert!(matches!(
            fault(&manifest_with("regex = [\"(unclosed\"]")),
            ManifestFault::InvalidPattern { field: "regex", .. }
        ));
        assert!(matches!(
            fault(&manifest_with("line_regex = [\"*\"]")),
            ManifestFault::InvalidPattern {
                field: "line_regex",
                ..
            }
        ));
        assert!(matches!(
            fault(&manifest_with(
                "contains = [\"x\"]\n\n[[rules.all]]\nregex = [\"[\"]"
            )),
            ManifestFault::InvalidPattern { .. }
        ));
        assert!(parse(&manifest_with("regex = [\"(?i)waiting\\\\s+for\"]")).is_ok());
    }

    #[test]
    fn a_gate_must_say_what_is_present() {
        assert_eq!(
            fault(&manifest_with("[[rules.not]]\ncontains = [\"x\"]")),
            ManifestFault::GateWithoutPositiveMatcher,
            "a rule that only says what is absent matches every other screen"
        );
        assert_eq!(
            fault(&manifest_with(
                "contains = [\"x\"]\n\n[[rules.all]]\n[[rules.all.not]]\ncontains = [\"y\"]"
            )),
            ManifestFault::GateWithoutPositiveMatcher,
            "the rule holds for nested gates too"
        );
        assert_eq!(
            fault(&manifest_with("contains = [\"x\"]\nany = [{}]")),
            ManifestFault::EmptyGate
        );
        assert_eq!(
            fault(&manifest_with("priority = 1")),
            ManifestFault::EmptyGate
        );
        assert!(
            parse(&manifest_with(
                "contains = [\"x\"]\n\n[[rules.not]]\ncontains = [\"y\"]"
            ))
            .is_ok(),
            "a negative alongside a positive is the point of `not`"
        );
    }

    #[test]
    fn skip_state_update_means_the_screen_says_nothing() {
        assert!(
            parse(&manifest_with(
                "state = \"unknown\"\nskip_state_update = true\ncontains = [\"x\"]"
            ))
            .is_ok()
        );
        assert_eq!(
            fault(&manifest_with(
                "skip_state_update = true\ncontains = [\"x\"]"
            )),
            ManifestFault::SkipStateUpdateNeedsUnknown
        );
        assert_eq!(
            fault(&manifest_with(
                "state = \"idle\"\nskip_state_update = true\ncontains = [\"x\"]"
            )),
            ManifestFault::SkipStateUpdateNeedsUnknown
        );
        for flag in ["visible_idle", "visible_blocker", "visible_working"] {
            assert_eq!(
                fault(&manifest_with(&format!(
                    "state = \"unknown\"\nskip_state_update = true\n{flag} = true\ncontains = [\"x\"]"
                ))),
                ManifestFault::SkipStateUpdateWithVisibleFlag,
                "{flag} contradicts skip_state_update"
            );
        }
    }

    #[test]
    fn regions_must_be_ones_this_engine_has() {
        for region in [
            "whole_recent",
            "bottom_lines(5)",
            "bottom_non_empty_lines(8)",
            "top_non_empty_lines(1)",
            "prompt_box_body",
            "osc_title",
            "osc_progress",
        ] {
            let content = manifest_with(&format!("region = \"{region}\"\ncontains = [\"x\"]"));
            assert!(parse(&content).is_ok(), "{region} should be accepted");
        }
        for region in [
            "",
            "whole_screen",
            "bottom_lines",
            "bottom_lines(0)",
            "bottom_lines(x)",
        ] {
            let content = manifest_with(&format!("region = \"{region}\"\ncontains = [\"x\"]"));
            assert!(
                matches!(fault(&content), ManifestFault::UnknownRegion(_)),
                "{region:?} should be rejected"
            );
        }
    }

    #[test]
    fn an_engine_floor_above_this_engine_is_refused() {
        let body = "contains = [\"x\"]";
        for floor in [1, 2, SCREEN_ENGINE_VERSION] {
            let content = format!(
                "id = \"agent\"\nmin_engine_version = {floor}\n\n[[rules]]\nid = \"r\"\n{body}\n"
            );
            assert!(parse(&content).is_ok(), "floor {floor} should be accepted");
        }
        let content = format!(
            "id = \"agent\"\nmin_engine_version = {}\n\n[[rules]]\nid = \"r\"\n{body}\n",
            SCREEN_ENGINE_VERSION + 1
        );
        assert_eq!(
            fault(&content),
            ManifestFault::EngineTooNew {
                required: SCREEN_ENGINE_VERSION + 1,
                engine: SCREEN_ENGINE_VERSION,
            }
        );
    }

    #[test]
    fn a_region_may_not_outrun_the_floor_the_manifest_declares() {
        let rule = "id = \"r\"\nregion = \"top_non_empty_lines(3)\"\ncontains = [\"x\"]";
        for floor in [1, 2] {
            let content =
                format!("id = \"agent\"\nmin_engine_version = {floor}\n\n[[rules]]\n{rule}\n");
            assert_eq!(
                fault(&content),
                ManifestFault::RegionNeedsNewerEngine {
                    region: "top_non_empty_lines(3)".to_owned(),
                    required: 3,
                    declared: floor,
                },
                "an engine believing floor {floor} would read the region as empty"
            );
        }
        assert!(
            parse(&format!(
                "id = \"agent\"\nmin_engine_version = 3\n\n[[rules]]\n{rule}\n"
            ))
            .is_ok()
        );
        assert!(
            parse(&format!("id = \"agent\"\n\n[[rules]]\n{rule}\n")).is_ok(),
            "a manifest that declares no floor makes no promise to break"
        );
    }

    #[test]
    fn identify_entries_must_be_able_to_identify_something() {
        assert!(
            parse(
                "id = \"agent\"\n\n[identify]\nnames = [\"agent\"]\n\n[[rules]]\nid = \"r\"\ncontains = [\"x\"]\n"
            )
            .is_ok()
        );
        for (field, table) in [
            ("names", "names = [\"agent\", \" \"]"),
            ("wrappers", "wrappers = [\"\"]"),
        ] {
            let content = format!(
                "id = \"agent\"\n\n[identify]\n{table}\n\n[[rules]]\nid = \"r\"\ncontains = [\"x\"]\n"
            );
            assert_eq!(fault(&content), ManifestFault::EmptyIdentifyEntry { field });
        }
    }

    #[test]
    fn validate_can_be_run_on_a_manifest_that_did_not_come_from_toml() {
        let mut manifest = parse(&manifest_with("contains = [\"x\"]")).expect("parses");
        assert!(manifest.validate().is_ok());
        manifest.rules.clear();
        assert!(matches!(
            manifest.validate(),
            Err(ScreenManifestError::Manifest {
                fault: ManifestFault::NoRules,
                ..
            })
        ));
    }
}
