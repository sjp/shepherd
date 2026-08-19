//! Resolution against an `ssh` that is a recording.
//!
//! Nothing here runs `ssh` and nothing here reaches a network. What is under
//! test is a command line, what comes back from it and what is made of that, so
//! the command is a closure that answers with output captured from a real ssh
//! and remembers what it was asked — which is also the only way to assert the
//! thing that matters most here, that a declaration carrying a command is
//! refused *before* anything is run.

use std::fs;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use super::*;

/// The versions of OpenSSH whose output is recorded beside these tests.
const VERSIONS: [&str; 4] = [
    "openssh-8.9",
    "openssh-9.6",
    "openssh-10.0",
    "openssh-unreleased",
];

/// Who, where and on what port the recorded `fileserver` resolutions describe.
const FILESERVER: (&str, &str, u16) = ("vscode", "192.168.0.42", 22);

/// One of the recorded resolutions.
fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ssh")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// What the recorded resolution `name` says.
fn resolved(name: &str) -> Resolved {
    Resolved::read(&fixture(name))
}

/// An `ssh` that never runs: it answers with what a test gave it and remembers
/// everything it was asked.
struct Fake {
    status: i32,
    stdout: String,
    stderr: String,
    broken: bool,
    asked: Mutex<Vec<Vec<String>>>,
}

impl Fake {
    /// One that succeeds, printing `stdout`.
    fn printing(stdout: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            status: 0,
            stdout: stdout.into(),
            stderr: String::new(),
            broken: false,
            asked: Mutex::new(Vec::new()),
        })
    }

    /// One that exits with `status` and complains.
    fn failing(status: i32, stderr: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            status,
            stdout: String::new(),
            stderr: stderr.into(),
            broken: false,
            asked: Mutex::new(Vec::new()),
        })
    }

    /// One that cannot be started at all.
    fn broken() -> Arc<Self> {
        Arc::new(Self {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
            broken: true,
            asked: Mutex::new(Vec::new()),
        })
    }

    fn run(&self, argv: &[String]) -> io::Result<Output> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(argv.to_vec());
        if self.broken {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(Output {
            status: ExitStatus::from_raw(self.status << 8),
            stdout: self.stdout.clone().into_bytes(),
            stderr: self.stderr.clone().into_bytes(),
        })
    }

    /// Every argument vector it was run with, oldest first.
    fn asked(&self) -> Vec<Vec<String>> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many times it was run.
    fn runs(&self) -> usize {
        self.asked().len()
    }
}

/// A resolver driven by `fake` and watching nothing, so that only what a test
/// does can empty it.
fn resolver(fake: &Arc<Fake>) -> Resolver {
    watching(fake, Vec::new())
}

/// The same, watching `configs`.
fn watching(fake: &Arc<Fake>, configs: Vec<PathBuf>) -> Resolver {
    let fake = Arc::clone(fake);
    Resolver::new()
        .watching(configs)
        .running(move |argv: &[String]| fake.run(argv))
}

/// The words of a declaration, as somebody would have typed them.
fn declared(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|word| (*word).to_owned()).collect()
}

#[test]
fn every_recorded_version_parses_to_the_same_endpoint() {
    for version in VERSIONS {
        let resolved = resolved(&format!("{version}/fileserver"));
        assert_eq!(
            resolved.provisional_identity(),
            (
                FILESERVER.0.to_owned(),
                FILESERVER.1.to_owned(),
                FILESERVER.2
            ),
            "{version}"
        );
        assert_eq!(resolved.host(), Some("fileserver"), "{version}");
        assert!(
            resolved.keys().count() > 50,
            "{version} resolved to {} keys",
            resolved.keys().count()
        );
    }
}

#[test]
fn keys_nobody_here_has_heard_of_are_kept_rather_than_refused() {
    let resolved = resolved("openssh-unreleased/fileserver");
    assert_eq!(resolved.first("postquantumonly"), Some("no"));
    assert_eq!(resolved.first("keystrokeintervalms"), Some("20"));
    assert_eq!(resolved.first("warnweakcrypto"), Some("yes"));
    // A key printed with nothing after it is a key with no value, not a line to
    // be thrown away: the next release may well give it one.
    assert_eq!(resolved.first("attestationfile"), Some(""));
    assert_eq!(resolved.first("tags"), Some(""));
    // And the ordinary ones are still there beside them.
    assert_eq!(resolved.hostname(), Some("192.168.0.42"));
}

#[test]
fn a_key_ssh_prints_with_a_capital_in_it_is_found_by_its_lowercase_name() {
    let resolved = resolved("openssh-10.0/fileserver");
    assert_eq!(resolved.first("canonicalizepermittedcnames"), Some("none"));
}

