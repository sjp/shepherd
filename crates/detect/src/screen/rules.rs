//! Deciding what a screen says: compiling a manifest's rules, matching them
//! against one snapshot, and picking the winner.
//!
//! The dialect is a small boolean algebra over text — three matcher kinds under
//! recursive and/or/not gates, ranked by an integer priority — and nothing
//! else. There is no arithmetic, no capture, no back-reference to earlier
//! screens and above all no code: a manifest that arrived over a network can do
//! exactly one thing, which is look for text in a region. That ceiling is the
//! reason a manifest is safe to download at all, so it is a property of the
//! evaluator rather than a convention manifests are asked to observe.
//!
//! # The compiled form
//!
//! Matching a screen is something a consumer may do several times a second, and
//! recompiling a regex per frame would dominate its cost. A manifest is
//! therefore compiled once — needles folded, patterns built, regions resolved —
//! into a [`CompiledManifest`] that answers snapshots without allocating a
//! matcher again.
//!
//! Compilation never fails. A rule the engine cannot build — a pattern that
//! does not compile, a region it does not have — is switched off and named in a
//! warning, and the rest of the file goes on working. The alternative, refusing
//! the manifest whole, means one typo in a hand-written override blinds
//! detection for that agent entirely; a manifest arriving from somewhere less
//! trusted is held to the stricter standard by validating it before it gets
//! here.

use std::collections::HashMap;

use regex::Regex;
use serde::Serialize;

use crate::screen::region::{RegionSpec, ScreenInput};
use crate::screen::schema::{Gate, GateView, ManifestFault, Rule, ScreenManifest, ScreenState};

/// Why a known agent's screen was read as idle although nothing matched.
///
/// The dialect's posture in one constant: `blocked` is strict and
/// evidence-based, so a screen no rule recognizes is reported as calm. A false
/// alarm teaches people to ignore the signal, which costs more than the
/// occasional missed one.
pub const KNOWN_AGENT_IDLE_FALLBACK: &str = "default_known_agent_idle_fallback";

/// Why a verdict says nothing at all: there was no manifest to consult.
pub const UNKNOWN_AGENT_FALLBACK: &str = "unknown_agent";

/// What one screen was evidence of.
///
/// Serializes to JSON with the same stable snake_case field names
/// [`Explain`](crate::Explain) uses for the fields the two share, so that a
/// consumer asking for a verdict and a consumer asking for the working read the
/// answer out of the same place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Detection {
    /// What the screen says the agent is doing.
    pub state: ScreenState,
    /// Whether the winning state's chrome is live on the screen right now,
    /// rather than merely mentioned by text the agent happens to be showing.
    pub visible: bool,
    /// Whether this screen describes the past rather than the present — a
    /// transcript being reviewed, a scrolled-back history — so that a reader
    /// keeps whatever it believed before instead of adopting the verdict.
    pub skip: bool,
    /// The id of the rule that won, when one did.
    pub matched_rule: Option<String>,
    /// Why there is no winning rule, when there is none.
    pub fallback: Option<&'static str>,
}

impl Detection {
    /// The verdict for an agent nothing describes.
    ///
    /// Distinct from a known agent whose screen matched nothing: that one is a
    /// judgement (calm), this one is an admission (no idea).
    pub fn unknown_agent() -> Self {
        Self {
            state: ScreenState::Unknown,
            visible: false,
            skip: false,
            matched_rule: None,
            fallback: Some(UNKNOWN_AGENT_FALLBACK),
        }
    }

    /// The verdict for a known agent whose screen matched no rule.
    fn known_agent_idle() -> Self {
        Self {
            state: ScreenState::Idle,
            visible: false,
            skip: false,
            matched_rule: None,
            fallback: Some(KNOWN_AGENT_IDLE_FALLBACK),
        }
    }

    /// The verdict a winning rule produces.
    fn from_winner(rule: &Rule) -> Self {
        let state = rule.state.unwrap_or(ScreenState::Unknown);
        Self {
            state,
            visible: match state {
                ScreenState::Idle => rule.visible_idle,
                ScreenState::Working => rule.visible_working,
                ScreenState::Blocked => rule.visible_blocker,
                ScreenState::Unknown => false,
            },
            skip: rule.skip_state_update,
            matched_rule: Some(rule.id.clone()),
            fallback: None,
        }
    }
}

