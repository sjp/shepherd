//! Reaching a host through an `ssh` that is a recording.
//!
//! Nothing here runs `ssh`, opens a socket or touches a network. What is under
//! test is the command lines this daemon builds, the files it writes beside them
//! and what it makes of what ssh says — so ssh is a fake that remembers every
//! argument list it was given and answers with whatever the test chose, which is
//! also the only way to assert the things that matter most: that the words
//! somebody declared arrive unaltered, and that a failure nobody can retry their
//! way out of is recognized as one.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::sync::{Arc, Mutex, PoisonError};

use super::control::Masters;
use super::resolve::Resolver;
use super::transport::{Driver, Host};
use super::trouble::Trouble;
use crate::remote::transport::{Registry, Running, Transport};

/// The declaration most of these use, and the resolution ssh gives for it.
const DECLARED: [&str; 3] = ["-p", "2222", "bob@fs.example.net"];

/// What the recorded `ssh -G` for that declaration says.
const RESOLUTION: &str = "host fs.example.net\nuser bob\nhostname fs.example.net\nport 2222\n";

/// The words a declaration is made of.
fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// An exit status that says a command failed the way ssh fails.
fn failed() -> ExitStatus {
    ExitStatus::from_raw(255 << 8)
}

/// What one call of the fake ssh answers with.
#[derive(Debug, Clone)]
struct Answer {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Answer {
    fn ok() -> Self {
        Self {
            status: ExitStatus::from_raw(0),
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn printing(stdout: &str) -> Self {
        Self {
            stdout: stdout.to_owned(),
            ..Self::ok()
        }
    }

    fn refused(stderr: &str) -> Self {
        Self {
            status: failed(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }
}

/// An `ssh` that never runs: it remembers what it was asked and answers with
/// what the test decided, choosing by the first word of the command it was given
/// that says which kind of question it is.
#[derive(Debug, Default)]
struct Fake {
    asked: Mutex<Vec<Vec<String>>>,
    poured: Mutex<Vec<u8>>,
    answers: Mutex<Vec<(String, Answer)>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Answers a command containing `matching` with `answer`, and everything
    /// else with success and no output.
    fn answering(self: &Arc<Self>, matching: &str, answer: Answer) -> Arc<Self> {
        self.answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((matching.to_owned(), answer));
        Arc::clone(self)
    }

    /// Every argument list it was given, in order.
    fn asked(&self) -> Vec<Vec<String>> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The argument lists whose words include `word`.
    fn asked_about(&self, word: &str) -> Vec<Vec<String>> {
        self.asked()
            .into_iter()
            .filter(|argv| argv.iter().any(|token| token.contains(word)))
            .collect()
    }

    fn record(&self, argv: &[String]) -> Answer {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(argv.to_vec());
        let answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        answers
            .iter()
            .find(|(matching, _)| argv.iter().any(|token| token.contains(matching)))
            .map_or_else(Answer::ok, |(_, answer)| answer.clone())
    }
}

impl Driver for Arc<Fake> {
    fn start(&self, argv: &[String], _stdin: Option<&str>) -> io::Result<Running> {
        let answer = self.record(argv);
        // A handle has to be a real process, so it is the one local command
        // every machine has: it says what the fake decided and stops.
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            &format!(
                "printf %s {}; exit {}",
                shell(&answer.stdout),
                answer.status.code().unwrap_or_default()
            ),
        ]);
        Running::spawn(&mut command, None)
    }

    fn collect(&self, argv: &[String], stdin: Option<&mut dyn Read>) -> io::Result<Output> {
        if let Some(bytes) = stdin {
            let mut poured = Vec::new();
            bytes.read_to_end(&mut poured)?;
            self.poured
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(poured);
        }
        let answer = self.record(argv);
        Ok(Output {
            status: answer.status,
            stdout: answer.stdout.into_bytes(),
            stderr: answer.stderr.into_bytes(),
        })
    }
}

/// One word for `sh -c`, so the fake can print whatever a test chose.
fn shell(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The path a copy script wrote to before moving it into place, read back out
/// of the script itself rather than predicted, since what makes it safe is
/// that nothing but the script that generated it knows it in advance.
fn partial_name_in(script: &str) -> String {
    let after = script
        .split("cat > '")
        .nth(1)
        .expect("no partial name in the script");
    after
        .split('\'')
        .next()
        .expect("no closing quote")
        .to_owned()
}

/// A resolver that answers with `RESOLUTION` and never runs anything.
fn resolver() -> Resolver {
    Resolver::new().watching([]).running(|_argv: &[String]| {
        Ok(Output {
            status: ExitStatus::from_raw(0),
            stdout: RESOLUTION.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    })
}

/// A host reached through `driver`, with its connections kept in `dir`.
fn host(dir: &Path, driver: &Arc<Fake>) -> Arc<dyn Transport> {
    Host::built(
        &words(&DECLARED),
        &resolver(),
        &Masters::under(dir),
        Arc::new(Arc::clone(driver)),
    )
    .expect("that declaration was refused")
}

#[test]
fn our_options_come_first_the_declaration_next_and_the_command_after_a_separator() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new();
    let host = host(dir.path(), &ssh);

    let mut running = host
        .run("agentbus", &["subscribe", "--ensure-daemon"], None)
        .expect("nothing was started");
    running.wait().unwrap();

    let started = ssh.asked_about("subscribe");
    assert_eq!(started.len(), 1);
    let socket = format!("{}/cm-%C", dir.path().display());
    let config = dir.path().join("config-").display().to_string();
    let argv = &started[0];
    assert_eq!(argv[0], "-F");
    assert!(argv[1].starts_with(&config), "{argv:?}");
    assert_eq!(
        argv[2..],
        words(&[
            "-o",
            "ControlMaster=auto",
            "-o",
            &format!("ControlPath={socket}"),
            "-o",
            "ControlPersist=60",
            "-o",
            "BatchMode=yes",
            // Verbatim, in order, untouched.
            "-p",
            "2222",
            "bob@fs.example.net",
            "--",
            "agentbus",
            "subscribe",
            "--ensure-daemon",
        ])[..]
    );
}

#[test]
fn the_generated_configuration_puts_every_override_before_the_users_own() {
    let dir = tempfile::tempdir().unwrap();
    let masters = Masters::under(dir.path());
    masters.prepare().unwrap();

    let path = masters.config_for(&words(&DECLARED)).unwrap();

    let written = fs::read_to_string(&path).unwrap();
    assert_eq!(
        written.lines().collect::<Vec<&str>>(),
        [
            "# generated; overrides come first because ssh takes the first match",
            "ServerAliveInterval 15",
            "ServerAliveCountMax 3",
            "BatchMode yes",
            "ControlMaster auto",
            &format!("ControlPath {}/cm-%C", dir.path().display()),
            "ControlPersist 60",
            // Last, because ssh takes the first value it is given and this is
            // the one the user wrote.
            "Include ~/.ssh/config",
        ]
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn two_declarations_get_two_configurations_and_the_same_one_twice_gets_one() {
    let dir = tempfile::tempdir().unwrap();
    let masters = Masters::under(dir.path());
    masters.prepare().unwrap();

    let one = masters.config_for(&words(&DECLARED)).unwrap();
    let again = masters.config_for(&words(&DECLARED)).unwrap();
    let other = masters.config_for(&words(&["fileserver"])).unwrap();

    assert_eq!(one, again);
    assert_ne!(one, other);
}

#[test]
fn a_directory_with_no_room_for_a_socket_is_left_for_one_that_has() {
    let deep = PathBuf::from(format!("/run/user/1000/{}", "d".repeat(64)));

    let roomy = Masters::beside(Path::new("/run/user/1000/agentbus"));
    let cramped = Masters::beside(&deep);

    assert_eq!(roomy.dir(), Path::new("/run/user/1000/agentbus/ssh"));
    assert_eq!(cramped.dir(), crate::paths::per_user_dir().join("ssh"));
    // The point of the budget: whatever ssh expands the template into, the path
    // it builds still fits in the field the kernel keeps it in.
    for masters in [roomy, cramped] {
        assert!(
            masters.socket().len() - "%C".len() + 64 < 108,
            "{}",
            masters.socket()
        );
    }
}

#[test]
fn what_ssh_complained_says_whether_another_attempt_could_go_differently() {
    let expected = [
        ("Host key verification failed.", Trouble::HostKey, false),
        (
            "@@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@@",
            Trouble::HostKey,
            false,
        ),
        (
            "bob@fs.example.net: Permission denied (publickey).",
            Trouble::Credentials,
            false,
        ),
        (
            "Enter passphrase for key '/home/bob/.ssh/id_ed25519': ",
            Trouble::Asking,
            false,
        ),
        (
            "ssh: connect to host fs.example.net port 2222: Connection refused",
            Trouble::Unreachable,
            true,
        ),
        (
            "ssh: Could not resolve hostname fs.example.net: Name or service not known",
            Trouble::Unreachable,
            true,
        ),
        (
            "ssh: connect to host fs.example.net port 22: Connection timed out",
            Trouble::Unreachable,
            true,
        ),
        (
            "ssh: connect to host fs.example.net port 22: No route to host",
            Trouble::Unreachable,
            true,
        ),
        (
            "mux_client_request_session: session request failed",
            Trouble::Unrecognized,
            true,
        ),
    ];

    for (said, trouble, retries) in expected {
        assert_eq!(Trouble::of(said), trouble, "{said}");
        assert_eq!(Trouble::of(said).retries(), retries, "{said}");
    }
}

#[test]
fn a_refusal_reaches_the_transport_through_whatever_wrapped_it() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new();
    let host = host(dir.path(), &ssh);

    // Whatever kind of failure carried the words, it is the words that decide.
    let wrapped = io::Error::other(
        "the bootstrap failed at bob@fs: exit status: 255: bob@fs.example.net: Permission denied (publickey).",
    );
    let down =
        io::Error::other("the bootstrap failed at bob@fs: exit status: 255: Connection refused");

    assert!(!host.recoverable(&wrapped));
    assert!(host.recoverable(&down));
}

#[test]
fn a_connection_that_has_died_is_cleared_before_anything_tries_to_use_it() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cm-deadbeef");
    let ssh = Fake::new()
        .answering(
            "check",
            Answer::refused("Control socket connect: No such file"),
        )
        .answering(
            "-G",
            Answer::printing(&format!("controlpath {}\n", socket.display())),
        );
    let host = host(dir.path(), &ssh);
    fs::write(&socket, "").unwrap();

    let mut running = host.run("uname", &["-s", "-m"], None).expect("nothing ran");
    running.wait().unwrap();

    assert!(!socket.exists(), "the dead socket was left behind");
    assert_eq!(ssh.asked_about("check").len(), 1);
    // And the command went ahead regardless, which is the whole point of
    // clearing it.
    assert_eq!(ssh.asked_about("uname").len(), 1);

    // Asked once for the life of the transport, not once per command.
    let mut again = host.run("uname", &["-s", "-m"], None).expect("nothing ran");
    again.wait().unwrap();
    assert_eq!(ssh.asked_about("check").len(), 1);
}

#[test]
fn a_connection_that_answers_is_left_exactly_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cm-deadbeef");
    let ssh = Fake::new().answering(
        "-G",
        Answer::printing(&format!("controlpath {}\n", socket.display())),
    );
    let host = host(dir.path(), &ssh);
    fs::write(&socket, "").unwrap();

    let mut running = host.run("uname", &["-s", "-m"], None).expect("nothing ran");
    running.wait().unwrap();

    assert!(socket.exists());
    // Nothing asked ssh where the socket was, because nothing had to.
    assert!(ssh.asked_about("-G").is_empty());
}

#[test]
fn a_path_still_carrying_its_template_names_no_file_and_nothing_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new()
        .answering(
            "check",
            Answer::refused("Control socket connect: No such file"),
        )
        .answering(
            "-G",
            Answer::printing(&format!("controlpath {}/cm-%C\n", dir.path().display())),
        );
    let host = host(dir.path(), &ssh);

    let mut running = host.run("uname", &["-s", "-m"], None).expect("nothing ran");
    running.wait().unwrap();

    assert_eq!(ssh.asked_about("uname").len(), 1);
}

#[test]
fn letting_go_of_a_host_closes_the_connection_once() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new();

    {
        let host = host(dir.path(), &ssh);
        assert!(ssh.asked_about("exit").is_empty());
        drop(host);
    }

    let closed = ssh.asked_about("exit");
    assert_eq!(closed.len(), 1);
    assert_eq!(
        closed[0][closed[0].len() - 5..],
        words(&["-O", "exit", "-p", "2222", "bob@fs.example.net"])[..]
    );
}

#[test]
fn a_copy_is_poured_down_the_connection_and_moved_into_place_over_there() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new();
    let host = host(dir.path(), &ssh);
    let local = dir.path().join("agentbus");
    fs::write(&local, "a whole binary").unwrap();

