//! The published wire format of the agent event bus: the event envelope, the
//! closed set of normalized event kinds, and the fold that turns a stream of
//! events into the current status of each agent session. Everything here is
//! pure data and pure functions — no sockets, no files, no clock — so the fold,
//! which is the correctness-critical part of the system, can be exercised
//! exhaustively by unit tests. Emitters, the daemon and subscribers all agree on
//! this crate and nothing else, which is what lets them be written, versioned
//! and debugged independently.
