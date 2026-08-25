//! Reading the event bus, and holding on to what it said.
//!
//! The bus publishes a stream: a snapshot of everything it currently knows,
//! then a line for every event, observation and claim as it happens, and a
//! heartbeat whenever it would otherwise be silent. [`Subscriber`] is the half
//! that reads it — one connection, kept up, reconnected whenever the stream
//! stops being trustworthy — and [`BusState`] is the half that remembers what
//! arrived, so that a caller asks "what are my agents doing" rather than
//! reimplementing the protocol's rules at every place that wants an answer.
//!
//! ```no_run
//! use shepherd_core::bus::{BusState, Subscriber, now};
//!
//! let bus = Subscriber::resolve().spawn();
//! let mut state = BusState::new();
//! while let Ok(update) = bus.updates().recv() {
//!     state.apply(&update, &now());
//!     for session in state.sessions() {
//!         println!("{} {} {}", session.agent, session.session, session.status);
//!     }
//! }
//! ```
//!
//! # Reconnecting is the whole of the recovery story
//!
//! Every way this can lose track of the bus has the same remedy: drop the
//! connection and open another, because the next stream begins with a snapshot
//! of current state. That is true of a daemon that went away, one that dropped
//! us for reading too slowly, a gap in the sequence numbers, and a stream that
//! has gone quiet for longer than the heartbeat allows. So there is one loop
//! here and it reconnects; there is no repair path, no catch-up request, and
//! nothing that tries to reconstruct what it missed.
//!
//! The other half of that bargain is that a fresh snapshot **supersedes**
//! everything held before it. A session missing from the new snapshot is a
//! session that is over, not one to keep because it was there a moment ago —
//! merging the two would keep exactly the rows the bus is no longer willing to
//! vouch for.
//!
//! # Where the work happens
//!
//! [`Subscriber::spawn`] takes a thread and blocks on a socket in it. The bus's
//! own daemon is asynchronous because it serves many connections at once; this
//! reads one, and an asynchronous runtime would only make that slower to start
//! and harder to shut down. What comes back is an ordinary channel, which a
//! caller can drain from whatever loop it already has.

pub mod state;
pub mod subscriber;

#[cfg(test)]
mod tests;

pub use agentbus_paths::SocketPaths;
pub use agentbus_protocol::Timestamp;
pub use state::BusState;
pub use subscriber::{
    DEFAULT_FIRST_RETRY, DEFAULT_MAX_RETRY, DEFAULT_QUEUE, DEFAULT_SILENCE, Subscriber,
    SubscriberHandle, Update,
};

/// The current time, for the caller that has to say when it heard something.
///
/// [`BusState`] is told the time rather than reading it, so that what it does
/// with a quiet session is decided by a test rather than by how long the test
/// took to run. This is where an application that is not a test gets the answer,
/// and it is the bus's own clock rather than a second one that could disagree
/// with the timestamps on the stream.
pub fn now() -> Timestamp {
    Timestamp::now()
}