    host.copy_in(&local, "/tmp/agentbus-1.2.3")
        .expect("the copy failed");

    let sent = ssh.asked_about("mkdir");
    assert_eq!(sent.len(), 1);
    let script = sent[0].last().expect("no command");
    let partial = partial_name_in(script);
    assert!(partial.starts_with("/tmp/agentbus-1.2.3.tmp."), "{script}");
    assert_eq!(
        *script,
        format!(
            "mkdir -p '/tmp' && cat > '{partial}' && chmod +x '{partial}' \
             && mv -f '{partial}' '/tmp/agentbus-1.2.3'"
        )
    );
    assert_eq!(
        *ssh.poured.lock().unwrap_or_else(PoisonError::into_inner),
        b"a whole binary".to_vec()
    );
    // And the far end is asked whether what arrived is runnable, rather than it
    // being concluded from the commands having succeeded.
    let checked = ssh.asked_about("test -x");
    assert_eq!(checked.len(), 1);
    assert_eq!(
        checked[0].last().expect("no command"),
        "test -x '/tmp/agentbus-1.2.3'"
    );
}

#[test]
fn two_copies_to_one_host_at_once_write_under_different_partial_names() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new();
    let host = host(dir.path(), &ssh);
    let local = dir.path().join("agentbus");
    fs::write(&local, "a whole binary").unwrap();

