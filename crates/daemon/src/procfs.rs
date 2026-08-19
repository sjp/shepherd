//! Reading a process table out of a procfs mount.
//!
//! This is a reading layer and nothing else: it has no timers, no threads and no
//! state beyond the directory it was pointed at, and it knows the meaning of no
//! process name and no environment variable. Everything it can be asked is a
//! question about one pid, answered from the files the kernel exposes under
//! `/proc/<pid>`.
//!
//! # The root is a parameter
//!
//! A [`ProcFs`] is constructed from a path. A daemon passes [`DEFAULT_ROOT`]; a
//! test passes a directory of files it wrote itself. Nothing here reaches for
//! `/proc` on its own, which is what makes every case that matters — a name with
//! parentheses in it, a process that vanished mid-read, an `environ` nobody is
//! allowed to open — reachable from a test rather than only from a machine that
//! happens to be in the right state.
//!
//! It follows that the module is portable by construction. It is `std::fs` over
//! a path, so it compiles anywhere; on a system with no procfs the answer to
//! every question is simply that there is nothing there.
//!
//! # Everything fails soft
//!
//! A process may exit between one read and the next, and reading another user's
//! `environ` requires the same uid or `CAP_SYS_PTRACE`. Neither is a fault: the
//! caller is sampling a table that changes underneath it, and "no answer" is an
//! ordinary answer that has to be as cheap to handle as a real one. So every
//! operation returns `None`, or nothing, and writes one debug line saying which
//! file it was and what the system said. There is no error type to match on.
//!
//! # Parsing `stat`
//!
//! The one place this is subtle is the `stat` file, whose second field is the
//! executable's name and may itself contain spaces and parentheses:
//!
//! ```text
//! 400 (my (weird) proc) S 1 400 400 34817 401 4194304 ...
//! ```
//!
//! Splitting the line on whitespace therefore gets the wrong answer for a name
//! that is quite legal to have. The name runs from the first `(` to the **last**
//! `)`, and everything after that last `)` is fixed-position and
//! whitespace-separated. [`Stat::parse`] works that way round and never any
//! other.

use std::fs;
use std::path::{Path, PathBuf};

use tracing::debug;

/// Where a Linux kernel mounts procfs.
///
/// Held here as a constant rather than reached for directly, because the whole
/// of this module's testability rests on the root being something a caller
/// chooses; see [`ProcFs::new`].
pub const DEFAULT_ROOT: &str = "/proc";

/// A process id, signed as the kernel reports it.
///
/// The signedness is load-bearing rather than incidental: `tpgid` is `-1` for a
/// process with no controlling terminal, and a caller that resolves a foreground
/// group hands that field straight back to [`ProcFs::stat`]. Keeping one type
/// throughout means that call needs no conversion and no guard — there is no
/// `/proc/-1`, so the answer is `None`, which is the answer it wanted.
pub type Pid = i32;

/// The fields of `/proc/<pid>/stat` that say who a process is and which terminal
/// it is attached to.
///
/// The kernel writes fifty-odd fields there, nearly all of them accounting. The
/// ones kept are those that answer identity and foreground questions; the rest
/// are parsed past and discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    /// The process's own id, as the file reports it.
    pub pid: Pid,
    /// The executable name, without the parentheses the kernel wraps it in. May
    /// contain spaces and parentheses, and is truncated by the kernel to fifteen
    /// bytes.
    pub comm: String,
    /// The one-character run state: `R` running, `S` sleeping, `Z` zombie, `T`
    /// stopped, and the rest of the set `proc(5)` lists.
    pub state: char,
    /// The parent's id.
    pub ppid: Pid,
    /// The id of the process group this process belongs to.
    pub pgrp: Pid,
    /// The id of the session this process belongs to.
    pub session: Pid,
    /// The controlling terminal, as a packed device number; `0` for a process
    /// that has none.
    pub tty_nr: i32,
    /// The process group in the foreground of that controlling terminal, or `-1`
    /// where there is no terminal.
    pub tpgid: Pid,
}

