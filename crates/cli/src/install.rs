//! What `agentbus install` and `agentbus uninstall` print.
//!
//! Installing edits files somebody else maintains and runs commands somebody
//! else wrote, and the least an installer owes them is a full account of both.
//! So every file involved is named and every command is printed the way it would
//! be typed, including the ones nothing happened to: "unchanged" and "already
//! run" are the lines that tell a user running the command twice that the second
//! run really did nothing, which is otherwise indistinguishable from a command
//! that failed quietly.
//!
//! A dry run prints the same lines in the conditional, from the same values, so
//! that what it says would happen is what the real run has been asked to do
//! rather than a second description of it.

use std::fmt::Write;

use agentbus_install::{Agent, Change, DetectedAgent, Mode, Outcome};

/// Which way an installation is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Put the hooks in.
    Install,
    /// Take them out.
    Uninstall,
}

impl Direction {
    /// How the command names itself in what it prints.
    pub fn context(self) -> &'static str {
        match self {
            Self::Install => crate::INSTALL,
            Self::Uninstall => crate::UNINSTALL,
        }
    }

    /// What the command is called, inside a sentence.
    fn verb(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Uninstall => "uninstall",
        }
    }

    /// What it means for an agent to have nothing left to do.
    fn settled(self) -> &'static str {
        match self {
            Self::Install => "already installed",
            Self::Uninstall => "nothing of ours is there",
        }
    }
}

/// The whole of what one run has to say.
pub fn render(
    found: &[DetectedAgent],
    outcomes: &[Outcome],
    direction: Direction,
    mode: Mode,
) -> String {
    let mut out = String::new();
    match found.is_empty() {
        true => out.push_str("no coding agent found on this machine\n"),
        false => {
            for agent in found {
                let _ = writeln!(out, "found {agent}");
            }
        }
    }
    if outcomes.is_empty() {
        let _ = writeln!(
            out,
            "nothing to {}: this build only handles {}",
            direction.verb(),
            supported()
        );
        return out;
    }
    for outcome in outcomes {
        let _ = writeln!(out, "{}", outcome.agent);
        for change in &outcome.changes {
            let _ = writeln!(out, "  {}", describe(change, mode));
        }
        if !outcome.is_change() {
            let _ = writeln!(out, "  {}", direction.settled());
        }
    }
    out
}

/// What to say to a run that named an agent this build cannot act on yet.
///
/// This program knows more agents than it can install for: knowing one is what
/// lets it be recognized on a machine and have its events understood, and
/// installing for it is a separate piece of work that lands later. An agent the
/// user named by hand is one they are waiting on an answer about, so the answer
/// is that this build does not have it yet — rather than a silent run that did
/// nothing, which is what asking for an agent nothing has been written for
/// would otherwise look like.
pub fn unhandled(agents: &[Agent], direction: Direction) -> String {
    format!(
        "cannot {} {} yet; this build handles {}",
        direction.verb(),
        listed(agents.iter().map(Agent::to_string).collect()),
        supported()
    )
}

/// The agents this build can act on, listed the way they would be said aloud.
///
/// Named rather than counted, because the user this line is for has just been
/// told that nothing happened, and what they need to know next is whether their
/// agent is one this build has heard of.
fn supported() -> String {
    listed(
        agentbus_install::supported()
            .iter()
            .map(Agent::to_string)
            .collect(),
    )
}