    // Not actually concurrent — the fake records synchronously — but what
    // matters is that nothing about a second attempt reuses the first one's
    // name, which holds however the two are interleaved.
    host.copy_in(&local, "/tmp/agentbus-1.2.3").unwrap();
    host.copy_in(&local, "/tmp/agentbus-1.2.3").unwrap();

    let sent = ssh.asked_about("mkdir");
    assert_eq!(sent.len(), 2);
    let first = partial_name_in(sent[0].last().unwrap());
    let second = partial_name_in(sent[1].last().unwrap());
    assert_ne!(
        first, second,
        "two attempts wrote under the same partial name"
    );
}

#[test]
fn a_copy_that_did_not_arrive_runnable_is_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new().answering("test -x", Answer::refused(""));
    let host = host(dir.path(), &ssh);
    let local = dir.path().join("agentbus");
    fs::write(&local, "a whole binary").unwrap();

    assert!(host.copy_in(&local, "/tmp/agentbus-1.2.3").is_err());
}

#[test]
fn a_host_is_what_it_was_declared_as_and_where_ssh_says_that_is() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new();

    let host = host(dir.path(), &ssh);

    assert_eq!(host.label(), "-p 2222 bob@fs.example.net");
    assert_eq!(host.kind(), "ssh");
    assert_eq!(host.way_in().as_deref(), Some("bob@fs.example.net:2222"));
    // And it does not claim to know what is there. Where ssh would go is not
    // what is at the other end of it, and the daemon over there is the party
    // that settles that.
    assert_eq!(host.identity(), None);
    assert_eq!(host.install_path("1.2.3"), "/tmp/agentbus-1.2.3");
    // Its own schedule, and a slower one than a container on this machine gets.
    assert_eq!(host.backoff().initial, std::time::Duration::from_secs(5));
    assert_eq!(host.backoff().max, std::time::Duration::from_secs(60));
}

