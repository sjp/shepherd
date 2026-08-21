//! Installing this program on a machine over ssh, driven through an `ssh` that
//! is a shell script.
//!
//! Everything here goes through the real binary, because what is being tested
//! is what somebody at a shell gets: the copy that ends up on the machine is
//! this build's own executable, the version check that decides whether to send
//! it is the real one, and the hooks that appear when they are asked for are
//! written by the real installer. The only pretence is `ssh` itself, which
//! keeps the far end's filesystem in a directory and runs what it is given
//! there rather than opening a connection to anything.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The version this build is, which is what a copy of it has to answer with.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The machine these tests provision, as somebody would name it.
const HOST: &str = "bob@fs.example.net";

/// What `ssh -G` is made to say about that name.
const RESOLUTION: &str = "host fs.example.net\\nuser bob\\nhostname fs.example.net\\nport 22\\n";

/// The machine the stand-in refuses to log in to, and how it refuses.
///
/// Verbatim from OpenSSH, because what decides whether this is worth another
/// attempt is the sentence ssh printed and nothing else.
const REFUSING: &str = "nobody@fs.example.net";
const REFUSED: &str = "nobody@fs.example.net: Permission denied (publickey).";

/// The variables that would otherwise decide, behind a test's back, where any
/// of this ends up.
const INHERITED: &[&str] = &[
    "AGENTBUS_CONFIG_DIR",
    "AGENTBUS_DIR",
    "AGENTBUS_LOG",
    "AGENTBUS_REMOTE_BINARY",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
];

/// An `ssh` that is a script, and the machine it pretends to reach.
struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new() -> Self {
        let fake = Self {
            dir: tempfile::tempdir().expect("cannot make a temporary directory"),
        };
        for directory in ["bin", "home/bin", "local"] {
            fs::create_dir_all(fake.dir.path().join(directory)).expect("cannot make a directory");
        }
        fake.command("ssh", &script(fake.dir.path()));
        fake
    }

    /// Writes a runnable stand-in for a command, on the far end's `PATH` if it
    /// is one of the far end's and on this one's if it is `ssh` itself.
    fn command(&self, name: &str, body: &str) -> PathBuf {
        let path = match name {
            "ssh" => self.dir.path().join("bin").join(name),
            _ => self.far().join("bin").join(name),
        };
        // Renamed onto its name rather than written onto it: a file open for
        // writing when something forks is a file the fork holds open, and
        // `exec` of one is refused with `ETXTBSY`.
        let writing = path.with_extension("writing");
        fs::write(&writing, body).expect("cannot write the stand-in");
        fs::set_permissions(&writing, fs::Permissions::from_mode(0o700))
            .expect("cannot make the stand-in runnable");
        fs::rename(&writing, &path).expect("cannot put the stand-in in place");
        path
    }

    /// The home directory of the machine on the other side of that `ssh`.
    fn far(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    /// Makes the machine over there say something about itself.
    ///
    /// Where an installation goes is worked out on that machine out of
    /// variables only a shell running there can read, so a test that wants it
    /// somewhere else says so to the far end and not to the command being run.
    fn far_end_says(&self, name: &str, value: &Path) {
        use std::io::Write;
        let mut env = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.path().join("env"))
            .expect("cannot write what the far end says about itself");
        writeln!(env, "{name}='{}'; export {name}", value.display())
            .expect("cannot write what the far end says about itself");
    }

    /// Where a copy of this program is installed over there.
    fn binary(&self) -> PathBuf {
        self.far().join(".local/bin/agentbus")
    }

    /// Where the record of what was installed is kept over there.
    fn marker(&self) -> PathBuf {
        self.far().join(".local/share/agentbus/installed")
    }

    /// Where Claude Code's hook ends up over there, once anything has asked
    /// for it.
    fn hooks(&self) -> PathBuf {
        self.far().join(".claude/hooks/agentbus.sh")
    }
}

/// The stand-in itself.
///
/// It answers the two questions this daemon asks ssh without connecting to
/// anything — what a declaration resolves to, and whether a multiplexed
/// connection is alive — and runs everything else here, with the far end's home
/// directory in front of it and nothing of this machine's environment left
/// pointing anywhere.
fn script(root: &Path) -> String {
    format!(
        r#"#!/bin/sh
root='{root}'
mode=
prev=
for word in "$@"; do
  case $prev in -O) mode=$word ;; esac
  case $word in -G) mode=dump ;; esac
  prev=$word
