//! Containers, driven through a `docker` that is a shell script.
//!
//! What is being tested is a command line and what is done with what it prints,
//! so the far end is a directory per container and the command is a script that
//! records every argument it was given before doing the small local thing that
//! stands in for the real one. That is enough to run the whole of provisioning
//! for real — the bootstrap script, the push, the version check, the retry —
//! against a machine with no Docker on it at all, which is where these tests
//! have to pass.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentbus_protocol::{
    Agent, OriginHop, SessionEntry, SessionStatus, Snapshot, Source, Timestamp,
};

use super::super::attachments::{Attachments, Entry, State};
use super::super::bootstrap::{Bootstrap, TARGET};
use super::super::discover::{Context, Discovery};
use super::super::reconcile::{Plan, Reconciling};
use super::super::release::Release;
use super::super::targets::Targets;
use super::super::transport::{Registry, Transport};
use super::{Containers, Docker, listing, project};
use crate::bus::Bus;
use crate::remote::attach;

/// Builds an agent id from a literal, which is what every one of these is.
fn agent(name: &str) -> Agent {
    Agent::new(name).expect("a test's own agent id is a valid one")
}

/// The version these tests provision.
///
/// Deliberately not this build's: the bootstrap script looks for a person's own
/// installation before it pushes anything, and a version nobody could have
/// installed is the only way to be sure that what a test watched happen was the
/// push and not somebody's `agentbus` being found on the path.
const VERSION: &str = "9.9.9-for-tests";

/// Where a copy of that version is put inside a container.
/// The word every script that asks a container where a copy should go carries,
/// and nothing else does.
const PROBE: &str = "mkdir -m 700";

/// How long a test waits for something that should happen almost at once.
const PATIENCE: Duration = Duration::from_secs(10);

/// The word the fake `agentbus` replaces with the container it is running in.
const WHERE: &str = "the-container";

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

/// A `docker` that is a script.
///
/// It keeps every container's filesystem in a directory of its own and rewrites
/// the container-absolute paths a caller uses into it, so a test can look at
/// what was written where without anything being written outside the temporary
/// directory this owns.
struct Fake {
    dir: tempfile::TempDir,
    binary: PathBuf,
}

impl Fake {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("cannot make a temporary directory");
        let binary = dir.path().join("docker");
        // Renamed onto its name rather than written onto it. These tests run
        // on several threads and each of them runs commands, and a fork that
        // happens while this file is open for writing inherits that handle —
        // which is `ETXTBSY` for whoever tries to run it next.
        let writing = binary.with_extension("writing");
        fs::write(&writing, script(dir.path())).expect("cannot write the stand-in");
        fs::set_permissions(&writing, fs::Permissions::from_mode(0o700))
            .expect("cannot make the stand-in runnable");
        fs::rename(&writing, &binary).expect("cannot put the stand-in in place");
        runnable(&binary);
        Self { dir, binary }
    }

    /// The command this is reached by.
    fn docker(&self) -> Docker {
        Docker::named(&self.binary)
    }

    /// The containers on it, as `docker ps` will print them.
    fn listing(&self, printed: &str) {
        fs::write(self.dir.path().join("ps"), printed).expect("cannot write the listing");
    }

    /// Every command it was given, oldest first.
    fn argv(&self) -> Vec<Vec<String>> {
        let Ok(recorded) = fs::read_to_string(self.dir.path().join("argv")) else {
            return Vec::new();
        };
        recorded
            .split("\n<end>\n")
            .filter(|invocation| !invocation.is_empty())
            .map(|invocation| invocation.lines().map(str::to_owned).collect())
            .collect()
    }

    /// Every command it was given, each as one line, for asserting on.
    fn calls(&self) -> Vec<String> {
        self.argv().iter().map(|argv| argv.join(" ")).collect()
    }

    /// The same, without the one that asks a container where a copy should go.
    ///
    /// That question is asked once per container and its answer is a whole
    /// shell fragment, so leaving it in would make every assertion about a
    /// sequence of commands an assertion about the wording of that fragment.
    /// It is asserted about on its own instead.
    fn calls_beside_the_probe(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|call| !call.contains(PROBE))
            .collect()
    }

    /// Where a copy of this program goes inside `container`, as the container
    /// itself answered.
    ///
    /// Read back out of what was asked rather than written down, because the
    /// directory is per-user and the answer belongs to the far end.
    fn installed(&self, container: &str) -> String {
        let copied = self
            .calls()
            .into_iter()
            .find(|call| call.starts_with("cp "))
            .expect("nothing was ever copied in");
        copied
            .rsplit_once(&format!("{container}:"))
            .expect("the copy went nowhere nameable")
            .1
            .to_owned()
    }

    /// What is inside one of its containers.
    fn inside(&self, container: &str) -> PathBuf {
        self.dir.path().join("fs").join(container)
    }
}

