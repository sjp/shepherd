//! The files this program writes into the agents, and the contract they are
//! written to.
//!
//! Each is compiled in with `include_str!` from the crate's `assets` directory,
//! one directory per agent, rather than read from disk at install time: the same
//! binary is copied onto machines that have no checkout of this source and no
//! directory to read from, and a template that could go missing between building
//! and installing would be an installation that fails on exactly the machines it
//! is hardest to debug on.
//!
//! Where an agent needs a different file on each kind of machine, both are kept
//! side by side and the one to write is chosen when the installation is planned,
//! not when this program is compiled — the plan already knows which machine it
//! is for, and a build that could only install for the machine it was built on
//! could not describe any other.
//!
//! # What a wrapper does
//!
//! Almost everything here is a *wrapper*: a small script an agent runs on each
//! of its events, whose whole job is to hand that event to `agentbus emit
//! --agent <name>` and get out of the way. Nothing here decides what an event
//! means. That lives in the mappings the binary carries, so a wrapper never has
//! to be upgraded in step with anything: it forwards what it was given, exactly
//! as it was given it, and forgets about it.
//!
//! Most agents deliver the event as JSON on the wrapper's standard input, and
//! the wrapper passes that through untouched — which is what lets the mapping
//! for that agent be written against the agent's own documented payload rather
//! than against something this program invented. The agents whose plugin
//! interfaces hand a script an event object and no standard input have nothing
//! to forward, so those wrappers compose a small object of their own; what is in
//! it is written down beside the wrapper, because a mapping is written against
//! it.
//!
//! # What a wrapper may not do
//!
//! A wrapper runs inside somebody's coding session, on every event their agent
//! produces, and is therefore held to what the emit path itself is held to:
//!
//! - It never writes to the standard output the agent is reading. Several agents
//!   interpret what a hook prints as an instruction, and this program has no
//!   instructions for anybody's agent.
//! - It always reports success. A failure raised here is a fault the user did
//!   not ask for, in a session that had nothing to do with this program.
//! - It never keeps the agent waiting beyond the moment the event is handed
//!   over.
//! - It does nothing at all when the binary it names is not there. An
//!   installation left behind by a binary that has since been removed is inert
//!   rather than broken, which is the same thing that happens on a machine where
//!   nothing is listening.
//!
//! # What a wrapper says about itself
//!
//! Two marks go in the opening comment of every one of them. The first line
//! carries the mark that says this program wrote the file, so that installing
//! and uninstalling can tell it from a file of the user's own that happens to
//! share a name — see [`crate::sentinel`]. Among the lines below it goes the
//! generation the file is, so that a machine can be asked whether what it
//! carries is what this build writes — see [`crate::version`].
//!
//! Neither mark has to share the first line with anything, because none of these
//! files begins by naming the program that runs it: the entry written into the
//! agent names the interpreter, so what is on disk is only ever the script.

use crate::paths::Platform;

/// One file an installer writes, in each of the forms a machine may need it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asset {
    unix: &'static str,
    windows: &'static str,
}

impl Asset {
    /// An asset written one way on one kind of machine and another way on the
    /// other, because the two do not run the same kind of script.
    pub const fn pair(unix: &'static str, windows: &'static str) -> Self {
        Self { unix, windows }
    }

    /// An asset whose one form runs on either kind of machine, because what
    /// runs it is an interpreter the agent brings with it rather than the
    /// machine's own shell.
    pub const fn portable(text: &'static str) -> Self {
        Self {
            unix: text,
            windows: text,
        }
    }

    /// The form written on a machine of `platform`.
    pub const fn text(&self, platform: Platform) -> &'static str {
        match platform {
            Platform::Unix => self.unix,
            Platform::Windows => self.windows,
        }
    }
}

/// The wrapper Claude Code runs on each of its events.
pub const CLAUDE_WRAPPER: Asset = Asset::pair(
    include_str!("../assets/claude/agentbus.sh"),
    include_str!("../assets/claude/agentbus.ps1"),
);

/// The wrapper Codex runs on each of its events.
pub const CODEX_WRAPPER: Asset = Asset::pair(
    include_str!("../assets/codex/agentbus.sh"),
    include_str!("../assets/codex/agentbus.ps1"),
);