#[test]
fn a_key_printed_more_than_once_keeps_every_value() {
    let ordinary = resolved("openssh-10.0/fileserver");
    let identities = ordinary.all("identityfile");
    assert!(
        identities.len() > 1 && identities.contains(&"~/.ssh/id_ed25519".to_owned()),
        "{identities:?}"
    );
    assert_eq!(ordinary.first("identityfile"), Some("~/.ssh/id_rsa"));
    assert_eq!(
        resolved("openssh-unreleased/fileserver").all("sendenv"),
        ["LANG".to_owned(), "LC_*".to_owned()]
    );
}

#[test]
fn a_value_that_is_a_list_keeps_the_spaces_in_it() {
    let resolved = resolved("openssh-10.0/fileserver");
    assert_eq!(resolved.first("rekeylimit"), Some("0 0"));
    assert_eq!(resolved.first("ipqos"), Some("ef cs1"));
}

/// A declaration, the resolution recorded for it, and who and where that
/// resolution says the machine is.
type Case = (
    &'static [&'static str],
    &'static str,
    (&'static str, &'static str, u16),
);

#[test]
fn the_argument_vectors_people_type_resolve_to_the_endpoint_they_name() {
    let cases: [Case; 5] = [
        (&["fileserver"], "fileserver", FILESERVER),
        (
            &["vscode@fileserver"],
            "vscode-at-fileserver",
            ("vscode", "192.168.0.42", 22),
        ),
        (
            &[
                "-p",
                "2222",
                "-o",
                "StrictHostKeyChecking=no",
                "bob@fs.haze.nz",
            ],
            "bob-at-fs-haze-nz",
            ("bob", "fs.haze.nz", 2222),
        ),
        (
            &["-J", "bastion.example.com", "deep@inner"],
            "deep-at-inner",
            ("deep", "10.0.7.9", 22),
        ),
        (
            &["-p2222", "fileserver"],
            "glued-port",
            ("vscode", "192.168.0.42", 2222),
        ),
    ];
    for (argv, recorded, expected) in cases {
        let fake = Fake::printing(fixture(&format!("openssh-10.0/{recorded}")));
        let declared = declared(argv);
        let resolved = resolver(&fake)
            .resolve(&declared)
            .unwrap_or_else(|error| panic!("{argv:?}: {error}"));
        let (user, hostname, port) = resolved.provisional_identity();
        assert_eq!(
            (user.as_str(), hostname.as_str(), port),
            expected,
            "{argv:?}"
        );
    }
}

#[test]
fn what_was_declared_is_what_ssh_is_run_with() {
    let fake = Fake::printing(fixture("openssh-10.0/bob-at-fs-haze-nz"));
    let declared = declared(&[
        "-p",
        "2222",
        "-o",
        "StrictHostKeyChecking=no",
        "bob@fs.haze.nz",
    ]);
    resolver(&fake).resolve(&declared).expect("cannot resolve");
    let mut expected = vec![DUMP.to_owned()];
    expected.extend(declared);
    assert_eq!(fake.asked(), vec![expected]);
}

#[test]
fn a_declaration_that_carries_a_command_is_refused_before_ssh_is_run() {
    for argv in [
        &["host", "vim"][..],
        &["host", "--", "vim"][..],
        &["-o", "Foo=bar", "host", "ls", "-la"][..],
    ] {
        let fake = Fake::printing(fixture("openssh-10.0/fileserver"));
        let error = resolver(&fake)
            .resolve(&declared(argv))
            .expect_err("that should not have resolved");
        assert!(
            matches!(error, Error::RemoteCommandNotAllowed { .. }),
            "{argv:?} gave {error:?}"
        );
        assert_eq!(fake.runs(), 0, "{argv:?} reached ssh");
        assert!(error.to_string().contains("command"), "{error}");
    }
}

#[test]
fn a_declaration_that_names_no_destination_is_refused_before_ssh_is_run() {
    for argv in [&[][..], &["-v"][..], &["-p", "2222"][..], &[""][..]] {
        let fake = Fake::printing(fixture("openssh-10.0/fileserver"));
        let error = resolver(&fake)
            .resolve(&declared(argv))
            .expect_err("that should not have resolved");
        assert!(
            matches!(error, Error::DestinationMissing { .. }),
            "{argv:?} gave {error:?}"
        );
        assert_eq!(fake.runs(), 0, "{argv:?} reached ssh");
    }
}