/// The stand-in itself.
///
/// `exec` is where the pretence lives: it maps the paths that mean something
/// inside a container onto that container's directory, points the bootstrap
/// script at whichever copy has been pushed there, and then simply runs the
/// command here.
fn script(root: &Path) -> String {
    format!(
        r#"#!/bin/sh
root='{root}'
case ${{1:-}} in --ready) exit 0 ;; esac
printf '%s\n' "$@" >> "$root/argv"
printf '%s\n' '<end>' >> "$root/argv"
case "$1" in
  ps)
    if [ -f "$root/ps" ]; then cat "$root/ps"; fi
    ;;
  inspect)
    printf 'full-%s\n' "$4"
    ;;
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
    AGENTBUS_FAKE_CONTAINER="$ref" \
    AGENTBUS_REMOTE_BINARY="$remote" \
    AGENTBUS_DIR="$fs/bus" \
    HOME="$fs" \
    PATH="$fs/bin:/usr/bin:/bin" \
    exec "$@"
    ;;
esac
"#,
        root = root.display()
    )
}

/// A file standing in for this program, ready to be pushed into a container.
///
/// It answers the one question the bootstrap asks, and when it is asked to
/// subscribe it says what the daemon in that container knows and stays on the
/// line — which is what makes an attachment to it a real attachment.
struct Local {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Local {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("cannot make a temporary directory");
        let path = dir.path().join("agentbus");
        let (head, tail) = said();
        fs::write(
            &path,
            format!(
                "#!/bin/sh\n\
                 case \"$1\" in\n\
                 --version) echo 'agentbus {VERSION}'; exit 0 ;;\n\
                 subscribe) printf '%s%s%s\\n' '{head}' \"$AGENTBUS_FAKE_CONTAINER\" '{tail}'\n\
                 exec sleep 3600 ;;\n\
                 esac\n\
                 exit 0\n"
            ),
        )
        .expect("cannot write the stand-in");
        Self { _dir: dir, path }
    }
}

/// What a daemon inside a container says it knows, in two halves with the name
/// of the container it is in taken out of the middle.
fn said() -> (String, String) {
    let snapshot = Snapshot::new(
        1,
        vec![SessionEntry {
            session: format!("session-in-{WHERE}"),
            agent: agent("claude"),
            status: SessionStatus::Blocked,
            source: Source::Hook,
            status_source: None,
            cwd: None,
            correlation: None,
            origin: Vec::new(),
            since: Timestamp::parse("2026-08-19T10:00:00.000Z").expect("not a timestamp"),
        }],
    );
    let written = serde_json::to_string(&snapshot).expect("cannot write a snapshot");
    let (head, tail) = written.split_once(WHERE).expect("the name is not in there");
    (head.to_owned(), tail.to_owned())
}

/// A provisioner that sends the stand-in and has nowhere to fetch from, so that
/// a test which unexpectedly reaches the fetch path fails here rather than
/// going to the network.
fn sending(local: &Local) -> Bootstrap {
    Bootstrap::new(VERSION)
        .sending(&local.path, TARGET)
        .fetching(Release::at("file:///no/such/release", VERSION))
}

/// One line of what `docker ps` prints.
fn line(state: &str, folder: &str, name: &str, id: &str) -> String {
    format!("{state}\t{folder}\t{name}\t{id}\n")
}

/// What a discovery is told when nothing about this machine matters.
fn nothing() -> Context<'static> {
    Context {
        working: &[],
        declared: &[],
    }
}

fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// Waits for `wanted`, or fails the test saying what it was waiting for.
fn until(what: &str, wanted: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !wanted() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn every_field_of_a_listed_container_is_read_back() {
    let listed = listing::listed(&line(
        "running",
        "/home/u/work/api",
        "api_devcontainer-app-1",
        "3f9c1a2b4d5e",
    ));

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, "running");
    assert_eq!(listed[0].folder, "/home/u/work/api");
    assert_eq!(listed[0].name, "api_devcontainer-app-1");
    assert_eq!(listed[0].id, "3f9c1a2b4d5e");
    assert!(listed[0].running());
}

#[test]
fn a_project_path_with_a_comma_or_a_space_in_it_is_still_one_path() {
    let printed = format!(
        "{}{}",
        line("running", "/home/u/work/a,b", "one", "1111"),
        line("running", "/home/u/My Projects/api", "two", "2222"),
    );

    let listed = listing::listed(&printed);

    assert_eq!(listed[0].folder, "/home/u/work/a,b");
    assert_eq!(listed[1].folder, "/home/u/My Projects/api");
}

#[test]
fn a_blank_line_or_one_that_is_not_a_container_is_passed_over() {
    let printed = format!(
        "\n{}not a line at all\n\t\t\n{}\n",
        line("running", "/w/one", "one", "1111"),
        line("exited", "/w/two", "two", "2222").trim_end(),
    );

    let listed = listing::listed(&printed);

    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].name, "one");
    assert_eq!(listed[1].name, "two");
}

#[test]
fn a_state_this_has_never_heard_of_is_carried_as_the_word_it_was() {
    let listed = listing::listed(&line("restarting", "/w/one", "one", "1111"));

    assert_eq!(listed[0].state, "restarting");
    // And is not reached: the only state that means the daemon in there can be
    // spoken to is the one that says so.
    assert!(!listed[0].running());
}

#[test]
fn a_container_with_several_names_is_known_by_the_first() {
    let listed = listing::listed(&line("running", "/w/one", "app,app_1", "1111"));

    assert_eq!(listed[0].name, "app");
}

/// A directory tree with a devcontainer definition somewhere in it.
fn planted(at: &Path, layout: &str) {
    let dir = at.join(layout);
    fs::create_dir_all(dir.parent().expect("no parent")).expect("cannot make the tree");
    fs::write(dir, "{}").expect("cannot write the definition");
}

#[test]
fn a_definition_is_found_wherever_it_is_written() {
    for layout in [
        ".devcontainer.json",
        ".devcontainer/devcontainer.json",
        ".devcontainer/production/devcontainer.json",
    ] {
        let home = tempfile::tempdir().expect("cannot make a temporary directory");
        let root = home.path().join("work/api");
        let deep = root.join("crates/server/src");
        fs::create_dir_all(&deep).expect("cannot make the tree");
        planted(&root, layout);

        assert_eq!(
            project::root(&deep, Some(home.path())),
            Some(root.clone()),
            "{layout}"
        );
        // And the project's own directory is its own root.
        assert_eq!(
            project::root(&root, Some(home.path())),
            Some(root),
            "{layout}"
        );
    }
}

#[test]
fn the_walk_stops_at_the_parent_of_home() {
    let above = tempfile::tempdir().expect("cannot make a temporary directory");
    let home = above.path().join("people/u");
    let work = home.join("work/api");
    fs::create_dir_all(&work).expect("cannot make the tree");

    // The parent of home is as far as it goes, and a definition there is found.
    planted(above.path().join("people").as_path(), ".devcontainer.json");
    assert_eq!(
        project::root(&work, Some(&home)),
        Some(above.path().join("people"))
    );

    // One above that is not, however much of the tree is above it.
    fs::remove_file(above.path().join("people/.devcontainer.json")).expect("cannot remove it");
    planted(above.path(), ".devcontainer.json");
    assert_eq!(project::root(&work, Some(&home)), None);
}

#[test]
fn a_directory_in_no_project_belongs_to_none() {
    let home = tempfile::tempdir().expect("cannot make a temporary directory");
    let elsewhere = home.path().join("notes/tuesday");
    fs::create_dir_all(&elsewhere).expect("cannot make the tree");

    assert_eq!(project::root(&elsewhere, Some(home.path())), None);
}

