//! What `agentbus install` and `agentbus uninstall` print.
//!
//! Installing edits files somebody else maintains, and the least an installer
//! owes them is a full account of which files it touched and what it did to
//! each. So every file involved is named, including the ones nothing happened
//! to: "unchanged" is the line that tells a user running the command twice that
//! the second run really did nothing, which is otherwise indistinguishable from
//! a command that failed quietly.
//!
//! A dry run prints the same lines in the conditional, from the same values, so
//! that what it says would happen is what the real run has been asked to do
//! rather than a second description of it.

use std::fmt::Write;

use agentbus_install::{Change, DetectedAgent, Mode, Outcome};

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
            "nothing to {}: this build has no installer for any agent",
            direction.verb()
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

/// One file, and what became of it.
fn describe(change: &Change, mode: Mode) -> String {
    let path = change.path().display();
    match (mode, change) {
        (Mode::Apply, Change::Create { .. }) => format!("created {path}"),
        (Mode::Apply, Change::Rewrite { .. }) => format!("updated {path}"),
        (Mode::Apply, Change::Delete { .. }) => format!("removed {path}"),
        (Mode::DryRun, Change::Create { .. }) => format!("would create {path}"),
        (Mode::DryRun, Change::Rewrite { .. }) => format!("would update {path}"),
        (Mode::DryRun, Change::Delete { .. }) => format!("would remove {path}"),
        (_, Change::Keep { .. }) => format!("unchanged {path}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agentbus_install::Agent;

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
            "no coding agent found on this machine\n\
             nothing to install: this build has no installer for any agent\n"
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
        }];

        let rendered = render(&[], &outcome(changes), Direction::Install, Mode::DryRun);

        assert!(
            rendered.contains("  would update /home/u/.codex/hooks.json\n"),
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
