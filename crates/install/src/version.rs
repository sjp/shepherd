//! Which generation of this program's hooks a file on a machine is.
//!
//! An installation that is either present or absent cannot answer the question
//! a user actually has, which is whether what they installed months ago is what
//! this build would install today. So every file this program writes into an
//! agent says so itself, in a comment among its opening lines:
//!
//! ```text
//! # AGENTBUS_HOOK_VERSION=1
//! ```
//!
//! A count, not a version of anything. Nobody releases it, nothing depends on a
//! particular value of it, and the only question ever asked of it is whether
//! what is on disk is at least what this build writes. One count per agent,
//! because the agents change independently: rewriting the wrapper one of them
//! runs is no reason to tell everybody else's user that their hooks are behind.
//!
//! It is read from the file rather than from this program's own record of what
//! it installed. The file is the thing the agent actually runs, a user can read
//! it for themselves, and a record that disagreed with it would be answering a
//! question about the wrong machine.

use crate::agent::Agent;

/// What an installed file says its generation is, ahead of the number.
///
/// The same word whatever the file is written in; only the comment leader
/// around it differs, and that is the file's business rather than this one's.
pub const MARKER: &str = "AGENTBUS_HOOK_VERSION=";

/// The generation of `agent`'s hooks this build installs.
///
/// Bumped whenever what this program writes for that agent changes — the text
/// of a file it drops, or the shape of the entry that points at it — because
/// that is what makes an installation already on a machine an old one.
pub fn expected_version(agent: Agent) -> u32 {
    match agent {
        Agent::Antigravity => 1,
        Agent::Claude => 1,
        Agent::Codex => 1,
        Agent::Cursor => 1,
        Agent::Devin => 1,
        Agent::Droid => 1,
        Agent::GithubCopilot => 1,
        Agent::Grok => 1,
        Agent::Hermes => 1,
        Agent::Kilo => 1,
        Agent::Kimi => 1,
        Agent::Mastracode => 1,
        Agent::Omp => 1,
        Agent::OpenCode => 1,
        Agent::Pi => 1,
        Agent::QoderCli => 1,
        Agent::Qwen => 1,
    }
}

/// What generation the file `text` holds says it is, if it says at all.
///
/// Only the comment at the top of the file is read: the first line that is
/// neither blank nor a comment ends the search. A mark further down would be one
/// a file could acquire by accident — from a string, a heredoc, or a line of
/// somebody else's that happens to say the same thing — and the answer here
/// decides whether a user is told their hooks are current.
///
/// Nothing found means nothing is known, which is what a file written before
/// this program marked its work, and a file that was never this program's, both
/// look like. Neither is current, and treating them the same is right: the fix
/// for both is to install again.
pub fn parse_asset_version(text: &str) -> Option<u32> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let body = comment(line)?;
        if let Some(value) = body.strip_prefix(MARKER) {
            return value.trim().parse().ok();
        }
    }
    None
}

/// What a line says, if it is a comment, whichever of the two leaders the file
/// it came from uses.
fn comment(line: &str) -> Option<&str> {
    let body = line.strip_prefix("//").or_else(|| line.strip_prefix('#'))?;
    Some(body.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AGENTS;

    #[test]
    fn every_agent_has_a_generation_and_it_starts_at_one_or_later() {
        for agent in AGENTS {
            assert!(expected_version(agent) >= 1, "{agent}");
        }
    }

    #[test]
    fn the_mark_is_read_whichever_leader_the_file_is_written_with() {
        let files = [
            ("#!/bin/sh\n# AGENTBUS_HOOK_VERSION=3\n", 3),
            ("# AGENTBUS_HOOK_VERSION=4\nparam($Action)\n", 4),
            ("# AGENTBUS_HOOK_VERSION=5\nimport json\n", 5),
            (
                "// AGENTBUS_HOOK_VERSION=6\nimport net from \"node:net\";\n",
                6,
            ),
            ("// AGENTBUS_HOOK_VERSION=7\nexport const plugin = {};\n", 7),
        ];

        for (text, version) in files {
            assert_eq!(parse_asset_version(text), Some(version), "{text}");
        }
    }

    #[test]
    fn the_mark_is_read_anywhere_in_the_comment_at_the_top() {
        let text = "#!/bin/sh\n\
                    # _agentbus — written by this program\n\
                    #\n\
                    \n\
                    # AGENTBUS_HOOK_VERSION=2\n\
                    \n\
                    exit 0\n";

        assert_eq!(parse_asset_version(text), Some(2));
    }

    #[test]
    fn a_mark_below_the_comment_at_the_top_is_not_read() {
        let text = "#!/bin/sh\n\
                    exit 0\n\
                    # AGENTBUS_HOOK_VERSION=2\n";

        assert_eq!(parse_asset_version(text), None);
    }

    #[test]
    fn a_file_that_says_nothing_says_nothing() {
        assert_eq!(parse_asset_version(""), None);
        assert_eq!(parse_asset_version("{\n  \"hooks\": {}\n}\n"), None);
        assert_eq!(
            parse_asset_version("#!/bin/sh\n# written by hand\nexit 0\n"),
            None
        );
    }

    #[test]
    fn a_mark_that_is_not_a_number_is_no_mark_at_all() {
        assert_eq!(parse_asset_version("# AGENTBUS_HOOK_VERSION=one\n"), None);
        assert_eq!(parse_asset_version("# AGENTBUS_HOOK_VERSION=\n"), None);
        assert_eq!(parse_asset_version("# AGENTBUS_HOOK_VERSION=-1\n"), None);
    }

    #[test]
    fn whitespace_around_the_mark_is_not_part_of_it() {
        assert_eq!(
            parse_asset_version("  #   AGENTBUS_HOOK_VERSION= 9 \n"),
            Some(9)
        );
    }
}