#[test]
fn an_options_value_is_never_mistaken_for_a_destination() {
    let one = [
        &["fileserver"][..],
        &["vscode@fileserver"][..],
        &["-p", "2222", "fileserver"][..],
        &["-p2222", "fileserver"][..],
        &["-o", "StrictHostKeyChecking=no", "bob@fs.haze.nz"][..],
        &["-oStrictHostKeyChecking=no", "bob@fs.haze.nz"][..],
        &["-J", "bastion.example.com", "deep@inner"][..],
        &["-i", "/home/vscode/.ssh/id_ed25519", "fileserver"][..],
        &["-4", "-v", "-p", "22", "fileserver"][..],
        // Clustered, the way people actually type it.
        &["-vp", "2222", "fileserver"][..],
        &["-4vvv", "fileserver"][..],
        &["--", "fileserver"][..],
        // An option nobody here knows about, taking no argument, which is the
        // guess that keeps a declaration usable.
        &["-Z", "fileserver"][..],
    ];
    for argv in one {
        assert_eq!(
            one_destination(&declared(argv)).ok(),
            argv.last().copied(),
            "{argv:?}"
        );
    }
}

#[test]
fn an_unfamiliar_option_that_takes_a_value_is_refused_rather_than_obeyed() {
    // The worst this module does when OpenSSH grows an option it has not been
    // told about: the value looks like a second positional argument and the
    // declaration is refused with a message, rather than the value being
    // forwarded as something to run.
    let error = one_destination(&declared(&["-Z", "something", "fileserver"]))
        .expect_err("that should not have been accepted");
    assert!(matches!(error, Error::RemoteCommandNotAllowed { .. }));
}

#[test]
fn an_invalid_target_is_reported_with_what_ssh_said() {
    let complaint = "ssh: Could not resolve hostname nope.invalid: Name or service not known";
    let fake = Fake::failing(255, format!("{complaint}\n"));
    let error = resolver(&fake)
        .resolve(&declared(&["nope.invalid"]))
        .expect_err("that should not have resolved");
    let Error::TargetInvalid { argv, stderr } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(argv, &declared(&["nope.invalid"]));
    assert_eq!(stderr, &format!("{complaint}\n"));
    let printed = error.to_string();
    assert!(printed.contains("invalid"), "{printed}");
    assert!(printed.contains(complaint), "{printed}");
}

#[test]
fn a_failure_that_is_not_a_verdict_on_the_target_is_reported_separately() {
    let fake = Fake::failing(1, "something else went wrong\n");
    let error = resolver(&fake)
        .resolve(&declared(&["fileserver"]))
        .expect_err("that should not have resolved");
    assert!(matches!(error, Error::ResolveFailed { .. }), "{error:?}");
    assert!(error.to_string().contains("something else went wrong"));
}

#[test]
fn an_ssh_that_cannot_be_started_is_reported_as_such() {
    let fake = Fake::broken();
    let error = resolver(&fake)
        .resolve(&declared(&["fileserver"]))
        .expect_err("that should not have resolved");
    assert!(matches!(error, Error::Run { .. }), "{error:?}");
}

#[test]
fn the_same_declaration_is_asked_about_once() {
    let fake = Fake::printing(fixture("openssh-10.0/fileserver"));
    let resolver = resolver(&fake);
    let first = resolver.resolve(&declared(&["fileserver"])).unwrap();
    let again = resolver.resolve(&declared(&["fileserver"])).unwrap();
    assert_eq!(fake.runs(), 1);
    assert!(Arc::ptr_eq(&first, &again));
}

#[test]
fn declarations_that_differ_at_all_are_different_declarations() {
    let fake = Fake::printing(fixture("openssh-10.0/fileserver"));
    let resolver = resolver(&fake);
    for argv in [
        &["fileserver"][..],
        &["other"][..],
        // The same words in another order: deciding these were one question
        // would mean knowing which of ssh's options commute.
        &["-p", "2222", "fileserver"][..],
        &["fileserver", "-p", "2222"][..],
    ] {
        resolver.resolve(&declared(argv)).expect("cannot resolve");
    }
    assert_eq!(fake.runs(), 4);
}

#[test]
fn a_changed_configuration_is_asked_about_again() {
    let dir = tempfile::tempdir().expect("cannot make a temporary directory");
    let config = dir.path().join("config");
    fs::write(&config, "Host fileserver\n  HostName 192.168.0.42\n").expect("cannot write it");
    let fake = Fake::printing(fixture("openssh-10.0/fileserver"));
    let resolver = watching(&fake, vec![config.clone()]);

    resolver.resolve(&declared(&["fileserver"])).unwrap();
    resolver.resolve(&declared(&["fileserver"])).unwrap();
    assert_eq!(fake.runs(), 1);

    touch(&config, SystemTime::now() + Duration::from_secs(10));
    resolver.resolve(&declared(&["fileserver"])).unwrap();
    assert_eq!(fake.runs(), 2);
}

