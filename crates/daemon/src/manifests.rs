//! Keeping this machine's detection manifests up to date.
//!
//! Detection runs on data, and the point of data is that a change to an agent's
//! interface can be answered by publishing a file rather than by cutting a
//! release. Somebody still has to fetch the file, and on a machine nobody is
//! sitting at, the daemon is the only thing here that runs for long enough to
//! notice that one was published. So it carries the timer: a check shortly
//! after it starts, and a check every half hour after that.
//!
//! # It fetches; it does not read
//!
//! Nothing in this crate consults a manifest, and this module does not change
//! that. What it does is write newer copies into the tier the manifest store
//! reads fetched copies from, and there is nothing in this process to tell
//! about them afterwards: the commands that read manifests are one-shot and
//! open a store per invocation, and a long-lived program embedding the store
//! decides for itself when to reload. A daemon that has just taken a newer
//! manifest is therefore finished with it.
//!
//! # Why a thread
//!
//! One thread, sleeping between checks. A check is a handful of blocking
//! requests whose timeouts are measured in seconds, plus a few files written,
//! and a runtime worker parked in that is a worker not accepting the connection
//! a hook is waiting on — the same reasoning that keeps the process table and
//! the endpoint reconciler off the workers. A timer is also not a reason to
//! grow the async machinery: what is wanted here is a sleep and a stop.
//!
//! # Nothing here can reach the bus
//!
//! The thread shares no state with the sockets, the session table or the clock.
//! A catalog that cannot be reached, a manifest that is refused, a status file
//! that cannot be written: every one of those is reported by the check itself,
//! is swallowed here, and is tried again at the next look. There is no failure
//! in this module that a subscriber or an emitter could observe.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use agentbus_detect::{CheckResult, ItemResult, ManifestStore, StorePaths, catalog_url, update};
use tracing::{info, warn};

/// How long after starting the first check runs.
///
/// Long enough that a fleet restarted together does not arrive at the catalog
/// as one crowd, and short enough that a machine which has just been given a
/// daemon is reading screens with what is published rather than with what its
/// binary was built with.
pub const FIRST_CHECK_AFTER: Duration = Duration::from_secs(60);

/// How long between checks after the first.
///
/// A manifest is published when an interface changed, which is an event of the
/// scale of days; half an hour is far finer than that and is one small request
/// per machine per half hour, most of them answered with "what you have is
/// what is published".
pub const INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Where a daemon keeps its manifests, where newer ones are published, and how
/// often it looks.
///
/// The timings are here rather than in the daemon's other settings because they
/// belong with the location: something pointing a daemon at a catalog of its
/// own — a test, a mirror being tried out — wants it looked at on its own
/// cadence too, and separating the two would mean the pair could be set apart
/// from each other for no gain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updates {
    /// The manifests this machine holds, and where a fetched copy is written.
    pub store: StorePaths,
    /// The catalog listing what is published.
    pub catalog: String,
    /// How long after starting to run the first check.
    pub first_after: Duration,
    /// How long between checks after that.
    pub every: Duration,
}

impl Updates {
    /// What this process's environment describes: the manifest directories it
    /// would read, and the catalog it would check.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            store: StorePaths::from_env(),
            catalog: catalog_url(),
            first_after: FIRST_CHECK_AFTER,
            every: INTERVAL,
        }
    }
}

/// A running timer.
///
/// Dropping it asks the thread to stop at the next thing it does, which is
/// either waking from its sleep or finishing the check it is in.
#[derive(Debug)]
pub struct Refreshing {
    halt: Arc<Halt>,
}

impl Refreshing {
    /// Starts checking, beginning after [`Updates::first_after`].
    #[must_use]
    pub fn start(updates: Updates) -> Self {
        info!(
            catalog = %updates.catalog,
            first_after_secs = updates.first_after.as_secs(),
            every_secs = updates.every.as_secs(),
            "checking for newer manifests",
        );
        let halt = Arc::new(Halt::default());
        std::thread::spawn({
            let halt = Arc::clone(&halt);
            move || run(&halt, &updates)
        });
        Self { halt }
    }
}

