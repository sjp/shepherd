//! Every file this program writes into an agent, held to the promises all of
//! them make.
//!
//! Each agent's own tests already check the files it installs. What they cannot
//! check is that *every* agent has such a file and that nobody's was left out,
//! because a test written beside an agent is a test that exists only once
//! somebody has written it. So the list below is the list of what this build
//! installs, checked against the list of agents it installs for: an agent added
//! without a file named here fails the first test in the file, and a file named
//! here that breaks one of the promises fails the rest.
//!
//! The promises are the ones stated where the assets are declared, restated
//! here as questions that can be asked of text:
//!
//! * it says who wrote it, and which generation of this program's hooks it is;
//! * it hands the agent's events to this program, under the name of the agent
//!   that produced them;
//! * it says nothing to the session it runs in;
//! * it carries no path from the machine this build was made on — the one path
//!   in it is the mark an installation writes the binary over.

use agentbus_install::agent::AGENTS;
use agentbus_install::assets::{self, Asset};
use agentbus_install::paths::Platform;
use agentbus_install::{Agent, expected_version, sentinel, version};

/// The machines a file has to be right for. Both forms of every asset are
/// checked, because a machine that only ever runs one of them is not a reason
/// for the other to be wrong.
const PLATFORMS: [Platform; 2] = [Platform::Unix, Platform::Windows];

/// Where an installation writes the binary's own path, which is the only path
/// any of these files carries.
const BINARY_MARK: &str = "@BINARY@";

/// What a file installed into an agent is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// It is run on the agent's events and hands each one over.
    Wrapper,
    /// It hands nothing over. It is what makes the file beside it be loaded,
    /// and it is here because it is a whole file this program writes and is
    /// held to everything that does not depend on running.
    Companion,
}

/// One file an agent's installation is made of.
struct Installed {
    /// The agent whose installation it is part of.
    agent: Agent,
    /// What it is called where it is written, so that a failure names the file
    /// a reader would go and look at. Without the extension where that is only
    /// there to say which kind of machine the file is for.
    name: &'static str,
    /// The text of it, in each form a machine may need.
    asset: &'static Asset,
    /// What it is for.
    role: Role,
}

/// Every file this build installs, agent by agent.
///
/// The names are the ones the installers write, spelled again here rather than
/// borrowed from them: this is the list a person checks a machine against, and
/// a name that changed on one side and not the other is exactly the kind of
/// drift worth being told about.
const INSTALLED: &[Installed] = &[
    Installed {
        agent: Agent::Antigravity,
        name: "agentbus",
        asset: &assets::ANTIGRAVITY_WRAPPER,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Claude,
        name: "agentbus",
        asset: &assets::CLAUDE_WRAPPER,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Codex,
        name: "agentbus",
        asset: &assets::CODEX_WRAPPER,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Cursor,
        name: "agentbus",
        asset: &assets::CURSOR_WRAPPER,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Devin,
        name: "agentbus",
        asset: &assets::DEVIN_WRAPPER,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Droid,
        name: "agentbus",
        asset: &assets::DROID_WRAPPER,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::GithubCopilot,
        name: "agentbus",
        asset: &assets::GITHUB_COPILOT_WRAPPER,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Grok,
        name: "agentbus",
        asset: &assets::GROK_WRAPPER,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Hermes,
        name: "__init__.py",
        asset: &assets::HERMES_PLUGIN,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Hermes,
        name: "plugin.yaml",
        asset: &assets::HERMES_PLUGIN_MANIFEST,
        role: Role::Companion,
    },
    Installed {
        agent: Agent::Kilo,
        name: "agentbus.js",
        asset: &assets::KILO_PLUGIN,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Kimi,
        name: "agentbus",
        asset: &assets::KIMI_WRAPPER,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Mastracode,
        name: "agentbus",
        asset: &assets::MASTRACODE_WRAPPER,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Omp,
        name: "agentbus-omp.ts",
        asset: &assets::OMP_EXTENSION,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::OpenCode,
        name: "agentbus.js",
        asset: &assets::OPENCODE_PLUGIN,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::OpenCode,
        name: "agentbus-tui.js",
        asset: &assets::OPENCODE_TUI_PLUGIN,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Pi,
        name: "agentbus.ts",
        asset: &assets::PI_EXTENSION,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::QoderCli,
        name: "agentbus",
        asset: &assets::QODERCLI_WRAPPER,
        role: Role::Wrapper,
    },
    Installed {
        agent: Agent::Qwen,
        name: "agentbus",
        asset: &assets::QWEN_WRAPPER,
        role: Role::Wrapper,
    },
];

/// Every way a file could speak to the session it runs in, in each of the
/// languages these are written in.
///
/// The agent is reading its own output, and several of them read what a hook
/// prints as an instruction. This program has no instructions for anybody's
/// agent, so the only correct amount to say is none — and the check is over the
/// whole file rather than over its code, because a spelling that only appears
/// in a comment today is a spelling somebody uncomments tomorrow.
const SILENCE: [&str; 8] = [
    "console.",
    "process.stdout",
    "print(",
    "sys.stdout",
    "Write-Host",
    "Write-Output",
    "echo ",
    "printf ",
];

