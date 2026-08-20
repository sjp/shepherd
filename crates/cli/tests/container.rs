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
use std::time::{Duration, Instant};

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
    "XDG_RUNTIME_DIR",
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
        // Renamed onto its name rather than written onto it: a file open for
        // writing when something forks is a file the fork holds open, and
        // `exec` of one is refused with `ETXTBSY`.
        let writing = binary.with_extension("writing");
        fs::write(&writing, script(dir.path())).expect("cannot write the stand-in");
        fs::set_permissions(&writing, fs::Permissions::from_mode(0o700))
            .expect("cannot make the stand-in runnable");
        fs::rename(&writing, &binary).expect("cannot put the stand-in in place");
        runnable(&binary);
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

/// Where a copy of this build sits inside the container, relative to the
/// container's root.
///
/// The directory is per-user, and this test's user is whoever is running it,
/// so the name is asked for rather than written down.
fn inside_at(version: &str) -> String {
    let said = Command::new("id")
        .arg("-u")
        .output()
        .expect("cannot ask who this is");
    let uid = String::from_utf8_lossy(&said.stdout).trim().to_owned();
    format!("tmp/agentbus-{uid}/agentbus-{version}")
}

/// Waits until `path` can actually be run, and says so if it never can.
///
/// A file this process has just written is a file another of its threads may
/// have forked while it was open for writing, and a fork holds that handle
/// until it execs — during which the kernel refuses to run the file at all.
/// The condition passes on its own in microseconds; what it cannot do is be
/// assumed away, because these tests run commands on every thread at once.
fn runnable(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Command::new(path).arg("--ready").status() {
            Ok(status) if status.success() => return,
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            other => panic!("the stand-in at {} will not run: {other:?}", path.display()),
        }
    }
}

/// Every copy of this program anywhere below `dir`, however deep.
fn copies(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(copies(&path));
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("agentbus-"))
        {
            found.push(path);
        }
    }
    found
}

/// The stand-in itself: it records what it was asked, and its `exec` runs the
/// command here with the container's own filesystem in front of it.
fn script(root: &Path) -> String {
    format!(
        r#"#!/bin/sh
root='{root}'
case ${{1:-}} in --ready) exit 0 ;; esac
printf '%s\n' "$@" >> "$root/argv"
printf '%s\n' '<end>' >> "$root/argv"
case "$1" in
  cp)
    ref=${{3%%:*}}
    path=${{3#*:}}
    fs="$root/fs/$ref"
    # A path the container worked out for itself has already been rewritten
    # into this filesystem by the `exec` branch below, and prefixing it again
    # would nest it inside itself.
    case $path in
      "$fs"/*) dst=$path ;;
      *) dst="$fs$path" ;;
    esac
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
      # Rewriting has to survive being applied to a path it has already been
      # applied to: the container's filesystem lives under a temporary
      # directory that is itself below /tmp, so a second pass over an answer
      # this stand-in gave earlier would nest it inside itself.
      set -- "$@" "$(printf '%s' "$a" \
        | sed "s#$fs/tmp/#@FSTMP@#g; s#/tmp/#@FSTMP@#g; s#@FSTMP@#$fs/tmp/#g")"
      i=$((i+1))
    done
    # A container has no session of its own, so it has no runtime directory
    # either, whatever the machine running these tests happens to have: the
    # copy inside goes to the per-user directory under the container's own
    # /tmp, which is the only place this stand-in can put one.
    unset XDG_RUNTIME_DIR
    remote=
    for candidate in "$fs"/tmp/agentbus-* "$fs"/tmp/*/agentbus-*; do
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

    // A copy of this build is in there, in the directory the container itself
    // said to put one in, and the agents in there were wired up to that copy
    // rather than to a path anything on this side chose.
    let calls = fake.calls();
    let put = calls
        .iter()
        .find(|call| call.starts_with("cp "))
        .unwrap_or_else(|| panic!("nothing was copied in: {calls:#?}"));
    let there = put
        .rsplit_once(':')
        .expect("the copy went nowhere nameable")
        .1;
    assert!(
        Path::new(there).is_file(),
        "nothing arrived at {there}: {calls:#?}"
    );
    assert!(
        there.ends_with(&format!("agentbus-{VERSION}")),
        "the copy is not named for its version: {there}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("exec -i {CONTAINER} {there} install")),
        "the agents in there were never wired up to the copy: {calls:#?}"
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
    // Two older ones: where a copy goes now, and where copies used to go
    // before that directory was made a per-user one.
    fs::write(fake.inside().join(inside_at("0.0.1")), "older").expect("cannot write it");
    fs::write(fake.inside().join("tmp/agentbus-0.0.2"), "older still").expect("cannot write it");
    let _ = fake.calls();

    let output = agentbus(&fake, config.path(), &["uninstall", "docker", CONTAINER]);

    assert!(output.status.success(), "{}", said(&output));
    let printed = String::from_utf8_lossy(&output.stdout);
    assert!(
        printed.contains(&format!("taken out of {CONTAINER}")),
        "{printed}"
    );
    assert!(printed.contains("no longer declared"), "{printed}");
    assert!(
        copies(&fake.inside().join("tmp")).is_empty(),
        "copies were left behind"
    );
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