/// A manifest with its patterns built and its regions resolved, ready to be run
/// against screens.
#[derive(Debug)]
pub struct CompiledManifest {
    manifest: ScreenManifest,
    /// One entry per rule, in the manifest's own order.
    rules: Vec<CompiledRule>,
    /// Every distinct region any rule looks at, so that a screen is sliced and
    /// folded once per region rather than once per rule.
    regions: Vec<CompiledRegion>,
    warnings: Vec<String>,
}

/// One region a manifest cares about, and what has to be prepared for it.
#[derive(Debug)]
struct CompiledRegion {
    spec: RegionSpec,
    /// Whether any rule over this region uses a case-insensitive matcher, and
    /// so whether the folded copy is worth making.
    folded: bool,
}

/// One rule's matchers, built — or nothing at all, for a rule that compilation
/// switched off.
#[derive(Debug)]
struct CompiledRule {
    live: Option<LiveRule>,
}

/// A rule that will actually be run.
#[derive(Debug)]
struct LiveRule {
    /// Which of the manifest's regions this rule reads.
    region: usize,
    /// The rule's implicit root gate.
    gate: CompiledGate,
}

/// A built group of matchers.
#[derive(Debug)]
struct CompiledGate {
    all: Vec<CompiledGate>,
    any: Vec<CompiledGate>,
    not: Vec<CompiledGate>,
    /// Needles already folded, so that matching folds only the region.
    contains: Vec<String>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
}

impl CompiledGate {
    /// Whether the gate holds for a region.
    ///
    /// `text` is the region as the author's own bytes; `folded` is the same
    /// text lowercased, which the caller prepares once per region because every
    /// `contains` in the tree wants it.
    fn matches(&self, text: &str, folded: &str) -> bool {
        self.contains.iter().all(|needle| folded.contains(needle))
            && self.regex.iter().all(|pattern| pattern.is_match(text))
            && self
                .line_regex
                .iter()
                .all(|pattern| text.lines().any(|line| pattern.is_match(line)))
            && self.all.iter().all(|gate| gate.matches(text, folded))
            && (self.any.is_empty() || self.any.iter().any(|gate| gate.matches(text, folded)))
            && !self.not.iter().any(|gate| gate.matches(text, folded))
    }

    /// Whether anything in this gate or below it folds case.
    fn folds_case(&self) -> bool {
        !self.contains.is_empty()
            || self
                .all
                .iter()
                .chain(&self.any)
                .chain(&self.not)
                .any(Self::folds_case)
    }
}

/// What one rule made of one screen.
#[derive(Debug, Clone, Copy)]
pub struct RuleVerdict<'m, 'i> {
    /// The rule as its author wrote it.
    pub rule: &'m Rule,
    /// The slice of the screen this rule saw.
    pub region_text: &'i str,
    /// Whether the rule's matchers held.
    pub matched: bool,
    /// Whether the rule was live at all. A rule that compilation switched off
    /// never matches, and saying so is the difference between "your rule is
    /// wrong" and "your rule was never run".
    pub enabled: bool,
}

/// Every rule's verdict over one screen, and which of them won.
///
/// Rules are always evaluated in full, never short-circuited at the first
/// match: a manifest author debugging a misfire needs the whole table, and at
/// the dialect's rule ceiling the difference is not measurable.
#[derive(Debug, Clone)]
pub struct Verdicts<'m, 'i> {
    /// One entry per rule, in the manifest's own order — which is the order the
    /// author edits, and the order that breaks priority ties.
    pub rules: Vec<RuleVerdict<'m, 'i>>,
    /// Which entry won, when any rule matched.
    pub winner: Option<usize>,
}

impl<'m> Verdicts<'m, '_> {
    /// The rule that won, when one did.
    pub fn winning_rule(&self) -> Option<&'m Rule> {
        self.winner.map(|index| self.rules[index].rule)
    }

    /// The verdict these rules add up to.
    pub fn detection(&self) -> Detection {
        match self.winning_rule() {
            Some(rule) => Detection::from_winner(rule),
            None => Detection::known_agent_idle(),
        }
    }
}