/// The plugin OpenCode is given, as a single file dropped into its plugin
/// directory.
pub const OPENCODE_PLUGIN: &str = include_str!("../assets/opencode/agentbus.js");

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::sentinel;
    use crate::version;

    /// The machines an asset has to be right for.
    const PLATFORMS: [Platform; 2] = [Platform::Unix, Platform::Windows];

    /// Which of the rules every wrapper obeys `text` breaks, for `agent`.
    ///
    /// Held apart from the assertion below so that the rules themselves can be
    /// tested against text written to break them, which is the only way to know
    /// that a rule is being checked rather than merely stated.
    pub(crate) fn faults(agent: Agent, text: &str) -> Vec<String> {
        let mut faults = Vec::new();
        let expected = version::expected_version(agent);
        if version::parse_asset_version(text) != Some(expected) {
            faults.push(format!(
                "says it is generation {:?} where this build writes {expected}",
                version::parse_asset_version(text)
            ));
        }
        if !sentinel::is_generated(text) {
            faults.push("does not say on its first line who wrote it".to_owned());
        }
        let handover = handover(agent);
        if !text.contains(&handover) {
            faults.push(format!("never runs `{handover}`"));
        }
        faults
    }

    /// Fails unless `asset` obeys them, in each form it may be written in.
    ///
    /// Every agent's own tests call this for every file that agent installs, so
    /// that the rules are checked where the file is rather than restated
    /// wherever one is added.
    pub(crate) fn is_well_formed(agent: Agent, asset: &Asset) {
        for platform in PLATFORMS {
            let faults = faults(agent, asset.text(platform));
            assert!(faults.is_empty(), "{agent} on {platform:?}: {faults:?}");
        }
    }

    /// What the far end of every wrapper is: this program, told which agent is
    /// speaking.
    fn handover(agent: Agent) -> String {
        format!("emit --agent {agent}")
    }

    /// A wrapper that obeys every rule, as a starting point for text that does
    /// not.
    fn well_formed(agent: Agent) -> String {
        format!(
            "# {} — written by this program\n\
             # AGENTBUS_HOOK_VERSION={}\n\
             \n\
             exec agentbus {} </dev/null\n",
            sentinel::KEY,
            version::expected_version(agent),
            handover(agent),
        )
    }

    #[test]
    fn a_wrapper_that_obeys_the_rules_breaks_none_of_them() {
        for agent in crate::agent::AGENTS {
            assert!(faults(agent, &well_formed(agent)).is_empty(), "{agent}");
        }
    }

    #[test]
    fn a_wrapper_of_the_wrong_generation_is_caught() {
        let agent = Agent::Codex;
        let behind = well_formed(agent).replace(
            &format!("AGENTBUS_HOOK_VERSION={}", version::expected_version(agent)),
            "AGENTBUS_HOOK_VERSION=0",
        );

        assert_eq!(faults(agent, &behind).len(), 1, "{behind}");
    }

    #[test]
    fn a_wrapper_that_does_not_name_itself_is_caught() {
        let agent = Agent::Codex;
        let anonymous = well_formed(agent).replace(sentinel::KEY, "something else");

        assert_eq!(faults(agent, &anonymous).len(), 1, "{anonymous}");
    }

    #[test]
    fn a_wrapper_that_hands_the_event_to_something_else_is_caught() {
        let agent = Agent::Codex;
        let astray = well_formed(agent).replace(&handover(agent), "emit --agent somebody-else");

        assert_eq!(faults(agent, &astray).len(), 1, "{astray}");
    }

    #[test]
    fn both_forms_of_an_asset_are_checked() {
        let agent = Agent::Codex;
        // An asset is text compiled into the program, so text made up for this
        // has to last as long; the test process ending is what frees it.
        let good: &'static str = well_formed(agent).leak();

        is_well_formed(agent, &Asset::portable(good));
        assert_eq!(Asset::pair(good, "").text(Platform::Unix), good);
        assert_eq!(Asset::pair("", good).text(Platform::Windows), good);
    }
}
