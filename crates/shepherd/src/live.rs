//! The half of the window that is fed from outside the process.
//!
//! Everything the window knows about agents comes from one place: a stream
//! published by the event bus's daemon, read on a thread of its own, folded into
//! what is currently true, and joined against the shells this application has
//! open. All four of those are somebody else's code — this is what turns them,
//! on the window's own timer, and the answer it keeps between turns.
//!
//! # Nothing was asked of the bus to make this work
//!
//! A shell is started with the bus's environment variable set to a string of
//! this application's own devising; an agent started in that shell inherits it;
//! the bus copies it onto everything it reports about that agent without ever
//! being told what is in it; and the join back to a shell happens here. That is
//! the whole of the integration, and it is worth stating plainly because it is
//! the property the arrangement was designed for: the bus has no idea any of
//! this exists, and would behave identically if it did not.

use std::time::Instant;

use shepherd_core::bus::{SocketPaths, now};
use shepherd_core::daemon::{Host, Lifecycle, Presence};
use shepherd_core::{
    Attribution, BusState, ShellAddress, Subscriber, SubscriberHandle, Workspace, status_source,
};
use tracing::{info, warn};

/// The bus, as far as one window is concerned.
///
/// It holds the connection, the daemon behind it, the state that has been folded
/// out of the stream, and the placement of every session among the shells that
/// are open. [`Live::poll`] moves all of it forward by however much has happened
/// since it was last called.
#[derive(Debug)]
pub struct Live {
    updates: SubscriberHandle,
    lifecycle: Lifecycle,
    host: Host,
    state: BusState,
    attribution: Attribution,
}

impl Live {
    /// Starts reading whichever bus this environment names, and starts a daemon
    /// for it if nobody else has.
    pub fn new() -> Self {
        let paths = SocketPaths::resolve();
        info!(dir = %paths.dir().display(), "reading the bus");
        Self {
            updates: Subscriber::at(paths.clone()).spawn(),
            lifecycle: Lifecycle::new(paths),
            host: Host::from_env(),
            state: BusState::new(),
            attribution: Attribution::default(),
        }
    }

    /// Takes in everything the bus has said since the last look, and works out
    /// where all of it is running among `workspaces`.
    ///
    /// Answers whether any of that changed what a window would draw, so a caller
    /// can call this as often as it likes and redraw only when there is
    /// something different to show.
    pub fn poll(&mut self, workspaces: &[Workspace]) -> bool {
        // Taken before anything is applied, because the first update of a
        // stream is what turns "looking for a bus" into "reading one" and that
        // is the transition most worth noticing.
        let was = self.lifecycle.presence().clone();
        for update in self.updates.updates().try_iter() {
            self.lifecycle.heard(&update, Instant::now());
            self.state.apply(&update, &now());
        }
        // Both are due whether or not anything arrived: one is what starts a
        // daemon where there is none, and the other is what makes a session
        // nobody has heard from go quiet.
        self.lifecycle.tick(&mut self.host, Instant::now());
        self.state.tick(&now());

        // Derived from what is currently true rather than patched, every time.
        // It is a walk over the shells that are open and the sessions that
        // exist, both counted in tens, and it is the only way a reconnection
        // cannot leave a stale placement behind.
        let attribution = Attribution::derive(workspaces, self.state.sessions());
        let moved = attribution != self.attribution;
        if moved {
            let before = std::mem::replace(&mut self.attribution, attribution);
            self.report(&before);
        }
        let presence = was != *self.lifecycle.presence();
        if presence {
            self.announce();
        }
        moved || presence
    }

    /// What is known about the bus.
    pub fn presence(&self) -> &Presence {
        self.lifecycle.presence()
    }

    /// Where everything the bus is reporting is running, as of the last look.
    pub fn attribution(&self) -> &Attribution {
        &self.attribution
    }

    /// Says what is now running in each shell that changed.
    ///
    /// Only the shells that changed: a session appearing in one moves the whole
    /// attribution, and saying every shell over again each time one did would
    /// bury the line that matters. Each is written out in full — which agent,
    /// which session, what it is doing and on whose word — because this is the
    /// record that says the join worked, and a count would not be.
    fn report(&self, before: &Attribution) {
        for shell in self.changed(before) {
            let sessions = self.attribution.sessions_at(shell);
            if sessions.is_empty() {
                info!(shell = %shell.shell, "nothing is running in this shell");
                continue;
            }
            for session in sessions {
                info!(
                    shell = %shell.shell,
                    agent = %session.agent,
                    session = %session.session,
                    status = %session.status,
                    source = %status_source(session),
                    correlation = session.correlation.as_deref().unwrap_or("none"),
                    "an agent session is attributed to this shell"
                );
            }
        }
    }

    /// The shells whose sessions are not what they were.
    ///
    /// Both sides are walked, because a shell that has just had its last
    /// session taken away is absent from one of them and is exactly the change
    /// worth reporting.
    fn changed(&self, before: &Attribution) -> Vec<ShellAddress> {
        let mut changed: Vec<ShellAddress> = self
            .attribution
            .shells()
            .chain(before.shells())
            .map(|(shell, _)| shell)
            .filter(|shell| before.sessions_at(*shell) != self.attribution.sessions_at(*shell))
            .collect();
        changed.sort_unstable();
        changed.dedup();
        changed
    }

    /// Says what became of the bus, when that changes.
    fn announce(&self) {
        match self.lifecycle.presence() {
            Presence::Waiting => info!("waiting for the bus"),
            Presence::Running => info!("the bus is serving"),
            Presence::Lost => warn!("the bus stopped serving"),
            Presence::Unavailable(why) => warn!(%why, "there is no bus to read"),
        }
    }
}

#[cfg(test)]
impl Live {
    /// Takes one update in as though it had come off the stream.
    ///
    /// For a test that needs the bus to have said something without a daemon
    /// behind it saying it: everything after this point — the fold, the
    /// attribution, the badges — is the same code an update off a real socket
    /// goes through.
    pub fn heard(&mut self, update: &shepherd_core::Update, workspaces: &[Workspace]) {
        self.state.apply(update, &now());
        self.attribution = Attribution::derive(workspaces, self.state.sessions());
    }
}

/// What the window says about the bus in one line.
pub fn described(presence: &Presence) -> String {
    match presence {
        Presence::Waiting => "bus: looking for one".to_owned(),
        Presence::Running => "bus: serving".to_owned(),
        Presence::Lost => "bus: lost".to_owned(),
        Presence::Unavailable(why) => format!("bus: {why}"),
    }
}
