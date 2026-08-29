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
//!
//! # Checking it against a real container
//!
//! Nothing here needs Docker, and nothing here can show that Docker behaves the
//! way the stand-in does. On a machine that has it, that is a few commands
//! against a scratch container — from a checkout, with the declarations kept in
//! a directory of their own so that trying this does not attach anything to the
//! bus somebody is actually running:
//!
//! ```sh
//! d=$(mktemp -d)
//! c=$(docker run -d --rm debian:stable-slim sleep 3600)
//! docker exec "$c" mkdir -p /root/.codex   # give it an agent to find
//!
//! cargo run --bin agentbus -- install docker "$c" --config-dir "$d"
//! #   agentbus <version> in <c>
//! #     found codex (configuration directory /root/.codex)
//! #     codex
//! #       created /root/.codex/hooks.json          … and the rest of them
//! #   declared: docker <c>
//! docker exec "$c" cat /root/.codex/hooks.json     # it is really in there
//!
//! cargo run --bin agentbus -- install docker "$c" --config-dir "$d"
//! #   … already installed / already declared, and nothing copied in again
//!
//! cargo run --bin agentbus -- uninstall docker "$c" --config-dir "$d"
//! docker rm -f "$c"; rm -rf "$d"
//! ```
//!
//! A container that is not there, which is the other half of what these tests
//! stand in for, is `install docker no-such-container`: it should name that
//! container and say which step could not be done.
//!
//! This only works from a machine whose own build can run inside the container.
//! Where it cannot — a mac reaching a Linux container — the copy is fetched from
//! the published release for the container's triple instead, so the check needs
//! a release of this version to exist.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// The version this build is, which is what a copy of it has to answer with.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The container these tests provision.
const CONTAINER: &str = "eager_mclean";

/// A container the stand-in refuses every command for, the way `docker` refuses
/// one for a container that was never created or has been stopped.
const ABSENT: &str = "zealous_ride";

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

    /// Puts a coding agent's configuration directory inside the container, so
    /// that an installation run in there has something to find.
    fn holding(&self, agent: &str) -> PathBuf {
        let dir = self.inside().join(agent);
        fs::create_dir_all(&dir).expect("cannot make the agent's directory");
        dir
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
# What docker says about a container that was never created, and about one that
# has been stopped: nothing on stdout, a sentence naming it on stderr, and a
# non-zero status. Whichever of the two it is, no command runs in it.
for a in "$@"; do
  case $a in
    '{absent}')
      printf 'Error response from daemon: No such container: %s\n' "$a" >&2
      exit 1
      ;;
  esac
done
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
        root = root.display(),
        absent = ABSENT,
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
    let codex = fake.holding(".codex");

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

    // What the agents in there got is reported agent by agent, the way an
    // installation on this machine reports it, under the line saying where it
    // happened.
    assert!(printed.contains("\n  found codex"), "{printed}");
    assert!(
        printed.contains(&format!(
            "\n    created {}\n",
            codex.join("hooks.json").display()
        )),
        "{printed}"
    );
    assert!(codex.join("hooks.json").is_file(), "{printed}");

    // A copy of this build is in there, in the directory the container itself
    // said to put one in, and it is the copy that ran the installation: the
    // search that accepted it is what execs it, so nothing on this side had to
    // guess at the path.
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
    // Once, and once only: the transport would put the hooks in itself for a
    // container it found by looking, and a run somebody asked for has to take
    // that over rather than have it happen twice.
    assert_eq!(
        calls
            .iter()
            .filter(|call| *call == &format!("exec -i {CONTAINER} {there} install"))
            .count(),
        1,
        "the agents in there were not wired up to the copy exactly once: {calls:#?}"
    );

    // And the declaration is the one a daemon reads.
    let declared =
        fs::read_to_string(config.path().join("targets.json")).expect("nothing was declared");
    assert!(declared.contains(CONTAINER), "{declared}");

    let second = agentbus(&fake, config.path(), &["install", "docker", CONTAINER]);

    assert!(second.status.success(), "{}", said(&second));
    let printed = String::from_utf8_lossy(&second.stdout);
    assert!(printed.contains("already declared"), "{printed}");
    assert!(printed.contains("\n    already installed\n"), "{printed}");
    // Nothing was written the second time: the copy that is there answers to
    // the version that was wanted, which is the whole of the check.
    let calls = fake.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("cp ")),
        "a second run copied a binary in again: {calls:#?}"
    );
}

#[test]
fn an_installation_that_failed_in_there_fails_the_command_and_declares_nothing() {
    let mut fake = Fake::new();
    let config = tempfile::tempdir().expect("cannot make a temporary directory");
    // A directory where the agent's hooks file goes, which is a path nothing
    // can write a file over.
    fs::create_dir_all(fake.holding(".codex").join("hooks.json")).expect("cannot block the path");

    let output = agentbus(&fake, config.path(), &["install", "docker", CONTAINER]);

    assert!(!output.status.success(), "{}", said(&output));
    let complained = String::from_utf8_lossy(&output.stderr);
    assert!(
        complained.contains(&format!("wiring up the agents in {CONTAINER} failed")),
        "{complained}"
    );
    assert!(
        complained.contains(&format!("{CONTAINER} has not been declared")),
        "{complained}"
    );
    assert!(
        !config.path().join("targets.json").exists(),
        "a container whose agents were not wired up was declared anyway",
    );
    // The copy is still in there. What failed is one step of the command, and
    // saying which one is worth more than pretending none of it happened.
    assert!(
        fake.calls().iter().any(|call| call.starts_with("cp ")),
        "nothing was copied in at all",
    );
}

#[test]
fn a_container_that_is_not_there_is_named_along_with_what_could_not_be_done() {
    let fake = Fake::new();
    let config = tempfile::tempdir().expect("cannot make a temporary directory");

    let output = agentbus(&fake, config.path(), &["install", "docker", ABSENT]);

    assert!(!output.status.success(), "{}", said(&output));
    let complained = String::from_utf8_lossy(&output.stderr);
    assert!(complained.contains(ABSENT), "{complained}");
    assert!(complained.contains("bootstrap"), "{complained}");
    assert!(complained.contains("No such container"), "{complained}");
    assert!(
        !config.path().join("targets.json").exists(),
        "a container that could not be reached was declared anyway",
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
