//! Which coding agents this program knows about, where each keeps its
//! configuration, and which of them are on this machine.
//!
//! The three questions are one module because they are one body of knowledge
//! about an agent, and answering any of them from a different place is how a
//! program ends up looking for an agent under one name and installing for it
//! under another. So each agent is one enum variant, and everything this crate
//! knows about it is a match arm: what it is called, the variable it lets a user
//! move its configuration with, where that configuration sits when they have
//! not, and the names its command is run by.
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
use std::path::PathBuf;
use std::str::FromStr;

use crate::paths::{Environment, Platform};

/// A coding agent the bus can install hooks into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Agent {
    Antigravity,
    Claude,
    Codex,
    Cursor,
    Devin,
    Droid,
    GithubCopilot,
    Grok,
    Hermes,
    Kilo,
    Kimi,
    Mastracode,
    Omp,
    OpenCode,
    Pi,
    QoderCli,
    Qwen,
}

/// Every agent, in the order they are reported in.
pub const AGENTS: [Agent; 17] = [
    Agent::Antigravity,
    Agent::Claude,
    Agent::Codex,
    Agent::Cursor,
    Agent::Devin,
    Agent::Droid,
    Agent::GithubCopilot,
    Agent::Grok,
    Agent::Hermes,
    Agent::Kilo,
    Agent::Kimi,
    Agent::Mastracode,
    Agent::Omp,
    Agent::OpenCode,
    Agent::Pi,
    Agent::QoderCli,
    Agent::Qwen,
];

/// The variable Claude Code keeps its configuration directory under.
pub const CLAUDE_CONFIG_DIR_VAR: &str = "CLAUDE_CONFIG_DIR";

/// The variable Codex keeps its configuration directory under.
pub const CODEX_HOME_VAR: &str = "CODEX_HOME";

/// The variable Kimi keeps its configuration directory under.
pub const KIMI_HOME_VAR: &str = "KIMI_CODE_HOME";

/// The variable GitHub Copilot's command line keeps its configuration
/// directory under.
pub const COPILOT_HOME_VAR: &str = "COPILOT_HOME";

/// The variable Qoder keeps its configuration directory under.
pub const QODER_CONFIG_DIR_VAR: &str = "QODER_CONFIG_DIR";

/// The variable Qwen keeps its configuration directory under.
pub const QWEN_HOME_VAR: &str = "QWEN_HOME";

/// The variable Cursor's command line keeps its configuration directory under.
pub const CURSOR_CONFIG_DIR_VAR: &str = "CURSOR_CONFIG_DIR";

/// The variable Antigravity's command line reads its global customizations,
/// hooks included, from.
pub const ANTIGRAVITY_CONFIG_DIR_VAR: &str = "ANTIGRAVITY_CLI_CONFIG_DIR";

/// The variable Grok keeps its configuration directory under.
pub const GROK_HOME_VAR: &str = "GROK_HOME";

/// The variable Hermes keeps its configuration directory under.
pub const HERMES_HOME_VAR: &str = "HERMES_HOME";

/// The variable Pi keeps its agent directory under, honoured by both the agents
/// that read that layout.
pub const PI_AGENT_DIR_VAR: &str = "PI_CODING_AGENT_DIR";

/// The variable naming the directory, below the home directory, that Omp keeps
/// its agent directory in.
pub const PI_CONFIG_DIR_VAR: &str = "PI_CONFIG_DIR";

/// The variable naming the base directory a user's configuration goes under.
pub const CONFIG_HOME_VAR: &str = "XDG_CONFIG_HOME";

/// Every variable that moves an agent's configuration, so that a machine can be
/// read for all of them at once.
///
/// The list is here rather than where the environment is read because it is the
/// agents that decide what these are called, and a variable that was read but
/// never asked for — or asked for but never read — would be a directory this
/// program looked in and the agent did not.
pub const OVERRIDE_VARS: [&str; 13] = [
    ANTIGRAVITY_CONFIG_DIR_VAR,
    CLAUDE_CONFIG_DIR_VAR,
    CODEX_HOME_VAR,
    CONFIG_HOME_VAR,
    COPILOT_HOME_VAR,
    CURSOR_CONFIG_DIR_VAR,
    GROK_HOME_VAR,
    HERMES_HOME_VAR,
    KIMI_HOME_VAR,
    PI_AGENT_DIR_VAR,
    PI_CONFIG_DIR_VAR,
    QODER_CONFIG_DIR_VAR,
    QWEN_HOME_VAR,
];

/// The directory Omp keeps its agent directory in when nothing says otherwise.
const OMP_DIR: &str = ".omp";