#[test]
fn only_the_containers_that_are_up_are_offered() {
    let fake = Fake::new();
    fake.listing(&format!(
        "{}{}",
        line("running", "/w/api", "api", "1111aaaa"),
        line("exited", "/w/web", "web", "2222bbbb"),
    ));
    let containers = Containers::through(fake.docker());

    let found = containers.sweep(&nothing()).expect("docker said nothing");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].args, words(&["api"]));
    assert_eq!(found[0].transport.label(), "api");
    assert_eq!(found[0].transport.kind(), "container");
    // And it was asked the one question, in the shape it was meant to be asked.
    assert_eq!(
        fake.calls(),
        vec![format!(
            "ps -a --filter label=devcontainer.local_folder --format {}",
            listing::FORMAT
        )]
    );
}

#[test]
fn every_running_container_of_one_project_is_a_machine_of_its_own() {
    let fake = Fake::new();
    fake.listing(&format!(
        "{}{}",
        line("running", "/w/api", "api-app-1", "1111aaaa"),
        line("running", "/w/api", "api-db-1", "2222bbbb"),
    ));
    let containers = Containers::through(fake.docker());

    let found = containers.sweep(&nothing()).expect("docker said nothing");

    assert_eq!(found.len(), 2);
    assert_eq!(found[0].args, words(&["api-app-1"]));
    assert_eq!(found[1].args, words(&["api-db-1"]));
}

#[test]
fn of_two_containers_for_one_project_only_the_running_one_is_offered() {
    let fake = Fake::new();
    fake.listing(&format!(
        "{}{}",
        line("exited", "/w/api", "api-old", "1111aaaa"),
        line("running", "/w/api", "api-new", "2222bbbb"),
    ));
    let containers = Containers::through(fake.docker());

    let found = containers.sweep(&nothing()).expect("docker said nothing");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].args, words(&["api-new"]));
}

#[test]
fn a_container_somebody_has_already_asked_for_is_not_offered_again() {
    let fake = Fake::new();
    fake.listing(&format!(
        "{}{}{}",
        line("running", "/w/api", "api", "1111aaaa"),
        line("running", "/w/web", "web", "2222bbbb"),
        line("running", "/w/db", "db", "3333cccc"),
    ));
    let containers = Containers::through(fake.docker());
    let declared = vec![
        // By name.
        words(&["api"]),
        // By as much of the id as somebody happened to copy.
        words(&["2222bbbb0000000000000000"]),
    ];

    let found = containers
        .sweep(&Context {
            working: &[],
            declared: &declared,
        })
        .expect("docker said nothing");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].args, words(&["db"]));
}

#[test]
fn the_container_of_the_project_being_worked_in_is_reached_first() {
    let home = tempfile::tempdir().expect("cannot make a temporary directory");
    let mine = home.path().join("work/api");
    fs::create_dir_all(mine.join("src")).expect("cannot make the tree");
    planted(&mine, ".devcontainer/devcontainer.json");

    let fake = Fake::new();
    fake.listing(&format!(
        "{}{}",
        line("running", "/w/somebody-elses", "other", "1111aaaa"),
        line("running", &mine.display().to_string(), "mine", "2222bbbb"),
    ));
    let containers = Containers::through(fake.docker()).under(home.path());
    let working = vec![mine.join("src").display().to_string()];

    let found = containers
        .sweep(&Context {
            working: &working,
            declared: &[],
        })
        .expect("docker said nothing");

    // Both, because what is attached to is never decided by where anybody is
    // working — only the order is.
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].args, words(&["mine"]));
    assert_eq!(found[1].args, words(&["other"]));
}

#[test]
fn a_machine_with_no_docker_on_it_says_it_could_not_look_rather_than_that_there_is_nothing() {
    let containers = Containers::through(Docker::named("no-such-docker-on-this-machine"));
    assert_eq!(containers.every(), Duration::from_secs(15));

    assert!(containers.sweep(&nothing()).is_none());

    // Slower from now on, which is the same state the one log line was written
    // from: it is said when this turns over and not again until it turns back.
    assert_eq!(containers.every(), Duration::from_secs(60));
    assert!(containers.sweep(&nothing()).is_none());
    assert_eq!(containers.every(), Duration::from_secs(60));
}

