//! Showing the working: every rule that was evaluated, the text it saw, and why
//! the winner won.
//!
//! A verdict on its own is unfalsifiable. When a manifest misfires — a rule that
//! should have matched did not, or one that should have stayed quiet took the
//! screen — the author needs the table underneath the answer: which rules ran,
//! over which slice of the evidence, which of them held, and which one outranked
//! the rest. That table is what this module produces, and producing it is the
//! whole maintenance loop for manifest data: capture a screen, ask why, adjust
//! the rule.
//!
//! # A contract, not a debug dump
//!
//! [`Explain`] serializes to JSON with stable snake_case field names, because it
//! is what a command-line `--explain` prints and what a person's tooling parses.
//! Fields are always present — an absent value is `null`, never a missing key —
//! so a reader can index into the shape without guarding every step.
//!
//! # Bounded on purpose
//!
//! Each rule's evidence carries a **preview** of the region it read, capped at
//! [`PREVIEW_CHARS`] characters and taken from the region rather than the
//! screen. Both bounds matter: an explanation is frequently logged, and an
//! unbounded one turns "why did this match" into a way to spill an entire
//! terminal's worth of someone's work into a log file by accident.
//!
//! # One engine, two views
//!
//! [`explain`] and [`detect`](crate::detect) are the same evaluation read two
//! ways — the verdict comes from the same rule table the explanation reports, so
//! the two can never disagree about what happened.

use std::path::PathBuf;

use serde::Serialize;

use crate::screen::region::ScreenInput;
use crate::screen::rules::{CompiledManifest, RuleVerdict};
use crate::screen::schema::{GateView, ScreenState};

/// How much of a region an explanation quotes back.
///
/// Long enough to recognize the screen at a glance, short enough that an
/// explanation of a hundred-rule manifest stays readable and cheap to log.
pub const PREVIEW_CHARS: usize = 240;

/// What marks a preview as having more text behind it.
const PREVIEW_ELLIPSIS: char = '…';

/// Why one screen was read the way it was.
///
/// The verdict fields — `state`, `visible`, `skip`, `matched_rule`, `fallback` —
/// are the same ones [`Detection`](crate::Detection) carries, by construction:
/// both come from a single evaluation of the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Explain {
    /// The agent whose manifest was consulted, or nothing when none was.
    pub agent: Option<String>,
    /// What the screen says the agent is doing.
    pub state: ScreenState,
    /// Which copy of the manifest answered. Filled in by whoever loaded it:
    /// this module is handed a compiled manifest and never opens a file.
    pub source: Option<ManifestSource>,
    /// The rule that won, when one did.
    pub matched_rule: Option<MatchedRule>,
    /// Whether the winning state's chrome is live on the screen right now.
    pub visible: bool,
    /// Whether this screen describes the past rather than the present, and so
    /// asks a reader to keep whatever it believed before.
    pub skip: bool,
    /// What asked for that, in the form `matched_rule:<id>`. Present exactly
    /// when `skip` is set.
    pub skipped_reason: Option<String>,
    /// Why there is no winning rule, when there is none.
    pub fallback: Option<String>,
    /// Every rule, in the manifest's own order — which is the order its author
    /// edits, not the priority order the winner was picked in. A rule ranked
    /// last still appears where it was written.
    pub evaluated: Vec<EvaluatedRule>,
    /// What was wrong with the manifest itself: rules compilation switched off,
    /// copies that lost to a higher-precedence one.
    pub warnings: Vec<String>,
    /// The manifest's own version, when it declared one.
    pub manifest_version: Option<String>,
}

/// Which copy of a manifest was used.
///
/// An explanation that does not say where its rules came from sends people to
/// edit the wrong file — the usual confusion is a local override quietly
/// shadowing the copy someone is reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManifestSource {
    /// The copy that shipped with the software.
    Bundled,
    /// A copy fetched and cached since.
    Remote {
        /// The version that copy declared.
        version: String,
    },
    /// A copy the operator placed on this machine, which outranks the rest.
    Override {
        /// Where it was read from.
        path: PathBuf,
    },
}

