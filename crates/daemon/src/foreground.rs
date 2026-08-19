//! What is running in front of each correlated shell.
//!
//! A shell is correlated by an environment variable it carries: whatever started
//! it exported [`CORRELATION_VAR`], and every event emitted from underneath it
//! copies that value verbatim. This module answers the other half of the same
//! question — not "what are my agents doing", which the events say, but "what
//! process is in front of that terminal right now", which only the process table
//! knows. A terminal running `vim` has an answer here and none there.
//!
//! # Identity, never state
//!
//! An observation says a process by this name holds the terminal. It never says
//! what that process is doing, and its absence says nothing at all: an agent
//! under a nested multiplexer, or backgrounded, or launched by a script, is
//! never in the foreground and is no less alive for it. So this reports positive
//! observations and withdraws them, and never reports "nothing here".
//!
//! # A state machine, not a loop
//!
//! Everything here is driven by [`Monitor::tick`], which is handed the current
//! time and does one round of reads. There is no clock, no thread and no timer
//! inside, which is what lets every state and every transition between states be
//! reached by a test that writes a directory and calls `tick` twice.
//!
//! # Follow the pid, not the name
//!
//! Once the foreground resolves to a pid, that pid is what is followed. Three
//! outcomes then fall out of one `stat`: it is still the foreground group, it is
//! alive but no longer holds the terminal, or it is not in the table at all.
//! Following the *name* instead would fold the last two together — a suspended
//! agent would be indistinguishable from one that exited, which is exactly the
//! distinction anything reaping sessions has to get right.
//!
//! # Transitions only
//!
//! A `tick` that finds the same pid in the same state returns nothing. A stream
//! of "still `claude`, still `claude`" is noise, and the cost of suppressing it
//! here is one comparison against what was last reported.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use agentbus_protocol::{ForegroundEntry, ForegroundState, Timestamp};
use tracing::debug;

use crate::procfs::{Pid, ProcFs};

/// The environment variable that correlates a shell with the events emitted
/// under it.
///
/// The value is opaque. Whatever exported it decides what it means; this module
/// only ever asks whether two of them are the same string, and never splits,
/// validates or interprets one.
pub const CORRELATION_VAR: &str = "AGENTBUS_PANE";

/// An environment variable carried alongside an observation for whoever has to
/// match observations made on two sides of a connection.
///
/// Read and copied, never parsed. This module has no idea what the value means
/// and takes no decision on it.
pub const SSH_CONNECTION_VAR: &str = "SSH_CONNECTION";

/// The longest a full environ sweep is left un-run.
///
/// Reading every process's environment is the one expensive thing here — it is a
/// read per process, where the rest of a tick is a couple of reads per shell —
/// so it happens on this cadence and not on every tick. A correlation that
/// arrives between sweeps does not have to wait for one: see [`Monitor::want`].
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// A change in what can be seen for one correlation.
///
/// Produced only when something is different from what was last reported for
/// that shell, so a consumer may treat every one of these as news.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// The correlation value this is about, exactly as the shell carried it.
    pub correlation: String,
    /// The shell the observation was made through. Two shells may carry the same
    /// correlation, and this is what tells their observations apart.
    pub shell: Pid,
    /// What is now in the foreground, or `None` where the observation is
    /// withdrawn because there is no longer anything to observe.
    pub foreground: Option<ForegroundEntry>,
    /// A pid that was being followed and has since left the process table.
    ///
    /// Carried separately from the observation because it is the one thing here
    /// that is *certain*: the process did not merely stop being in front of the
    /// terminal, it ended.
    pub gone: Option<Pid>,
}

/// The foreground process behind each correlation, and how it got there.
///
/// Owns a [`ProcFs`] and the state needed to tell one tick from the last. It is
/// driven from outside and does nothing on its own.
#[derive(Debug)]
pub struct Monitor {
    proc: ProcFs,
    /// Every shell being watched, keyed by the correlation it carries and its
    /// own pid, so that two shells carrying one correlation stay distinct.
    shells: BTreeMap<Key, Shell>,
    /// Every correlation value this monitor has ever heard of, from a caller or
    /// from the process table. Only used to tell a new value from a known one.
    seen: BTreeSet<String>,
    /// When the last sweep ran, if one has.
    swept: Option<Timestamp>,
    /// Whether a sweep is owed regardless of the cadence.
    resweep: bool,
    /// Whether there is a process table here at all.
    available: bool,
}