#[test]
fn a_docker_that_comes_back_is_watched_again_at_the_ordinary_cadence() {
    let fake = Fake::new();
    let containers = Containers::through(Docker::named(fake.dir.path().join("not-yet")));
    assert!(containers.sweep(&nothing()).is_none());

    // Pointed at the stand-in rather than copied onto the name. A copy is a
    // file open for writing for as long as it takes, and these tests run
    // commands on several threads: a fork that happens inside that window
    // inherits the handle, and whoever tries to run the file next is refused
    // with `ETXTBSY`. A symbolic link opens nothing.
    std::os::unix::fs::symlink(&fake.binary, fake.dir.path().join("not-yet"))
        .expect("cannot put it there");

    assert!(
        containers
            .sweep(&nothing())
            .is_some_and(|found| found.is_empty())
    );
    assert_eq!(containers.every(), Duration::from_secs(15));
}

#[test]
fn provisioning_a_container_pushes_a_copy_starts_it_and_wires_up_the_agents_inside() {
    let fake = Fake::new();
    let local = Local::new();
    let container = fake.docker().container("eager_mclean");

    let mut running = sending(&local)
        .run(&container, &attach::FAR_END)
        .expect("nothing was started");

    let installed = fake.installed("eager_mclean");
    let calls = fake.calls_beside_the_probe();
    assert_eq!(
        calls,
        vec![
            // What it is, asked once and remembered.
            "inspect -f {{.Id}} eager_mclean".to_owned(),
            // Asked before anything is written, because a container that
            // already has the right copy is not written to at all.
            format!("exec -i eager_mclean sh -s -- {VERSION} subscribe --ensure-daemon"),
            format!("cp {} eager_mclean:{installed}", local.path.display()),
            format!("exec -i eager_mclean chmod +x {installed}"),
            format!("exec -i eager_mclean sh -s -- {VERSION} subscribe --ensure-daemon"),
            format!("exec -i eager_mclean {installed} install"),
        ],
        "{calls:#?}"
    );
    // The container was asked where a copy goes, once, and the copy went to a
    // directory of its own rather than straight into the one everybody in
    // there shares.
    let asked: Vec<String> = fake
        .calls()
        .into_iter()
        .filter(|call| call.contains(PROBE))
        .collect();
    assert_eq!(asked.len(), 1, "{asked:#?}");
    assert!(
        installed.ends_with(&format!("/agentbus-{VERSION}")),
        "{installed}"
    );
    assert!(
        Path::new(&installed)
            .parent()
            .is_some_and(|dir| dir.file_name().is_some_and(|name| name != "tmp")),
        "{installed}"
    );
    // Which is a copy of what was sent, at exactly that path and runnable.
    let copy = PathBuf::from(&installed);
    assert_eq!(
        fs::read(&copy).expect("nothing was written"),
        fs::read(&local.path).expect("cannot read the stand-in")
    );
    let mode = fs::metadata(&copy)
        .expect("nothing was written")
        .permissions()
        .mode();
    assert_ne!(
        mode & 0o100,
        0,
        "the copy that arrived cannot be run: {mode:o}"
    );
    let _ = running.kill();
    let _ = running.wait();
}

#[test]
fn a_caller_that_is_wiring_the_agents_up_itself_is_not_done_for_underneath() {
    let fake = Fake::new();
    let local = Local::new();
    let container = fake.docker().container("eager_mclean").wired_by_hand();

    let mut running = sending(&local)
        .run(&container, &attach::FAR_END)
        .expect("nothing was started");

    let installed = fake.installed("eager_mclean");
    let calls = fake.calls_beside_the_probe();
    assert!(
        !calls.contains(&format!("exec -i eager_mclean {installed} install")),
        "the hooks were put in anyway: {calls:#?}"
    );
    // Everything else about getting a copy in there is what it always was:
    // what the caller has taken over is the agents, not the provisioning.
    assert!(
        calls.contains(&format!(
            "cp {} eager_mclean:{installed}",
            local.path.display()
        )),
        "{calls:#?}"
    );
    let _ = running.kill();
    let _ = running.wait();
}