/// The rule that decided the verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MatchedRule {
    /// The rule's id, as its author wrote it.
    pub id: String,
    /// The priority it beat the other matching rules with.
    pub priority: i32,
    /// The region it read, as its author wrote it.
    pub region: String,
    /// The state it declared. A rule that declares none still wins, and the
    /// screen is then evidence of nothing in particular.
    pub state: Option<ScreenState>,
}

/// One rule's run over one screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvaluatedRule {
    /// The rule's id, as its author wrote it.
    pub id: String,
    /// Highest priority among the matching rules wins; ties go to the earlier
    /// rule in the file.
    pub priority: i32,
    /// The region the rule reads, as its author wrote it — including a spelling
    /// this engine does not recognize, which is exactly the case someone
    /// reading an explanation needs to see.
    pub region: String,
    /// The state the rule declares.
    pub state: Option<ScreenState>,
    /// Whether the rule's matchers held.
    pub matched: bool,
    /// Whether the rule ran at all. A rule compilation switched off never
    /// matches, and saying so is the difference between "your rule is wrong"
    /// and "your rule was never run".
    pub enabled: bool,
    /// What the rule was working with.
    pub evidence: Evidence,
}

/// What one rule had to go on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Evidence {
    /// How many matchers of each kind the rule holds.
    pub matchers: MatcherCounts,
    /// How many gates of each kind the rule holds.
    pub gates: GateCounts,
    /// The length of the region text in bytes, before any truncation — the
    /// honest size of what was matched against, which the preview is not.
    pub region_bytes: usize,
    /// The beginning of the region text, bounded by [`PREVIEW_CHARS`] and
    /// ending in an ellipsis when there was more. Empty for a rule that never
    /// ran, which saw nothing.
    pub preview: String,
}

/// How many matchers of each kind a rule holds.
///
/// Counted over the rule's whole gate tree rather than its top level: a rule
/// whose matchers all sit inside an `any` is not a rule with no matchers, and
/// reporting it as one would send its author looking for the wrong bug.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct MatcherCounts {
    /// Case-insensitive substrings.
    pub contains: usize,
    /// Patterns matched against the region as a whole.
    pub regex: usize,
    /// Patterns matched against each line on its own.
    pub line_regex: usize,
}

/// How many gates of each kind a rule holds, counted as [`MatcherCounts`] is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct GateCounts {
    /// Gates that must all hold.
    pub all: usize,
    /// Gates of which at least one must hold.
    pub any: usize,
    /// Gates none of which may hold.
    pub not: usize,
}

impl Explain {
    /// The explanation for an agent nothing describes.
    ///
    /// The counterpart of [`Detection::unknown_agent`](crate::Detection::unknown_agent):
    /// there were no rules, so there is no table, and the honest answer is that
    /// nobody looked.
    pub fn unknown_agent() -> Self {
        let detection = crate::Detection::unknown_agent();
        Self {
            agent: None,
            state: detection.state,
            source: None,
            matched_rule: None,
            visible: detection.visible,
            skip: detection.skip,
            skipped_reason: None,
            fallback: detection.fallback.map(str::to_owned),
            evaluated: Vec::new(),
            warnings: Vec::new(),
            manifest_version: None,
        }
    }

    /// The same explanation, with the provenance its loader knows and this
    /// module cannot.
    #[must_use]
    pub fn with_source(mut self, source: ManifestSource) -> Self {
        self.source = Some(source);
        self
    }
}