done
case $mode in
  dump) printf '{RESOLUTION}'; exit 0 ;;
  check|exit) exit 0 ;;
esac
case "$*" in
  *{REFUSING}*)
    echo '{REFUSED}' >&2
    exit 255 ;;
esac
while [ $# -gt 0 ] && [ "$1" != "--" ]; do shift; done
[ $# -gt 0 ] && shift
HOME="$root/home"
PATH="$root/home/bin:/usr/bin:/bin"
AGENTBUS_DIR=
AGENTBUS_REMOTE_BINARY=
XDG_CONFIG_HOME=
XDG_DATA_HOME=
XDG_STATE_HOME=
XDG_RUNTIME_DIR=
export HOME PATH AGENTBUS_DIR AGENTBUS_REMOTE_BINARY XDG_CONFIG_HOME XDG_DATA_HOME \
       XDG_STATE_HOME XDG_RUNTIME_DIR
# Whatever the test decided this particular machine says about itself, last, so
# that it beats the clearing above.
if [ -f "$root/env" ]; then . "$root/env"; fi
exec sh -c "$*"
"#,
        root = root.display()
    )
}

/// A `claude` that agrees to everything and knows about nothing, which is what
/// a machine with Claude Code installed and this program not yet installed
/// looks like.
const CLAUDE: &str = "#!/bin/sh\nexit 0\n";

