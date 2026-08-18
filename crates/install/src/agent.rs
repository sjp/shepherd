//! Which coding agents are on this machine, and where.
//!
//! Detection is deliberately shallow: an agent counts as present when its
//! configuration directory exists or its command can be run by name. Neither is
//! proof — a configuration directory outlives the uninstall of the thing that
//! made it, and a command on the `PATH` may be a wrapper that no longer works.
//! But the question being asked is only *is it worth offering to install for
//! this?*, and for that a false positive costs a line of output while a false
//! negative costs a user who never finds out their agent is supported.
//!
//! What was found is kept rather than reduced to a yes, because someone running
//! the installer and not getting what they expected needs to know which of the
//! two answers this program got, and from where.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::paths::Environment;

/// A coding agent the bus can install hooks into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Agent {
    Claude,
    Codex,
    OpenCode,
}

/// Every agent, in the order they are reported in.
pub const AGENTS: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::OpenCode];

impl Agent {
    /// What the agent is called, on a command line and in a report.
    ///
    /// This is also the command it is run by; the two have not diverged for any
    /// supported agent, and a name that differed from the command would be a
    /// name nobody could guess.
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }

    /// Where the agent keeps its configuration, below `home`.
    pub fn config_dir(self, home: &Path) -> PathBuf {
        match self {
            Self::Claude => home.join(".claude"),
            Self::Codex => home.join(".codex"),
            Self::OpenCode => home.join(".config").join("opencode"),
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Agent {
    type Err = UnknownAgent;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        AGENTS
            .into_iter()
            .find(|agent| agent.name() == name)
            .ok_or_else(|| UnknownAgent(name.to_owned()))
    }
}

/// A name that is not one of the agents.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown agent \"{0}\": this knows claude, codex and opencode")]
pub struct UnknownAgent(String);

/// An agent that is present on the machine, and the evidence for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedAgent {
    /// Which agent this is.
    pub agent: Agent,
    /// Its configuration directory, if that is what was found.
    pub config_dir: Option<PathBuf>,
    /// Its command, if that is what was found.
    pub command: Option<PathBuf>,
}

impl fmt::Display for DetectedAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.agent)?;
        let mut separator = " (";
        if let Some(dir) = &self.config_dir {
            write!(f, "{separator}configuration directory {}", dir.display())?;
            separator = ", ";
        }
        if let Some(command) = &self.command {
            write!(f, "{separator}command {}", command.display())?;
            separator = ", ";
        }
        match separator {
            " (" => Ok(()),
            _ => f.write_str(")"),
        }
    }
}

/// Every agent that is present on `env`.
pub fn detect(env: &Environment) -> Vec<DetectedAgent> {
    AGENTS
        .into_iter()
        .filter_map(|agent| detected(agent, env))
        .collect()
}

/// Whether one agent is present, and how it was found.
fn detected(agent: Agent, env: &Environment) -> Option<DetectedAgent> {
    let config_dir = Some(agent.config_dir(env.home())).filter(|dir| dir.is_dir());
    let command = env.look_up(agent.name());
    (config_dir.is_some() || command.is_some()).then_some(DetectedAgent {
        agent,
        config_dir,
        command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_agent_is_named_by_the_message_that_refuses_a_name() {
        let refusal = "nonesuch".parse::<Agent>().unwrap_err().to_string();
        for agent in AGENTS {
            assert!(
                refusal.contains(agent.name()),
                "{refusal:?} does not mention {agent}"
            );
        }
    }

    #[test]
    fn an_agent_round_trips_through_its_name() {
        for agent in AGENTS {
            assert_eq!(agent.name().parse::<Agent>(), Ok(agent));
        }
    }

    #[test]
    fn what_was_found_is_reported_and_nothing_else_is() {
        let found = DetectedAgent {
            agent: Agent::Claude,
            config_dir: Some(PathBuf::from("/home/u/.claude")),
            command: None,
        };
        assert_eq!(
            found.to_string(),
            "claude (configuration directory /home/u/.claude)"
        );

        let found = DetectedAgent {
            agent: Agent::Codex,
            config_dir: None,
            command: Some(PathBuf::from("/usr/bin/codex")),
        };
        assert_eq!(found.to_string(), "codex (command /usr/bin/codex)");
    }
}
