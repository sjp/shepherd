//! What is running in front of each watched shell.
//!
//! A shell is correlated by an environment variable it carries: whatever started
//! it exported [`CORRELATION_VAR`], and every event emitted from underneath it
//! copies that value verbatim. This module answers the other half of the same
//! question — not "what are my agents doing", which the events say, but "what
//! process is in front of that terminal right now", which only the process table
//! knows. A terminal running `vim` has an answer here and none there.
//!
//! # Shells nobody labelled
//!
//! A shell that arrived over a connection may carry no correlation at all — the
//! variable is exported by whoever started the shell, and nothing guarantees it
//! survives a boundary. Such a shell is still worth watching, so it is filed
//! under [`SSH_CONNECTION_VAR`] instead: see [`Slot`]. Whoever reads these
//! observations may then match one against another, made at the other end of
//! the same connection, without either end having smuggled anything across.
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

/// The variable read for a correlation when [`CORRELATION_VAR`] is unset.
///
/// Purely a second name for the same thing, and read the same way: the value is
/// copied and compared, never looked inside. It earns its place because a shell
/// on the far side of an `ssh` connection inherits only what the server agreed
/// to accept, and an `LC_`-prefixed name is what a default `sshd` configuration
/// lets through. Nothing about that is this module's business beyond reading a
/// second name.
pub const CORRELATION_FALLBACK_VAR: &str = "LC_AGENTBUS_PANE";

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

/// What a watched shell is filed under.
///
/// A shell that carries a correlation is filed under it, whatever else it
/// carries. One that carries none is filed under the connection it arrived
/// over, which is then the only thing that says which shell it is.
///
/// Both are opaque strings taken out of an environment block. This module
/// compares two of them and does nothing else with either: it never splits one,
/// never validates one, and has no opinion about what either variable's value
/// is supposed to look like.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot {
    /// The shell carried [`CORRELATION_VAR`], or [`CORRELATION_FALLBACK_VAR`].
    Correlation(String),
    /// The shell carried only [`SSH_CONNECTION_VAR`].
    Connection(String),
}

impl Slot {
    /// The value, whichever of the two it came from.
    pub fn value(&self) -> &str {
        match self {
            Self::Correlation(value) | Self::Connection(value) => value,
        }
    }

    /// The correlation, where the shell carried one.
    pub fn correlation(&self) -> Option<&str> {
        match self {
            Self::Correlation(value) => Some(value),
            Self::Connection(_) => None,
        }
    }
}

/// A change in what can be seen for one shell.
///
/// Produced only when something is different from what was last reported for
/// that shell, so a consumer may treat every one of these as news.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// What the shell is filed under, exactly as it carried it.
    pub slot: Slot,
    /// The shell the observation was made through. Two shells may be filed under
    /// the same value, and this is what tells their observations apart.
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

/// The foreground process behind each watched shell, and how it got there.
///
/// Owns a [`ProcFs`] and the state needed to tell one tick from the last. It is
/// driven from outside and does nothing on its own.
#[derive(Debug)]
pub struct Monitor {
    proc: ProcFs,
    /// Every shell being watched, keyed by what it is filed under and its own
    /// pid, so that two shells filed under one value stay distinct.
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

/// What a shell is filed under and the shell that is filed under it.
type Key = (Slot, Pid);

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