/// Why one screen reads the way it does, rule by rule.
///
/// The verdict this reports is [`detect`](crate::detect)'s, taken from the same
/// evaluation as the table below it.
pub fn explain(manifest: &CompiledManifest, input: ScreenInput<'_>) -> Explain {
    let verdicts = manifest.evaluate(input);
    let detection = verdicts.detection();
    let matched_rule = verdicts.winning_rule().map(|rule| MatchedRule {
        id: rule.id.clone(),
        priority: rule.priority,
        region: rule.region.clone(),
        state: rule.state,
    });
    let skipped_reason = detection
        .skip
        .then(|| {
            matched_rule
                .as_ref()
                .map(|rule| format!("matched_rule:{}", rule.id))
        })
        .flatten();

    Explain {
        agent: Some(manifest.manifest().id.clone()),
        state: detection.state,
        source: None,
        matched_rule,
        visible: detection.visible,
        skip: detection.skip,
        skipped_reason,
        fallback: detection.fallback.map(str::to_owned),
        evaluated: verdicts.rules.iter().map(evaluated).collect(),
        warnings: manifest.warnings().to_vec(),
        manifest_version: manifest
            .manifest()
            .version
            .as_ref()
            .map(ToString::to_string),
    }
}

/// One rule's row in the table.
fn evaluated(verdict: &RuleVerdict<'_, '_>) -> EvaluatedRule {
    let rule = verdict.rule;
    let mut matchers = MatcherCounts::default();
    let mut gates = GateCounts::default();
    count(rule.gate(), &mut matchers, &mut gates);
    EvaluatedRule {
        id: rule.id.clone(),
        priority: rule.priority,
        region: rule.region.clone(),
        state: rule.state,
        matched: verdict.matched,
        enabled: verdict.enabled,
        evidence: Evidence {
            matchers,
            gates,
            region_bytes: verdict.region_text.len(),
            preview: preview(verdict.region_text),
        },
    }
}

/// Adds one gate's matchers and gates, and everything below it, to the totals.
fn count(gate: GateView<'_>, matchers: &mut MatcherCounts, gates: &mut GateCounts) {
    matchers.contains += gate.contains.len();
    matchers.regex += gate.regex.len();
    matchers.line_regex += gate.line_regex.len();
    gates.all += gate.all.len();
    gates.any += gate.any.len();
    gates.not += gate.not.len();
    for nested in gate.all.iter().chain(gate.any).chain(gate.not) {
        count(nested.view(), matchers, gates);
    }
}