impl Drop for Refreshing {
    /// Asks the thread to stop, and does not wait for it.
    ///
    /// Nothing it holds has to be taken down in an order — it owns no
    /// connection anything else can see and no state the bus shares — and the
    /// one thing it might be doing when this runs is waiting out a request's
    /// timeout, which is not a wait worth making a daemon's shutdown carry.
    fn drop(&mut self) {
        self.halt.stop();
    }
}

/// Checks until it is told to stop.
fn run(halt: &Halt, updates: &Updates) {
    // Opened once and asked nothing: a check needs somewhere to look, and what
    // it compares a fetched copy against it reads from the disk itself. So
    // there is nothing cached here that a commit could leave stale.
    let store = ManifestStore::open(updates.store.clone());
    if !halt.wait(updates.first_after) {
        return;
    }
    loop {
        check(&store, &updates.catalog);
        if !halt.wait(updates.every) {
            return;
        }
    }
}

/// One check, reported in one line.
///
/// What became of each manifest is said by the check as it goes; this is the
/// line that says a check happened at all, which is what somebody asking "is
/// this machine still looking" needs and what a machine that is quietly current
/// would otherwise never produce.
fn check(store: &ManifestStore, catalog: &str) {
    let outcome = update(store, catalog);
    let counted = |wanted: fn(&ItemResult) -> bool| {
        outcome
            .manifests
            .iter()
            .filter(|manifest| wanted(&manifest.result))
            .count()
    };
    match &outcome.result {
        CheckResult::Checked => info!(
            catalog,
            updated = counted(|result| matches!(result, ItemResult::Updated)),
            current = counted(|result| matches!(result, ItemResult::Current)),
            refused = counted(|result| matches!(result, ItemResult::Failed(_))),
            "checked what is published",
        ),
        // Said again here, and by the check itself, because the two are
        // different news: one is a catalog that could not be read, the other is
        // that this daemon has now gone a round without knowing what is
        // published. It will look again at the next interval.
        CheckResult::Failed(reason) => warn!(
            catalog,
            %reason,
            "nothing could be taken from the catalog; will look again",
        ),
    }
}

/// The handle a sleeping timer is stopped through.
#[derive(Debug, Default)]
struct Halt {
    stopping: Mutex<bool>,
    changed: Condvar,
}

impl Halt {
    /// Asks the loop to end.
    fn stop(&self) {
        *self.stopping.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.changed.notify_all();
    }

    /// Waits for `how_long` or for the end, whichever comes first. Says whether
    /// there is any point carrying on.
    fn wait(&self, how_long: Duration) -> bool {
        let stopping = self.stopping.lock().unwrap_or_else(PoisonError::into_inner);
        let (stopping, _) = self
            .changed
            .wait_timeout_while(stopping, how_long, |stopping| !*stopping)
            .unwrap_or_else(PoisonError::into_inner);
        !*stopping
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn a_timer_that_is_dropped_stops_sleeping() {
        let halt = Arc::new(Halt::default());
        let waiting = std::thread::spawn({
            let halt = Arc::clone(&halt);
            move || (halt.wait(Duration::from_secs(3_600)), Instant::now())
        });

        // Given long enough to be asleep rather than about to be.
        std::thread::sleep(Duration::from_millis(20));
        let asked = Instant::now();
        halt.stop();

        let (carry_on, woke) = waiting.join().expect("the waiting thread panicked");
        assert!(!carry_on, "a stopped timer said to carry on");
        assert!(
            woke.duration_since(asked) < Duration::from_secs(60),
            "the wait outlived the asking",
        );
    }

    #[test]
    fn a_timer_that_is_not_stopped_waits_out_its_interval() {
        let halt = Halt::default();
        let started = Instant::now();

        assert!(halt.wait(Duration::from_millis(30)));
        assert!(started.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn the_environment_says_where_to_look() {
        let updates = Updates::from_env();

        assert_eq!(updates.first_after, FIRST_CHECK_AFTER);
        assert_eq!(updates.every, INTERVAL);
        assert!(
            updates.catalog.starts_with("http") || updates.catalog.starts_with("file"),
            "{} is not somewhere to fetch from",
            updates.catalog,
        );
    }
}