impl CompiledManifest {
    /// Builds the matchers one manifest describes.
    ///
    /// Never fails: see the module documentation for why a broken rule is
    /// switched off rather than allowed to take its file down with it.
    pub fn compile(manifest: ScreenManifest) -> Self {
        let mut regions: Vec<CompiledRegion> = Vec::new();
        let mut region_index: HashMap<RegionSpec, usize> = HashMap::new();
        let mut rules = Vec::with_capacity(manifest.rules.len());
        let mut warnings = Vec::new();

        for rule in &manifest.rules {
            rules.push(CompiledRule {
                live: match compile_rule(rule, &mut regions, &mut region_index) {
                    Ok(live) => Some(live),
                    Err(fault) => {
                        warnings.push(disabled(rule, &fault));
                        None
                    }
                },
            });
        }

        Self {
            manifest,
            rules,
            regions,
            warnings,
        }
    }

    /// The manifest this was built from, exactly as its author wrote it.
    pub fn manifest(&self) -> &ScreenManifest {
        &self.manifest
    }

    /// What compilation had to switch off, in the order it found it. Empty for
    /// a manifest that passed [`ScreenManifest::validate`].
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Runs every rule against one screen and reports each verdict.
    ///
    /// The borrowed rules come from this manifest and the borrowed region text
    /// from `input`, so a caller can quote either without copying.
    pub fn evaluate<'m, 'i>(&'m self, input: ScreenInput<'i>) -> Verdicts<'m, 'i> {
        let texts: Vec<&'i str> = self
            .regions
            .iter()
            .map(|region| region.spec.extract(input))
            .collect();
        let folded: Vec<String> = self
            .regions
            .iter()
            .zip(&texts)
            .map(|(region, text)| {
                if region.folded {
                    text.to_lowercase()
                } else {
                    String::new()
                }
            })
            .collect();

        let mut rules = Vec::with_capacity(self.rules.len());
        let mut winner: Option<usize> = None;
        for (index, (rule, compiled)) in self.manifest.rules.iter().zip(&self.rules).enumerate() {
            // A switched-off rule saw nothing and matched nothing; reporting an
            // empty region rather than some stand-in slice keeps a diagnostic
            // from suggesting it was ever run.
            let (region_text, matched) = match &compiled.live {
                Some(live) => (
                    texts[live.region],
                    live.gate.matches(texts[live.region], &folded[live.region]),
                ),
                None => ("", false),
            };
            rules.push(RuleVerdict {
                rule,
                region_text,
                matched,
                enabled: compiled.live.is_some(),
            });
            // A later rule displaces an earlier one only by being strictly
            // higher: equal priorities keep whichever was written first.
            let displaces =
                winner.is_none_or(|best| rule.priority > self.manifest.rules[best].priority);
            if matched && displaces {
                winner = Some(index);
            }
        }

        Verdicts { rules, winner }
    }

    /// What one screen is evidence of.
    pub fn detect(&self, input: ScreenInput<'_>) -> Detection {
        self.evaluate(input).detection()
    }
}

/// Builds one rule, registering the region it reads.
fn compile_rule(
    rule: &Rule,
    regions: &mut Vec<CompiledRegion>,
    region_index: &mut HashMap<RegionSpec, usize>,
) -> Result<LiveRule, ManifestFault> {
    let spec = rule.region_spec().map_err(ManifestFault::UnknownRegion)?;
    let gate = compile_gate(rule.gate())?;
    let region = intern(regions, region_index, spec);
    if gate.folds_case() {
        regions[region].folded = true;
    }
    Ok(LiveRule { region, gate })
}

/// The index of `spec` among the regions seen so far, adding it if it is new.
fn intern(
    regions: &mut Vec<CompiledRegion>,
    index: &mut HashMap<RegionSpec, usize>,
    spec: RegionSpec,
) -> usize {
    *index.entry(spec).or_insert_with(|| {
        regions.push(CompiledRegion {
            spec,
            folded: false,
        });
        regions.len() - 1
    })
}

/// How a switched-off rule is reported.
fn disabled(rule: &Rule, fault: &ManifestFault) -> String {
    format!("rule {:?} is disabled: {fault}", rule.id)
}