impl Stat {
    /// Parses the contents of a `stat` file.
    ///
    /// `None` for anything that is not one — a truncated read, a file that was
    /// replaced by something else, a process that exited while the read was in
    /// flight and left a short line behind.
    pub fn parse(text: &str) -> Option<Self> {
        // The name is delimited by the first `(` and the last `)` precisely
        // because it may contain either; both are ASCII, so these are character
        // boundaries and the slices below are safe.
        let open = text.find('(')?;
        let close = text.rfind(')')?;
        if close < open {
            return None;
        }

        let pid = text[..open].trim().parse().ok()?;
        let comm = text[open + 1..close].to_owned();

        let mut fields = text[close + 1..].split_ascii_whitespace();
        let state = one_char(fields.next()?)?;
        Some(Self {
            pid,
            comm,
            state,
            ppid: fields.next()?.parse().ok()?,
            pgrp: fields.next()?.parse().ok()?,
            session: fields.next()?.parse().ok()?,
            tty_nr: fields.next()?.parse().ok()?,
            tpgid: fields.next()?.parse().ok()?,
        })
    }

    /// Whether this process leads its own process group.
    ///
    /// A foreground process group may hold several processes — every stage of a
    /// shell pipeline is in one — and the one that stands for the group is the
    /// leader.
    pub fn is_group_leader(&self) -> bool {
        self.pid == self.pgrp
    }
}

/// A procfs mount, or a directory laid out like one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcFs {
    root: PathBuf,
}

impl ProcFs {
    /// Reads the process table under `root`.
    ///
    /// Nothing is opened here and nothing is checked: a root that does not exist
    /// is not an error, it is a process table with no processes in it.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory this handle reads.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory a process's files live in.
    pub fn dir(&self, pid: Pid) -> PathBuf {
        self.root.join(pid.to_string())
    }

    /// Whether a process is still there.
    ///
    /// This is one `lstat` and no reads, which is what makes it affordable to
    /// ask about every tracked process on every tick. Absence is the expected
    /// answer often enough that it is not worth logging.
    pub fn exists(&self, pid: Pid) -> bool {
        fs::symlink_metadata(self.dir(pid)).is_ok()
    }

