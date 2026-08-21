//! What one agent's hooks are on a machine, and whether that is what this build
//! would put there.
//!
//! The question is answered from the files themselves rather than from this
//! program's own record of what it wrote. The file is what the agent actually
//! runs, a user can read it without this program's help, and a record that
//! disagreed with it would be a confident answer about the wrong machine — one
//! restored from a backup, one whose configuration directory was copied from
//! somewhere else, one somebody has edited.
//!
//! Two things are read for each agent. The mark inside the file it installed
//! says which generation of the hooks that file is, and comparing it with what
//! this build writes is the whole of "current" and "behind". But a file is only
//! half of an installation for most agents: the other half is the entry
//! somewhere else that points at it, and a wrapper nothing calls is a wrapper
//! that never runs. So an agent may look at the rest of its own installation
//! and say that what is there is not working, which is a different answer from
//! either of the first two and needs a different sentence said about it.

use std::path::Path;

use crate::agent::{Agent, DetectedAgent, detect};
use crate::paths::Environment;
use crate::{Error, file, version};

/// What one agent's hooks are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookStatus {
    /// Nothing of this program's is there.
    NotInstalled,
    /// What is there is at least what this build writes.
    Current(u32),
    /// What is there is older than what this build writes, or says nothing
    /// about which generation it is — which is what everything installed
    /// before this program marked its work looks like.
    Outdated { found: Option<u32>, expected: u32 },
    /// The file is current, and something else the installation is made of is
    /// missing or has been changed, so the hooks do not run.
    NeedsRepair(u32),
}

impl HookStatus {
    /// What the file at `path` says `agent`'s hooks are.
    ///
    /// A file that is not there means nothing is installed. Everything else is
    /// the mark inside it, or the absence of one.
    pub fn of_asset(agent: Agent, path: &Path) -> Result<Self, Error> {
        let text = file::read(path).map_err(|source| Error::Read {
            path: path.to_owned(),
            source,
        })?;
        Ok(match text {
            Some(text) => Self::of_text(agent, &text),
            None => Self::NotInstalled,
        })
    }

    /// The same reading, of a file already in hand.
    ///
    /// A generation newer than this build's counts as current rather than as a
    /// disagreement: a machine somebody has already upgraded is not one an older
    /// build should be talking into installing over it.
    pub fn of_text(agent: Agent, text: &str) -> Self {
        let expected = version::expected_version(agent);
        match version::parse_asset_version(text) {
            Some(found) if found >= expected => Self::Current(found),
            found => Self::Outdated { found, expected },
        }
    }

    /// The same reading, with the rest of the installation taken into account.
    ///
    /// Only a current file can be downgraded by this. A file that is behind is
    /// already going to be written again, and everything around it with it, so
    /// saying that the rest of it is also wrong would be telling a user about a
    /// second problem with the same one fix.
    pub fn confirmed(self, intact: bool) -> Self {
        match (self, intact) {
            (Self::Current(found), false) => Self::NeedsRepair(found),
            (status, _) => status,
        }
    }
}

/// What this build has to say about one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    /// The agent this is about.
    pub agent: Agent,
    /// What was found of it on the machine, if anything.
    pub detected: Option<DetectedAgent>,
    /// What its hooks are.
    pub status: HookStatus,
}

impl Recommendation {
    /// Whether installing for this agent now would achieve something.
    ///
    /// Three things have to be true, and each of them is a different thing to
    /// tell a user who asked why their agent is not on the list: the agent is on
    /// the machine, this build knows how to install for it, and what is there is
    /// not already what this build writes.
    pub fn needs_install(&self) -> bool {
        self.detected.is_some()
            && crate::supported().contains(&self.agent)
            && !matches!(self.status, HookStatus::Current(_))
    }
}