#[test]
fn a_configuration_that_did_not_exist_and_now_does_is_a_change() {
    let dir = tempfile::tempdir().expect("cannot make a temporary directory");
    let config = dir.path().join("config");
    let fake = Fake::printing(fixture("openssh-10.0/fileserver"));
    let resolver = watching(&fake, vec![config.clone()]);

    resolver.resolve(&declared(&["fileserver"])).unwrap();
    assert_eq!(fake.runs(), 1);

    fs::write(&config, "Host fileserver\n").expect("cannot write it");
    resolver.resolve(&declared(&["fileserver"])).unwrap();
    assert_eq!(fake.runs(), 2);
}

#[test]
fn everything_can_be_forgotten_on_request() {
    let fake = Fake::printing(fixture("openssh-10.0/fileserver"));
    let resolver = resolver(&fake);
    resolver.resolve(&declared(&["fileserver"])).unwrap();
    resolver.invalidate_all();
    resolver.resolve(&declared(&["fileserver"])).unwrap();
    assert_eq!(fake.runs(), 2);
}

#[test]
fn the_settings_the_daemon_asks_about_are_read_off_the_resolution() {
    let multiplexed = resolved("openssh-10.0/with-multiplexing");
    assert_eq!(multiplexed.batchmode(), Some(true));
    assert_eq!(multiplexed.proxycommand(), Some("/usr/bin/nc %h %p"));
    assert!(
        multiplexed
            .controlpath()
            .is_some_and(|path| path.starts_with("/run/user/1000/agentbus/ssh-")),
        "{:?}",
        multiplexed.controlpath()
    );
    assert_eq!(multiplexed.proxyjump(), None);

    let jumped = resolved("openssh-10.0/deep-at-inner");
    assert_eq!(jumped.proxyjump(), Some("bastion.example.com"));

    let plain = resolved("openssh-10.0/fileserver");
    assert_eq!(plain.batchmode(), Some(false));
    assert_eq!(plain.controlpath(), None);
    assert_eq!(plain.proxycommand(), None);
}

#[test]
fn a_setting_that_is_not_there_is_nothing_rather_than_a_failure() {
    let nothing = Resolved::default();
    assert_eq!(nothing.host(), None);
    assert_eq!(nothing.user(), None);
    assert_eq!(nothing.hostname(), None);
    assert_eq!(nothing.port(), None);
    assert_eq!(nothing.controlpath(), None);
    assert_eq!(nothing.proxyjump(), None);
    assert_eq!(nothing.proxycommand(), None);
    assert_eq!(nothing.batchmode(), None);
    assert_eq!(nothing.all("anything"), Vec::<String>::new().as_slice());
}

#[test]
fn a_port_that_is_not_a_number_ssh_could_have_meant_is_no_port() {
    for printed in ["port whatever", "port -1", "port 99999", "port"] {
        assert_eq!(Resolved::read(printed).port(), None, "{printed}");
    }
    assert_eq!(Resolved::read("port 2222").port(), Some(2222));
}

#[test]
fn what_ssh_did_not_say_is_filled_in_the_way_ssh_would_have_filled_it_in() {
    let sparse = Resolved::read("host fileserver\n");
    assert_eq!(
        sparse.provisional_identity(),
        (login_name(), "fileserver".to_owned(), DEFAULT_PORT)
    );
    let empty = Resolved::read("user\nhostname box\n");
    assert_eq!(empty.provisional_identity().0, login_name());
}

#[test]
fn blank_lines_and_trailing_space_are_not_settings() {
    let resolved = Resolved::read("\n  \nuser  vscode  \n\nport 22\n");
    assert_eq!(resolved.keys().collect::<Vec<_>>(), ["port", "user"]);
    assert_eq!(resolved.user(), Some("vscode"));
    assert_eq!(resolved.port(), Some(22));
}

#[test]
fn the_configuration_watched_is_the_users_and_the_machines() {
    assert_eq!(
        well_known(Some("/home/vscode".into())),
        vec![
            PathBuf::from("/home/vscode/.ssh/config"),
            PathBuf::from(SYSTEM_CONFIG)
        ]
    );
    for missing in [None, Some(String::new().into())] {
        assert_eq!(well_known(missing), vec![PathBuf::from(SYSTEM_CONFIG)]);
    }
}

#[test]
fn this_process_can_say_what_it_is_called() {
    assert!(!login_name().is_empty());
}

/// Puts `when` on a file's modification time, the way an editor writing it
/// again would.
fn touch(path: &Path, when: SystemTime) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("cannot open it");
    file.set_times(fs::FileTimes::new().set_modified(when))
        .expect("cannot set its times");
}
