//! The hook and plugin templates that are written into the agents.
//!
//! Each is compiled in with `include_str!` from the crate's `assets` directory,
//! rather than read from disk at install time, because the same binary is copied
//! onto machines that have no checkout of this source and no directory to read
//! from. A template that could go missing between building and installing would
//! be a hook installation that fails on exactly the machines it is hardest to
//! debug on.
//!
//! One constant per file, grouped by the agent that installs it.

/// The manifest of the marketplace Claude Code installs the plugin from.
pub const CLAUDE_MARKETPLACE: &str =
    include_str!("../assets/claude-marketplace/.claude-plugin/marketplace.json");

/// The manifest of the plugin that marketplace offers.
pub const CLAUDE_PLUGIN: &str =
    include_str!("../assets/claude-marketplace/agentbus/.claude-plugin/plugin.json");

/// The hooks that plugin registers.
pub const CLAUDE_HOOKS: &str =
    include_str!("../assets/claude-marketplace/agentbus/hooks/hooks.json");

/// The plugin OpenCode is given, as a single file dropped into its plugin
/// directory.
pub const OPENCODE_PLUGIN: &str = include_str!("../assets/opencode/agentbus.js");
