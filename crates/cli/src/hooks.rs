//! What `agentbus hooks status` prints.
//!
//! Installing puts a file inside somebody else's coding agent and leaves it
//! there, and the file outlives the build that wrote it: a user upgrades this
//! program months later and their agents are still running whatever the old one
//! left behind. So there has to be a way to ask a machine what it is carrying,
//! and the answer has to be one line per agent — short enough to read all of at
//! once, and complete enough that nothing is left out because there was nothing
//! to say about it.
//!
//! Four answers, and each of them is a different thing to do next. Hooks that
//! are what this build writes need nothing. Hooks from an earlier build, or
//! from before this program marked its work at all, are installed again. Hooks
//! whose file is current and whose other half is missing are a wrapper nothing
//! calls, which looks like a working installation from every direction except
//! the one that matters, and are also installed again. And an agent with no
//! hooks at all is only worth a sentence when it is actually on the machine —
//! at which point the sentence worth saying is the command that would fix it.
//!
//! The file the answer came out of is named wherever there is one, because
//! somebody who has just been told their hooks are old or broken is about to go
//! and look.

use std::fmt::Write;

use agentbus_install::{Agent, HookStatus, Recommendation};
use serde::Serialize;

/// The shape `--json` is written in.
const SCHEMA: u32 = 1;

/// What is said when nothing on the machine answers to what was asked for,
/// which is what filtering down to the hooks needing attention usually finds.
const NOTHING: &str = "nothing installed here is behind what this build writes";

/// One agent as the report states it.
#[derive(Debug, Serialize)]
struct Reported<'a> {
    /// Which agent this is about.
    agent: &'a str,
    /// `current`, `outdated`, `needs-repair` or `not-installed`.
    state: &'static str,
    /// Which generation of the hooks is on the machine, where the file there
    /// says — an installation from before this program marked its work does
    /// not.
    installed_version: Option<u32>,
    /// Which generation this build writes.
    expected_version: u32,
    /// The file the answer was read out of, where there was one.
    path: Option<String>,
    /// Whether the agent itself is on this machine.
    detected: bool,
    /// Whether installing for it now would achieve something.
    install_recommended: bool,
}

/// Whether this is one of the agents `--outdated-only` keeps.
///
/// Both of the answers that mean the hooks on the machine are not the hooks
/// this build writes, and neither of the two that mean there is nothing to do:
/// an agent that is already current needs no work, and one with no hooks at all
/// has nothing that could be out of date.
pub fn is_behind(recommendation: &Recommendation) -> bool {
    matches!(
        recommendation.status,
        HookStatus::Outdated { .. } | HookStatus::NeedsRepair(_)
    )
}

/// The report, one line per agent, ending in a newline.
pub fn render(reported: &[&Recommendation]) -> String {
    if reported.is_empty() {
        return format!("{NOTHING}\n");
    }
    let mut text = String::new();
    for recommendation in reported {
        let _ = writeln!(text, "{}", line(recommendation));
    }
    text
}

/// The same facts, for something that is going to read them.
pub fn json(reported: &[&Recommendation]) -> String {
    let agents: Vec<Reported<'_>> = reported.iter().map(|one| stated(one)).collect();
    let mut written = serde_json::to_string(&serde_json::json!({
        "v": SCHEMA,
        "agents": agents,
    }))
    .unwrap_or_else(|_| String::from("{}"));
    written.push('\n');
    written
}

/// One agent's line.
fn line(recommendation: &Recommendation) -> String {
    let mut line = format!("{}: {}", recommendation.agent, said(recommendation.status));
    if let Some(path) = &recommendation.asset {
        let _ = write!(line, " ({})", path.display());
    }
    // Only for an agent with nothing installed. Everywhere else the state
    // already says that installing again is the fix, and repeating it on every
    // line would bury the one line where it is news.
    if recommendation.status == HookStatus::NotInstalled && recommendation.needs_install() {
        let _ = write!(
            line,
            " — detected; run agentbus install --agent {}",
            recommendation.agent
        );
    }
    line
}

