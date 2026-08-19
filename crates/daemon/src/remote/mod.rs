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
//!
//! The copy that gets sent is normally this running executable, which needs
//! nothing but a filesystem. [`Release`] is where the other copies come from:
//! the far end is often a machine this one does not contain a binary for, and
//! then the matching one is fetched from where this version was published and
//! checked against what the release said it would be before it goes anywhere.
//!
//! [`Attachment`] is what all of that is for. It subscribes to the daemon at the
//! far end, stamps every session, observation and event it reports with the hop
//! that reaches it, and merges them into this daemon's own stream — so that
//! something reading one socket on this machine is told about agents running on
//! all of them.
//!
//! # Being told which endpoints to reach
//!
//! An endpoint reached over a network is one somebody has to name, because
//! there is no register of "the machines that matter" to read one off. So they
//! are declared, in a file that outlives every daemon ([`Targets`]), and a
//! daemon keeps its attachments in step with it and writes down what came of
//! each ([`Attachments`]). [`Reconciling`] is the loop that does the keeping in
//! step, and [`Registry`] is how a name in a declaration becomes a way of
//! reaching something.
//!
//! # One endpoint, named several ways
//!
//! Nothing about a name says what is behind it. Somebody declares `fileserver`,
//! `192.168.0.42` and `fs.example.net` and means one machine; somebody else
//! declares one name that meant a different machine last week. Two answers are
//! kept apart accordingly. Before anything has been reached there is what the
//! transport can work out for nothing — [`Transport::way_in`], which says two
//! declarations obviously go to one place and never that two of them do not.
//! Once a stream is open there is the daemon's own account of itself, which is
//! authoritative and settles it: several declarations that reach one daemon
//! become one attachment listing all of them, and one stream is let go of
//! without withdrawing what it reported, because the attachment that is left is
//! reporting the same thing. What the daemon here calls itself, for whoever is
//! doing this to it, is [`crate::identity`].
//!
//! # Endpoints nobody had to name
//!
//! Where something on this machine already keeps an authoritative list of what
//! is running — Docker does — being told is unnecessary and being wrong is
//! avoidable, so the transport reads the list instead ([`Discovery`]). An
//! endpoint found that way is attached and reported exactly like a declared
//! one; the difference is only that nothing has to be written down for it to
//! happen and nothing has to be taken back for it to stop.

pub mod attach;
pub mod attachments;
pub mod bootstrap;
pub mod discover;
pub mod docker;
pub mod reconcile;
pub mod release;
pub mod ssh;
pub mod store;
pub mod targets;
pub mod transport;

#[cfg(test)]
mod loopback;
#[cfg(test)]
mod published;
#[cfg(test)]
mod tests;

pub use attach::Attachment;
pub use attachments::Attachments;
pub use bootstrap::{Bootstrap, SCRIPT, TARGET};
pub use discover::{Discovery, Found};
pub use reconcile::{Plan, Reconciling};
pub use release::Release;
pub use targets::{Target, Targets};
pub use transport::{Backoff, Platform, Registry, Running, Transport};