    /// Every process in the table, in ascending order.
    ///
    /// A procfs root holds a good deal besides process directories — `meminfo`,
    /// `self`, `net` — so entries whose names are not purely decimal are passed
    /// over. The ordering is not procfs's, which is arbitrary; it is imposed
    /// here so that a caller iterating the table twice sees the same table.
    pub fn pids(&self) -> Vec<Pid> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) => {
                debug!(path = %self.root.display(), %error, "cannot list the process table");
                return Vec::new();
            }
        };
        let mut pids: Vec<Pid> = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                name.bytes()
                    .all(|byte| byte.is_ascii_digit())
                    .then(|| name.parse().ok())
                    .flatten()
            })
            .collect();
        pids.sort_unstable();
        pids
    }

    /// The `stat` file of one process, parsed.
    pub fn stat(&self, pid: Pid) -> Option<Stat> {
        let bytes = self.read(pid, "stat")?;
        let text = String::from_utf8_lossy(&bytes);
        match Stat::parse(&text) {
            Some(stat) => Some(stat),
            None => {
                debug!(pid, "the stat file is not a stat line");
                None
            }
        }
    }

    /// The executable name of one process, with the newline procfs ends it with
    /// taken off.
    ///
    /// This is the same name [`Stat::comm`] carries, from a file that holds
    /// nothing else. Reading it is cheaper than reading `stat` when the name is
    /// all that is wanted.
    pub fn comm(&self, pid: Pid) -> Option<String> {
        let bytes = self.read(pid, "comm")?;
        let text = String::from_utf8_lossy(&bytes);
        Some(text.trim_end_matches('\n').to_owned())
    }

    /// The argument vector of one process.
    ///
    /// An empty vector is a real answer rather than a failure: a kernel thread
    /// has no arguments, and a process part-way through exiting has already
    /// given its up. `None` means the file could not be read at all.
    pub fn cmdline(&self, pid: Pid) -> Option<Vec<String>> {
        let bytes = self.read(pid, "cmdline")?;
        Some(
            nul_separated(&bytes)
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect(),
        )
    }

    /// The environment of one process, in the order the kernel holds it.
    ///
    /// A list rather than a map, because an environment block may name the same
    /// variable twice and a map would have to decide which one to lose. Entries
    /// with no `=` in them are not variables and are passed over.
    ///
    /// `None` where the file cannot be opened, which for a process belonging to
    /// another user is the normal outcome — reading it needs the same uid or
    /// `CAP_SYS_PTRACE` — and is not a fault of any kind.
    pub fn environ(&self, pid: Pid) -> Option<Vec<(String, String)>> {
        let bytes = self.read(pid, "environ")?;
        Some(
            nul_separated(&bytes)
                .filter_map(|entry| {
                    let equals = entry.iter().position(|byte| *byte == b'=')?;
                    Some((
                        String::from_utf8_lossy(&entry[..equals]).into_owned(),
                        String::from_utf8_lossy(&entry[equals + 1..]).into_owned(),
                    ))
                })
                .collect(),
        )
    }

    /// One variable from the environment of one process.
    ///
    /// Stops at the first entry that matches, so a caller looking for a single
    /// name over a whole process table never builds the rest of the block.
    pub fn environ_var(&self, pid: Pid, name: &str) -> Option<String> {
        let bytes = self.read(pid, "environ")?;
        let name = name.as_bytes();
        nul_separated(&bytes).find_map(|entry| {
            let value = entry.strip_prefix(name)?.strip_prefix(b"=")?;
            Some(String::from_utf8_lossy(value).into_owned())
        })
    }

    /// The working directory of one process.
    ///
    /// Procfs answers this with a symbolic link, and the link is read rather
    /// than followed: the target may be a directory this process cannot enter,
    /// or one that has since been deleted, and the name of it is the answer
    /// either way.
    pub fn cwd(&self, pid: Pid) -> Option<PathBuf> {
        let path = self.dir(pid).join("cwd");
        match fs::read_link(&path) {
            Ok(target) => Some(target),
            Err(error) => {
                debug!(path = %path.display(), %error, "cannot read");
                None
            }
        }
    }

    /// One of a process's files, whole.
    fn read(&self, pid: Pid, name: &str) -> Option<Vec<u8>> {
        let path = self.dir(pid).join(name);
        match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                debug!(path = %path.display(), %error, "cannot read");
                None
            }
        }
    }
}

/// The entries of a NUL-separated procfs file.
///
/// The kernel terminates these files rather than separating with them, so the
/// last entry is followed by a NUL and a naive split would report a trailing
/// empty entry that is not there. Exactly one trailing NUL is taken off, and
/// nothing else is dropped: an empty entry in the middle of an argument vector
/// is an empty argument, which the process really was passed.
fn nul_separated(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let body = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    let empty = body.is_empty();
    body.split(|byte| *byte == 0).skip(usize::from(empty))
}