/// The beginning of `text`, cut at a character boundary.
fn preview(text: &str) -> String {
    match text.char_indices().nth(PREVIEW_CHARS) {
        Some((cut, _)) => {
            let mut preview = String::with_capacity(cut + PREVIEW_ELLIPSIS.len_utf8());
            preview.push_str(&text[..cut]);
            preview.push(PREVIEW_ELLIPSIS);
            preview
        }
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::{Value, json};

    use crate::screen::rules::Detection;
    use crate::screen::schema::ScreenManifest;

    /// Two rules over one synthetic agent: enough shape for the whole
    /// explanation — a nested gate, a bounded region, a default region, flags,
    /// priorities — and small enough to write the expected JSON out in full.
    const GOLDEN: &str = r#"
id = "synthetic"
version = "2026.06.11.1"

[[rules]]
id = "approval-prompt"
state = "blocked"
priority = 100
region = "bottom_lines(3)"
visible_blocker = true
contains = ["do you want to proceed?"]
any = [{ line_regex = ["^\\s*1\\. Yes"] }, { line_regex = ["^\\s*2\\. No"] }]

[[rules]]
id = "empty-prompt"
state = "idle"
priority = 10
visible_idle = true
line_regex = ["^> $"]
"#;

    const GOLDEN_SCREEN: &str =
        "editing src/main.rs\n\nDo you want to proceed?\n  1. Yes\n  2. No\n";

    fn compile(content: &str) -> CompiledManifest {
        CompiledManifest::compile(ScreenManifest::parse(content).expect("valid manifest"))
    }

    fn explain_screen(manifest: &CompiledManifest, screen: &str) -> Explain {
        explain(manifest, ScreenInput::from_screen(screen))
    }

    #[test]
    fn an_explanation_serializes_to_its_published_shape() {
        let explanation = explain_screen(&compile(GOLDEN), GOLDEN_SCREEN);
        let expected = json!({
            "agent": "synthetic",
            "state": "blocked",
            "source": null,
            "matched_rule": {
                "id": "approval-prompt",
                "priority": 100,
                "region": "bottom_lines(3)",
                "state": "blocked"
            },
            "visible": true,
            "skip": false,
            "skipped_reason": null,
            "fallback": null,
            "evaluated": [
                {
                    "id": "approval-prompt",
                    "priority": 100,
                    "region": "bottom_lines(3)",
                    "state": "blocked",
                    "matched": true,
                    "enabled": true,
                    "evidence": {
                        "matchers": { "contains": 1, "regex": 0, "line_regex": 2 },
                        "gates": { "all": 0, "any": 2, "not": 0 },
                        "region_bytes": 41,
                        "preview": "Do you want to proceed?\n  1. Yes\n  2. No\n"
                    }
                },
                {
                    "id": "empty-prompt",
                    "priority": 10,
                    "region": "whole_recent",
                    "state": "idle",
                    "matched": false,
                    "enabled": true,
                    "evidence": {
                        "matchers": { "contains": 0, "regex": 0, "line_regex": 1 },
                        "gates": { "all": 0, "any": 0, "not": 0 },
                        "region_bytes": 62,
                        "preview": "editing src/main.rs\n\nDo you want to proceed?\n  1. Yes\n  2. No\n"
                    }
                }
            ],
            "warnings": [],
            "manifest_version": "2026.06.11.1"
        });
        assert_eq!(
            serde_json::to_value(&explanation).expect("an explanation serializes"),
            expected
        );
    }

    #[test]
    fn a_source_is_attached_by_whoever_loaded_the_manifest() {
        let sources = [
            (ManifestSource::Bundled, json!({ "kind": "bundled" })),
            (
                ManifestSource::Remote {
                    version: "2026.06.11.1".to_owned(),
                },
                json!({ "kind": "remote", "version": "2026.06.11.1" }),
            ),
            (
                ManifestSource::Override {
                    path: PathBuf::from("/etc/agentbus/synthetic.toml"),
                },
                json!({ "kind": "override", "path": "/etc/agentbus/synthetic.toml" }),
            ),
        ];
        for (source, expected) in sources {
            let explanation = explain_screen(&compile(GOLDEN), GOLDEN_SCREEN).with_source(source);
            let value = serde_json::to_value(&explanation).expect("an explanation serializes");
            assert_eq!(value["source"], expected);
        }
    }

    /// A manifest whose rules cover each state, each flag and a screen that
    /// asks to be ignored, so that the agreement below is tested over every
    /// verdict the engine can reach.
    const EVERY_VERDICT: &str = r#"
id = "synthetic"

[[rules]]
id = "transcript"
state = "unknown"
priority = 200
skip_state_update = true
contains = ["transcript of"]

[[rules]]
id = "blocked-visible"
state = "blocked"
priority = 100
visible_blocker = true
contains = ["proceed?"]

[[rules]]
id = "working"
state = "working"
priority = 50
contains = ["thinking"]

[[rules]]
id = "idle-visible"
state = "idle"
priority = 10
visible_idle = true
contains = ["> "]

[[rules]]
id = "stateless"
priority = 5
contains = ["banner"]
"#;

    #[test]
    fn an_explanation_and_a_verdict_never_disagree() {
        let manifest = compile(EVERY_VERDICT);
        let fragments = ["", "transcript of ", "proceed?", "thinking", "> ", "banner"];
        // Every combination of fragments up to three deep, so that rules
        // compete, veto and fall through against the same evidence.
        let mut screens = Vec::new();
        for first in fragments {
            for second in fragments {
                for third in fragments {
                    screens.push(format!("{first}\n{second}\n{third}\n"));
                }
            }
        }
        assert_eq!(screens.len(), 216);

        for screen in &screens {
            let input = ScreenInput::from_screen(screen);
            let detection = crate::detect(&manifest, input);
            let explanation = explain(&manifest, input);
            assert_eq!(explanation.state, detection.state, "{screen:?}");
            assert_eq!(explanation.visible, detection.visible, "{screen:?}");
            assert_eq!(explanation.skip, detection.skip, "{screen:?}");
            assert_eq!(
                explanation
                    .matched_rule
                    .as_ref()
                    .map(|rule| rule.id.clone()),
                detection.matched_rule,
                "{screen:?}"
            );
            assert_eq!(
                explanation.fallback.as_deref(),
                detection.fallback,
                "{screen:?}"
            );
        }

        // The grid is only worth anything if it reached every verdict.
        let states: Vec<Value> = screens
            .iter()
            .map(|screen| {
                let explanation = explain_screen(&manifest, screen);
                json!([explanation.state, explanation.visible, explanation.skip])
            })
            .collect();
        for reached in [
            json!(["blocked", true, false]),
            json!(["working", false, false]),
            json!(["idle", true, false]),
            json!(["idle", false, false]),
            json!(["unknown", false, true]),
        ] {
            assert!(states.contains(&reached), "never reached {reached}");
        }
    }

    #[test]
    fn a_screen_that_asks_to_be_ignored_names_the_rule_that_asked() {
        let explanation = explain_screen(&compile(EVERY_VERDICT), "transcript of yesterday\n");
        assert!(explanation.skip);
        assert_eq!(
            explanation.skipped_reason.as_deref(),
            Some("matched_rule:transcript")
        );

        let quiet = explain_screen(&compile(EVERY_VERDICT), "thinking\n");
        assert!(!quiet.skip);
        assert_eq!(quiet.skipped_reason, None);
    }

    #[test]
    fn rules_are_reported_in_file_order_whatever_the_priorities_say() {
        let explanation = explain_screen(&compile(EVERY_VERDICT), "proceed? thinking\n");
        let order: Vec<&str> = explanation
            .evaluated
            .iter()
            .map(|rule| rule.id.as_str())
            .collect();
        assert_eq!(
            order,
            [
                "transcript",
                "blocked-visible",
                "working",
                "idle-visible",
                "stateless"
            ]
        );
        // Two rules matched; the explanation still names only the winner.
        let matched: Vec<&str> = explanation
            .evaluated
            .iter()
            .filter(|rule| rule.matched)
            .map(|rule| rule.id.as_str())
            .collect();
        assert_eq!(matched, ["blocked-visible", "working"]);
        assert_eq!(
            explanation.matched_rule.map(|rule| rule.id),
            Some("blocked-visible".to_owned())
        );
    }

    #[test]
    fn a_known_agent_whose_screen_says_nothing_reports_the_fallback() {
        let explanation = explain_screen(&compile(EVERY_VERDICT), "$ ls -la\ntotal 0\n");
        assert_eq!(explanation.state, ScreenState::Idle);
        assert_eq!(explanation.matched_rule, None);
        assert_eq!(
            explanation.fallback.as_deref(),
            Some(crate::KNOWN_AGENT_IDLE_FALLBACK)
        );
        assert_eq!(explanation.evaluated.len(), 5);
        assert!(explanation.evaluated.iter().all(|rule| rule.enabled));
    }

    #[test]
    fn an_agent_nothing_describes_has_no_table_to_show() {
        let explanation = Explain::unknown_agent();
        let detection = Detection::unknown_agent();
        assert_eq!(explanation.agent, None);
        assert_eq!(explanation.state, detection.state);
        assert_eq!(explanation.visible, detection.visible);
        assert_eq!(explanation.skip, detection.skip);
        assert_eq!(explanation.fallback.as_deref(), detection.fallback);
        assert!(explanation.evaluated.is_empty());
        assert_eq!(explanation.manifest_version, None);
    }

    #[test]
    fn a_rule_that_was_never_run_says_so_rather_than_quoting_a_region() {
        let mut manifest = ScreenManifest::parse(
            "id = \"synthetic\"\n\n[[rules]]\nid = \"broken\"\nstate = \"blocked\"\ncontains = [\"here\"]\n",
        )
        .expect("valid manifest");
        manifest.rules[0].regex = vec!["(unclosed".to_owned()];

        let compiled = CompiledManifest::compile(manifest);
        let explanation = explain(&compiled, ScreenInput::from_screen("here"));
        let rule = &explanation.evaluated[0];
        assert!(!rule.enabled);
        assert!(!rule.matched);
        assert_eq!(rule.evidence.region_bytes, 0);
        assert_eq!(rule.evidence.preview, "");
        // The matchers it would have used are still reported: the rule is
        // broken, not empty.
        assert_eq!(rule.evidence.matchers.contains, 1);
        assert_eq!(rule.evidence.matchers.regex, 1);
        assert_eq!(explanation.warnings.len(), 1);
        assert!(explanation.warnings[0].contains("\"broken\""));
    }

    #[test]
    fn matchers_and_gates_are_counted_through_the_whole_tree() {
        let manifest = compile(
            "id = \"synthetic\"\n\
             \n[[rules]]\nid = \"nested\"\ncontains = [\"a\"]\n\
             all = [{ regex = [\"b\"], not = [{ contains = [\"c\"] }] }]\n\
             any = [{ line_regex = [\"d\"] }, { contains = [\"e\", \"f\"] }]\n",
        );
        let evidence = &explain_screen(&manifest, "nothing here").evaluated[0].evidence;
        assert_eq!(
            evidence.matchers,
            MatcherCounts {
                contains: 4,
                regex: 1,
                line_regex: 1,
            }
        );
        assert_eq!(
            evidence.gates,
            GateCounts {
                all: 1,
                any: 2,
                not: 1,
            }
        );
    }

    /// A region far longer than a preview, whose 241st character is multi-byte
    /// so that a naive byte cut would split it.
    fn long_region(head: usize) -> String {
        let mut screen = "a".repeat(head);
        screen.push('é');
        screen.push_str(&"b".repeat(4096));
        screen
    }

    #[test]
    fn a_preview_is_bounded_and_cut_at_a_character_boundary() {
        let manifest =
            compile("id = \"synthetic\"\n\n[[rules]]\nid = \"any\"\ncontains = [\"a\"]\n");

        // The multi-byte character sits just inside the bound…
        let inside = long_region(PREVIEW_CHARS - 1);
        let evidence = &explain_screen(&manifest, &inside).evaluated[0].evidence;
        assert_eq!(evidence.region_bytes, inside.len());
        assert!(evidence.region_bytes > 4096);
        assert_eq!(
            evidence.preview,
            format!("{}é{PREVIEW_ELLIPSIS}", "a".repeat(PREVIEW_CHARS - 1))
        );

        // …and just outside it, where a byte-wise cut would have split it.
        let outside = long_region(PREVIEW_CHARS);
        let evidence = &explain_screen(&manifest, &outside).evaluated[0].evidence;
        assert_eq!(
            evidence.preview,
            format!("{}{PREVIEW_ELLIPSIS}", "a".repeat(PREVIEW_CHARS))
        );

        for preview in [&inside, &outside].map(|screen| {
            explain_screen(&manifest, screen).evaluated[0]
                .evidence
                .preview
                .clone()
        }) {
            assert_eq!(preview.chars().count(), PREVIEW_CHARS + 1);
            assert!(preview.ends_with(PREVIEW_ELLIPSIS));
        }
    }

    #[test]
    fn a_region_that_fits_is_quoted_whole_without_an_ellipsis() {
        let manifest =
            compile("id = \"synthetic\"\n\n[[rules]]\nid = \"any\"\ncontains = [\"a\"]\n");
        let exact = "é".repeat(PREVIEW_CHARS);
        let evidence = &explain_screen(&manifest, &exact).evaluated[0].evidence;
        assert_eq!(evidence.preview, exact);
        assert_eq!(evidence.region_bytes, PREVIEW_CHARS * 2);
    }
}
