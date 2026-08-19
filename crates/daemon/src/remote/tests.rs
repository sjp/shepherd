//! What provisioning an endpoint does, driven end to end through a transport
//! whose far end is a temporary directory on this machine.
//!
//! Every one of these runs the real script through a real shell and looks at
//! what really happened on the far end's filesystem, because the interesting
//! part of provisioning is not the Rust: it is which of six candidate paths the
//! script picked, what it did about a version that did not match, and whether
//! anything was written that should not have been.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::bootstrap::{self, Bootstrap, TARGET};
use super::loopback::Loopback;
use super::published::Published;
use super::release::Release;
use super::transport::{Backoff, Platform, Transport};
use crate::VERSION;

/// What `uname` says about the machine these tests are running on.
///
/// Written out rather than shelled out for, because a test that wants to know
/// what a push would decide has to know it before it runs anything.
fn here() -> Platform {
    let os = match std::env::consts::OS {
        "macos" => "Darwin",
        _ => "Linux",
    };
    let arch = match (os, std::env::consts::ARCH) {
        ("Darwin", "aarch64") => "arm64",
        (_, arch) => arch,
    };
    Platform::new(os, arch)
}

/// A machine that is not this one.
fn elsewhere() -> Platform {
    match here().os.as_str() {
        "Darwin" => Platform::new("Linux", "x86_64"),
        _ => Platform::new("Darwin", "arm64"),
    }
}

/// A shell script that answers `--version` with `version` and otherwise repeats
/// what it was asked to do.
///
/// This is the whole of what the bootstrap knows about an `agentbus`: a file
/// that gives the right answer to one question is one it will run, and what it
/// runs is what the caller gets a handle to.
fn agentbus(version: &str) -> String {
    format!(
        "#!/bin/sh\n\
         if [ \"$1\" = --version ]; then echo \"agentbus {version}\"; exit 0; fi\n\
         echo \"ran: $*\"\n"
    )
}

/// A `uname` that says the machine is `platform`, whatever it really is.
fn uname(platform: &Platform) -> String {
    let Platform { os, arch } = platform;
    format!(
        "#!/bin/sh\n\
         out=\n\
         for word in \"$@\"; do\n\
         case $word in\n\
         -s) out=\"$out {os}\" ;;\n\
         -m) out=\"$out {arch}\" ;;\n\
         esac\n\
         done\n\
         echo ${{out# }}\n"
    )
}