#[test]
fn letting_go_of_a_host_closes_the_connection_it_was_holding_open() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new();

    drop(host(dir.path(), &ssh));

    let closed = ssh.asked_about("exit");
    assert_eq!(closed.len(), 1, "{:?}", ssh.asked());
    // Asked of the master for exactly the words that made it, and of no other.
    assert_eq!(
        closed[0][closed[0].len() - 5..],
        words(&["-O", "exit", "-p", "2222", "bob@fs.example.net"])[..]
    );
}

#[test]
fn a_host_that_is_told_something_else_is_using_its_connection_leaves_it_open() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new();
    let host = host(dir.path(), &ssh);

    host.keep_open();
    drop(host);

    // Nothing was asked to close: the connection belongs to the declaration
    // that is still being read through it as much as to this one.
    assert!(ssh.asked_about("exit").is_empty(), "{:?}", ssh.asked());
}

#[test]
fn a_declaration_ssh_will_not_have_is_refused_before_anything_is_reached() {
    let dir = tempfile::tempdir().unwrap();
    let masters = Masters::under(dir.path());
    let refusing = Resolver::new().watching([]).running(|_argv: &[String]| {
        Ok(Output {
            status: ExitStatus::from_raw(255 << 8),
            stdout: Vec::new(),
            stderr: b"ssh: Could not resolve hostname nowhere\n".to_vec(),
        })
    });
    let ssh = Fake::new();

    let refused = Host::built(
        &words(&["nowhere"]),
        &refusing,
        &masters,
        Arc::new(Arc::clone(&ssh)),
    )
    .expect_err("that should not have been made");

    assert!(refused.contains("Could not resolve"), "{refused}");
    // And nothing was run: what refused it was the resolution, not a connection.
    assert!(ssh.asked().is_empty());
}