/// A correlation value and the shell that carries it.
type Key = (String, Pid);

impl Monitor {
    /// A monitor over one process table.
    ///
    /// Reading the table is the only thing that can say whether there is one, so
    /// that is settled here rather than left for the first tick to discover: a
    /// caller that wants to know whether to report a foreground at all can ask
    /// [`Monitor::available`] before it has ticked.
    pub fn new(proc: ProcFs) -> Self {
        let available = !proc.pids().is_empty();
        Self {
            proc,
            shells: BTreeMap::new(),
            seen: BTreeSet::new(),
            swept: None,
            resweep: false,
            available,
        }
    }

    /// The process table this monitor reads.
    pub fn procfs(&self) -> &ProcFs {
        &self.proc
    }

    /// Whether there is a process table to read.
    ///
    /// False where there is no procfs — another operating system, a root that
    /// does not exist, a mount nothing may list. A caller should then say
    /// nothing about the foreground rather than say there is none, because it
    /// does not know.
    pub fn available(&self) -> bool {
        self.available
    }

    /// Names a correlation somebody is interested in.
    ///
    /// This is not a subscription and nothing is filtered by it: a correlation
    /// found in the process table is reported whether or not anybody asked for
    /// it. What this does buy is latency — a value nobody has heard of before
    /// brings the next sweep forward to the next tick, so a shell that has just
    /// appeared is seen at once instead of within the sweep interval.
    pub fn want(&mut self, correlation: &str) {
        if self.seen.insert(correlation.to_owned()) {
            self.resweep = true;
        }
    }

    /// Every observation currently held, in a stable order.
    pub fn observations(&self) -> Vec<ForegroundEntry> {
        self.shells
            .values()
            .filter_map(|shell| {
                shell
                    .observed
                    .as_ref()
                    .map(|observed| observed.entry.clone())
            })
            .collect()
    }

    /// One round of work: what changed since the last one.
    ///
    /// Sweeps for shells when one is due, then looks at each shell it knows
    /// about. In the steady state that is two reads per shell — the shell's own
    /// `stat` for the foreground group of its terminal, and the followed pid's
    /// for what became of it.
    pub fn tick(&mut self, now: &Timestamp) -> Vec<Transition> {
        let mut transitions = Vec::new();
        if self.sweep_due(now) {
            self.sweep(now, &mut transitions);
        }
        if !self.available {
            // Nothing was read, so nothing is known, so nothing is reported —
            // including the disappearance of anything reported earlier.
            return Vec::new();
        }

        let proc = &self.proc;
        self.shells.retain(|(correlation, pid), shell| {
            match shell.look(proc, correlation, *pid, now) {
                Step::Quiet => true,
                Step::Changed(transition) => {
                    transitions.push(transition);
                    true
                }
                Step::Lost(transition) => {
                    transitions.extend(transition);
                    false
                }
            }
        });
        transitions
    }

    /// Whether this tick owes a sweep.
    ///
    /// A clock that has gone backwards sweeps immediately rather than falling
    /// silent until it has caught up again, which is why this is a range test
    /// and not a comparison.
    fn sweep_due(&self, now: &Timestamp) -> bool {
        let Some(swept) = &self.swept else {
            return true;
        };
        self.resweep || !(0..sweep_interval_millis()).contains(&now.millis_since(swept))
    }

