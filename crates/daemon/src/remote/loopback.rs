//! A transport whose far end is this machine.
//!
//! Everything the other transports do across a boundary, this one does across no
//! boundary at all: it runs commands here, copies files here, and its `uname` is
//! this machine's. What makes it useful rather than pointless is that it is
//! still driven entirely through [`Transport`], so the code that provisions and
//! attaches to an endpoint can be exercised whole — script, exit status, push,
//! retry — on a machine with no Docker daemon, no ssh server and no network.
//!
//! Its far end is a temporary directory. `HOME` points into it, so the places
//! the bootstrap script looks for somebody's own installation are places a test
//! can plant one, and `PATH` begins with a directory of its own, so a command
//! can be replaced with a script that says whatever the test needs it to say.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use super::transport::{Backoff, Error, Running, Transport};

/// The mode a planted command is given: runnable, by its owner alone.
const RUNNABLE: u32 = 0o700;

/// This machine, pretending to be another one.
#[derive(Debug)]
pub struct Loopback {
    root: tempfile::TempDir,
    copying: bool,
    copied: Mutex<Vec<(PathBuf, String)>>,
    told: Vec<(String, OsString)>,
}

impl Loopback {
    /// A far end with nothing on it.
    pub fn new() -> io::Result<Self> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("bin"))?;
        Ok(Self {
            root,
            copying: true,
            copied: Mutex::new(Vec::new()),
            told: Vec::new(),
        })
    }

    /// The same far end, with one more variable set on it.
    ///
    /// Where an installation of this program goes is worked out over there, out
    /// of variables only a shell running there can read, so a test that wants
    /// it somewhere else says so to the far end rather than to the code being
    /// tested. Applied after the variables this machine's are taken away, so a
    /// value said here is the far end's own however this machine is set up.
    pub fn told(mut self, name: &str, value: impl Into<OsString>) -> Self {
        self.told.push((name.to_owned(), value.into()));
        self
    }

    /// The same far end, but where a file copied to it arrives empty.
    ///
    /// A transfer that ends early is the shape of most of the ways a push goes
    /// wrong at a real endpoint — a link that dropped, a filesystem that filled
    /// — and it is the case the version check exists for: what arrives is a file
    /// at the right path that must never be executed.
    pub fn truncating_copies(mut self) -> Self {
        self.copying = false;
        self
    }

    /// The far end's home directory, which is the whole of the far end.
    pub fn home(&self) -> &Path {
        self.root.path()
    }

    /// The directory that comes first on the far end's `PATH`.
    pub fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    /// Writes `script` to the far end as a runnable command called `name`, in
    /// the directory that comes first on its `PATH`.
    pub fn plant(&self, name: &str, script: &str) -> io::Result<PathBuf> {
        let path = self.bin().join(name);
        // Written beside the name and renamed onto it, never onto it: this
        // process runs commands on several threads, and a fork that happens
        // while a file is open for writing inherits that handle — so a name
        // written in place is a name another thread's `exec` can be refused
        // with `ETXTBSY` for as long as the write is open.
        let writing = path.with_extension("writing");
        fs::write(&writing, script)?;
        fs::set_permissions(&writing, fs::Permissions::from_mode(RUNNABLE))?;
        fs::rename(&writing, &path)?;
        Ok(path)
    }

    /// Everything that has been copied to the far end, oldest first.
    pub fn copied(&self) -> Vec<(PathBuf, String)> {
        self.copied.lock().expect("the record was poisoned").clone()
    }

    /// Runs a command over there, letting the caller say one more thing about
    /// the process first.
    ///
    /// The hook is what a test uses to put something in the far end's
    /// environment without putting it in this process's, which for a transport
    /// whose far end is this machine is the only difference between the two.
    pub fn running(
        &self,
        command: &str,
        args: &[&str],
        stdin: Option<&str>,
        also: impl FnOnce(&mut Command),
    ) -> Result<Running, Error> {
        let mut process = Command::new(command);
        process.args(args);
        self.environment(&mut process);
        also(&mut process);
        Running::spawn(&mut process, stdin).map_err(|source| Error::Run {
            label: self.label(),
            command: command.to_owned(),
            source,
        })
    }

    /// The environment a command runs in over there.
    ///
    /// The far end is a machine with a home directory and nothing else said
    /// about it. Every variable that would otherwise point a program at a
    /// directory of this machine's — where a user's configuration is, where
    /// their state goes, where a session's runtime files are — is taken away,
    /// because a far end that quietly inherited one of those would be a test
    /// writing into whoever is running it.
    fn environment(&self, command: &mut Command) {
        command
            .env("PATH", self.path())
            .env("HOME", self.root.path())
            .env("AGENTBUS_DIR", self.root.path().join("bus"))
            .env_remove("AGENTBUS_REMOTE_BINARY")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_RUNTIME_DIR");
        for (name, value) in &self.told {
            command.env(name, value);
        }
    }

    /// The far end's `PATH`: its own directory, then this machine's, so that the
    /// ordinary commands a script uses are still there.
    fn path(&self) -> OsString {
        let mut path = OsString::from(self.bin());
        if let Some(inherited) = std::env::var_os("PATH") {
            path.push(":");
            path.push(inherited);
        }
        path
    }
}

impl Transport for Loopback {
    fn kind(&self) -> &'static str {
        "loopback"
    }

    fn label(&self) -> String {
        "loopback".to_owned()
    }

    fn identity(&self) -> Option<String> {
        Some("loopback".to_owned())
    }

    /// Under the far end's home directory, which is one of the places the
    /// bootstrap script looks: a copy put here is a copy the next run finds.
    fn install_path(&self, _version: &str) -> String {
        self.root
            .path()
            .join(".local/bin/agentbus")
            .display()
            .to_string()
    }

    fn run(&self, command: &str, args: &[&str], stdin: Option<&str>) -> Result<Running, Error> {
        self.running(command, args, stdin, |_| {})
    }

    fn copy_in(&self, local: &Path, remote: &str) -> Result<(), Error> {
        let failed = |source| Error::Copy {
            label: self.label(),
            local: local.to_owned(),
            remote: remote.to_owned(),
            source,
        };
        let remote_path = Path::new(remote);
        if let Some(parent) = remote_path.parent() {
            fs::create_dir_all(parent).map_err(failed)?;
        }
        match self.copying {
            true => fs::copy(local, remote_path).map(|_| ()).map_err(failed)?,
            false => fs::write(remote_path, []).map_err(failed)?,
        }
        self.copied
            .lock()
            .expect("the record was poisoned")
            .push((local.to_owned(), remote.to_owned()));
        Ok(())
    }

    fn backoff(&self) -> Backoff {
        Backoff {
            initial: Duration::from_millis(50),
            max: Duration::from_secs(1),
            multiplier: 2.0,
            jitter: 0.0,
        }
    }
}
