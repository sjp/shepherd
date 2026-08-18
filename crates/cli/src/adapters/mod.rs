//! Turning one agent's hook payload into the bus's envelope.
//!
//! There is one module per agent, and each is a single pure function from a
//! `serde_json::Value` to an optional unstamped event. Purity is the point:
//! these functions run inside somebody's coding agent, on the emit path, where
//! the budget is a few milliseconds and a panic is a bug in their editor. Having
//! no I/O to do — no clock, no environment, no socket — is what makes them
//! testable by replaying captured payloads, and what keeps the risky part of
//! the emit path (deciding what an agent's event *means*) separate from the
//! part that has to survive contact with the filesystem.
//!
//! A payload that produces no event yields `None` rather than an error. Agents
//! emit far more than the bus normalizes, and "nothing to say about this one" is
//! the ordinary case, not a failure.

pub mod claude;
