//! Reaching a daemon that is running somewhere else.
//!
//! Another endpoint — a container, a machine across the network — runs its own
//! daemon, complete and independent, and this side subscribes to it and merges
//! what it says into the local stream. Running a full daemon over there rather
//! than reaching back across the boundary from each hook is forced rather than
//! chosen: a hook has a few milliseconds to connect and no way to report that it
//! could not, and only a socket on the same machine meets that. The cost is that
//! a daemon has to be got over there first, which is what this module is for.
//!
//! # The shape of it
//!
//! [`Transport`] is everything a way of reaching an endpoint has to provide, and
//! deliberately no more: run a command, put a file, say what kind of machine it
//! is, say how long to wait before trying again. [`Bootstrap`] uses those four
//! to establish a known-good copy of this program at the far end and start it.
//! Neither of them names a particular endpoint, which is the property worth
//! protecting: another kind of endpoint should be a new implementation of one
//! trait and nothing else.

pub mod bootstrap;
pub mod transport;

#[cfg(test)]
mod loopback;
#[cfg(test)]
mod tests;

pub use bootstrap::{Bootstrap, SCRIPT, TARGET};
pub use transport::{Backoff, Platform, Running, Transport};