/// A file standing in for this program's own executable, ready to be sent.
///
/// Deliberately not made runnable: what makes a pushed copy runnable is the
/// provisioner, and a stand-in that arrived already executable would hide it
/// failing to do that.
struct Local {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Local {
    fn answering(version: &str) -> Self {
        let dir = tempfile::tempdir().expect("cannot make a temporary directory");
        let path = dir.path().join("agentbus");
        std::fs::write(&path, agentbus(version)).expect("cannot write the stand-in");
        Self { _dir: dir, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

/// A provisioner that sends `local` rather than whatever executable the tests
/// themselves are running from, and that has nowhere to fetch from.
///
/// Every test that is about the push path is given a release that is not there,
/// so that a test which unexpectedly reaches the fetch path fails locally
/// instead of going to the network.
fn sending(local: &Local) -> Bootstrap {
    Bootstrap::new(VERSION)
        .sending(local.path(), TARGET)
        .fetching(Release::at("file:///no/such/release", VERSION))
}

/// The far end's first line, which is what the command that was started there
/// said for itself.
fn first_line(running: &mut super::Running) -> String {
    let mut line = String::new();
    std::io::BufRead::read_line(running.stdout(), &mut line).expect("cannot read the far end");
    line.trim_end().to_owned()
}

#[test]
fn a_transport_can_be_held_without_knowing_which_kind_it_is() {
    let transport: Box<dyn Transport> = Box::new(Loopback::new().unwrap());

    assert_eq!(transport.kind(), "loopback");
    assert_eq!(transport.identity().as_deref(), Some("loopback"));
}

#[test]
fn what_the_far_end_is_comes_from_asking_it() {
    let transport = Loopback::new().unwrap();

    assert_eq!(transport.probe().unwrap(), here());
}

#[test]
fn a_copy_of_the_right_version_is_run_where_it_was_found() {
    let transport = Loopback::new().unwrap();
    let planted = transport.plant("agentbus", &agentbus(VERSION)).unwrap();
    let local = Local::answering(VERSION);

    let mut running = sending(&local)
        .run(&transport, &["subscribe", "--ensure-daemon"])
        .expect("the far end was not started");

    assert_eq!(first_line(&mut running), "ran: subscribe --ensure-daemon");
    assert!(
        transport.copied().is_empty(),
        "something was sent to a far end that was already current: {:?}",
        transport.copied()
    );
    assert_eq!(
        std::fs::read_to_string(&planted).unwrap(),
        agentbus(VERSION),
        "the copy that was already there was written over"
    );
}

#[test]
fn a_copy_of_the_wrong_version_is_left_alone_and_one_is_put_alongside_it() {
    let transport = Loopback::new().unwrap();
    let theirs = agentbus("0.0.1-theirs");
    let planted = transport.plant("agentbus", &theirs).unwrap();
    let local = Local::answering(VERSION);

    let mut running = sending(&local)
        .run(&transport, &["subscribe"])
        .expect("the far end was not started");

    assert_eq!(first_line(&mut running), "ran: subscribe");
    assert_eq!(
        std::fs::read_to_string(&planted).unwrap(),
        theirs,
        "somebody else's installation was written over"
    );
    let copied = transport.copied();
    assert_eq!(copied.len(), 1, "{copied:?}");
    assert_eq!(copied[0].1, transport.install_path(VERSION));
}

#[test]
fn a_far_end_with_nothing_on_it_is_provisioned_and_started() {
    let transport = Loopback::new().unwrap();
    let local = Local::answering(VERSION);

    let mut running = sending(&local)
        .run(&transport, &["subscribe"])
        .expect("the far end was not started");

    assert_eq!(first_line(&mut running), "ran: subscribe");
    assert_eq!(transport.copied().len(), 1);
}

#[test]
fn a_second_run_against_a_provisioned_far_end_writes_nothing() {
    let transport = Loopback::new().unwrap();
    let local = Local::answering(VERSION);
    sending(&local).run(&transport, &["subscribe"]).unwrap();

    sending(&local).run(&transport, &["subscribe"]).unwrap();

    assert_eq!(
        transport.copied().len(),
        1,
        "a far end that was already current was written to again"
    );
}

#[test]
fn a_copy_that_still_does_not_answer_is_reported_after_exactly_one_retry() {
    let transport = Loopback::new().unwrap();
    let local = Local::answering("0.0.1-not-what-was-asked-for");

    let error = sending(&local)
        .run(&transport, &["subscribe"])
        .expect_err("a far end that never became usable was reported as started");

    assert!(
        matches!(&error, bootstrap::Error::NotVerified { path, version, .. }
            if *path == transport.install_path(VERSION) && version == VERSION),
        "{error:?}"
    );
    assert_eq!(
        transport.copied().len(),
        1,
        "the copy was sent more than once"
    );
}

#[test]
fn a_copy_that_arrived_truncated_is_reported_rather_than_run() {
    let transport = Loopback::new().unwrap().truncating_copies();
    let local = Local::answering(VERSION);

    let error = sending(&local)
        .run(&transport, &["subscribe"])
        .expect_err("a truncated copy was run at the far end");

    assert!(
        matches!(error, bootstrap::Error::NotVerified { .. }),
        "{error:?}"
    );
    assert_eq!(
        transport.copied().len(),
        1,
        "the copy was sent more than once"
    );
}

#[test]
fn a_far_end_this_build_cannot_supply_is_fetched_for_and_provisioned() {
    let transport = Loopback::new().unwrap();
    let wanted = elsewhere();
    let triple = wanted.triple().unwrap();
    transport.plant("uname", &uname(&wanted)).unwrap();
    let local = Local::answering("0.0.1-would-not-do");
    let site = Published::of(VERSION, &[triple], &agentbus(VERSION)).write();
    let cache = tempfile::tempdir().unwrap();

    let mut running = Bootstrap::new(VERSION)
        .sending(local.path(), TARGET)
        .fetching(Release::at(site.base(), VERSION).caching_in(cache.path()))
        .run(&transport, &["subscribe"])
        .expect("the far end was not started");

    assert_eq!(first_line(&mut running), "ran: subscribe");
    let copied = transport.copied();
    assert_eq!(copied.len(), 1, "{copied:?}");
    assert_eq!(copied[0].1, transport.install_path(VERSION));
    assert_ne!(
        copied[0].0,
        local.path(),
        "the binary this build happens to hold was sent to a machine that cannot run it"
    );
    assert_eq!(
        std::fs::read_to_string(&copied[0].0).unwrap(),
        agentbus(VERSION),
        "what was sent is not what the release published"
    );
}

#[test]
fn a_far_end_nothing_can_supply_a_binary_for_is_named_and_nothing_is_sent() {
    let transport = Loopback::new().unwrap();
    let wanted = elsewhere();
    transport.plant("uname", &uname(&wanted)).unwrap();
    let local = Local::answering(VERSION);
    let site = Published::of(VERSION, &[], "").write();
    let cache = tempfile::tempdir().unwrap();

    let error = Bootstrap::new(VERSION)
        .sending(local.path(), TARGET)
        .fetching(Release::at(site.base(), VERSION).caching_in(cache.path()))
        .run(&transport, &["subscribe"])
        .expect_err("a binary for another kind of machine was sent anyway");

    assert!(
        matches!(&error, bootstrap::Error::NoBinaryFor { triple, target, .. }
            if triple == wanted.triple().unwrap() && target == TARGET),
        "{error:?}"
    );
    let said = error.to_string();
    assert!(
        said.contains("AGENTBUS_REMOTE_BINARY"),
        "the way out is not named: {said}"
    );
    assert!(transport.copied().is_empty(), "{:?}", transport.copied());
}

#[test]
fn a_machine_no_release_is_built_for_is_named_rather_than_guessed_at() {
    let transport = Loopback::new().unwrap();
    transport
        .plant("uname", &uname(&Platform::new("Plan9", "vax")))
        .unwrap();
    let local = Local::answering(VERSION);

    let error = sending(&local)
        .run(&transport, &["subscribe"])
        .expect_err("an unknown machine was provisioned anyway");

    assert!(
        matches!(&error, bootstrap::Error::UnknownPlatform { platform, .. }
            if platform == &Platform::new("Plan9", "vax")),
        "{error:?}"
    );
    assert!(transport.copied().is_empty(), "{:?}", transport.copied());
}

#[test]
fn a_far_end_that_is_told_where_its_binary_is_uses_that_one() {
    let transport = Loopback::new().unwrap();
    let named = transport.plant("chosen", &agentbus(VERSION)).unwrap();
    let local = Local::answering(VERSION);

    // The variable is read by the script itself, so it has to be in the
    // environment of the far end rather than of this process.
    let mut running = Bootstrap::new(VERSION)
        .sending(local.path(), TARGET)
        .run(&Named(&transport, named), &["subscribe"])
        .expect("the far end was not started");

    assert_eq!(first_line(&mut running), "ran: subscribe");
    assert!(transport.copied().is_empty());
}

/// A loopback whose far end has `AGENTBUS_REMOTE_BINARY` pointing somewhere.
#[derive(Debug)]
struct Named<'a>(&'a Loopback, PathBuf);

impl Transport for Named<'_> {
    fn kind(&self) -> &'static str {
        self.0.kind()
    }
    fn label(&self) -> String {
        self.0.label()
    }
    fn identity(&self) -> Option<String> {
        self.0.identity()
    }
    fn install_path(&self, version: &str) -> String {
        self.0.install_path(version)
    }
    fn copy_in(&self, local: &Path, remote: &str) -> Result<(), super::transport::Error> {
        self.0.copy_in(local, remote)
    }
    fn backoff(&self) -> Backoff {
        self.0.backoff()
    }
    fn run(
        &self,
        command: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<super::Running, super::transport::Error> {
        self.0.running(command, args, stdin, |process| {
            process.env("AGENTBUS_REMOTE_BINARY", &self.1);
        })
    }
}

#[test]
fn the_triples_are_the_ones_a_release_is_built_for() {
    assert_eq!(
        Platform::new("Linux", "x86_64").triple(),
        Some("x86_64-unknown-linux-musl")
    );
    assert_eq!(
        Platform::new("Linux", "aarch64").triple(),
        Some("aarch64-unknown-linux-musl")
    );
    assert_eq!(
        Platform::new("Darwin", "arm64").triple(),
        Some("aarch64-apple-darwin")
    );
    assert_eq!(
        Platform::new("Darwin", "x86_64").triple(),
        Some("x86_64-apple-darwin")
    );
    assert_eq!(Platform::new("Linux", "riscv64").triple(), None);
    assert_eq!(Platform::new("FreeBSD", "x86_64").triple(), None);
}

#[test]
fn a_machine_runs_a_build_for_its_own_system_whatever_libc_it_was_linked_against() {
    let linux = Platform::new("Linux", "x86_64");

    assert!(linux.runs("x86_64-unknown-linux-musl"));
    assert!(linux.runs("x86_64-unknown-linux-gnu"));
    assert!(!linux.runs("aarch64-unknown-linux-musl"));
    assert!(!linux.runs("x86_64-apple-darwin"));
}

#[test]
fn the_two_words_for_one_architecture_compare_equal() {
    assert!(Platform::new("Darwin", "arm64").runs("aarch64-apple-darwin"));
    assert!(!Platform::new("Darwin", "arm64").runs("aarch64-unknown-linux-musl"));
}

#[test]
fn this_build_is_one_this_machine_can_run() {
    assert!(here().runs(TARGET), "{TARGET} is not for {}", here());
}

#[test]
fn what_uname_prints_is_read_as_two_words_and_no_more() {
    assert_eq!(
        Platform::parse("Linux x86_64\n"),
        Some(Platform::new("Linux", "x86_64"))
    );
    assert_eq!(Platform::parse("Linux"), None);
    assert_eq!(Platform::parse("Linux x86_64 and something else"), None);
    assert_eq!(Platform::parse(""), None);
}

#[test]
fn a_backoff_grows_to_its_ceiling_and_stops_there() {
    let backoff = Backoff {
        initial: Duration::from_millis(100),
        max: Duration::from_secs(1),
        multiplier: 2.0,
        jitter: 0.5,
    };

    assert_eq!(backoff.base(0), Duration::from_millis(100));
    assert_eq!(backoff.base(1), Duration::from_millis(200));
    assert_eq!(backoff.base(4), Duration::from_secs(1));
    assert_eq!(backoff.base(40), Duration::from_secs(1));
}

#[test]
fn jitter_spreads_a_delay_either_side_of_itself() {
    let backoff = Backoff {
        initial: Duration::from_millis(100),
        max: Duration::from_secs(10),
        multiplier: 2.0,
        jitter: 0.5,
    };

    assert_eq!(backoff.delay(0, 0.5), Duration::from_millis(100));
    assert_eq!(backoff.delay(0, 0.0), Duration::from_millis(50));
    assert_eq!(backoff.delay(0, 1.0), Duration::from_millis(150));
}

#[test]
fn a_backoff_without_jitter_is_the_delay_itself() {
    let backoff = Backoff {
        initial: Duration::from_millis(100),
        max: Duration::from_secs(10),
        multiplier: 2.0,
        jitter: 0.0,
    };

    assert_eq!(backoff.delay(1, 0.0), backoff.base(1));
    assert_eq!(backoff.delay(1, 1.0), backoff.base(1));
}