impl Agent {
    /// What the agent is called, on a command line and in a report.
    ///
    /// This is the name the whole of this program calls it by — the value
    /// `--agent` takes, the id its events carry, the id its hook mapping is
    /// filed under — and it is chosen to be the name a user would think of
    /// rather than the name of the file that starts it. Those differ for
    /// several agents, and the command is asked for separately.
    pub fn name(self) -> &'static str {
        match self {
            Self::Antigravity => "antigravity",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Devin => "devin",
            Self::Droid => "droid",
            Self::GithubCopilot => "github-copilot",
            Self::Grok => "grok",
            Self::Hermes => "hermes",
            Self::Kilo => "kilo",
            Self::Kimi => "kimi",
            Self::Mastracode => "mastracode",
            Self::Omp => "omp",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
            Self::QoderCli => "qodercli",
            Self::Qwen => "qwen",
        }
    }

    /// Where the agent keeps its configuration on `env`.
    ///
    /// Most agents let a user say, in a variable of their own naming, and that
    /// answer wins: somebody who has moved their agent's configuration has
    /// usually done it because the default was wrong for their machine, and an
    /// installer that wrote to the default anyway would put hooks somewhere the
    /// agent will never read them. The rest keep it in one place and are given
    /// as that place.
    pub fn config_dir(self, env: &Environment) -> PathBuf {
        match self {
            // Global customizations, hooks among them, are read out of the
            // configuration directory of the family of tools it belongs to; the
            // directory beside it holds runtime data and is never read for
            // hooks.
            Self::Antigravity => {
                overridable(env, ANTIGRAVITY_CONFIG_DIR_VAR, &[".gemini", "config"])
            }
            Self::Claude => overridable(env, CLAUDE_CONFIG_DIR_VAR, &[".claude"]),
            Self::Codex => overridable(env, CODEX_HOME_VAR, &[".codex"]),
            Self::Cursor => overridable(env, CURSOR_CONFIG_DIR_VAR, &[".cursor"]),
            Self::Devin => match env.var(CONFIG_HOME_VAR) {
                Some(base) => base.join("devin"),
                None => below_home(env, &[".config", "devin"]),
            },
            Self::Droid => below_home(env, &[".factory"]),
            Self::GithubCopilot => overridable(env, COPILOT_HOME_VAR, &[".copilot"]),
            Self::Grok => overridable(env, GROK_HOME_VAR, &[".grok"]),
            Self::Hermes => overridable(env, HERMES_HOME_VAR, &[".hermes"]),
            Self::Kilo => below_home(env, &[".config", "kilo"]),
            Self::Kimi => overridable(env, KIMI_HOME_VAR, &[".kimi-code"]),
            Self::Mastracode => below_home(env, &[".mastracode"]),
            // Reads the same layout as Pi, and the same variable for it. What
            // differs is only the directory it goes in when that is unset,
            // which has a variable of its own.
            Self::Omp => match env.var(PI_AGENT_DIR_VAR) {
                Some(dir) => dir,
                None => env
                    .home()
                    .join(env.var(PI_CONFIG_DIR_VAR).unwrap_or_else(|| OMP_DIR.into()))
                    .join("agent"),
            },
            Self::OpenCode => below_home(env, &[".config", "opencode"]),
            Self::Pi => overridable(env, PI_AGENT_DIR_VAR, &[".pi", "agent"]),
            Self::QoderCli => overridable(env, QODER_CONFIG_DIR_VAR, &[".qoder"]),
            Self::Qwen => overridable(env, QWEN_HOME_VAR, &[".qwen"]),
        }
    }

    /// The names the agent's command is run by on `platform`, best first.
    ///
    /// More than one where an agent is shipped under more than one name, so
    /// that a user who installed it under either is found. The list depends on
    /// the machine because some of those names exist only on one of them.
    pub fn commands(self, platform: Platform) -> &'static [&'static str] {
        match self {
            Self::Antigravity => &["agy"],
            Self::Claude => &["claude"],
            Self::Codex => &["codex"],
            Self::Cursor => &["cursor-agent"],
            Self::Devin => &["devin"],
            Self::Droid => &["droid"],
            Self::GithubCopilot => &["copilot"],
            Self::Grok => &["grok"],
            Self::Hermes => &["hermes"],
            Self::Kilo => &["kilo", "kilo-code"],
            Self::Kimi => &["kimi"],
            Self::Mastracode => &["mastracode"],
            Self::Omp => &["omp"],
            Self::OpenCode => &["opencode"],
            Self::Pi => &["pi"],
            Self::QoderCli => match platform {
                Platform::Unix => &["qodercli"],
                Platform::Windows => &["qodercli", "qoder", "qoderclicn", "qodercn"],
            },
            Self::Qwen => &["qwen"],
        }
    }

    /// Whether the agent runs on `platform` at all.
    ///
    /// Asked before an agent is looked for, so that one that does not exist for
    /// a kind of machine is left out of what was found rather than reported as
    /// missing from it: those are different answers, and only one of them is
    /// worth a user's time. It is answered from the names the agent is run by,
    /// so that the question and the search cannot drift apart — a machine on
    /// which there is no name that would start the agent is one the agent has
    /// not been built for.
    pub fn runs_on(self, platform: Platform) -> bool {
        !self.commands(platform).is_empty()
    }

    /// Where the agent's command is on `env`, if it is anywhere.
    pub fn command(self, env: &Environment) -> Option<PathBuf> {
        self.commands(env.platform())
            .iter()
            .find_map(|name| env.look_up(name))
    }
}

