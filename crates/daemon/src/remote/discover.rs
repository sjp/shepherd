//! Endpoints a transport finds for itself.
//!
//! Some kinds of endpoint have to be named by a person, because nothing on the
//! machine knows which of them matter. Others do not: where something already
//! keeps an authoritative list of what is running here, asking it is both
//! cheaper and more accurate than asking somebody to keep a declaration up to
//! date. A transport that has such a list implements this, and everything above
//! it treats what it found exactly as it treats what was declared, except in
//! two respects — nobody asked for it, so nobody has to take it back, and it
//! stops being attached the moment the list stops mentioning it.
//!
//! # What a discovery is told
//!
//! As little as possible, and nothing that came from a subscriber. [`Context`]
//! carries the working directories this daemon's own sessions happen to have
//! reported and the declarations already made, and a discovery is free to
//! ignore both: the list it is reading is the thing that decides what exists,
//! and anything derived from what agents happen to be doing may only order or
//! name what that list already said. A discovery that could not find its
//! endpoints without being fed cwds would be one whose answer depended on who
//! was watching.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use super::docker;
use super::transport::Transport;

/// What a discovery may take into account besides its own list.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// Where this daemon's own sessions said they were working, newest first.
    ///
    /// For associating an endpoint with a project, and for deciding which of
    /// several to reach first. Never for deciding whether an endpoint exists.
    pub working: &'a [String],
    /// The words already declared for this discovery's transport.
    ///
    /// A discovery leaves an endpoint somebody has declared alone: that one is
    /// attached because it was asked for, and offering it again would mean two
    /// attachments to one place. Which of its own endpoints a set of words
    /// names is a question only the transport can answer, which is why it is
    /// asked here rather than settled by whoever calls this.
    pub declared: &'a [Vec<String>],
}

/// One endpoint a discovery found.
pub struct Found {
    /// The words that name it, as a declaration of it would have been written.
    pub args: Vec<String>,
    /// A way of reaching it.
    pub transport: Arc<dyn Transport>,
}

impl fmt::Debug for Found {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Found")
            .field("args", &self.args)
            .field("transport", &self.transport.label())
            .finish()
    }
}

/// A transport that knows where its own endpoints are.
pub trait Discovery: fmt::Debug + Send + Sync {
    /// The name a declaration of one of these endpoints would give.
    ///
    /// It ties what was found to what was declared, so that the two are one
    /// endpoint rather than two, and it is what an endpoint found this way is
    /// written down as.
    fn transport(&self) -> &'static str;

    /// How long to wait before looking again.
    ///
    /// Asked after every look rather than once, so that a discovery which has
    /// just found the thing it looks with missing can back off to a cadence
    /// suited to waiting for it to appear.
    fn every(&self) -> Duration;

    /// Everything reachable through this transport right now, or nothing at all
    /// when looking did not work.
    ///
    /// The two are different answers and must not be confused: an empty list
    /// means there is nothing there and whatever was attached should be let go
    /// of, and nothing means the question could not be asked and everything
    /// should be left exactly as it is.
    fn sweep(&self, context: &Context<'_>) -> Option<Vec<Found>>;
}

/// Every way this build has of finding endpoints without being told about them.
pub fn standard() -> Vec<Arc<dyn Discovery>> {
    vec![Arc::new(docker::Containers::resolve())]
}