/// What one agent's hooks are, as a person reads it.
///
/// A generation is written the way the mark inside the file writes it, so that
/// what somebody reads here and what they find when they open the file are the
/// same thing said twice rather than two things to reconcile.
fn said(status: HookStatus) -> String {
    match status {
        HookStatus::NotInstalled => "not installed".to_owned(),
        HookStatus::Current(found) => format!("current (v{found})"),
        HookStatus::Outdated {
            found: Some(found),
            expected,
        } => format!("outdated (v{found} < v{expected})"),
        // Nothing to compare with: the file predates this program saying which
        // generation its files are, which is the whole of what is known about
        // it and is enough to know it needs writing again.
        HookStatus::Outdated { found: None, .. } => "outdated (pre-versioning)".to_owned(),
        HookStatus::NeedsRepair(found) => format!("needs repair (v{found})"),
    }
}

/// One agent's facts, as a reader of the JSON gets them.
fn stated(recommendation: &Recommendation) -> Reported<'_> {
    Reported {
        agent: recommendation.agent.name(),
        state: state(recommendation.status),
        installed_version: installed(recommendation.status),
        expected_version: agentbus_install::expected_version(recommendation.agent),
        path: recommendation
            .asset
            .as_ref()
            .map(|path| path.display().to_string()),
        detected: recommendation.detected.is_some(),
        install_recommended: recommendation.needs_install(),
    }
}

/// What one agent's hooks are, as one word for a program.
fn state(status: HookStatus) -> &'static str {
    match status {
        HookStatus::NotInstalled => "not-installed",
        HookStatus::Current(_) => "current",
        HookStatus::Outdated { .. } => "outdated",
        HookStatus::NeedsRepair(_) => "needs-repair",
    }
}

/// Which generation is on the machine, where the file there says.
fn installed(status: HookStatus) -> Option<u32> {
    match status {
        HookStatus::NotInstalled => None,
        HookStatus::Current(found) | HookStatus::NeedsRepair(found) => Some(found),
        HookStatus::Outdated { found, .. } => found,
    }
}