/// The directory `var` names on `env`, or the one `segments` name below its
/// home directory.
fn overridable(env: &Environment, var: &str, segments: &[&str]) -> PathBuf {
    env.var(var).unwrap_or_else(|| below_home(env, segments))
}

/// The directory `segments` name below the home directory of `env`.
fn below_home(env: &Environment, segments: &[&str]) -> PathBuf {
    segments
        .iter()
        .fold(env.home().to_owned(), |dir, segment| dir.join(segment))
}

/// Every agent's name, in a list a person can read.
fn names() -> String {
    AGENTS.map(Agent::name).join(", ")
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
#[error("unknown agent \"{}\": this knows {}", .0, names())]
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
        .filter(|agent| agent.runs_on(env.platform()))
        .filter_map(|agent| detected(agent, env))
        .collect()
}

/// Whether one agent is present, and how it was found.
fn detected(agent: Agent, env: &Environment) -> Option<DetectedAgent> {
    let config_dir = Some(agent.config_dir(env)).filter(|dir| dir.is_dir());
    let command = agent.command(env);
    (config_dir.is_some() || command.is_some()).then_some(DetectedAgent {
        agent,
        config_dir,
        command,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// A machine with a home directory that is not anybody's.
    fn machine() -> Environment {
        Environment::rooted("/home/u")
    }

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
    fn no_two_agents_are_the_same_agent() {
        let mut names: Vec<&str> = AGENTS.iter().copied().map(Agent::name).collect();
        names.sort_unstable();
        names.dedup();

        assert_eq!(names.len(), AGENTS.len());
    }

    #[test]
    fn no_two_agents_keep_their_configuration_in_one_place() {
        let env = machine();
        let mut dirs: Vec<PathBuf> = AGENTS.iter().map(|agent| agent.config_dir(&env)).collect();
        let all = dirs.len();
        dirs.sort();
        dirs.dedup();

        assert_eq!(
            dirs.len(),
            all,
            "two agents share a configuration directory"
        );
    }

    #[test]
    fn every_agent_keeps_its_configuration_where_it_is_documented_to() {
        let env = machine();
        let documented = [
            (Agent::Antigravity, "/home/u/.gemini/config"),
            (Agent::Claude, "/home/u/.claude"),
            (Agent::Codex, "/home/u/.codex"),
            (Agent::Cursor, "/home/u/.cursor"),
            (Agent::Devin, "/home/u/.config/devin"),
            (Agent::Droid, "/home/u/.factory"),
            (Agent::GithubCopilot, "/home/u/.copilot"),
            (Agent::Grok, "/home/u/.grok"),
            (Agent::Hermes, "/home/u/.hermes"),
            (Agent::Kilo, "/home/u/.config/kilo"),
            (Agent::Kimi, "/home/u/.kimi-code"),
            (Agent::Mastracode, "/home/u/.mastracode"),
            (Agent::Omp, "/home/u/.omp/agent"),
            (Agent::OpenCode, "/home/u/.config/opencode"),
            (Agent::Pi, "/home/u/.pi/agent"),
            (Agent::QoderCli, "/home/u/.qoder"),
            (Agent::Qwen, "/home/u/.qwen"),
        ];

        assert_eq!(
            documented.len(),
            AGENTS.len(),
            "an agent is missing a place"
        );
        for (agent, dir) in documented {
            assert_eq!(agent.config_dir(&env), Path::new(dir), "{agent}");
        }
    }

    #[test]
    fn every_variable_an_agent_documents_moves_that_agent_and_nothing_else() {
        let env = machine();
        // What each variable does to the agent that reads it, given the same
        // value. Most name the configuration directory itself; the one that
        // names a base directory has the agent's own directory below it.
        let documented = [
            (Agent::Antigravity, ANTIGRAVITY_CONFIG_DIR_VAR, "/moved"),
            (Agent::Claude, CLAUDE_CONFIG_DIR_VAR, "/moved"),
            (Agent::Codex, CODEX_HOME_VAR, "/moved"),
            (Agent::Cursor, CURSOR_CONFIG_DIR_VAR, "/moved"),
            (Agent::Devin, CONFIG_HOME_VAR, "/moved/devin"),
            (Agent::GithubCopilot, COPILOT_HOME_VAR, "/moved"),
            (Agent::Grok, GROK_HOME_VAR, "/moved"),
            (Agent::Hermes, HERMES_HOME_VAR, "/moved"),
            (Agent::Kimi, KIMI_HOME_VAR, "/moved"),
            (Agent::Omp, PI_AGENT_DIR_VAR, "/moved"),
            (Agent::Pi, PI_AGENT_DIR_VAR, "/moved"),
            (Agent::QoderCli, QODER_CONFIG_DIR_VAR, "/moved"),
            (Agent::Qwen, QWEN_HOME_VAR, "/moved"),
        ];

        for (agent, var, moved) in documented {
            let set = env.clone().with_var(var, "/moved");
            assert_eq!(agent.config_dir(&set), Path::new(moved), "{var}");

            let untouched: Vec<Agent> = AGENTS
                .iter()
                .copied()
                .filter(|other| other.config_dir(&set) != other.config_dir(&env))
                .filter(|other| {
                    !documented
                        .iter()
                        .any(|(read, name, _)| read == other && *name == var)
                })
                .collect();
            assert!(untouched.is_empty(), "{var} moved {untouched:?} as well");

            let empty = env.clone().with_var(var, "");
            assert_eq!(
                agent.config_dir(&empty),
                agent.config_dir(&env),
                "{var} set to nothing is {var} not set"
            );
        }
    }

    #[test]
    fn every_variable_that_is_read_is_one_an_agent_asks_for() {
        for var in OVERRIDE_VARS {
            let set = machine().with_var(var, "/moved");
            assert!(
                AGENTS
                    .iter()
                    .any(|agent| agent.config_dir(&set).starts_with("/moved")),
                "{var} moves nothing"
            );
        }
    }

    #[test]
    fn a_variable_may_say_where_it_means_with_a_tilde() {
        let env = machine().with_var(GROK_HOME_VAR, "~/elsewhere/grok");

        assert_eq!(
            Agent::Grok.config_dir(&env),
            Path::new("/home/u/elsewhere/grok")
        );
    }

    #[test]
    fn the_two_agents_that_share_a_layout_do_not_share_a_directory() {
        let env = machine();

        assert_eq!(Agent::Pi.config_dir(&env), Path::new("/home/u/.pi/agent"));
        assert_eq!(Agent::Omp.config_dir(&env), Path::new("/home/u/.omp/agent"));

        let named = env.clone().with_var(PI_CONFIG_DIR_VAR, ".mine");
        assert_eq!(
            Agent::Omp.config_dir(&named),
            Path::new("/home/u/.mine/agent")
        );
        assert_eq!(
            Agent::Pi.config_dir(&named),
            Path::new("/home/u/.pi/agent"),
            "the directory below the home directory is Omp's alone"
        );

        let shared = env.with_var(PI_AGENT_DIR_VAR, "/srv/agent");
        assert_eq!(Agent::Pi.config_dir(&shared), Path::new("/srv/agent"));
        assert_eq!(
            Agent::Omp.config_dir(&shared),
            Path::new("/srv/agent"),
            "the variable for the layout is read by both agents that have it"
        );
    }

    #[test]
    fn a_base_directory_for_configuration_is_honoured_where_an_agent_uses_one() {
        let env = machine().with_var(CONFIG_HOME_VAR, "/cfg");

        assert_eq!(Agent::Devin.config_dir(&env), Path::new("/cfg/devin"));
        assert_eq!(
            Agent::Kilo.config_dir(&env),
            Path::new("/home/u/.config/kilo"),
            "an agent that documents no such thing keeps its fixed directory"
        );
    }

    #[test]
    fn an_agent_is_run_by_the_names_it_is_shipped_under() {
        assert_eq!(Agent::Cursor.commands(Platform::Unix), ["cursor-agent"]);
        assert_eq!(Agent::Antigravity.commands(Platform::Unix), ["agy"]);
        assert_eq!(Agent::GithubCopilot.commands(Platform::Unix), ["copilot"]);
        assert!(
            Agent::QoderCli
                .commands(Platform::Windows)
                .starts_with(Agent::QoderCli.commands(Platform::Unix)),
            "the names of one machine are the names of the other and then some"
        );
    }

    #[test]
    fn every_agent_runs_somewhere() {
        for agent in AGENTS {
            assert!(
                agent.runs_on(Platform::Unix) || agent.runs_on(Platform::Windows),
                "{agent} runs nowhere"
            );
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