/// The one file allowed to break that silence, and the whole of what it may
/// say.
///
/// One agent reads a hook that says nothing as an answer of its own rather than
/// as no answer, so having nothing to add to somebody's session is itself
/// something that has to be said there. It is spelled out here so that the
/// exception stays as small as it is: two spellings of an empty object, one per
/// kind of machine, and nothing else.
const SPOKEN: (Agent, [&str; 2]) = (Agent::Antigravity, ["printf '{}\\n'", "Write-Output '{}'"]);

/// Paths that would say which machine this build was made on.
///
/// A file carrying one would be a file that works where it was built and
/// nowhere else. The binary's own path is written in when a file is installed,
/// which is a fact about the machine being installed on and is why the mark
/// below is what these files carry instead.
const MACHINE_PATHS: [&str; 4] = ["/home/", "/Users/", "/root/", "C:\\Users\\"];

/// Each of the two ways a file spells handing an event to this program.
///
/// A file passes the arguments in whichever form the thing that runs it takes
/// them — a command line for a shell to split, or a list that nothing splits at
/// all — and both say exactly the same thing: the far end is this program, and
/// it is told which agent is speaking.
fn handovers(agent: Agent) -> [String; 2] {
    [
        format!("emit --agent {agent}"),
        format!("\"emit\", \"--agent\", \"{agent}\""),
    ]
}

#[test]
fn every_agent_this_build_installs_for_has_a_file_here_that_hands_its_events_over() {
    for agent in AGENTS {
        let files: Vec<&str> = INSTALLED
            .iter()
            .filter(|installed| installed.agent == agent)
            .filter(|installed| installed.role == Role::Wrapper)
            .map(|installed| installed.name)
            .collect();

        assert!(
            !files.is_empty(),
            "nothing is listed for {agent}, so nothing here is checked for it",
        );
    }
}

#[test]
fn nothing_is_listed_here_for_an_agent_this_build_does_not_install_for() {
    for installed in INSTALLED {
        assert!(
            AGENTS.contains(&installed.agent),
            "{} is listed for {}, which is not an agent this build installs for",
            installed.name,
            installed.agent,
        );
    }
}

#[test]
fn every_installed_file_says_who_wrote_it_and_which_generation_it_is() {
    for installed in INSTALLED {
        for platform in PLATFORMS {
            let text = installed.asset.text(platform);
            let where_ = (installed.agent, installed.name, platform);

            assert!(
                sentinel::is_generated(text),
                "{where_:?} does not say on its first line who wrote it",
            );
            assert_eq!(
                version::parse_asset_version(text),
                Some(expected_version(installed.agent)),
                "{where_:?} does not say it is the generation this build writes",
            );
        }
    }
}

#[test]
fn every_wrapper_hands_the_events_to_this_program_under_the_agent_that_produced_them() {
    for installed in INSTALLED {
        if installed.role != Role::Wrapper {
            continue;
        }
        let handovers = handovers(installed.agent);
        for platform in PLATFORMS {
            let text = installed.asset.text(platform);

            assert!(
                handovers.iter().any(|handover| text.contains(handover)),
                "{:?} never runs `{}`",
                (installed.agent, installed.name, platform),
                handovers[0],
            );
        }
    }
}

#[test]
fn no_installed_file_says_anything_to_the_session_it_runs_in() {
    let (allowed_agent, allowed) = SPOKEN;

    for installed in INSTALLED {
        for platform in PLATFORMS {
            let text = installed.asset.text(platform);
            for spelling in SILENCE {
                // The one file that has something to say is left with exactly
                // what it is allowed to say and nothing else, so that a second
                // sentence added to it would be caught here.
                let said = match installed.agent == allowed_agent {
                    true => allowed
                        .iter()
                        .fold(text.to_owned(), |text, spoken| text.replacen(spoken, "", 1)),
                    false => text.to_owned(),
                };

                assert!(
                    !said.contains(spelling),
                    "{:?} says `{spelling}` in the session it runs in",
                    (installed.agent, installed.name, platform),
                );
            }
        }
    }
}

#[test]
fn no_installed_file_carries_a_path_from_the_machine_it_was_built_on() {
    for installed in INSTALLED {
        for platform in PLATFORMS {
            let text = installed.asset.text(platform);
            let where_ = (installed.agent, installed.name, platform);

            for path in MACHINE_PATHS {
                assert!(
                    !text.contains(path),
                    "{where_:?} carries `{path}`, which is a path from somebody's machine",
                );
            }
            assert!(
                !text.contains(env!("CARGO_MANIFEST_DIR")),
                "{where_:?} carries the directory this build was made in",
            );
            // The place a real path goes, which is filled in against the
            // machine being installed on. A wrapper without it would be one
            // that names the binary some other way, and the only other ways are
            // a bare command that may not be on the agent's search path and a
            // path from this machine.
            if installed.role == Role::Wrapper {
                assert!(
                    text.contains(BINARY_MARK),
                    "{where_:?} has nowhere for the binary's path to be written",
                );
            }
        }
    }
}
