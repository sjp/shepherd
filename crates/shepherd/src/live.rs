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
    Attribution, BusState, ShellAddress, ShellStatus, Subscriber, SubscriberHandle, Workspace,
    status_source,
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
    /// The shell this window is showing, so that a session arriving in it can be
    /// said out loud. With no sidebar to draw a badge on, saying it is how it is
    /// seen.
    watching: ShellAddress,
}

impl Live {
    /// Starts reading whichever bus this environment names, and starts a daemon
    /// for it if nobody else has.
    pub fn watching(shell: ShellAddress) -> Self {
        let paths = SocketPaths::resolve();
        info!(dir = %paths.dir().display(), "reading the bus");
        Self {
            updates: Subscriber::at(paths.clone()).spawn(),
            lifecycle: Lifecycle::new(paths),
            host: Host::from_env(),
            state: BusState::new(),
            attribution: Attribution::default(),
            watching: shell,
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
            // Said only when it is *this* shell that changed. A session
            // appearing somewhere else moves the attribution and is nothing to
            // do with the shell being watched, and saying so again every time
            // one did would bury the line that matters.
            let said = self.attribution.sessions_at(self.watching)
                != attribution.sessions_at(self.watching);
            self.attribution = attribution;
            if said {
                self.report();
            }
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

    /// What one shell's badge would say.
    pub fn status_at(&self, shell: ShellAddress) -> ShellStatus {
        self.attribution.status_at(shell)
    }

    /// How many sessions the bus is reporting that are not in any shell here.
    pub fn elsewhere(&self) -> usize {
        self.attribution.elsewhere().len()
    }

    /// Says what is now running in the shell being watched.
    ///
    /// Written out in full — which agent, which session, what it is doing and on
    /// whose word — because this is the record that says the join worked, and a
    /// count would not be.
    fn report(&self) {
        let sessions = self.attribution.sessions_at(self.watching);
        if sessions.is_empty() {
            info!(shell = %self.watching.shell, "nothing is running in this shell");
            return;
        }
        for session in sessions {
            info!(
                shell = %self.watching.shell,
                agent = %session.agent,
                session = %session.session,
                status = %session.status,
                source = %status_source(session),
                correlation = session.correlation.as_deref().unwrap_or("none"),
                "an agent session is attributed to this shell"
            );
        }
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

/// What the window says about the bus in one line.
pub fn described(presence: &Presence) -> String {
    match presence {
        Presence::Waiting => "bus: looking for one".to_owned(),
        Presence::Running => "bus: serving".to_owned(),
        Presence::Lost => "bus: lost".to_owned(),
        Presence::Unavailable(why) => format!("bus: {why}"),
    }
}

/// What one shell's badge says in one line, given no room to draw a badge.
pub fn badged(status: ShellStatus) -> String {
    match status.status.session() {
        None => "no agent here".to_owned(),
        Some(session) => match status.source {
            Some(source) => format!("{session} ({source})"),
            None => session.to_string(),
        },
    }
}