        // One read of the connection tables serves every shell this round: they
        // are two files however many processes are asked about, where the fd
        // directory that has to be listed to use them is one per process.
        let established = match self.shells.is_empty() {
            true => BTreeMap::new(),
            false => self.proc.established_tcp(),
        };
        let proc = &self.proc;
        self.shells.retain(|(slot, pid), shell| {
            match shell.look(proc, slot, *pid, now, &established) {
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
    /// A shell carries whatever it is filed under, and so does everything it
    /// started, so the shell is the *root* of the subtree that carries one
    /// value: the process whose parent does not carry it too. A parent whose
    /// environment cannot be read is not carrying anything as far as this can
    /// tell, which makes its child a root — the right answer, since the child is
    /// then the highest process that can be seen to carry it.
    fn sweep(&mut self, now: &Timestamp, out: &mut Vec<Transition>) {
        self.swept = Some(now.clone());
        self.resweep = false;

        let pids = self.proc.pids();
        self.available = !pids.is_empty();
        if !self.available {
            debug!(root = %self.proc.root().display(), "there is no process table here");
            return;
        }

        let mut carriers: BTreeMap<Pid, Carried> = BTreeMap::new();
        for pid in pids {
            let carried = self.carried(pid);
            if carried.correlation.is_some() || carried.connection.is_some() {
                carriers.insert(pid, carried);
            }
        }

        let mut roots: BTreeMap<Slot, Vec<Root>> = BTreeMap::new();
        for (pid, carried) in &carriers {
            let Some(stat) = self.proc.stat(*pid) else {
                continue;
            };
            let parent = carriers.get(&stat.ppid);
            let slot = match &carried.correlation {
                // A shell that carries a correlation is filed under it whatever
                // else it carries, so its subtree is bounded by the correlation
                // and the connection takes no part in deciding where it starts.
                Some(value) => {
                    if parent.and_then(|carried| carried.correlation.as_ref()) == Some(value) {
                        continue;
                    }
                    Slot::Correlation(value.clone())
                }
                // No correlation, so the connection is the only thing left that
                // says which shell this is. A process inside a subtree whose
                // root does carry a correlation is skipped here, because its
                // parent carries the same connection value and it is therefore
                // not a root.
                None => {
                    let Some(value) = &carried.connection else {
                        continue;
                    };
                    if parent.and_then(|carried| carried.connection.as_ref()) == Some(value) {
                        continue;
                    }
                    Slot::Connection(value.clone())
                }
            };
            roots.entry(slot).or_default().push(Root {
                pid: *pid,
                leads_session: stat.session == *pid,
            });
        }

        let mut shells = BTreeMap::new();
        for (slot, mut found) in roots {
            // Two shells filed under the same value are two answers to one
            // question. A session leader is the better answer where there is
            // one — it is the shell a terminal belongs to rather than something
            // started underneath one — and where that does not decide it, both
            // are kept and reported separately.
            if found.len() > 1 && found.iter().any(|root| root.leads_session) {
                found.retain(|root| root.leads_session);
            }
            for root in found {
                let connection = carriers
                    .get(&root.pid)
                    .and_then(|carried| carried.connection.clone());
                let key = (slot.clone(), root.pid);
                let mut shell = self.shells.remove(&key).unwrap_or_default();
                shell.ssh_connection = connection;
                shells.insert(key, shell);
            }
            if let Slot::Correlation(value) = slot {
                self.seen.insert(value);
            }
        }

        // Whatever the sweep did not find again is not there any more.
        for ((slot, pid), mut shell) in std::mem::replace(&mut self.shells, shells) {
            out.extend(shell.withdraw(&self.proc, &slot, pid));
        }
    }

    /// What one process carries of the two things a shell may be filed under.
    ///
    /// Both come out of one read of the environment block, which is what keeps a
    /// sweep at one read per process however many names it is looking for.
    ///
    /// A value that is set to nothing is treated as unset. An empty correlation
    /// ties nothing to anything, and an empty connection identifies no
    /// connection, so in both cases the variable being there says no more than
    /// its absence would.
    fn carried(&self, pid: Pid) -> Carried {
        let mut values = self
            .proc
            .environ_vars(
                pid,
                &[
                    CORRELATION_VAR,
                    CORRELATION_FALLBACK_VAR,
                    SSH_CONNECTION_VAR,
                ],
            )
            .into_iter()
            .map(|value| value.filter(|value| !value.is_empty()));
        let correlation = values.next().flatten();
        let fallback = values.next().flatten();
        Carried {
            correlation: correlation.or(fallback),
            connection: values.next().flatten(),
        }
    }
}

/// What one process carries of the two things a shell may be filed under.
#[derive(Debug, Default)]
struct Carried {
    /// [`CORRELATION_VAR`], or [`CORRELATION_FALLBACK_VAR`] where that is unset.
    correlation: Option<String>,
    /// [`SSH_CONNECTION_VAR`].
    connection: Option<String>,
}

/// The sweep interval in the units a timestamp difference comes in.
fn sweep_interval_millis() -> i64 {
    i64::try_from(SWEEP_INTERVAL.as_millis()).unwrap_or(i64::MAX)
}

/// A process carrying a value whose parent does not carry the same one.
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
    ///
    /// `established` is the connection table for this round of reads, shared by
    /// every shell; see [`ProcFs::established_tcp`].
    fn look(
        &mut self,
        proc: &ProcFs,
        slot: &Slot,
        pid: Pid,
        now: &Timestamp,
        established: &BTreeMap<u64, u16>,
    ) -> Step {
        let Some(shell) = proc.stat(pid) else {
            return Step::Lost(self.withdraw(proc, slot, pid));
        };
        if shell.tpgid <= 0 {
            // No controlling terminal, so no foreground to speak of. The shell
            // is still there and may get one back.
            return match self.withdraw(proc, slot, pid) {
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
            // `since` moves with it. What connection it holds open may have
            // moved too, and that is re-read rather than kept, because a
            // process that has just connected somewhere is the whole of what
            // this field is for.
            Some((mut held, state)) => {
                if held.entry.state != Some(state) {
                    held.entry.state = Some(state);
                    held.entry.since = now.clone();
                }
                held.entry.ssh_connection = self.ssh_connection.clone();
                held.entry.ssh_client_port = sole_port(proc, held.pid, established);
                Some(held)
            }
            None => resolve(
                proc,
                slot,
                shell.tpgid,
                self.ssh_connection.clone(),
                now,
                established,
            ),
        };

        let changed = self.observed.as_ref().map(|held| &held.entry)
            != observed.as_ref().map(|held| &held.entry);
        if !changed && gone.is_none() {
            return Step::Quiet;
        }
        self.observed = observed;
        Step::Changed(Transition {
            slot: slot.clone(),
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
    fn withdraw(&mut self, proc: &ProcFs, slot: &Slot, pid: Pid) -> Option<Transition> {
        let observed = self.observed.take()?;
        Some(Transition {
            slot: slot.clone(),
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
    slot: &Slot,
    leader: Pid,
    ssh_connection: Option<String>,
    now: &Timestamp,
    established: &BTreeMap<u64, u16>,
) -> Option<Observed> {
    let stat = proc.stat(leader)?;
    let pid = u32::try_from(leader).ok()?;
    // The name comes off the `stat` already read rather than out of `comm`,
    // which holds the same string; the command line is the one further read the
    // identity of a new pid costs.
    let cmdline = proc.cmdline(leader).unwrap_or_default().join(" ");
    let mut entry = ForegroundEntry::new(pid, stat.comm, cmdline, now.clone());
    entry.correlation = slot.correlation().map(str::to_owned);
    entry.state = Some(ForegroundState::Foreground);
    entry.ssh_connection = ssh_connection;
    entry.ssh_client_port = sole_port(proc, leader, established);
    Some(Observed { pid: leader, entry })
}

/// The source port of the one connection the observed process holds open.
///
/// Asked of every process this monitor follows and of no particular kind of
/// process: there is no list here of programs that are allowed to have
/// connections, because there could not be an honest one. What the field means
/// where it is present, and why it is absent for a process with none or with
/// several, is [`ProcFs::sole_established_port`].
fn sole_port(proc: &ProcFs, pid: Pid, established: &BTreeMap<u64, u16>) -> Option<u32> {
    proc.sole_established_port(pid, established).map(u32::from)
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

            if !process.sockets.is_empty() {
                let fd = dir.join("fd");
                fs::create_dir_all(&fd).unwrap();
                for (descriptor, inode) in process.sockets.iter().enumerate() {
                    let link = fd.join(descriptor.to_string());
                    let _ = fs::remove_file(&link);
                    std::os::unix::fs::symlink(format!("socket:[{inode}]"), link).unwrap();
                }
            }

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

        /// Writes the connection table, as established connections of
        /// `(socket inode, local port)`, replacing whatever was there.
        fn established(&self, connections: &[(u64, u16)]) {
            let net = self.root().join("net");
            fs::create_dir_all(&net).unwrap();
            let mut table = String::from(
                "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when \
                 retrnsmt   uid  timeout inode\n",
            );
            for (slot, (inode, port)) in connections.iter().enumerate() {
                table.push_str(&format!(
                    "{slot:4}: 0A00000A:{port:04X} 0A000009:0016 01 \
                     00000000:00000000 00:00000000 00000000  1000        0 {inode} 1\n"
                ));
            }
            fs::write(net.join("tcp"), table).unwrap();
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
        sockets: Vec<u64>,
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
            sockets: Vec::new(),
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

        /// The sockets this process holds descriptors on, by inode.
        fn holding(mut self, inodes: &[u64]) -> Self {
            self.sockets = inodes.to_vec();
            self
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

        assert_eq!(transition.slot, Slot::Correlation("pane-7".to_owned()));
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
            assert_eq!(transition.slot, Slot::Correlation("pane-7".to_owned()));
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

        assert_eq!(transition.slot, Slot::Correlation("pane-7".to_owned()));
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
        assert_eq!(
            only(monitor.tick(&at(0))).slot,
            Slot::Correlation("pane-a".to_owned())
        );

        tree.write(process(200, "bash").correlated("pane-b"));
        assert!(
            monitor.tick(&at(1)).is_empty(),
            "no sweep is due, so the new shell has not been looked for"
        );

        monitor.want("pane-b");
        assert_eq!(
            only(monitor.tick(&at(1))).slot,
            Slot::Correlation("pane-b".to_owned())
        );

        tree.write(process(300, "bash").correlated("pane-c"));
        monitor.want("pane-a");
        assert!(
            monitor.tick(&at(2)).is_empty(),
            "a value already known brings nothing forward"
        );

        assert_eq!(
            only(monitor.tick(&at(6))).slot,
            Slot::Correlation("pane-c".to_owned()),
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

    #[test]
    fn a_shell_that_carries_only_a_connection_is_watched_without_a_correlation() {
        const CONNECTION: &str = "10.0.0.5 51234 10.0.0.9 22";

        let tree = Tree::new();
        tree.write(
            process(100, "bash")
                .fronted_by(200)
                .env(SSH_CONNECTION_VAR, CONNECTION),
        );
        tree.write(
            process(200, "claude")
                .child_of(100)
                .in_group(200)
                .in_session(100)
                .env(SSH_CONNECTION_VAR, CONNECTION),
        );
        let mut monitor = tree.monitor();

        let transition = only(monitor.tick(&at(0)));

        assert_eq!(transition.slot, Slot::Connection(CONNECTION.to_owned()));
        assert_eq!(transition.shell, 100);
        let entry = observation(&transition);
        assert_eq!(entry.correlation, None);
        assert_eq!(entry.ssh_connection.as_deref(), Some(CONNECTION));
        assert_eq!(entry.pid, 200);
        assert_eq!(entry.process, "claude");
    }

    #[test]
    fn the_second_correlation_name_is_read_where_the_first_is_unset() {
        let tree = Tree::new();
        tree.write(process(100, "bash").env(CORRELATION_FALLBACK_VAR, "w1"));
        let mut monitor = tree.monitor();

        let transition = only(monitor.tick(&at(0)));

        assert_eq!(transition.slot, Slot::Correlation("w1".to_owned()));
        assert_eq!(observation(&transition).correlation.as_deref(), Some("w1"));
    }

    #[test]
    fn the_first_correlation_name_is_the_one_that_counts_where_both_are_set() {
        let tree = Tree::new();
        tree.write(
            process(100, "bash")
                .correlated("the-one-here")
                .env(CORRELATION_FALLBACK_VAR, "the-one-that-arrived"),
        );
        let mut monitor = tree.monitor();

        assert_eq!(
            only(monitor.tick(&at(0))).slot,
            Slot::Correlation("the-one-here".to_owned())
        );
    }

    #[test]
    fn a_correlation_set_to_nothing_is_a_correlation_nobody_set() {
        let tree = Tree::new();
        tree.write(
            process(100, "bash")
                .correlated("")
                .env(CORRELATION_FALLBACK_VAR, "w1"),
        );
        let mut monitor = tree.monitor();

        assert_eq!(
            only(monitor.tick(&at(0))).slot,
            Slot::Correlation("w1".to_owned()),
            "an empty value ties nothing to anything, so the second name is read"
        );
    }

    #[test]
    fn a_shell_with_both_is_filed_under_its_correlation_and_reports_both() {
        const CONNECTION: &str = "10.0.0.5 51234 10.0.0.9 22";

        let tree = Tree::new();
        tree.write(
            process(100, "bash")
                .env(CORRELATION_FALLBACK_VAR, "w1")
                .env(SSH_CONNECTION_VAR, CONNECTION),
        );
        // Everything under it carries both, and none of it is a second shell.
        tree.write(
            process(200, "claude")
                .child_of(100)
                .in_group(100)
                .in_session(100)
                .env(CORRELATION_FALLBACK_VAR, "w1")
                .env(SSH_CONNECTION_VAR, CONNECTION),
        );
        let mut monitor = tree.monitor();

        let transition = only(monitor.tick(&at(0)));

        assert_eq!(transition.slot, Slot::Correlation("w1".to_owned()));
        let entry = observation(&transition);
        assert_eq!(entry.correlation.as_deref(), Some("w1"));
        assert_eq!(entry.ssh_connection.as_deref(), Some(CONNECTION));
    }

    #[test]
    fn the_one_connection_the_foreground_process_holds_is_reported_as_its_port() {
        let tree = Tree::new();
        tree.established(&[(1001, 51234)]);
        tree.write(process(100, "bash").fronted_by(200).correlated("pane-7"));
        tree.write(
            process(200, "ssh")
                .child_of(100)
                .in_group(200)
                .in_session(100)
                .holding(&[1001]),
        );
        let mut monitor = tree.monitor();

        let entry = observation(&only(monitor.tick(&at(0)))).clone();

        assert_eq!(entry.pid, 200);
        assert_eq!(entry.ssh_client_port, Some(51234));
    }

    #[test]
    fn a_foreground_process_with_no_connection_or_several_reports_no_port() {
        let tree = Tree::new();
        tree.established(&[(1001, 51234), (1002, 51235)]);
        tree.write(process(100, "bash").fronted_by(200).correlated("pane-a"));
        tree.write(
            process(200, "curl")
                .child_of(100)
                .in_group(200)
                .in_session(100)
                .holding(&[1001, 1002]),
        );
        tree.write(process(300, "bash").fronted_by(400).correlated("pane-b"));
        tree.write(
            process(400, "vim")
                .child_of(300)
                .in_group(400)
                .in_session(300),
        );
        let mut monitor = tree.monitor();

        let ports: Vec<Option<u32>> = monitor
            .tick(&at(0))
            .iter()
            .map(|transition| observation(transition).ssh_client_port)
            .collect();

        assert_eq!(ports, [None, None]);
    }

    #[test]
    fn a_connection_that_appears_under_the_foreground_process_is_a_transition() {
        let tree = Tree::new();
        tree.established(&[]);
        tree.write(process(100, "bash").fronted_by(200).correlated("pane-7"));
        tree.write(
            process(200, "ssh")
                .child_of(100)
                .in_group(200)
                .in_session(100)
                .holding(&[1001]),
        );
        let mut monitor = tree.monitor();
        assert_eq!(
            observation(&only(monitor.tick(&at(0)))).ssh_client_port,
            None
        );

        // The same process, now connected: the pid did not change, so nothing
        // about its identity did, and the port is still news.
        tree.established(&[(1001, 51234)]);
        let transition = only(monitor.tick(&at(1)));

        let entry = observation(&transition);
        assert_eq!(entry.pid, 200);
        assert_eq!(entry.ssh_client_port, Some(51234));
        assert_eq!(
            entry.since,
            at(0),
            "the process did not change, so how long it has held did not either"
        );
        assert!(monitor.tick(&at(2)).is_empty());
    }

    #[test]
    fn a_correlated_subtree_inside_a_connection_is_a_shell_of_its_own() {
        const CONNECTION: &str = "10.0.0.5 51234 10.0.0.9 22";

        let tree = Tree::new();
        // A login shell with no correlation, and an agent under it that was
        // started with one. Two roots, filed under two different things.
        tree.write(
            process(100, "bash")
                .fronted_by(200)
                .env(SSH_CONNECTION_VAR, CONNECTION),
        );
        tree.write(
            process(200, "bash")
                .child_of(100)
                .in_group(200)
                .in_session(100)
                .fronted_by(200)
                .correlated("w1")
                .env(SSH_CONNECTION_VAR, CONNECTION),
        );
        let mut monitor = tree.monitor();

        let slots: Vec<Slot> = monitor
            .tick(&at(0))
            .into_iter()
            .map(|transition| transition.slot)
            .collect();

        assert_eq!(
            slots,
            [
                Slot::Correlation("w1".to_owned()),
                Slot::Connection(CONNECTION.to_owned()),
            ]
        );
    }
}
