//! A process table written as files, for the tests whose subject is what a
//! daemon makes of one.
//!
//! The reader a daemon uses takes the root it reads from, which is what makes
//! this possible: every state worth asserting — a command replacing another, a
//! process suspended, a process that ended — lasts a few milliseconds on a real
//! machine and cannot be asked for on purpose. Written as files they are all
//! just writes, and the daemon under test is the shipped binary reading what it
//! is pointed at, exactly as it reads `/proc`.

use std::fs;
use std::path::{Path, PathBuf};

/// A process table written as files, which a test may then change.
pub struct Tree {
    dir: tempfile::TempDir,
}

impl Tree {
    /// An empty table.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("cannot make a temporary directory");
        fs::create_dir(dir.path().join("proc")).expect("cannot make the process table");
        Self { dir }
    }

    /// The root a daemon is pointed at.
    pub fn root(&self) -> PathBuf {
        self.dir.path().join("proc")
    }

    /// Writes one process, replacing it if it is already there.
    pub fn write(&self, process: &Process) {
        let dir = self.root().join(process.pid.to_string());
        fs::create_dir_all(&dir).expect("cannot make a process directory");
        write(
            &dir.join("stat"),
            format!(
                "{} ({}) S {} {} {} 34816 {} 4194304 0 0 0 0 5 2 0 0 20 0 1 0 0\n",
                process.pid,
                process.comm,
                process.ppid,
                process.pgrp,
                process.session,
                process.tpgid,
            )
            .into_bytes(),
        );
        write(
            &dir.join("comm"),
            format!("{}\n", process.comm).into_bytes(),
        );
        write(&dir.join("cmdline"), nul_terminated(&process.cmdline));
        let environ: Vec<String> = process
            .environ
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        let environ: Vec<&str> = environ.iter().map(String::as_str).collect();
        write(&dir.join("environ"), nul_terminated(&environ));
    }

    /// Takes one process out of the table, as an exit does.
    pub fn remove(&self, pid: i32) {
        fs::remove_dir_all(self.root().join(pid.to_string())).expect("cannot remove a process");
    }
}

/// One process to write into a [`Tree`].
pub struct Process {
    /// The process id, which is also the name of its directory.
    pub pid: i32,
    /// The name in `stat` and `comm`.
    pub comm: &'static str,
    /// The parent.
    pub ppid: i32,
    /// The process group this process is in.
    pub pgrp: i32,
    /// The session this process is in.
    pub session: i32,
    /// The process group in front of this process's terminal, or `-1` for a
    /// process with no controlling terminal.
    pub tpgid: i32,
    /// The argument vector.
    pub cmdline: Vec<&'static str>,
    /// The environment, as pairs.
    pub environ: Vec<(&'static str, String)>,
}

impl Process {
    /// A process that is its own group and session leader, with nothing in its
    /// environment and no terminal.
    pub fn new(pid: i32, comm: &'static str) -> Self {
        Self {
            pid,
            comm,
            ppid: 1,
            pgrp: pid,
            session: pid,
            tpgid: -1,
            cmdline: vec![comm],
            environ: Vec::new(),
        }
    }
}

/// A shell carrying `correlation`, with `foreground` in front of its terminal.
pub fn shell(pid: i32, correlation: &str, foreground: i32) -> Process {
    Process {
        tpgid: foreground,
        cmdline: vec!["-bash"],
        environ: vec![("AGENTBUS_PANE", correlation.to_owned())],
        ..Process::new(pid, "bash")
    }
}

/// A process in the foreground of the terminal a shell owns.
pub fn running(pid: i32, comm: &'static str, cmdline: Vec<&'static str>) -> Process {
    Process {
        cmdline,
        ..Process::new(pid, comm)
    }
}

/// Writes a file, replacing whatever was there, in one step.
///
/// A rename rather than a truncate-and-write because a daemon is reading these
/// while they change: a kernel hands out a whole `stat` line or nothing, and a
/// test whose files could be read half-written would be exercising a state no
/// real process table is ever in.
fn write(path: &Path, contents: Vec<u8>) {
    let writing = path.with_extension("writing");
    fs::write(&writing, contents)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", writing.display()));
    fs::rename(&writing, path)
        .unwrap_or_else(|error| panic!("cannot replace {}: {error}", path.display()));
}

/// The bytes a process table holds a NUL-separated file in.
fn nul_terminated(entries: &[&str]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in entries {
        bytes.extend_from_slice(entry.as_bytes());
        bytes.push(0);
    }
    bytes
}
