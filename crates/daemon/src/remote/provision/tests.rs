//! What installing this program on a machine does to that machine.
//!
//! The far end is a temporary directory with a home directory in it, reached
//! through the same [`Transport`](super::Transport) an ssh host is reached
//! through, and every one of these runs the real scripts through a real shell
//! and then looks at the filesystem. That is where the interesting part is: not
//! in the Rust, but in which file ended up at which path with which mode, what
//! was left alone, and what a second run did — which is supposed to be nothing.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use super::{Error, Hooks, Provision};
use crate::VERSION;
use crate::remote::bootstrap::{Bootstrap, TARGET};
use crate::remote::loopback::Loopback;
use crate::remote::release::Release;

/// A shell script that answers `--version` with `version` and otherwise says
/// what it was run as and what it was asked to do.
///
/// The path it reports is the point of the second line: what installs hooks at
/// a far end is this program run by an absolute path, and a hook naming
/// anything else would be a hook resolved against somebody else's `PATH`.
fn agentbus(version: &str) -> String {
    format!(
        "#!/bin/sh\n\
         if [ \"$1\" = --version ]; then echo \"agentbus {version}\"; exit 0; fi\n\
         echo \"ran: $0 $*\"\n"
    )
}

/// A file standing in for this program's own executable, ready to be sent.
///
/// Deliberately not made runnable: what makes an installed copy runnable is the
/// installation, and a stand-in that arrived already executable would hide it
/// failing to do that.
struct Local {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Local {
    fn answering(version: &str) -> Self {
        let dir = tempfile::tempdir().expect("cannot make a temporary directory");
        let path = dir.path().join("agentbus");
        fs::write(&path, agentbus(version)).expect("cannot write the stand-in");
        Self { _dir: dir, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

/// A provisioner that sends `local` rather than whatever executable the tests
/// themselves are running from, and that has nowhere to fetch from.
///
/// The release that is not there is deliberate: a test that unexpectedly
/// reached the fetch path would go to the network, and this way it fails
/// locally instead.
fn sending(local: &Local) -> Bootstrap {
    Bootstrap::new(VERSION)
        .sending(local.path(), TARGET)
        .fetching(Release::at("file:///no/such/release", VERSION))
}

/// Everything the far end has to say about itself, as paths on this machine.
struct Machine {
    far: Loopback,
}

impl Machine {
    fn new() -> Self {
        Self {
            far: Loopback::new().expect("cannot make a far end"),
        }
    }

    /// Where an installed copy goes.
    fn binary(&self) -> PathBuf {
        self.far.home().join(super::BINARY)
    }

    /// Where a copy is written before it is moved onto that name.
    fn partial(&self) -> PathBuf {
        self.far.home().join(super::PARTIAL)
    }

    /// Where the record of what was installed is kept.
    fn marker(&self) -> PathBuf {
        self.far.home().join(super::MARKER)
    }

    /// Puts a runnable `agentbus` of `version` at `path`, as somebody else's
    /// installation or as an older one of this program's.
    fn plant(&self, path: &Path, version: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("cannot make the directory");
        }
        fs::write(path, agentbus(version)).expect("cannot write it");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("cannot make it run");
    }

    /// Installs, and hands back everything that was said about it.
    fn install(&self, local: &Local, hooks: Hooks) -> Result<String, Error> {
        let bootstrap = sending(local);
        let mut out = Vec::new();
        Provision::new(&bootstrap).install(&self.far, hooks, &mut out)?;
        Ok(String::from_utf8(out).expect("what was said is not text"))
    }

    /// Uninstalls, and hands back everything that was said about it.
    fn uninstall(&self, local: &Local, hooks: Hooks) -> Result<String, Error> {
        let bootstrap = sending(local);
        let mut out = Vec::new();
        Provision::new(&bootstrap).uninstall(&self.far, hooks, &mut out)?;
        Ok(String::from_utf8(out).expect("what was said is not text"))
    }
}

/// The mode a file is kept at, without the bits nobody here is asserting about.
fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("it is not there").mode() & 0o777
}

/// Which file a path names, so that a file written again is told from one that
/// was left alone whatever the clock's resolution is.
fn which(path: &Path) -> u64 {
    fs::metadata(path).expect("it is not there").ino()
}

#[test]
fn a_machine_with_nothing_on_it_gets_one_copy_and_a_record_of_it() {
    let machine = Machine::new();
    let local = Local::answering(VERSION);

    let printed = machine
        .install(&local, Hooks::Untouched)
        .expect("nothing was installed");

    let binary = machine.binary();
    assert!(binary.is_file(), "nothing was installed at {binary:?}");
    assert_eq!(mode(&binary), 0o755);
    assert_eq!(fs::read_to_string(&binary).unwrap(), agentbus(VERSION));
    assert!(
        !machine.partial().exists(),
        "the copy was left at the name it arrived under as well"
    );
    assert_eq!(
        fs::read_to_string(machine.marker()).unwrap(),
        format!("version={VERSION}\npath={}\n", binary.display())
    );
    // What was sent went to a name nothing runs, and was moved onto the one
    // that is run only once it had answered for itself.
    assert_eq!(
        machine.far.copied(),
        vec![(
            local.path().to_owned(),
            machine.partial().display().to_string()
        )]
    );
    assert!(
        printed.contains(&format!(
            "installed agentbus {VERSION} at {}",
            binary.display()
        )),
        "{printed}"
    );
}

#[test]
fn a_second_run_writes_nothing_and_says_where_the_copy_is() {
    let machine = Machine::new();
    let local = Local::answering(VERSION);
    machine
        .install(&local, Hooks::Untouched)
        .expect("nothing was installed");
    let (binary, marker) = (which(&machine.binary()), which(&machine.marker()));

    let printed = machine
        .install(&local, Hooks::Untouched)
        .expect("the second run failed");

    assert_eq!(
        which(&machine.binary()),
        binary,
        "the copy that was there was written again"
    );
    assert_eq!(
        which(&machine.marker()),
        marker,
        "the record that was there was written again"
    );
    assert_eq!(
        machine.far.copied().len(),
        1,
        "a machine that was already current was sent another copy"
    );
    assert!(
        printed.contains(&format!(
            "agentbus {VERSION} is already at {}",
            machine.binary().display()
        )),
        "{printed}"
    );
}

#[test]
fn somebody_elses_installation_is_reported_and_a_copy_is_put_beside_it() {
    let machine = Machine::new();
    let local = Local::answering(VERSION);
    // On the far end's `PATH`, which is where a package manager's would be.
    let theirs = machine.far.bin().join("agentbus");
    machine.plant(&theirs, "0.0.1-theirs");

    let printed = machine
        .install(&local, Hooks::Untouched)
        .expect("nothing was installed");

    assert_eq!(
        fs::read_to_string(&theirs).unwrap(),
        agentbus("0.0.1-theirs"),
        "somebody else's installation was written over"
    );
    assert!(
        printed.contains(&format!(
            "found \"agentbus 0.0.1-theirs\" at {}; expected agentbus {VERSION}; leaving it alone",
            theirs.display()
        )),
        "{printed}"
    );
    assert!(machine.binary().is_file(), "nothing was installed");
}

#[test]
fn a_copy_this_did_not_install_is_not_written_over() {
    let machine = Machine::new();
    let local = Local::answering(VERSION);
    // At the one path this writes to, and with no record of this having put it
    // there — which is the whole of what makes it somebody else's.
    machine.plant(&machine.binary(), "0.0.1-theirs");

    let error = machine
        .install(&local, Hooks::Untouched)
        .expect_err("a copy this did not install was written over");

    assert!(
        matches!(&error, Error::Occupied { path, said, .. }
            if *path == machine.binary().display().to_string()
                && said == "agentbus 0.0.1-theirs"),
        "{error:?}"
    );
    let said = error.to_string();
    assert!(said.contains("AGENTBUS_REMOTE_BINARY"), "{said}");
    assert_eq!(
        fs::read_to_string(machine.binary()).unwrap(),
        agentbus("0.0.1-theirs")
    );
    assert!(
        machine.far.copied().is_empty(),
        "a copy was sent to a machine it was never going to be installed on"
    );
    assert!(!machine.partial().exists());
}

#[test]
fn the_agents_there_are_left_alone_until_they_are_asked_for() {
    let machine = Machine::new();
    let local = Local::answering(VERSION);

    let printed = machine
        .install(&local, Hooks::Untouched)
        .expect("nothing was installed");

    assert!(!printed.contains("ran:"), "{printed}");

    let printed = machine
        .install(&local, Hooks::Included)
        .expect("the second run failed");

    // Through the absolute path of the copy that is there, because that is the
    // path the hooks it writes will name.
    assert!(
        printed.contains(&format!("ran: {} install", machine.binary().display())),
        "{printed}"
    );
}

#[test]
fn taking_it_away_removes_the_copy_and_the_record() {
    let machine = Machine::new();
    let local = Local::answering(VERSION);
    machine
        .install(&local, Hooks::Untouched)
        .expect("nothing was installed");

    let printed = machine
        .uninstall(&local, Hooks::Included)
        .expect("nothing was taken away");

    assert!(
        printed.contains(&format!("ran: {} uninstall", machine.binary().display())),
        "{printed}"
    );
    assert!(
        printed.contains(&format!("removed {}", machine.binary().display())),
        "{printed}"
    );
    assert!(!machine.binary().exists());
    assert!(!machine.marker().exists());
}

#[test]
fn a_copy_that_is_not_the_one_that_was_installed_is_kept() {
    let machine = Machine::new();
    let local = Local::answering(VERSION);
    machine
        .install(&local, Hooks::Untouched)
        .expect("nothing was installed");
    // Somebody has upgraded theirs since, or replaced it with something else
    // entirely. Either way it is not what the record describes.
    machine.plant(&machine.binary(), "0.0.2-newer");

    let printed = machine
        .uninstall(&local, Hooks::Untouched)
        .expect("nothing was taken away");

    assert_eq!(
        fs::read_to_string(machine.binary()).unwrap(),
        agentbus("0.0.2-newer"),
        "a copy this program did not leave there was removed"
    );
    assert!(
        printed.contains("is not the copy that was installed there"),
        "{printed}"
    );
    assert!(
        printed.contains("it answers \"agentbus 0.0.2-newer\""),
        "{printed}"
    );
    // The record goes either way: it describes a file that is no longer there.
    assert!(!machine.marker().exists());
}

#[test]
fn nothing_is_taken_from_a_machine_this_never_touched() {
    let machine = Machine::new();
    let local = Local::answering(VERSION);
    let theirs = machine.far.bin().join("agentbus");
    machine.plant(&theirs, VERSION);

    let printed = machine
        .uninstall(&local, Hooks::Untouched)
        .expect("taking nothing away failed");

    assert!(
        printed.contains(&format!(
            "nothing at {} was installed by this",
            machine.binary().display()
        )),
        "{printed}"
    );
    assert_eq!(fs::read_to_string(&theirs).unwrap(), agentbus(VERSION));
}

#[test]
fn the_daemon_serving_the_far_end_is_asked_to_stop() {
    let machine = Machine::new();
    let local = Local::answering(VERSION);
    machine
        .install(&local, Hooks::Untouched)
        .expect("nothing was installed");
    let mut daemon = Sleeper::start();
    let lock = machine.far.home().join("bus/daemon.lock");
    fs::create_dir_all(lock.parent().unwrap()).expect("cannot make the directory");
    fs::write(&lock, format!("{}\n", daemon.pid())).expect("cannot write the lock");

    let printed = machine
        .uninstall(&local, Hooks::Untouched)
        .expect("nothing was taken away");

    assert!(
        printed.contains(&format!(
            "asked the daemon there to stop; it was process {}",
            daemon.pid()
        )),
        "{printed}"
    );
    assert!(daemon.stopped(), "the process was left running");
}

/// A process standing in for a daemon at the far end: it does nothing, and it
/// stops when it is asked to.
struct Sleeper(Child);

impl Sleeper {
    fn start() -> Self {
        Self(
            Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("cannot start a process to stop"),
        )
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    /// Whether it has gone, waiting a little for the signal to reach it.
    ///
    /// A signal is delivered when the kernel gets round to it rather than when
    /// the sender returns, so the question has to be asked more than once. The
    /// wait is a ceiling and not a delay: the ordinary case answers on the
    /// first or second ask.
    fn stopped(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.0.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => return false,
            }
        }
        false
    }
}

impl Drop for Sleeper {
    /// Leaves nothing behind when the test it belongs to has failed and never
    /// asked for it to stop.
    fn drop(&mut self) {
        let _: io::Result<()> = self.0.kill();
        let _ = self.0.wait();
    }
}