/// A string that is exactly one character, as that character.
fn one_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;

    /// One of the synthetic process tables beside this crate's tests.
    fn fixture(scenario: &str) -> ProcFs {
        ProcFs::new(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/proc")
                .join(scenario),
        )
    }

    /// A procfs root that the test can then write to or change the modes in.
    fn writable_copy(scenario: &str) -> (tempfile::TempDir, ProcFs) {
        let source = fixture(scenario);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proc");
        copy_tree(source.root(), &root);
        let proc = ProcFs::new(&root);
        (dir, proc)
    }

    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                copy_tree(&entry.path(), &target);
            } else if kind.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(entry.path()).unwrap(), target).unwrap();
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    /// Whether this test is running with the privilege to ignore file modes.
    fn is_root() -> bool {
        // Safe by construction: `geteuid` takes nothing, cannot fail and touches
        // no memory this process owns.
        (unsafe { libc::geteuid() }) == 0
    }

    #[test]
    fn a_name_with_spaces_and_parentheses_survives_parsing() {
        let stat = fixture("weird-comm").stat(400).unwrap();

        assert_eq!(
            stat,
            Stat {
                pid: 400,
                comm: "my (weird) proc".to_owned(),
                state: 'S',
                ppid: 1,
                pgrp: 400,
                session: 400,
                tty_nr: 34817,
                tpgid: 401,
            }
        );
    }

    #[test]
    fn a_shell_holding_its_own_terminal_is_its_own_foreground_group() {
        let stat = fixture("basic").stat(100).unwrap();

        assert_eq!(stat.comm, "bash");
        assert_eq!(stat.tpgid, stat.pgrp);
        assert!(stat.is_group_leader());
    }

    #[test]
    fn a_shell_reports_the_foreground_group_of_the_agent_in_front_of_it() {
        let proc = fixture("agent-foreground");

        let shell = proc.stat(100).unwrap();
        let agent = proc.stat(shell.tpgid).unwrap();

        assert_eq!(agent.pid, 200);
        assert_eq!(agent.comm, "claude");
        assert_eq!(agent.ppid, 100);
        assert!(agent.is_group_leader());
    }

    #[test]
    fn every_stage_of_a_pipeline_shares_one_foreground_group() {
        let proc = fixture("pipeline");
        let shell = proc.stat(100).unwrap();

        let members: Vec<Stat> = [300, 301, 302].map(|pid| proc.stat(pid).unwrap()).into();

        assert!(members.iter().all(|stat| stat.pgrp == shell.tpgid));
        assert!(members.iter().all(|stat| stat.pgrp == 300));
        let leaders: Vec<Pid> = members
            .iter()
            .filter(|stat| stat.is_group_leader())
            .map(|stat| stat.pid)
            .collect();
        assert_eq!(leaders, [300]);
    }

    #[test]
    fn a_foreground_group_that_has_already_exited_reads_as_nothing() {
        let proc = fixture("vanished");
        let shell = proc.stat(100).unwrap();

        assert_eq!(shell.tpgid, 999);
        assert!(!proc.exists(999));
        assert!(proc.stat(999).is_none());
        assert!(proc.comm(999).is_none());
        assert!(proc.cmdline(999).is_none());
        assert!(proc.environ(999).is_none());
        assert!(proc.cwd(999).is_none());
    }

    #[test]
    fn a_process_that_is_there_exists_and_one_that_never_was_does_not() {
        let proc = fixture("basic");

        assert!(proc.exists(100));
        assert!(!proc.exists(101));
        assert!(!proc.exists(-1));
    }

    #[test]
    fn the_process_table_is_the_numeric_entries_of_the_root_in_order() {
        assert_eq!(fixture("pipeline").pids(), [100, 300, 301, 302]);
        // `basic` also holds a `meminfo` file and a `self` symlink, as a real
        // procfs root does.
        assert_eq!(fixture("basic").pids(), [100]);
    }

    #[test]
    fn a_root_with_no_process_table_in_it_answers_nothing_to_everything() {
        let dir = tempfile::tempdir().unwrap();
        let proc = ProcFs::new(dir.path().join("no-such-directory"));

        assert!(proc.pids().is_empty());
        assert!(!proc.exists(1));
        assert!(proc.stat(1).is_none());
        assert!(proc.comm(1).is_none());
        assert!(proc.cmdline(1).is_none());
        assert!(proc.environ(1).is_none());
        assert!(proc.environ_var(1, "PATH").is_none());
        assert!(proc.cwd(1).is_none());
    }

    #[test]
    fn the_argument_vector_and_the_working_directory_are_read_as_written() {
        let proc = fixture("agent-foreground");

        assert_eq!(
            proc.cmdline(200).unwrap(),
            ["node", "/usr/local/bin/claude"]
        );
        assert_eq!(proc.comm(200).unwrap(), "claude");
        assert_eq!(proc.cwd(200).unwrap(), Path::new("/workspaces/project"));
    }

    #[test]
    fn a_process_with_no_arguments_has_an_empty_argument_vector() {
        let proc = fixture("kernel-thread");

        assert_eq!(proc.cmdline(2).unwrap(), Vec::<String>::new());
        assert_eq!(proc.environ(2).unwrap(), Vec::new());
        assert_eq!(proc.stat(2).unwrap().tpgid, -1);
    }

    #[test]
    fn an_empty_argument_in_the_middle_of_a_vector_is_kept() {
        let (_dir, proc) = writable_copy("basic");
        fs::write(proc.dir(100).join("cmdline"), b"sh\0-c\0\0tail\0").unwrap();

        assert_eq!(proc.cmdline(100).unwrap(), ["sh", "-c", "", "tail"]);
    }

    #[test]
    fn a_planted_variable_is_found_and_an_absent_one_is_not() {
        let proc = fixture("basic");

        assert_eq!(proc.environ_var(100, "AGENTBUS_PANE").unwrap(), "pane-7");
        assert_eq!(proc.environ_var(100, "HOME").unwrap(), "/home/vscode");
        assert!(proc.environ_var(100, "NOT_SET").is_none());
        // A prefix of a name that is set is not that name.
        assert!(proc.environ_var(100, "HOM").is_none());
    }

    #[test]
    fn the_whole_environment_comes_back_in_the_order_it_is_held() {
        let names: Vec<String> = fixture("basic")
            .environ(100)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();

        assert_eq!(names, ["PATH", "HOME", "AGENTBUS_PANE", "TERM"]);
    }

    #[test]
    fn an_environment_entry_that_is_not_an_assignment_is_passed_over() {
        let (_dir, proc) = writable_copy("basic");
        fs::write(proc.dir(100).join("environ"), b"A=1\0rubbish\0B=\0A=2\0").unwrap();

        assert_eq!(
            proc.environ(100).unwrap(),
            [
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), String::new()),
                ("A".to_owned(), "2".to_owned()),
            ]
        );
        assert_eq!(proc.environ_var(100, "A").unwrap(), "1");
    }

    #[test]
    fn an_environment_nobody_may_open_is_no_environment() {
        if is_root() {
            eprintln!("skipped: running as root, which file modes do not apply to");
            return;
        }
        let (_dir, proc) = writable_copy("unreadable-environ");
        let path = proc.dir(100).join("environ");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        assert!(proc.environ(100).is_none());
        assert!(proc.environ_var(100, "AGENTBUS_PANE").is_none());
        // Everything else about the process is still readable.
        assert_eq!(proc.stat(100).unwrap().comm, "bash");
    }

    #[test]
    fn a_stat_line_that_stops_early_is_refused() {
        for short in [
            "100",
            "100 (bash)",
            "100 (bash) S",
            "100 (bash) S 99",
            "100 (bash) S 99 100",
            "100 (bash) S 99 100 100",
            "100 (bash) S 99 100 100 34816",
        ] {
            assert!(
                Stat::parse(short).is_none(),
                "parsed a line missing a field: {short:?}"
            );
        }
        assert!(Stat::parse("100 (bash) S 99 100 100 34816 100").is_some());

        // A read that stopped inside a number leaves a shorter number, which
        // nothing can tell from the number having been shorter. All that can be
        // asked of the parser there is that it survives every prefix.
        let full = "100 (bash) S 99 100 100 34816 100 4194304 3400";
        for end in 0..=full.len() {
            let _ = Stat::parse(&full[..end]);
        }
    }

    #[test]
    fn a_stat_line_that_is_not_one_is_refused() {
        for text in [
            "",
            "100",
            "100 bash S 99 100 100 34816 100",
            "100 (bash S 99 100 100 34816 100",
            "100 bash) S 99 100 100 34816 100",
            ") 100 (bash S 99 100 100 34816 100",
            "(bash) S 99 100 100 34816 100",
            "abc (bash) S 99 100 100 34816 100",
            "100 (bash) SS 99 100 100 34816 100",
            "100 (bash) S 99 100 100 34816 abc",
            "100 (bash) S 99 100 100 34816 1.5",
        ] {
            assert!(Stat::parse(text).is_none(), "parsed {text:?}");
        }
    }

    #[test]
    fn a_stat_file_that_is_not_a_stat_line_reads_as_nothing() {
        let (_dir, proc) = writable_copy("basic");
        fs::write(proc.dir(100).join("stat"), b"\xff\xfe not a stat line").unwrap();

        assert!(proc.stat(100).is_none());
    }

    #[test]
    fn no_bytes_at_all_make_the_stat_parser_panic() {
        // A deterministic generator, so a failure reproduces from the seed
        // rather than from luck.
        struct Rng(u64);
        impl Rng {
            fn next(&mut self) -> u64 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                self.0
            }
        }

        let mut rng = Rng(0x5eed_1234_9abc_def1);
        let template = b"100 (bash) S 99 100 100 34816 100 4194304 3400 0 0 0";

        for _ in 0..20_000 {
            let length = (rng.next() % 64) as usize;
            let noise: Vec<u8> = (0..length).map(|_| (rng.next() & 0xff) as u8).collect();
            let _ = Stat::parse(&String::from_utf8_lossy(&noise));

            // The same again, but starting from something that is nearly a stat
            // line: random bytes rarely contain a balanced pair of parentheses,
            // and the interesting failures are all past that point.
            let mut mutated = template.to_vec();
            for _ in 0..1 + rng.next() % 4 {
                let at = (rng.next() as usize) % mutated.len();
                match rng.next() % 3 {
                    0 => mutated[at] = (rng.next() & 0xff) as u8,
                    1 => mutated.truncate(at),
                    _ => mutated.insert(at, b")("[(rng.next() % 2) as usize]),
                }
                if mutated.is_empty() {
                    mutated.push(b' ');
                }
            }
            let _ = Stat::parse(&String::from_utf8_lossy(&mutated));
        }
    }

    /// The fixtures above are this module's own idea of what procfs looks like,
    /// so on a machine that has a real one it is worth asking the real one the
    /// same questions. Everything asserted here is true of any live process;
    /// nothing depends on which process is running the test.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_process_reads_the_same_way_the_fixtures_do() {
        let proc = ProcFs::new(DEFAULT_ROOT);
        let me = Pid::try_from(std::process::id()).unwrap();

        assert!(proc.exists(me));
        assert!(proc.pids().contains(&me));

        let stat = proc.stat(me).unwrap();
        assert_eq!(stat.pid, me);
        assert!(stat.ppid > 0);
        assert!(stat.session > 0);
        assert!(
            "RSDZTtWXxKPI".contains(stat.state),
            "unknown run state {:?}",
            stat.state
        );

        // The `comm` file holds the same name the `stat` line does, truncated
        // by the kernel the same way.
        assert_eq!(proc.comm(me).unwrap(), stat.comm);
        assert!(!proc.cmdline(me).unwrap().is_empty());
        assert_eq!(proc.cwd(me).unwrap(), std::env::current_dir().unwrap());

        // The environment procfs reports is the one this process was executed
        // with, so only a variable inherited from the caller can be checked.
        let path = std::env::var("PATH").unwrap();
        assert_eq!(proc.environ_var(me, "PATH").unwrap(), path);
        assert!(
            proc.environ(me)
                .unwrap()
                .iter()
                .any(|(name, value)| { name == "PATH" && *value == path })
        );
    }

    #[test]
    fn the_root_is_whatever_it_was_constructed_from() {
        let proc = ProcFs::new(DEFAULT_ROOT);

        assert_eq!(proc.root(), Path::new("/proc"));
        assert_eq!(proc.dir(4471), Path::new("/proc/4471"));
    }
}