/// Builds one gate and everything below it.
///
/// Depth is not guarded here: a validated manifest is already within the
/// dialect's nesting limit, and an unvalidated one reaches this through a
/// caller that accepted the risk. The recursion mirrors the data's own shape,
/// so it is as deep as the file is.
fn compile_gate(gate: GateView<'_>) -> Result<CompiledGate, ManifestFault> {
    Ok(CompiledGate {
        all: compile_gates(gate.all)?,
        any: compile_gates(gate.any)?,
        not: compile_gates(gate.not)?,
        contains: gate.contains.iter().map(|it| it.to_lowercase()).collect(),
        regex: compile_patterns("regex", gate.regex)?,
        line_regex: compile_patterns("line_regex", gate.line_regex)?,
    })
}

/// Builds a list of sibling gates.
fn compile_gates(gates: &[Gate]) -> Result<Vec<CompiledGate>, ManifestFault> {
    gates.iter().map(|gate| compile_gate(gate.view())).collect()
}

/// Builds one gate's patterns from one of its matcher lists.
fn compile_patterns(field: &'static str, patterns: &[String]) -> Result<Vec<Regex>, ManifestFault> {
    patterns
        .iter()
        .map(|pattern| {
            Regex::new(pattern).map_err(|error| ManifestFault::InvalidPattern {
                field,
                pattern: pattern.clone(),
                message: error.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-rule manifest wrapped around whatever matchers the test wants.
    fn one_rule(body: &str) -> CompiledManifest {
        compile(&format!(
            "id = \"agent\"\n\n[[rules]]\nid = \"rule\"\n{body}\n"
        ))
    }

    fn compile(content: &str) -> CompiledManifest {
        CompiledManifest::compile(ScreenManifest::parse(content).expect("valid manifest"))
    }

    /// Whether a rule with these matchers holds for this screen.
    fn holds(body: &str, screen: &str) -> bool {
        one_rule(body)
            .evaluate(ScreenInput::from_screen(screen))
            .rules[0]
            .matched
    }

    fn detect_screen(manifest: &CompiledManifest, screen: &str) -> Detection {
        manifest.detect(ScreenInput::from_screen(screen))
    }

    #[test]
    fn contains_folds_case_and_regex_does_not() {
        let cases = [
            ("contains = [\"Allow\"]", "allow this?", true),
            ("contains = [\"allow\"]", "ALLOW THIS?", true),
            ("contains = [\"ÉDITER\"]", "éditer le fichier", true),
            ("regex = [\"Allow\"]", "allow this?", false),
            ("regex = [\"Allow\"]", "Allow this?", true),
            ("line_regex = [\"Allow\"]", "allow this?", false),
        ];
        for (body, screen, expected) in cases {
            assert_eq!(holds(body, screen), expected, "{body} against {screen:?}");
        }
    }

    #[test]
    fn line_regex_anchors_per_line_and_regex_does_not() {
        let screen = "first line\nfoo\nlast line\n";
        assert!(holds("line_regex = [\"^foo$\"]", screen));
        assert!(!holds("regex = [\"^foo$\"]", screen));
        assert!(holds("regex = [\"(?m)^foo$\"]", screen));
    }

    #[test]
    fn regex_does_not_cross_lines_without_being_told_to() {
        let screen = "one\ntwo\n";
        assert!(!holds("regex = [\"one.two\"]", screen));
        assert!(holds("regex = [\"(?s)one.two\"]", screen));
    }

    #[test]
    fn every_matcher_in_a_gate_must_hold() {
        let screen = "waiting for approval\ny to accept\n";
        assert!(holds(
            "contains = [\"WAITING\", \"approval\"]\nline_regex = [\"^y to accept$\"]",
            screen
        ));
        assert!(!holds("contains = [\"waiting\", \"denied\"]", screen));
        assert!(!holds(
            "contains = [\"waiting\"]\nregex = [\"never\"]",
            screen
        ));
        assert!(!holds(
            "contains = [\"waiting\"]\nline_regex = [\"^y to accept, please$\"]",
            screen
        ));
    }

    #[test]
    fn any_is_an_or() {
        let body = "any = [{ contains = [\"yes\"] }, { contains = [\"no\"] }]";
        assert!(holds(body, "say no"));
        assert!(holds(body, "say yes"));
        assert!(holds(body, "yes or no"));
        assert!(!holds(body, "say maybe"));
    }

    #[test]
    fn all_is_an_and_over_nested_gates() {
        let body = "all = [{ contains = [\"yes\"] }, { contains = [\"no\"] }]";
        assert!(holds(body, "yes or no"));
        assert!(!holds(body, "say yes"));
    }

    #[test]
    fn not_vetoes() {
        let body = "contains = [\"ready\"]\nnot = [{ contains = [\"error\"] }]";
        assert!(holds(body, "ready when you are"));
        assert!(!holds(body, "ready, but an error happened"));
    }

    #[test]
    fn a_gate_with_contains_and_any_means_and_of_contains_and_or_of_any() {
        let body =
            "contains = [\"proceed?\"]\nany = [{ contains = [\"yes\"] }, { contains = [\"no\"] }]";
        assert!(holds(body, "proceed? yes"));
        assert!(!holds(body, "proceed? maybe"));
        assert!(!holds(body, "yes"));
    }

    #[test]
    fn gates_nest_to_the_dialect_limit() {
        let mut body = "contains = [\"deep\"]".to_owned();
        for _ in 0..8 {
            body = format!("all = [{{ {body} }}]");
        }
        assert!(holds(&body, "deep enough"));
        assert!(!holds(&body, "shallow"));
    }

    #[test]
    fn a_rule_reads_only_the_region_it_names() {
        let manifest = one_rule("region = \"osc_title\"\ncontains = [\"busy\"]");
        let verdict = |input| manifest.evaluate(input).rules[0].matched;
        assert!(!verdict(ScreenInput::from_screen("busy")));
        assert!(verdict(ScreenInput {
            screen: "",
            osc_title: "busy",
            osc_progress: "",
        }));
    }

    /// The rules a priority test ranks, written most-recently-first so that
    /// order and priority disagree.
    fn ranked(rules: &[(&str, i32, &str)]) -> CompiledManifest {
        let mut content = "id = \"agent\"\n".to_owned();
        for (id, priority, needle) in rules {
            content.push_str(&format!(
                "\n[[rules]]\nid = \"{id}\"\nstate = \"blocked\"\npriority = {priority}\ncontains = [\"{needle}\"]\n"
            ));
        }
        compile(&content)
    }

    #[test]
    fn the_highest_priority_match_wins_wherever_it_sits() {
        for order in [
            [("low", 1, "here"), ("high", 5, "here")],
            [("high", 5, "here"), ("low", 1, "here")],
        ] {
            let winner = detect_screen(&ranked(&order), "here").matched_rule;
            assert_eq!(winner.as_deref(), Some("high"), "{order:?}");
        }
    }

    #[test]
    fn equal_priorities_keep_whichever_was_written_first() {
        let manifest = ranked(&[("first", 3, "here"), ("second", 3, "here")]);
        assert_eq!(
            detect_screen(&manifest, "here").matched_rule.as_deref(),
            Some("first")
        );
    }

    #[test]
    fn priorities_may_be_negative() {
        let manifest = ranked(&[("catch-all", -10, "here"), ("specific", 0, "here")]);
        assert_eq!(
            detect_screen(&manifest, "here").matched_rule.as_deref(),
            Some("specific")
        );

        let only_catch_all = ranked(&[("catch-all", -10, "here"), ("specific", 0, "elsewhere")]);
        assert_eq!(
            detect_screen(&only_catch_all, "here")
                .matched_rule
                .as_deref(),
            Some("catch-all")
        );
    }

    #[test]
    fn a_high_priority_rule_that_does_not_match_shadows_nothing() {
        let manifest = ranked(&[("loud", 99, "absent"), ("quiet", 0, "here")]);
        assert_eq!(
            detect_screen(&manifest, "here").matched_rule.as_deref(),
            Some("quiet")
        );
    }

    #[test]
    fn every_rule_is_evaluated_even_after_a_winner_is_found() {
        let manifest = ranked(&[("first", 9, "here"), ("second", 1, "here")]);
        let verdicts = manifest.evaluate(ScreenInput::from_screen("here"));
        assert!(verdicts.rules.iter().all(|verdict| verdict.matched));
        assert_eq!(verdicts.winner, Some(0));
    }

    #[test]
    fn a_visible_flag_counts_only_for_the_state_that_won() {
        let blocked_but_flagged_working = one_rule(
            "state = \"blocked\"\nvisible_working = true\nvisible_idle = true\ncontains = [\"here\"]",
        );
        assert!(!detect_screen(&blocked_but_flagged_working, "here").visible);

        let blocked_and_flagged_blocked =
            one_rule("state = \"blocked\"\nvisible_blocker = true\ncontains = [\"here\"]");
        let detection = detect_screen(&blocked_and_flagged_blocked, "here");
        assert_eq!(detection.state, ScreenState::Blocked);
        assert!(detection.visible);
    }

    #[test]
    fn each_state_reads_its_own_flag() {
        for (state, flag) in [
            ("idle", "visible_idle"),
            ("working", "visible_working"),
            ("blocked", "visible_blocker"),
        ] {
            let manifest = one_rule(&format!(
                "state = \"{state}\"\n{flag} = true\ncontains = [\"here\"]"
            ));
            assert!(detect_screen(&manifest, "here").visible, "{state}");
        }
    }

    #[test]
    fn a_screen_that_describes_the_past_asks_to_be_ignored() {
        let manifest =
            one_rule("state = \"unknown\"\nskip_state_update = true\ncontains = [\"transcript\"]");
        let detection = detect_screen(&manifest, "transcript");
        assert!(detection.skip);
        assert_eq!(detection.state, ScreenState::Unknown);
        assert!(!detect_screen(&manifest, "something else").skip);
    }

    #[test]
    fn a_known_agent_whose_screen_matches_nothing_is_calm() {
        let manifest = one_rule("contains = [\"never on this screen\"]");
        assert_eq!(
            detect_screen(&manifest, "an interface nobody described"),
            Detection {
                state: ScreenState::Idle,
                visible: false,
                skip: false,
                matched_rule: None,
                fallback: Some("default_known_agent_idle_fallback"),
            }
        );
    }

    #[test]
    fn an_agent_nothing_describes_admits_it() {
        assert_eq!(
            Detection::unknown_agent(),
            Detection {
                state: ScreenState::Unknown,
                visible: false,
                skip: false,
                matched_rule: None,
                fallback: Some("unknown_agent"),
            }
        );
    }

    #[test]
    fn a_rule_without_a_state_contributes_unknown() {
        let manifest =
            compile("id = \"agent\"\n\n[[rules]]\nid = \"seen\"\ncontains = [\"here\"]\n");
        let detection = detect_screen(&manifest, "here");
        assert_eq!(detection.state, ScreenState::Unknown);
        assert_eq!(detection.matched_rule.as_deref(), Some("seen"));
        assert_eq!(detection.fallback, None);
        assert!(!detection.visible);
    }

    #[test]
    fn a_pattern_that_does_not_compile_switches_off_only_its_own_rule() {
        let mut manifest = ScreenManifest::parse(
            "id = \"agent\"\n\
             \n[[rules]]\nid = \"broken\"\nstate = \"blocked\"\npriority = 9\ncontains = [\"here\"]\n\
             \n[[rules]]\nid = \"sound\"\nstate = \"working\"\ncontains = [\"here\"]\n",
        )
        .expect("valid manifest");
        manifest.rules[0].regex = vec!["(unclosed".to_owned()];

        let compiled = CompiledManifest::compile(manifest);
        assert_eq!(compiled.warnings().len(), 1);
        let warning = &compiled.warnings()[0];
        assert!(warning.contains("\"broken\""), "{warning}");
        assert!(warning.contains("(unclosed"), "{warning}");

        let verdicts = compiled.evaluate(ScreenInput::from_screen("here"));
        assert!(!verdicts.rules[0].enabled);
        assert!(!verdicts.rules[0].matched);
        assert_eq!(verdicts.rules[0].region_text, "");
        assert!(verdicts.rules[1].enabled);
        assert_eq!(
            verdicts.detection().matched_rule.as_deref(),
            Some("sound"),
            "the sound rule should have survived its neighbour"
        );
    }

    #[test]
    fn a_region_this_engine_does_not_have_switches_off_only_its_own_rule() {
        let mut manifest = ScreenManifest::parse(
            "id = \"agent\"\n\
             \n[[rules]]\nid = \"elsewhere\"\nstate = \"blocked\"\npriority = 9\ncontains = [\"here\"]\n\
             \n[[rules]]\nid = \"sound\"\nstate = \"working\"\ncontains = [\"here\"]\n",
        )
        .expect("valid manifest");
        manifest.rules[0].region = "the_bit_at_the_side".to_owned();

        let compiled = CompiledManifest::compile(manifest);
        assert_eq!(compiled.warnings().len(), 1);
        assert!(
            compiled.warnings()[0].contains("the_bit_at_the_side"),
            "{}",
            compiled.warnings()[0]
        );
        assert_eq!(
            detect_screen(&compiled, "here").matched_rule.as_deref(),
            Some("sound")
        );
    }

    #[test]
    fn a_validated_manifest_compiles_without_warnings() {
        let manifest = one_rule("contains = [\"here\"]\nregex = [\"h.re\"]");
        assert!(manifest.warnings().is_empty());
        assert_eq!(manifest.manifest().id, "agent");
    }

    /// Three rules over one synthetic agent, exercised end to end.
    const THREE_RULES: &str = r#"
id = "synthetic"

[[rules]]
id = "approval-prompt"
state = "blocked"
priority = 100
region = "bottom_lines(4)"
visible_blocker = true
contains = ["do you want to proceed?"]
any = [{ line_regex = ["^\\s*1\\. Yes"] }, { line_regex = ["^\\s*2\\. No"] }]

[[rules]]
id = "spinner"
state = "working"
priority = 50
region = "bottom_lines(4)"
visible_working = true
line_regex = ["^[*+x] \\w+…"]
not = [{ contains = ["do you want to proceed?"] }]

[[rules]]
id = "empty-prompt"
state = "idle"
priority = 10
region = "bottom_lines(2)"
visible_idle = true
line_regex = ["^> $"]
"#;

    #[test]
    fn three_rules_over_three_screens() {
        let manifest = compile(THREE_RULES);
        assert!(manifest.warnings().is_empty());

        let blocked = detect_screen(
            &manifest,
            "editing src/main.rs\n\nDo you want to proceed?\n  1. Yes\n  2. No, tell me more\n",
        );
        assert_eq!(blocked.state, ScreenState::Blocked);
        assert_eq!(blocked.matched_rule.as_deref(), Some("approval-prompt"));
        assert!(blocked.visible);
        assert!(!blocked.skip);
        assert_eq!(blocked.fallback, None);

        let working = detect_screen(&manifest, "reading files\n\n* Thinking…\n");
        assert_eq!(working.state, ScreenState::Working);
        assert_eq!(working.matched_rule.as_deref(), Some("spinner"));
        assert!(working.visible);

        let idle = detect_screen(&manifest, "all done\n\n> \n");
        assert_eq!(idle.state, ScreenState::Idle);
        assert_eq!(idle.matched_rule.as_deref(), Some("empty-prompt"));
        assert!(idle.visible);

        let unrecognized = detect_screen(&manifest, "$ ls -la\ntotal 0\n");
        assert_eq!(unrecognized.state, ScreenState::Idle);
        assert_eq!(unrecognized.matched_rule, None);
        assert_eq!(unrecognized.fallback, Some(KNOWN_AGENT_IDLE_FALLBACK));
        assert!(!unrecognized.visible);
    }

    #[test]
    fn a_rules_region_bounds_what_it_can_see() {
        let manifest = compile(THREE_RULES);
        // The same prompt text, but scrolled far enough up that the rule's
        // four-line window no longer covers it.
        let scrolled = "Do you want to proceed?\n  1. Yes\n\nrunning tests\nok\nok\nok\n";
        let detection = detect_screen(&manifest, scrolled);
        assert_eq!(detection.matched_rule, None);
        assert_eq!(detection.state, ScreenState::Idle);
    }

    #[test]
    fn the_top_level_matchers_of_a_rule_behave_as_one_gate() {
        let manifest = compile(THREE_RULES);
        // The spinner rule's `not` vetoes it while the approval prompt is up,
        // so the approval rule wins on evidence rather than only on priority.
        let both = "* Thinking…\nDo you want to proceed?\n  1. Yes\n";
        let verdicts = manifest.evaluate(ScreenInput::from_screen(both));
        assert!(verdicts.rules[0].matched, "approval prompt should match");
        assert!(!verdicts.rules[1].matched, "spinner should be vetoed");
        assert_eq!(
            verdicts.winning_rule().map(|rule| rule.id.as_str()),
            Some("approval-prompt")
        );
    }
}