/// The agents among `reported` whose hooks are behind what this build writes.
///
/// Deliberately not the same question as [`Recommendation::needs_install`]:
/// an agent on the machine with no hooks at all was a choice somebody made,
/// and a command that has just finished doing what it was asked is no place to
/// argue with it. What is worth saying unasked is that hooks somebody did
/// install have since been left behind by a newer build.
pub fn behind(reported: &[Recommendation]) -> Vec<Agent> {
    reported
        .iter()
        .filter(|one| is_behind(one))
        .map(|one| one.agent)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agentbus_install::DetectedAgent;

    use super::*;

    /// An agent with `status` and nothing else said about it.
    fn about(agent: Agent, status: HookStatus) -> Recommendation {
        Recommendation {
            agent,
            detected: None,
            status,
            asset: match status {
                HookStatus::NotInstalled => None,
                _ => Some(PathBuf::from("/home/u/.claude/hooks/agentbus.sh")),
            },
        }
    }

    /// The same, on a machine the agent is actually on.
    fn found(agent: Agent, status: HookStatus) -> Recommendation {
        Recommendation {
            detected: Some(DetectedAgent {
                agent,
                config_dir: Some(PathBuf::from("/home/u/.claude")),
                command: None,
            }),
            ..about(agent, status)
        }
    }

    #[test]
    fn hooks_of_this_builds_generation_are_current_and_say_which_file_they_are() {
        let line = line(&about(Agent::Claude, HookStatus::Current(3)));

        assert_eq!(
            line,
            "claude: current (v3) (/home/u/.claude/hooks/agentbus.sh)"
        );
    }

    #[test]
    fn hooks_from_an_earlier_build_are_reported_against_what_this_build_writes() {
        let status = HookStatus::Outdated {
            found: Some(1),
            expected: 3,
        };

        assert!(
            line(&about(Agent::Codex, status)).starts_with("codex: outdated (v1 < v3)"),
            "{}",
            line(&about(Agent::Codex, status))
        );
    }

    #[test]
    fn hooks_from_before_this_program_marked_its_work_say_so_in_words() {
        let status = HookStatus::Outdated {
            found: None,
            expected: 3,
        };

        assert!(
            line(&about(Agent::Codex, status)).starts_with("codex: outdated (pre-versioning)"),
            "there is no earlier number to compare with"
        );
    }

    #[test]
    fn a_current_file_nothing_runs_is_a_repair_rather_than_an_upgrade() {
        assert!(
            line(&about(Agent::OpenCode, HookStatus::NeedsRepair(3)))
                .starts_with("opencode: needs repair (v3)"),
        );
    }

    #[test]
    fn an_agent_with_no_hooks_names_no_file() {
        assert_eq!(
            line(&about(Agent::Kimi, HookStatus::NotInstalled)),
            "kimi: not installed"
        );
    }

    #[test]
    fn an_agent_on_the_machine_with_no_hooks_is_told_how_to_get_some() {
        let line = line(&found(Agent::Grok, HookStatus::NotInstalled));

        assert_eq!(
            line,
            "grok: not installed — detected; run agentbus install --agent grok"
        );
    }

    #[test]
    fn an_agent_that_already_has_hooks_is_not_told_to_install_them() {
        let line = line(&found(Agent::Grok, HookStatus::Current(1)));

        assert!(!line.contains("run agentbus install"), "{line}");
    }

    #[test]
    fn only_the_two_answers_that_mean_work_are_kept_when_that_is_what_was_asked_for() {
        let behind = HookStatus::Outdated {
            found: Some(1),
            expected: 2,
        };

        assert!(is_behind(&about(Agent::Claude, behind)));
        assert!(is_behind(&about(Agent::Claude, HookStatus::NeedsRepair(2))));
        assert!(!is_behind(&about(Agent::Claude, HookStatus::Current(2))));
        assert!(!is_behind(&about(Agent::Claude, HookStatus::NotInstalled)));
    }

    #[test]
    fn a_filter_that_matched_nothing_says_so_rather_than_printing_nothing() {
        assert_eq!(render(&[]), format!("{NOTHING}\n"));
    }

    #[test]
    fn every_agent_asked_about_gets_one_line() {
        let reported = [
            about(Agent::Claude, HookStatus::Current(1)),
            about(Agent::Codex, HookStatus::NotInstalled),
        ];
        let reported: Vec<&Recommendation> = reported.iter().collect();

        let rendered = render(&reported);

        assert_eq!(rendered.lines().count(), 2, "{rendered}");
        assert!(rendered.ends_with('\n'), "{rendered:?}");
    }

    #[test]
    fn what_is_written_for_a_program_says_the_same_as_the_line() {
        let reported = [
            found(Agent::Claude, HookStatus::Current(4)),
            about(Agent::Codex, HookStatus::NotInstalled),
        ];
        let reported: Vec<&Recommendation> = reported.iter().collect();

        let written = json(&reported);
        let value: serde_json::Value = serde_json::from_str(&written).expect("not JSON");

        let agents = value["agents"].as_array().expect("no agents");
        assert_eq!(value["v"], SCHEMA);
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0]["agent"], "claude");
        assert_eq!(agents[0]["state"], "current");
        assert_eq!(agents[0]["installed_version"], 4);
        assert_eq!(agents[0]["detected"], true);
        assert_eq!(agents[0]["path"], "/home/u/.claude/hooks/agentbus.sh");
        assert_eq!(agents[1]["state"], "not-installed");
        assert_eq!(agents[1]["installed_version"], serde_json::Value::Null);
        assert_eq!(agents[1]["path"], serde_json::Value::Null);
        assert_eq!(agents[1]["detected"], false);
    }

    #[test]
    fn a_generation_ahead_of_this_build_is_reported_as_the_generation_it_is() {
        let reported = [about(Agent::Claude, HookStatus::Current(9))];
        let reported: Vec<&Recommendation> = reported.iter().collect();

        let value: serde_json::Value = serde_json::from_str(&json(&reported)).expect("not JSON");

        assert_eq!(value["agents"][0]["installed_version"], 9);
        assert_eq!(
            value["agents"][0]["expected_version"],
            agentbus_install::expected_version(Agent::Claude)
        );
    }

    #[test]
    fn the_agents_a_newer_build_has_left_behind_are_the_ones_worth_naming() {
        let reported = vec![
            about(Agent::Claude, HookStatus::Current(1)),
            about(
                Agent::Codex,
                HookStatus::Outdated {
                    found: Some(1),
                    expected: 2,
                },
            ),
            about(Agent::OpenCode, HookStatus::NeedsRepair(1)),
            found(Agent::Grok, HookStatus::NotInstalled),
        ];

        assert_eq!(behind(&reported), vec![Agent::Codex, Agent::OpenCode]);
    }
}