#[test]
fn a_declaration_with_no_words_in_it_reaches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new();

    assert!(
        Host::built(
            &[],
            &resolver(),
            &Masters::under(dir.path()),
            Arc::new(Arc::clone(&ssh))
        )
        .is_err()
    );
}

#[test]
fn this_build_reaches_a_host_by_the_words_that_would_reach_it_with_ssh() {
    let registry = Registry::standard();

    assert!(registry.names().any(|name| name == "ssh"));
    assert!(registry.make("ssh", &[]).expect("unknown").is_err());
}

#[test]
fn a_far_end_that_complains_at_length_is_quoted_at_the_end_and_not_at_all_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let flood = format!("{}\nPermission denied (publickey).\n", "x".repeat(20_000));
    let ssh = Fake::new().answering("mkdir", Answer::refused(&flood));
    let host = host(dir.path(), &ssh);
    let local = dir.path().join("agentbus");
    fs::write(&local, "a whole binary").unwrap();

    let refused = host
        .copy_in(&local, "/tmp/agentbus-1.2.3")
        .expect_err("that should have failed");

    let said = refused.to_string();
    assert!(said.len() < 6000, "{} bytes were kept", said.len());
    // The end is the part that says what went wrong, so it is the part kept.
    assert!(said.contains("Permission denied"), "{said}");
}

#[test]
fn being_established_sweeps_every_superseded_copy_and_keeps_the_current_one() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new();
    let host = host(dir.path(), &ssh);

    host.established("1.2.3");

    let sweeps = ssh.asked_about("rm -f");
    assert_eq!(sweeps.len(), 1);
    let script = sweeps[0].last().expect("no command");
    assert!(
        script.starts_with("for f in /tmp/agentbus-*; do case \"$f\" in"),
        "{script}"
    );
    // The version this host was just told is running is kept, not removed...
    assert!(
        script.contains("'/tmp/agentbus-1.2.3') continue"),
        "{script}"
    );
    // ...and so is anything still being written by another attempt, which a
    // sweep run mid-write must not mistake for something superseded.
    assert!(script.contains("*.tmp.*) continue"), "{script}");
}

#[test]
fn being_established_a_second_time_sweeps_nothing_more() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new();
    let host = host(dir.path(), &ssh);

    host.established("1.2.3");
    host.established("1.2.3");

    assert_eq!(
        ssh.asked_about("rm -f").len(),
        1,
        "a host that had already been swept was swept again"
    );
}

#[test]
fn trouble_sweeping_is_not_trouble_establishing() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = Fake::new().answering("rm -f", Answer::refused("no such directory"));
    let host = host(dir.path(), &ssh);

    // Nothing here is a `Result`: a caller that has just confirmed the far
    // end is running the right version has nothing useful to do with a
    // housekeeping failure except be told about it, which is what the log
    // line inside `established` is for.
    host.established("1.2.3");
}