#[test]
fn a_container_that_already_has_the_right_copy_is_not_written_to_again() {
    let fake = Fake::new();
    let local = Local::new();
    let container = fake.docker().container("eager_mclean");
    let mut first = sending(&local)
        .run(&container, &attach::FAR_END)
        .expect("nothing was started");
    let _ = first.kill();
    let _ = first.wait();

    let mut second = sending(&local)
        .run(&container, &attach::FAR_END)
        .expect("nothing was started the second time");

    let after: Vec<String> = fake.calls_beside_the_probe().split_off(6);
    assert_eq!(
        after,
        vec![format!(
            "exec -i eager_mclean sh -s -- {VERSION} subscribe --ensure-daemon"
        )],
        "{after:#?}"
    );
    let _ = second.kill();
    let _ = second.wait();
}

#[test]
fn what_a_container_is_is_its_whole_id_however_it_was_named() {
    let fake = Fake::new();
    let local = Local::new();
    let container = fake.docker().container("eager_mclean");
    assert_eq!(container.identity(), None);

    let mut running = sending(&local)
        .run(&container, &attach::FAR_END)
        .expect("nothing was started");

    assert_eq!(container.identity().as_deref(), Some("full-eager_mclean"));
    assert!(
        fake.calls()
            .contains(&"inspect -f {{.Id}} eager_mclean".to_owned()),
        "{:#?}",
        fake.calls()
    );
    let _ = running.kill();
    let _ = running.wait();
}

#[test]
fn taking_it_back_out_of_a_container_removes_every_copy_that_was_put_there() {
    let fake = Fake::new();
    let local = Local::new();
    let container = fake.docker().container("eager_mclean");
    let mut running = sending(&local)
        .run(&container, &attach::FAR_END)
        .expect("nothing was started");
    let _ = running.kill();
    let _ = running.wait();
    let inside = fake.inside("eager_mclean");
    let installed = fake.installed("eager_mclean");
    // Two older ones: where a copy goes now, and where copies used to go
    // before that directory was made a per-user one.
    let beside = PathBuf::from(&installed)
        .parent()
        .expect("the copy has no directory")
        .join("agentbus-0.0.1");
    fs::write(&beside, "an older one").expect("cannot write it");
    fs::write(inside.join("tmp/agentbus-0.0.2"), "an older one still").expect("cannot write it");

    container.uninstall(VERSION).expect("cannot take it out");

    let last: Vec<String> = fake.calls_beside_the_probe().split_off(6);
    assert_eq!(last.len(), 2, "{last:#?}");
    assert_eq!(
        last[0],
        format!("exec -i eager_mclean {installed} uninstall")
    );
    assert!(
        last[1].contains("for f in \"$landing\"/agentbus-* /tmp/agentbus-*"),
        "{last:#?}"
    );
    assert!(
        copies(&inside.join("tmp")).is_empty(),
        "{:?}",
        copies(&inside.join("tmp"))
    );
}

/// A daemon's worth of state, and a reconciler over it.
struct Bench {
    _config: tempfile::TempDir,
    _run: tempfile::TempDir,
    targets: Targets,
    attachments: Attachments,
    bus: Arc<Bus>,
    local: Local,
}

impl Bench {
    fn new() -> Self {
        let config = tempfile::tempdir().expect("cannot make a temporary directory");
        let run = tempfile::tempdir().expect("cannot make a temporary directory");
        Self {
            targets: Targets::in_dir(config.path()),
            attachments: Attachments::in_dir(run.path()),
            bus: Arc::new(Bus::new()),
            local: Local::new(),
            _config: config,
            _run: run,
        }
    }

    fn reconciling(&self, containers: Containers) -> Reconciling {
        Reconciling::start(Plan {
            targets: self.targets.clone(),
            attachments: self.attachments.clone(),
            transports: Registry::new(),
            discoveries: vec![Arc::new(containers) as Arc<dyn Discovery>],
            bus: Arc::clone(&self.bus),
            bootstrap: sending(&self.local),
            attach: attach::Settings {
                liveness: Duration::from_secs(30),
                stable: Duration::from_secs(3_600),
            },
            every: Duration::from_millis(20),
        })
    }

    /// What is written down about the container called `name`, if anything.
    fn entry(&self, name: &str) -> Option<Entry> {
        self.attachments
            .read()
            .expect("cannot read what is attached")?
            .into_iter()
            .find(|entry| entry.args == words(&[name]))
    }

    /// What the bus says about the session the container called `name`
    /// reported.
    fn session_in(&self, name: &str) -> Option<SessionStatus> {
        self.session(name).map(|entry| entry.status)
    }