    /// Finds every shell in the process table and reconciles it with what was
    /// already being watched.
    ///
    /// A shell carries the correlation, and so does everything it started, so
    /// the shell is the *root* of the subtree that carries one value: the
    /// process whose parent does not carry it too. A parent whose environment
    /// cannot be read is not carrying anything as far as this can tell, which
    /// makes its child a root — the right answer, since the child is then the
    /// highest process that can be seen to be correlated.
    fn sweep(&mut self, now: &Timestamp, out: &mut Vec<Transition>) {
        self.swept = Some(now.clone());
        self.resweep = false;

        let pids = self.proc.pids();
        self.available = !pids.is_empty();
        if !self.available {
            debug!(root = %self.proc.root().display(), "there is no process table here");
            return;
        }

        let mut carriers: BTreeMap<Pid, String> = BTreeMap::new();
        for pid in pids {
            if let Some(value) = self.proc.environ_var(pid, CORRELATION_VAR) {
                carriers.insert(pid, value);
            }
        }

        let mut roots: BTreeMap<String, Vec<Root>> = BTreeMap::new();
        for (pid, value) in &carriers {
            let Some(stat) = self.proc.stat(*pid) else {
                continue;
            };
            if carriers.get(&stat.ppid) == Some(value) {
                continue;
            }
            roots.entry(value.clone()).or_default().push(Root {
                pid: *pid,
                leads_session: stat.session == *pid,
            });
        }

        let mut shells = BTreeMap::new();
        for (value, mut found) in roots {
            // Two shells that exported the same value are two answers to one
            // question. A session leader is the better answer where there is
            // one — it is the shell a terminal belongs to rather than something
            // started underneath one — and where that does not decide it, both
            // are kept and reported separately.
            if found.len() > 1 && found.iter().any(|root| root.leads_session) {
                found.retain(|root| root.leads_session);
            }
            for root in found {
                let key = (value.clone(), root.pid);
                let mut shell = self.shells.remove(&key).unwrap_or_default();
                shell.ssh_connection = self.proc.environ_var(root.pid, SSH_CONNECTION_VAR);
                shells.insert(key, shell);
            }
            self.seen.insert(value);
        }

        // Whatever the sweep did not find again is not there any more.
        for ((correlation, pid), mut shell) in std::mem::replace(&mut self.shells, shells) {
            out.extend(shell.withdraw(&self.proc, &correlation, pid));
        }
    }
}

/// The sweep interval in the units a timestamp difference comes in.
fn sweep_interval_millis() -> i64 {
    i64::try_from(SWEEP_INTERVAL.as_millis()).unwrap_or(i64::MAX)
}

/// A process carrying a correlation whose parent does not.
struct Root {
    pid: Pid,
    leads_session: bool,
}

/// One watched shell.
#[derive(Debug, Default)]
struct Shell {
    /// The shell's own `SSH_CONNECTION`, as read on the last sweep. Opaque.
    ssh_connection: Option<String>,
    /// The pid being followed and what was last reported about it.
    observed: Option<Observed>,
}

/// A pid being followed, and the observation last reported for it.
#[derive(Debug, Clone)]
struct Observed {
    pid: Pid,
    entry: ForegroundEntry,
}

/// What one look at one shell came to.
enum Step {
    /// Nothing to report.
    Quiet,
    /// Something is different.
    Changed(Transition),
    /// The shell is no longer there, with the withdrawal if it had anything to
    /// withdraw.
    Lost(Option<Transition>),
}

impl Shell {
    /// One tick's worth of work for one shell.
    fn look(&mut self, proc: &ProcFs, correlation: &str, pid: Pid, now: &Timestamp) -> Step {
        let Some(shell) = proc.stat(pid) else {
            return Step::Lost(self.withdraw(proc, correlation, pid));
        };
        if shell.tpgid <= 0 {
            // No controlling terminal, so no foreground to speak of. The shell
            // is still there and may get one back.
            return match self.withdraw(proc, correlation, pid) {
                Some(transition) => Step::Changed(transition),
                None => Step::Quiet,
            };
        }

        let mut gone = None;
        let followed = match &self.observed {
            Some(observed) => match proc.stat(observed.pid) {
                // Still the foreground group of the shell's terminal.
                Some(stat) if stat.pgrp == shell.tpgid => {
                    Some((observed.clone(), ForegroundState::Foreground))
                }
                // The shell has the terminal back, which is what a suspended or
                // backgrounded process leaves behind. It is still worth
                // reporting: the shell running it is less interesting than the
                // thing that was running.
                Some(_) if shell.tpgid == shell.pgrp => {
                    Some((observed.clone(), ForegroundState::Suspended))
                }
                // Some third group holds the terminal now, so whatever was being
                // followed has been overtaken and the foreground is resolved
                // afresh. It did not end, so there is nothing to note about it.
                Some(_) => None,
                // A pid whose directory is there but whose `stat` would not be
                // read is not evidence of anything; wait and look again.
                None if proc.exists(observed.pid) => return Step::Quiet,
                None => {
                    gone = Some(observed.pid);
                    None
                }
            },
            None => None,
        };

        let observed = match followed {
            // The same pid keeps the name and command line it was resolved
            // with; only how it stands to its terminal can have moved, and
            // `since` moves with it.
            Some((mut held, state)) => {
                if held.entry.state != Some(state) {
                    held.entry.state = Some(state);
                    held.entry.since = now.clone();
                }
                held.entry.ssh_connection = self.ssh_connection.clone();
                Some(held)
            }
            None => resolve(
                proc,
                correlation,
                shell.tpgid,
                self.ssh_connection.clone(),
                now,
            ),
        };

        let changed = self.observed.as_ref().map(|held| &held.entry)
            != observed.as_ref().map(|held| &held.entry);
        if !changed && gone.is_none() {
            return Step::Quiet;
        }
        self.observed = observed;
        Step::Changed(Transition {
            correlation: correlation.to_owned(),
            shell: pid,
            foreground: self.observed.as_ref().map(|held| held.entry.clone()),
            gone,
        })
    }

