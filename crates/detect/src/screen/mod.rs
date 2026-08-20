//! Reading a coding agent's state off its screen.
//!
//! The screen is the one signal every terminal agent produces, whether or not
//! it was built to be observed: whatever the agent is doing, it is drawing.
//! What each agent's drawing *means* is described by a manifest, so the code
//! here is agent-agnostic and the knowledge that goes stale lives in data.

pub mod region;
pub mod schema;