/// Runs the real binary against the stand-in, with nothing inherited from
/// whoever is running the tests.
fn agentbus(fake: &Fake, args: &[&str]) -> Output {
    let mut path = std::ffi::OsString::from(fake.dir.path().join("bin"));
    if let Some(inherited) = std::env::var_os("PATH") {
        path.push(":");
        path.push(inherited);
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
    command.args(args);
    for variable in INHERITED {
        command.env_remove(variable);
    }
    command
        .env("PATH", path)
        // This machine's own home, which is where ssh would read a
        // configuration from and where nothing in these tests writes.
        .env("HOME", fake.dir.path().join("local"))
        .env("AGENTBUS_DIR", fake.dir.path().join("bus"))
        .env("AGENTBUS_CONFIG_DIR", fake.dir.path().join("config"));
    command.output().expect("cannot run agentbus")
}

/// Everything a finished command said, for a failure message.
fn said(output: &Output) -> String {
    format!(
        "exited with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// What a command printed.
fn printed(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_machine_is_provisioned_once_and_its_agents_are_left_alone() {
    let fake = Fake::new();
    fake.command("claude", CLAUDE);

    let first = agentbus(&fake, &["install", "ssh", "--", HOST]);

    assert!(first.status.success(), "{}", said(&first));
    let binary = fake.binary();
    assert!(binary.is_file(), "nothing was installed: {}", said(&first));
    assert_eq!(
        fs::metadata(&binary).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::read_to_string(fake.marker()).unwrap(),
        format!("version={VERSION}\npath={}\n", binary.display())
    );
    let printed_first = printed(&first);
    assert!(
        printed_first.contains(&format!("installed agentbus {VERSION} at")),
        "{printed_first}"
    );
    assert!(printed_first.contains("--with-hooks"), "{printed_first}");
    // The agents on that machine are a separate decision, and nothing was said
    // about them.
    assert!(!fake.hooks().exists(), "hooks were written unasked");

    // Which file this is, so that a second run writing another one in its place
    // is caught however fast the two runs were.
    let was = fs::metadata(&binary).unwrap().ino();

    let again = agentbus(&fake, &["install", "ssh", "--", HOST]);

    assert!(again.status.success(), "{}", said(&again));
    let printed_again = printed(&again);
    assert!(
        printed_again.contains(&format!("agentbus {VERSION} is already at")),
        "{printed_again}"
    );
    assert_eq!(
        fs::metadata(&binary).unwrap().ino(),
        was,
        "the copy that was there was written again"
    );
}

#[test]
fn the_agents_are_wired_up_to_the_installed_copy_when_that_is_asked_for() {
    let fake = Fake::new();
    fake.command("claude", CLAUDE);

    let installed = agentbus(&fake, &["install", "ssh", "--with-hooks", "--", HOST]);

    assert!(installed.status.success(), "{}", said(&installed));
    let hooks = fs::read_to_string(fake.hooks()).expect("no hooks were written");
    // The hook names the copy that was installed, by an absolute path: the
    // directory it is in is on most machines' PATH and guaranteed on none.
    assert!(
        hooks.contains(&format!("'{}'", fake.binary().display())),
        "{hooks}"
    );
    assert!(hooks.contains("emit --agent claude"), "{hooks}");

    let removed = agentbus(&fake, &["uninstall", "ssh", "--with-hooks", "--", HOST]);

    assert!(removed.status.success(), "{}", said(&removed));
    assert!(!fake.hooks().exists(), "the hooks were left behind");
    assert!(!fake.binary().exists(), "the copy was left behind");
    assert!(!fake.marker().exists(), "the record was left behind");
}

#[test]
fn a_machine_that_says_where_it_wants_a_copy_gets_it_there_and_is_wired_up_to_it() {
    let fake = Fake::new();
    fake.command("claude", CLAUDE);
    let elsewhere = fake.far().join("opt/bin");
    fs::create_dir_all(&elsewhere).expect("cannot make the directory");
    fake.far_end_says("XDG_BIN_HOME", &elsewhere);

    let installed = agentbus(&fake, &["install", "ssh", "--with-hooks", "--", HOST]);

    assert!(installed.status.success(), "{}", said(&installed));
    let there = elsewhere.join("agentbus");
    assert!(
        there.is_file(),
        "nothing was installed where it was asked for"
    );
    assert!(
        !fake.binary().exists(),
        "a copy went to the ordinary place as well"
    );
    // The hook names the copy that was actually made, absolutely, which is the
    // whole reason the far end gets to decide where it went.
    let hooks = fs::read_to_string(fake.hooks()).expect("no hooks were written");
    assert!(hooks.contains(&format!("'{}'", there.display())), "{hooks}");
    assert!(hooks.contains("emit --agent claude"), "{hooks}");

    let removed = agentbus(&fake, &["uninstall", "ssh", "--with-hooks", "--", HOST]);

    assert!(removed.status.success(), "{}", said(&removed));
    assert!(!there.exists(), "the copy was left behind");
    assert!(!fake.hooks().exists(), "the hooks were left behind");
}

#[test]
fn a_machine_that_is_still_declared_is_said_to_be() {
    let fake = Fake::new();
    let installed = agentbus(&fake, &["install", "ssh", "--", HOST]);
    assert!(installed.status.success(), "{}", said(&installed));
    let declared = agentbus(&fake, &["attach", "--", HOST]);
    assert!(declared.status.success(), "{}", said(&declared));

    let removed = agentbus(&fake, &["uninstall", "ssh", "--", HOST]);

    assert!(removed.status.success(), "{}", said(&removed));
    let printed = printed(&removed);
    assert!(
        printed.contains(&format!("agentbus detach -- {HOST}")),
        "{printed}"
    );
    assert!(!fake.binary().exists(), "the copy was left behind");
}

#[test]
fn a_machine_that_will_not_let_us_in_says_so_and_says_what_to_do_about_it() {
    let fake = Fake::new();

    let output = agentbus(&fake, &["install", "ssh", "--", REFUSING]);

    assert!(!output.status.success(), "{}", said(&output));
    let complaint = String::from_utf8_lossy(&output.stderr);
    // ssh's own words, because they are what says which credential is missing.
    assert!(complaint.contains("Permission denied"), "{complaint}");
    // And the one thing ssh cannot say: that nothing here will retry its way
    // out of this, so a person has to connect once themselves.
    assert!(
        complaint.contains("connect to it by hand once"),
        "{complaint}"
    );
    assert!(!fake.binary().exists(), "something was installed anyway");
}

#[test]
fn the_flags_about_this_machine_are_refused_with_a_host() {
    let fake = Fake::new();

    let output = agentbus(&fake, &["install", "--dry-run", "ssh", "--", HOST]);

    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    assert!(output.stdout.is_empty(), "{}", said(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--dry-run"),
        "{}",
        said(&output)
    );
}