/// Several names, as a sentence rather than a list.
fn listed(names: Vec<String>) -> String {
    match names.split_last() {
        None => "no agent at all".to_owned(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// One step, and what became of it.
///
/// A directory made and a file written are reported with the same word, and so
/// are a directory cleared away and a file removed. The distinctions are ones
/// this program makes because a directory needs more care than a file; to a user
/// reading what happened to their machine, one of them is now there and the
/// other is not.
fn describe(change: &Change, mode: Mode) -> String {
    match (mode, change) {
        (Mode::Apply, Change::Make { path } | Change::Create { path, .. }) => {
            format!("created {}", path.display())
        }
        (Mode::Apply, Change::Rewrite { path, .. }) => format!("updated {}", path.display()),
        (Mode::Apply, Change::Delete { path } | Change::Clear { path }) => {
            format!("removed {}", path.display())
        }
        (Mode::DryRun, Change::Make { path } | Change::Create { path, .. }) => {
            format!("would create {}", path.display())
        }
        (Mode::DryRun, Change::Rewrite { path, .. }) => format!("would update {}", path.display()),
        (Mode::DryRun, Change::Delete { path } | Change::Clear { path }) => {
            format!("would remove {}", path.display())
        }
        (_, Change::Keep { path }) => format!("unchanged {}", path.display()),
        (Mode::Apply, Change::Run { command }) => format!("ran {command}"),
        (Mode::DryRun, Change::Run { command }) => format!("would run {command}"),
        (_, Change::Ran { command }) => format!("already run {command}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn outcome(changes: Vec<Change>) -> Vec<Outcome> {
        vec![Outcome {
            agent: Agent::Codex,
            changes,
        }]
    }

    fn hooks() -> PathBuf {
        PathBuf::from("/home/u/.codex/hooks.json")
    }

    #[test]
    fn an_empty_machine_is_told_so_rather_than_told_nothing() {
        let rendered = render(&[], &[], Direction::Install, Mode::Apply);
        assert_eq!(
            rendered,
            format!(
                "no coding agent found on this machine\n\
                 nothing to install: this build only handles {}\n",
                supported()
            )
        );
    }

    #[test]
    fn what_was_found_is_named_before_anything_is_done_about_it() {
        let found = [DetectedAgent {
            agent: Agent::Claude,
            config_dir: Some(PathBuf::from("/home/u/.claude")),
            command: None,
        }];

        let rendered = render(&found, &[], Direction::Uninstall, Mode::Apply);

        assert!(
            rendered.starts_with("found claude (configuration directory /home/u/.claude)\n"),
            "{rendered}"
        );
        assert!(rendered.contains("nothing to uninstall"), "{rendered}");
    }

    #[test]
    fn every_file_is_named_and_said_what_happened_to_it() {
        let changes = vec![
            Change::Create {
                path: hooks(),
                contents: String::new(),
                executable: false,
            },
            Change::Keep {
                path: PathBuf::from("/home/u/.codex/other.json"),
            },
        ];

        let rendered = render(&[], &outcome(changes), Direction::Install, Mode::Apply);

        assert!(
            rendered.contains("  created /home/u/.codex/hooks.json\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("  unchanged /home/u/.codex/other.json\n"),
            "{rendered}"
        );
    }

    #[test]
    fn a_dry_run_says_the_same_things_in_the_conditional() {
        let changes = vec![Change::Rewrite {
            path: hooks(),
            contents: String::new(),
            executable: false,
        }];

        let rendered = render(&[], &outcome(changes), Direction::Install, Mode::DryRun);

        assert!(
            rendered.contains("  would update /home/u/.codex/hooks.json\n"),
            "{rendered}"
        );
    }

    #[test]
    fn every_agent_this_build_handles_is_named_when_none_of_them_was_asked_for() {
        let listed = supported();

        for agent in agentbus_install::supported() {
            assert!(listed.contains(agent.name()), "{listed} omits {agent}");
        }
    }

    #[test]
    fn a_command_the_agent_had_to_run_is_reported_as_one() {
        let command = agentbus_install::Invocation::new(
            "/usr/local/bin/claude",
            ["plugin", "install", "agentbus@agentbus"],
        );
        let changes = vec![
            Change::Run {
                command: command.clone(),
            },
            Change::Ran { command },
        ];

        let done = render(
            &[],
            &outcome(changes.clone()),
            Direction::Install,
            Mode::Apply,
        );
        let would = render(&[], &outcome(changes), Direction::Install, Mode::DryRun);

        assert!(
            done.contains("  ran claude plugin install agentbus@agentbus\n"),
            "{done}"
        );
        assert!(
            done.contains("  already run claude plugin install agentbus@agentbus\n"),
            "{done}"
        );
        assert!(
            would.contains("  would run claude plugin install agentbus@agentbus\n"),
            "{would}"
        );
    }

    #[test]
    fn a_directory_cleared_away_is_reported_like_a_file_removed() {
        let changes = vec![Change::Clear {
            path: PathBuf::from("/home/u/.local/share/agentbus/claude-marketplace"),
        }];

        let rendered = render(&[], &outcome(changes), Direction::Uninstall, Mode::Apply);

        assert!(
            rendered.contains("  removed /home/u/.local/share/agentbus/claude-marketplace\n"),
            "{rendered}"
        );
    }

    #[test]
    fn an_agent_with_nothing_to_do_is_told_why() {
        let changes = vec![Change::Keep { path: hooks() }];

        let installed = render(
            &[],
            &outcome(changes.clone()),
            Direction::Install,
            Mode::Apply,
        );
        let removed = render(&[], &outcome(changes), Direction::Uninstall, Mode::Apply);

        assert!(installed.contains("  already installed\n"), "{installed}");
        assert!(
            removed.contains("  nothing of ours is there\n"),
            "{removed}"
        );
    }
}
