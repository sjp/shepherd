//! Reading a coding agent's state off its screen.
//!
//! The screen is the one signal every terminal agent produces, whether or not
//! it was built to be observed: whatever the agent is doing, it is drawing.
//! What each agent's drawing *means* is described by a manifest, so the code
//! here is agent-agnostic and the knowledge that goes stale lives in data.

pub mod region;
pub mod rules;
pub mod schema;

pub use region::ScreenInput;
pub use rules::{CompiledManifest, Detection};

/// What one screen is evidence of, according to one agent's manifest.
///
/// The manifest is compiled rather than parsed here on purpose: a consumer
/// watching a live terminal calls this several times a second, and the cost
/// that matters — building the matchers — belongs to loading the manifest, not
/// to reading a screen.
///
/// For an agent no manifest describes, the answer is
/// [`Detection::unknown_agent`] rather than anything this function could
/// return.
pub fn detect(manifest: &CompiledManifest, input: ScreenInput<'_>) -> Detection {
    manifest.detect(input)
}