    /// Drops whatever this shell was reporting, and says so if it was reporting
    /// anything.
    ///
    /// A pid that has left the table on the way out is still worth noting, so
    /// the followed one is checked for one last time: that a shell died is a
    /// different fact from that the process in front of it did, and something
    /// binding sessions to pids needs the second one.
    fn withdraw(&mut self, proc: &ProcFs, correlation: &str, pid: Pid) -> Option<Transition> {
        let observed = self.observed.take()?;
        Some(Transition {
            correlation: correlation.to_owned(),
            shell: pid,
            foreground: None,
            gone: (!proc.exists(observed.pid)).then_some(observed.pid),
        })
    }
}

/// Reads the process that holds a terminal.
///
/// `leader` is the foreground process group of the terminal, which is also the
/// pid of the process that leads that group. A group may hold a whole pipeline;
/// the leader is the one that stands for it.
fn resolve(
    proc: &ProcFs,
    correlation: &str,
    leader: Pid,
    ssh_connection: Option<String>,
    now: &Timestamp,
) -> Option<Observed> {
    let stat = proc.stat(leader)?;
    let pid = u32::try_from(leader).ok()?;
    // The name comes off the `stat` already read rather than out of `comm`,
    // which holds the same string; the command line is the one further read the
    // identity of a new pid costs.
    let cmdline = proc.cmdline(leader).unwrap_or_default().join(" ");
    let mut entry = ForegroundEntry::new(correlation, pid, stat.comm, cmdline, now.clone());
    entry.state = Some(ForegroundState::Foreground);
    entry.ssh_connection = ssh_connection;
    Some(Observed { pid: leader, entry })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use crate::clock::from_unix_millis;

    /// An arbitrary instant to count the tests' seconds from.
    const EPOCH: i64 = 1_786_962_721_412;

    /// `seconds` after the instant a test started at.
    fn at(seconds: i64) -> Timestamp {
        from_unix_millis(EPOCH + seconds * 1_000)
    }

    /// Whether this test is running with the privilege to ignore file modes.
    fn is_root() -> bool {
        // Safe by construction: `geteuid` takes nothing, cannot fail and touches
        // no memory this process owns.
        (unsafe { libc::geteuid() }) == 0
    }

    /// A process table a test writes for itself, and may then change.
    ///
    /// The scenarios here are about what happens *between* two ticks — a process
    /// suspended, a process that exited, a shell that appeared — so they are
    /// built as files a test owns rather than read from a fixture it cannot
    /// modify.
    struct Tree {
        dir: tempfile::TempDir,
    }

    impl Tree {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("proc")).unwrap();
            Self { dir }
        }

        fn root(&self) -> PathBuf {
            self.dir.path().join("proc")
        }

        fn proc(&self) -> ProcFs {
            ProcFs::new(self.root())
        }

        fn monitor(&self) -> Monitor {
            Monitor::new(self.proc())
        }

        /// Writes one process, replacing it if it is already there.
        fn write(&self, process: Process) {
            let dir = self.root().join(process.pid.to_string());
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("stat"),
                format!(
                    "{} ({}) S {} {} {} 34816 {} 4194304 0 0 0 0 5 2 0 0 20 0 1 0 0\n",
                    process.pid,
                    process.comm,
                    process.ppid,
                    process.pgrp,
                    process.session,
                    process.tpgid,
                ),
            )
            .unwrap();
            fs::write(dir.join("comm"), format!("{}\n", process.comm)).unwrap();
            let arguments: Vec<&str> = process.cmdline.iter().map(String::as_str).collect();
            fs::write(dir.join("cmdline"), nul_terminated(&arguments)).unwrap();

            let environ = dir.join("environ");
            let pairs: Vec<String> = process
                .environ
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            let refs: Vec<&str> = pairs.iter().map(String::as_str).collect();
            fs::write(&environ, nul_terminated(&refs)).unwrap();
            if !process.readable {
                fs::set_permissions(&environ, fs::Permissions::from_mode(0o000)).unwrap();
            }
        }

        /// Takes one process out of the table, as an exit does.
        fn remove(&self, pid: Pid) {
            fs::remove_dir_all(self.root().join(pid.to_string())).unwrap();
        }
    }

    /// The bytes procfs holds a NUL-separated file in.
    fn nul_terminated(entries: &[&str]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in entries {
            bytes.extend_from_slice(entry.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    /// One process to write into a [`Tree`].
    struct Process {
        pid: Pid,
        comm: String,
        ppid: Pid,
        pgrp: Pid,
        session: Pid,
        tpgid: Pid,
        cmdline: Vec<String>,
        environ: Vec<(String, String)>,
        readable: bool,
    }

    /// A session-leading process holding its own terminal, which every scenario
    /// then adjusts.
    fn process(pid: Pid, comm: &str) -> Process {
        Process {
            pid,
            comm: comm.to_owned(),
            ppid: 1,
            pgrp: pid,
            session: pid,
            tpgid: pid,
            cmdline: vec![comm.to_owned()],
            environ: Vec::new(),
            readable: true,
        }
    }

    impl Process {
        fn child_of(mut self, ppid: Pid) -> Self {
            self.ppid = ppid;
            self
        }

        fn in_group(mut self, pgrp: Pid) -> Self {
            self.pgrp = pgrp;
            self
        }

        fn in_session(mut self, session: Pid) -> Self {
            self.session = session;
            self
        }

        /// The process group in front of this process's terminal.
        fn fronted_by(mut self, tpgid: Pid) -> Self {
            self.tpgid = tpgid;
            self
        }

        fn cmdline(mut self, arguments: &[&str]) -> Self {
            self.cmdline = arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect();
            self
        }

        fn env(mut self, name: &str, value: &str) -> Self {
            self.environ.push((name.to_owned(), value.to_owned()));
            self
        }

        fn correlated(self, value: &str) -> Self {
            self.env(CORRELATION_VAR, value)
        }

        /// An environment file nobody may open, as another user's is.
        fn secret(mut self) -> Self {
            self.readable = false;
            self
        }
    }

    /// The one transition a tick was expected to produce.
    fn only(transitions: Vec<Transition>) -> Transition {
        assert_eq!(
            transitions.len(),
            1,
            "expected one transition: {transitions:?}"
        );
        transitions.into_iter().next().unwrap()
    }

    /// The observation a transition reports, which it was expected to have.
    fn observation(transition: &Transition) -> &ForegroundEntry {
        transition
            .foreground
            .as_ref()
            .unwrap_or_else(|| panic!("expected an observation: {transition:?}"))
    }

    #[test]
    fn a_shell_holding_its_own_terminal_reports_itself() {
        let tree = Tree::new();
        tree.write(
            process(100, "bash")
                .cmdline(&["bash", "-i"])
                .correlated("pane-7"),
        );
        let mut monitor = tree.monitor();

        let transition = only(monitor.tick(&at(0)));

        assert_eq!(transition.correlation, "pane-7");
        assert_eq!(transition.shell, 100);
        assert_eq!(transition.gone, None);
        let entry = observation(&transition);
        assert_eq!(entry.pid, 100);
        assert_eq!(entry.process, "bash");
        assert_eq!(entry.cmdline, "bash -i");
        assert_eq!(entry.state, Some(ForegroundState::Foreground));
        assert_eq!(entry.since, at(0));
        assert!(entry.origin.is_empty());
    }

    #[test]
    fn what_is_in_front_of_the_shell_is_what_is_reported() {
        let tree = Tree::new();
        tree.write(process(100, "bash").fronted_by(200).correlated("pane-7"));
        tree.write(
            process(200, "claude")
                .child_of(100)
                .in_group(200)
                .in_session(100)
                .cmdline(&["node", "/usr/local/bin/claude"])
                .correlated("pane-7"),
        );
        let mut monitor = tree.monitor();

        let transition = only(monitor.tick(&at(0)));
        let entry = observation(&transition);
        assert_eq!(entry.pid, 200);
        assert_eq!(entry.process, "claude");
        assert_eq!(entry.cmdline, "node /usr/local/bin/claude");
        assert_eq!(entry.state, Some(ForegroundState::Foreground));

        assert!(
            monitor.tick(&at(1)).is_empty(),
            "the same thing seen twice is not news"
        );
    }

    #[test]
    fn a_suspended_process_is_still_followed_and_said_to_be_suspended() {
        let tree = Tree::new();
        tree.write(process(100, "bash").fronted_by(200).correlated("pane-7"));
        tree.write(
            process(200, "claude")
                .child_of(100)
                .in_group(200)
                .in_session(100),
        );
        let mut monitor = tree.monitor();
        assert_eq!(observation(&only(monitor.tick(&at(0)))).pid, 200);

        // Ctrl-Z: the shell takes its terminal back, and the agent is still
        // there.
        tree.write(process(100, "bash").correlated("pane-7"));
        let transition = only(monitor.tick(&at(2)));

        let entry = observation(&transition);
        assert_eq!(entry.pid, 200, "the pid is still the one being followed");
        assert_eq!(entry.process, "claude");
        assert_eq!(entry.state, Some(ForegroundState::Suspended));
        assert_eq!(entry.since, at(2), "the state began now");
        assert_eq!(transition.gone, None, "nothing ended");
        assert!(
            monitor.tick(&at(3)).is_empty(),
            "still suspended is not news either"
        );
    }

    #[test]
    fn a_process_that_ends_is_reported_gone_and_the_terminal_resolved_again() {
        let tree = Tree::new();
        tree.write(process(100, "bash").fronted_by(200).correlated("pane-7"));
        tree.write(
            process(200, "claude")
                .child_of(100)
                .in_group(200)
                .in_session(100),
        );
        let mut monitor = tree.monitor();
        assert_eq!(observation(&only(monitor.tick(&at(0)))).pid, 200);

        // The agent exits, and the shell has its terminal back.
        tree.write(process(100, "bash").correlated("pane-7"));
        tree.remove(200);
        let transition = only(monitor.tick(&at(2)));

        assert_eq!(transition.gone, Some(200));
        let entry = observation(&transition);
        assert_eq!(entry.pid, 100);
        assert_eq!(entry.process, "bash");
        assert_eq!(entry.state, Some(ForegroundState::Foreground));
    }

    #[test]
    fn the_leader_of_a_pipeline_stands_for_it() {
        let tree = Tree::new();
        tree.write(process(100, "bash").fronted_by(300).correlated("pane-3"));
        for (pid, comm) in [(300, "grep"), (301, "sort"), (302, "head")] {
            tree.write(
                process(pid, comm)
                    .child_of(100)
                    .in_group(300)
                    .in_session(100),
            );
        }
        let mut monitor = tree.monitor();

        let entry = observation(&only(monitor.tick(&at(0)))).clone();

        assert_eq!(entry.pid, 300);
        assert_eq!(entry.process, "grep");
    }

    #[test]
    fn a_child_that_inherited_the_value_is_not_a_second_shell() {
        let tree = Tree::new();
        tree.write(process(100, "bash").fronted_by(200).correlated("pane-7"));
        tree.write(
            process(200, "claude")
                .child_of(100)
                .in_group(200)
                .in_session(100)
                .correlated("pane-7"),
        );
        let mut monitor = tree.monitor();

        let transition = only(monitor.tick(&at(0)));

        assert_eq!(
            transition.shell, 100,
            "the root of the subtree is the shell"
        );
    }

    #[test]
    fn two_unrelated_shells_carrying_one_value_are_two_observations() {
        let tree = Tree::new();
        tree.write(process(100, "bash").correlated("pane-7"));
        tree.write(process(500, "zsh").correlated("pane-7"));
        let mut monitor = tree.monitor();

        let transitions = monitor.tick(&at(0));

        assert_eq!(transitions.len(), 2);
        assert_eq!(transitions[0].shell, 100);
        assert_eq!(transitions[1].shell, 500);
        for transition in &transitions {
            assert_eq!(transition.correlation, "pane-7");
        }
    }

    #[test]
    fn a_session_leader_settles_which_of_two_roots_is_the_shell() {
        let tree = Tree::new();
        tree.write(process(100, "bash").correlated("pane-7"));
        // Correlated, unparented, and part of somebody else's session: not a
        // shell, whatever else it is.
        tree.write(process(400, "watch").in_session(300).correlated("pane-7"));
        let mut monitor = tree.monitor();

        let transition = only(monitor.tick(&at(0)));

        assert_eq!(transition.shell, 100);
    }

    #[test]
    fn a_shell_whose_environment_cannot_be_read_is_not_observed() {
        if is_root() {
            eprintln!("skipped: running as root, which file modes do not apply to");
            return;
        }
        let tree = Tree::new();
        tree.write(process(100, "bash").correlated("pane-7").secret());
        let mut monitor = tree.monitor();

        assert!(monitor.tick(&at(0)).is_empty());
        assert!(monitor.available(), "the process table itself is readable");
    }

    #[test]
    fn a_shell_with_no_terminal_has_no_foreground() {
        let tree = Tree::new();
        tree.write(process(100, "bash").fronted_by(-1).correlated("pane-7"));
        let mut monitor = tree.monitor();

        assert!(monitor.tick(&at(0)).is_empty());
    }

    #[test]
    fn a_shell_that_goes_away_withdraws_its_observation() {
        let tree = Tree::new();
        tree.write(process(100, "bash").fronted_by(200).correlated("pane-7"));
        tree.write(
            process(200, "claude")
                .child_of(100)
                .in_group(200)
                .in_session(100),
        );
        let mut monitor = tree.monitor();
        assert_eq!(observation(&only(monitor.tick(&at(0)))).pid, 200);

        tree.remove(100);
        tree.remove(200);
        // A process the table still holds keeps it available, so that this is a
        // shell going away rather than a procfs going away.
        tree.write(process(1, "init"));
        let transition = only(monitor.tick(&at(1)));

        assert_eq!(transition.correlation, "pane-7");
        assert_eq!(transition.shell, 100);
        assert_eq!(transition.foreground, None);
        assert_eq!(transition.gone, Some(200), "it went with the shell");
        assert!(monitor.observations().is_empty());
        assert!(monitor.tick(&at(2)).is_empty(), "withdrawn once is enough");
    }

    #[test]
    fn an_unseen_value_brings_the_sweep_forward_and_a_known_one_does_not() {
        let tree = Tree::new();
        tree.write(process(100, "bash").correlated("pane-a"));
        let mut monitor = tree.monitor();
        assert_eq!(only(monitor.tick(&at(0))).correlation, "pane-a");

        tree.write(process(200, "bash").correlated("pane-b"));
        assert!(
            monitor.tick(&at(1)).is_empty(),
            "no sweep is due, so the new shell has not been looked for"
        );

        monitor.want("pane-b");
        assert_eq!(only(monitor.tick(&at(1))).correlation, "pane-b");

        tree.write(process(300, "bash").correlated("pane-c"));
        monitor.want("pane-a");
        assert!(
            monitor.tick(&at(2)).is_empty(),
            "a value already known brings nothing forward"
        );

        assert_eq!(
            only(monitor.tick(&at(6))).correlation,
            "pane-c",
            "the sweep interval finds it in its own time"
        );
    }

    #[test]
    fn an_ssh_connection_is_carried_verbatim() {
        let tree = Tree::new();
        tree.write(
            process(100, "bash")
                .correlated("pane-7")
                .env(SSH_CONNECTION_VAR, "10.0.0.2 51234 10.0.0.9 22"),
        );
        let mut monitor = tree.monitor();

        let transition = only(monitor.tick(&at(0)));

        assert_eq!(
            observation(&transition).ssh_connection.as_deref(),
            Some("10.0.0.2 51234 10.0.0.9 22")
        );
    }

    #[test]
    fn a_process_table_that_is_not_there_is_reported_as_such() {
        let tree = Tree::new();
        let mut monitor = Monitor::new(ProcFs::new(tree.root().join("nowhere")));

        assert!(!monitor.available());
        assert!(monitor.tick(&at(0)).is_empty());
        assert!(!monitor.available());
    }

    #[test]
    fn a_run_of_ticks_over_a_quiet_shell_says_nothing_after_the_first() {
        let tree = Tree::new();
        tree.write(process(100, "bash").fronted_by(200).correlated("pane-7"));
        tree.write(
            process(200, "claude")
                .child_of(100)
                .in_group(200)
                .in_session(100)
                .correlated("pane-7"),
        );
        let mut monitor = tree.monitor();
        assert_eq!(monitor.tick(&at(0)).len(), 1);

        for second in 1..30 {
            assert!(
                monitor.tick(&at(second)).is_empty(),
                "nothing changed at second {second}"
            );
        }
        assert_eq!(monitor.observations().len(), 1);
    }
}