/// What this build has to say about every agent it knows, on `env`.
///
/// Every agent, including the ones that are not on the machine and the ones this
/// build cannot install for yet: somebody asking about the state of their hooks
/// is entitled to the answer "that one is not here" as much as to the answer
/// "that one is out of date", and a report that silently left out what it had
/// nothing to say about would be a report that could not be trusted to be
/// complete.
pub fn recommendations(env: &Environment) -> Result<Vec<Recommendation>, Error> {
    let found = detect(env);
    let installers = crate::installers();
    crate::agent::AGENTS
        .into_iter()
        .map(|agent| {
            let installer = installers.iter().find(|one| one.agent() == agent);
            let status = match installer {
                Some(installer) => installer.status(env)?,
                None => HookStatus::NotInstalled,
            };
            Ok(Recommendation {
                agent,
                detected: found.iter().find(|one| one.agent == agent).cloned(),
                status,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::agent::AGENTS;

    /// A file saying it is generation `version` of `agent`'s hooks.
    fn marked(dir: &Path, agent: Agent, version: u32) -> std::path::PathBuf {
        let path = dir.join(format!("{agent}-hook.sh"));
        fs::write(
            &path,
            format!("# _agentbus\n# {}{version}\n\nexit 0\n", version::MARKER),
        )
        .expect("cannot write the file the test is about");
        path
    }

    #[test]
    fn a_file_that_is_not_there_is_nothing_installed() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            HookStatus::of_asset(Agent::Claude, &dir.path().join("absent.sh")).unwrap(),
            HookStatus::NotInstalled
        );
    }

    #[test]
    fn a_file_of_this_builds_generation_is_current() {
        let dir = tempfile::tempdir().unwrap();
        let agent = Agent::Codex;
        let expected = version::expected_version(agent);
        let path = marked(dir.path(), agent, expected);

        assert_eq!(
            HookStatus::of_asset(agent, &path).unwrap(),
            HookStatus::Current(expected)
        );
    }

    #[test]
    fn a_file_of_a_later_generation_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let agent = Agent::Codex;
        let ahead = version::expected_version(agent) + 1;
        let path = marked(dir.path(), agent, ahead);

        assert_eq!(
            HookStatus::of_asset(agent, &path).unwrap(),
            HookStatus::Current(ahead)
        );
    }

    #[test]
    fn a_file_of_an_earlier_generation_is_behind() {
        let dir = tempfile::tempdir().unwrap();
        let agent = Agent::Codex;
        let expected = version::expected_version(agent);
        let path = marked(dir.path(), agent, 0);

        assert_eq!(
            HookStatus::of_asset(agent, &path).unwrap(),
            HookStatus::Outdated {
                found: Some(0),
                expected
            }
        );
    }

    #[test]
    fn a_file_that_says_nothing_about_itself_is_behind_too() {
        let dir = tempfile::tempdir().unwrap();
        let agent = Agent::Claude;
        let path = dir.path().join("hooks.json");
        fs::write(&path, "{\n  \"hooks\": {}\n}\n").unwrap();

        assert_eq!(
            HookStatus::of_asset(agent, &path).unwrap(),
            HookStatus::Outdated {
                found: None,
                expected: version::expected_version(agent)
            },
            "an installation from before this program marked its work"
        );
    }

    #[test]
    fn a_current_file_nothing_points_at_needs_repairing() {
        let dir = tempfile::tempdir().unwrap();
        let agent = Agent::Codex;
        let expected = version::expected_version(agent);
        let path = marked(dir.path(), agent, expected);

        let status = HookStatus::of_asset(agent, &path).unwrap();

        assert_eq!(status.confirmed(true), HookStatus::Current(expected));
        assert_eq!(status.confirmed(false), HookStatus::NeedsRepair(expected));
    }

    #[test]
    fn nothing_but_a_current_file_is_downgraded_by_the_rest_of_it() {
        let behind = HookStatus::Outdated {
            found: None,
            expected: 1,
        };

        assert_eq!(behind.confirmed(false), behind);
        assert_eq!(
            HookStatus::NotInstalled.confirmed(false),
            HookStatus::NotInstalled
        );
    }

    #[test]
    fn every_agent_is_reported_on_whether_or_not_it_is_here() {
        let home = tempfile::tempdir().unwrap();
        let env = Environment::rooted(home.path());

        let recommendations = recommendations(&env).unwrap();

        let reported: Vec<Agent> = recommendations.iter().map(|one| one.agent).collect();
        assert_eq!(reported, AGENTS.to_vec());
        for recommendation in &recommendations {
            assert_eq!(recommendation.detected, None);
            assert!(
                !recommendation.needs_install(),
                "{} is not on this machine",
                recommendation.agent
            );
        }
    }

    #[test]
    fn an_agent_that_is_here_and_has_no_hooks_is_worth_installing_for() {
        let home = tempfile::tempdir().unwrap();
        let agent = Agent::OpenCode;
        fs::create_dir_all(agent.config_dir(&Environment::rooted(home.path()))).unwrap();
        let env = Environment::rooted(home.path());

        let recommendations = recommendations(&env).unwrap();
        let opencode = recommendations
            .iter()
            .find(|one| one.agent == agent)
            .expect("every agent is reported on");

        assert!(opencode.detected.is_some());
        assert_eq!(opencode.status, HookStatus::NotInstalled);
        assert!(opencode.needs_install());
    }

    #[test]
    fn an_agent_this_build_cannot_install_for_is_reported_and_not_offered() {
        let supported = crate::supported();
        let Some(agent) = AGENTS.into_iter().find(|agent| !supported.contains(agent)) else {
            return; // A build that installs for every agent it knows has no such case.
        };
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(agent.config_dir(&Environment::rooted(home.path()))).unwrap();
        let env = Environment::rooted(home.path());

        let recommendations = recommendations(&env).unwrap();
        let unhandled = recommendations
            .iter()
            .find(|one| one.agent == agent)
            .expect("every agent is reported on");

        assert!(unhandled.detected.is_some(), "its directory is right there");
        assert!(
            !unhandled.needs_install(),
            "nothing can be installed for {agent} yet, so nothing is offered"
        );
    }
}
