//! Provisioning a container by hand, driven through a `docker` that is a shell
//! script.
//!
//! Everything here goes through the real binary, because what is being tested
//! is what somebody at a shell gets: the copy that ends up inside the
//! container is this build's own executable, the version check that decides
//! whether to send it is the real one, and the declaration left behind is the
//! file a daemon will read. The only pretence is `docker` itself, which keeps
//! each container's filesystem in a directory and rewrites the paths that mean
//! something inside one into it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The version this build is, which is what a copy of it has to answer with.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The container these tests provision.
const CONTAINER: &str = "eager_mclean";

/// The variables that would otherwise decide, behind a test's back, where any
/// of this ends up.
const INHERITED: &[&str] = &[
    "AGENTBUS_CONFIG_DIR",
    "AGENTBUS_DIR",
    "AGENTBUS_DOCKER_BIN",
    "AGENTBUS_LOG",
    "AGENTBUS_REMOTE_BINARY",
    "XDG_CONFIG_HOME",
];

/// A `docker` that is a script, and the machine it pretends to have.
struct Fake {
    dir: tempfile::TempDir,
    binary: PathBuf,
}

impl Fake {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("cannot make a temporary directory");
        let binary = dir.path().join("docker");
        fs::write(&binary, script(dir.path())).expect("cannot write the stand-in");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))
            .expect("cannot make the stand-in runnable");
        Self { dir, binary }
    }

    /// Every command it has been given since it was last asked, each as one
    /// line, and forgets them.
    fn calls(&mut self) -> Vec<String> {
        let path = self.dir.path().join("argv");
        let Ok(recorded) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        fs::remove_file(&path).expect("cannot forget what was recorded");
        recorded
            .split("\n<end>\n")
            .filter(|invocation| !invocation.is_empty())
            .map(|invocation| invocation.lines().collect::<Vec<&str>>().join(" "))
            .collect()
    }

    /// What is inside the container.
    fn inside(&self) -> PathBuf {
        self.dir.path().join("fs").join(CONTAINER)
    }
}

/// The stand-in itself: it records what it was asked, and its `exec` runs the
/// command here with the container's own filesystem in front of it.
fn script(root: &Path) -> String {
    format!(
        r#"#!/bin/sh
root='{root}'
printf '%s\n' "$@" >> "$root/argv"
printf '%s\n' '<end>' >> "$root/argv"
case "$1" in
  cp)
    ref=${{3%%:*}}
    path=${{3#*:}}
    dst="$root/fs/$ref$path"
    mkdir -p "$(dirname "$dst")"
    cp "$2" "$dst"
    ;;
  inspect)
    printf 'full-%s\n' "$4"
    ;;
  exec)
    shift
    shift
    ref=$1
    shift
    fs="$root/fs/$ref"
    mkdir -p "$fs/bin" "$fs/tmp"
    n=$#
    i=0
    while [ $i -lt $n ]; do
      a=$1
      shift
      set -- "$@" "$(printf '%s' "$a" | sed "s#/tmp/#$fs/tmp/#g")"
      i=$((i+1))
    done
    remote=
    for candidate in "$fs"/tmp/agentbus-*; do
      if [ -x "$candidate" ]; then remote=$candidate; fi
    done
    AGENTBUS_REMOTE_BINARY="$remote" \
    AGENTBUS_DIR="$fs/bus" \
    HOME="$fs" \
    XDG_CONFIG_HOME="$fs/.config" \
    XDG_STATE_HOME="$fs/.local/state" \
    XDG_DATA_HOME="$fs/.local/share" \
    PATH="$fs/bin:/usr/bin:/bin" \
    exec "$@"
    ;;
esac
"#,
        root = root.display()
    )
}

/// Runs the real binary against the stand-in, with nothing inherited from
/// whoever is running the tests.
fn agentbus(fake: &Fake, config: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
    command.args(args).arg("--config-dir").arg(config);
    for variable in INHERITED {
        command.env_remove(variable);
    }
    command.env("AGENTBUS_DOCKER_BIN", &fake.binary);
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

#[test]
fn a_container_is_provisioned_once_and_declared_once() {
    let mut fake = Fake::new();
    let config = tempfile::tempdir().expect("cannot make a temporary directory");

    let first = agentbus(&fake, config.path(), &["install", "docker", CONTAINER]);

    assert!(first.status.success(), "{}", said(&first));
    let printed = String::from_utf8_lossy(&first.stdout);
    assert!(
        printed.contains(&format!("agentbus {VERSION} in {CONTAINER}")),
        "{printed}"
    );
    assert!(
        printed.contains(&format!("declared: docker {CONTAINER}")),
        "{printed}"
    );

    // A copy of this build is in there, at the path its version names, and the
    // agents in there were wired up to it.
    let copy = fake.inside().join(format!("tmp/agentbus-{VERSION}"));
    assert!(copy.is_file(), "nothing was put in the container");
    let calls = fake.calls();
    assert!(
        calls.iter().any(|call| call.starts_with("cp ")),
        "nothing was copied in: {calls:#?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("exec -i {CONTAINER} /tmp/agentbus-{VERSION} install")),
        "the agents in there were never wired up: {calls:#?}"
    );

    // And the declaration is the one a daemon reads.
    let declared =
        fs::read_to_string(config.path().join("targets.json")).expect("nothing was declared");
    assert!(declared.contains(CONTAINER), "{declared}");

    let second = agentbus(&fake, config.path(), &["install", "docker", CONTAINER]);

    assert!(second.status.success(), "{}", said(&second));
    let printed = String::from_utf8_lossy(&second.stdout);
    assert!(printed.contains("already declared"), "{printed}");
    // Nothing was written the second time: the copy that is there answers to
    // the version that was wanted, which is the whole of the check.
    let calls = fake.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("cp ")),
        "a second run copied a binary in again: {calls:#?}"
    );
}

#[test]
fn taking_it_back_out_removes_every_copy_and_the_declaration() {
    let mut fake = Fake::new();
    let config = tempfile::tempdir().expect("cannot make a temporary directory");
    let installed = agentbus(&fake, config.path(), &["install", "docker", CONTAINER]);
    assert!(installed.status.success(), "{}", said(&installed));
    // An older one, left behind by a version that is no longer wanted.
    fs::write(fake.inside().join("tmp/agentbus-0.0.1"), "older").expect("cannot write it");
    let _ = fake.calls();

    let output = agentbus(&fake, config.path(), &["uninstall", "docker", CONTAINER]);

    assert!(output.status.success(), "{}", said(&output));
    let printed = String::from_utf8_lossy(&output.stdout);
    assert!(
        printed.contains(&format!("taken out of {CONTAINER}")),
        "{printed}"
    );
    assert!(printed.contains("no longer declared"), "{printed}");
    let left: Vec<PathBuf> = fs::read_dir(fake.inside().join("tmp"))
        .expect("cannot look inside")
        .map(|entry| entry.expect("cannot read it").path())
        .collect();
    assert!(left.is_empty(), "{left:?}");
    let declared = fs::read_to_string(config.path().join("targets.json"))
        .expect("cannot read the declarations");
    assert!(!declared.contains(CONTAINER), "{declared}");
}

#[test]
fn the_flags_about_this_machine_are_refused_with_a_container() {
    let fake = Fake::new();
    let config = tempfile::tempdir().expect("cannot make a temporary directory");

    let output = agentbus(
        &fake,
        config.path(),
        &["install", "--dry-run", "docker", CONTAINER],
    );

    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    assert!(output.stdout.is_empty(), "{}", said(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--dry-run"),
        "{}",
        said(&output)
    );
}