    /// The one hop that reaches the container that session is in.
    fn hop_to(&self, name: &str) -> Option<OriginHop> {
        self.session(name)
            .and_then(|entry| entry.origin.into_iter().next())
    }

    fn session(&self, name: &str) -> Option<SessionEntry> {
        self.bus
            .sessions()
            .into_iter()
            .find(|entry| entry.session == format!("session-in-{name}"))
    }
}

#[test]
fn containers_that_appear_and_go_away_are_attached_to_and_let_go_of() {
    let bench = Bench::new();
    let fake = Fake::new();
    fake.listing(&format!(
        "{}{}",
        line("running", "/w/api", "api", "1111aaaa"),
        line("exited", "/w/web", "web", "2222bbbb"),
    ));
    let containers = Containers::through(fake.docker())
        .looking_every(Duration::from_millis(20), Duration::from_millis(20));

    let _reconciling = bench.reconciling(containers);

    until("the running container was never attached to", || {
        bench.entry("api").map(|entry| entry.state) == Some(State::Attached)
    });
    let entry = bench.entry("api").expect("nothing was written down");
    assert_eq!(entry.transport, "docker");
    assert_eq!(entry.label, "api");
    assert_eq!(entry.identity.as_deref(), Some("full-1111aaaa"));
    // Nobody asked for it: it was found.
    assert!(entry.auto);
    // And the one that is not up is not attached to.
    assert_eq!(bench.entry("web"), None);
    // What the daemon in there knew is on this bus, under the id the container
    // is addressed by.
    assert_eq!(bench.session_in("1111aaaa"), Some(SessionStatus::Blocked));
    // And it arrived carrying the way to it: what it is drives everything, and
    // what it is called is there to be shown.
    let hop = bench.hop_to("1111aaaa").expect("it arrived from nowhere");
    assert_eq!(hop.kind, "container");
    assert_eq!(hop.id, "full-1111aaaa");
    assert_eq!(hop.name, "api");

    // It comes up.
    fake.listing(&format!(
        "{}{}",
        line("running", "/w/api", "api", "1111aaaa"),
        line("running", "/w/web", "web", "2222bbbb"),
    ));
    until("the container that came up was never noticed", || {
        bench.entry("web").map(|entry| entry.state) == Some(State::Attached)
    });

    // And the first one goes away.
    fake.listing(&line("running", "/w/web", "web", "2222bbbb"));
    until("the container that went away is still attached", || {
        bench.entry("api").is_none()
    });
    assert_eq!(
        bench.session_in("1111aaaa"),
        Some(SessionStatus::Done),
        "the sessions it was speaking for are still open"
    );
    assert_eq!(bench.session_in("2222bbbb"), Some(SessionStatus::Blocked));
}

#[test]
fn a_bus_on_a_machine_with_no_docker_carries_on_attached_to_nothing() {
    let bench = Bench::new();
    let containers = Containers::through(Docker::named("no-such-docker-on-this-machine"))
        .looking_every(Duration::from_millis(20), Duration::from_millis(20));

    let _reconciling = bench.reconciling(containers);

    // It says what it is doing, which is nothing, and keeps saying it: a
    // machine that cannot be asked about containers is an ordinary machine.
    until("nothing was ever written down", || {
        bench.attachments.read().expect("cannot read it").is_some()
    });
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        bench.attachments.read().expect("cannot read it"),
        Some(Vec::new())
    );
}

#[test]
fn a_container_is_declared_by_one_word_and_reached_whatever_it_carries() {
    let registry = Registry::standard();

    let made = registry
        .make("docker", &words(&["eager_mclean"]))
        .expect("this build cannot reach a container")
        .expect("a container could not be made");

    // Nothing about a label came into it: a container somebody names is one
    // they want, whether or not anything would ever have found it.
    assert_eq!(made.label(), "eager_mclean");
    assert_eq!(made.kind(), "container");
    assert_eq!(made.install_path("1.2.3"), "/tmp/agentbus-1.2.3");
    assert!(registry.make("docker", &[]).expect("unknown").is_err());
    assert!(
        registry
            .make("docker", &words(&["one", "two"]))
            .expect("unknown")
            .is_err()
    );
}
