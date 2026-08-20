//! Turning a payload from outside into the bus's envelope.
//!
//! What a coding agent's hook payload means is data: a mapping manifest names
//! the agent's events and says which of the bus's kinds each one is, and the
//! engine that reads those manifests answers for every agent at once. There is
//! nothing per-agent left to compile, so nothing per-agent is left here.
//!
//! What remains is [`observed`], the other shape of payload this client accepts
//! — fields already in the bus's own vocabulary, sent by a program that watched
//! a terminal and formed an opinion about it. That one is not data because it
//! is not somebody else's schema: it is the bus's own, and a manifest
//! describing it would describe this crate to itself.
//!
//! Either way the job is a pure function from a `serde_json::Value` to an
//! optional unstamped event. Purity is the point: this runs inside somebody's
//! coding agent, on the emit path, where the budget is a few milliseconds and a
//! panic is a bug in their editor. Having no I/O to do — no clock, no
//! environment, no socket — is what makes it testable by replaying captured
//! payloads, and what keeps the risky part of the emit path (deciding what an
//! event *means*) separate from the part that has to survive contact with the
//! filesystem.
//!
//! A payload that produces no event yields `None` rather than an error. Agents
//! emit far more than the bus normalizes, and "nothing to say about this one" is
//! the ordinary case, not a failure.

pub mod observed;
