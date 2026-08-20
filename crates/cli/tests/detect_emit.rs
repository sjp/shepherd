//! What `agentbus detect --emit` puts on the bus, and what it deliberately
//! does not.
//!
//! Every test here drives the whole path a program watching terminals would:
//! the shipped binary, a screen on its stdin, a real daemon on the far end of
//! the emit socket, and a subscriber reading what came out. That is the join
//! these are about — a screen someone captured becoming a claim anybody
//! watching the bus can act on — so nothing below stands in for any part of it.
//!
//! The manifests are the ones inside the binary. Each command is given a home
//! directory of its own with nothing in it, so a copy on the machine running
//! the tests cannot decide what a screen means.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use agentbus_protocol::{AssertedState, SessionStatus};
use common::Bus;
use tempfile::TempDir;

/// The slot every claim here is about. Opaque to everything it passes through.
const SLOT: &str = "w9:p3";

/// The session id the bus files a claim about that slot under, when no agent is
/// speaking for it.
const OBSERVED: &str = "observed:w9:p3";

/// The agent the bundled corpus describes rules for.
const AGENT: &str = "claude";

/// A screen the bundled manifest for that agent reads as blocked: a permission
/// prompt for a shell command, with the choices under it.
const BLOCKED: &str =
    "Bash command\n  rm -rf build\n\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n";

/// The rule in that manifest which says so.
const BLOCKED_RULE: &str = "bash_permission_prompt";

/// A screen showing what the agent did earlier rather than what it is doing:
/// the manifest recognizes the transcript viewer and declines to draw any
/// conclusion about now from it.
const TRANSCRIPT: &str = "Showing detailed transcript\nctrl+o to toggle\n";

/// How long a test whose subject is silence waits to be sure of it.
///
/// A claim that was going to be sent has been sent long before this: the
/// command writes its line and exits, and the daemon publishes under the lock
/// it received it under.
const SILENCE: Duration = Duration::from_millis(500);

/// The variables that would otherwise let the machine running these tests
/// decide which manifests answer.
const INHERITED: [&str; 2] = ["XDG_CONFIG_HOME", "XDG_STATE_HOME"];

/// Runs `agentbus detect` against `bus`, with `screen` on its stdin and no
/// manifests but the ones inside the binary.
fn detect(bus: &Bus, args: &[&str], screen: &str) -> Output {
    let home = TempDir::new().expect("a temporary directory");
    run(bus.command(&["detect"]), home.path(), args, screen)
}

/// The same, against no bus at all: a directory where a daemon would put its
/// sockets if one were running, and none is.
fn detect_without_a_bus(args: &[&str], screen: &str) -> Output {
    let dir = TempDir::new().expect("a temporary directory");
    let home = TempDir::new().expect("a temporary directory");
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
    command.arg("detect").arg("--dir").arg(dir.path());
    run(command, home.path(), args, screen)
}

/// Finishes building `command` and runs it to completion.
fn run(mut command: Command, home: &Path, args: &[&str], screen: &str) -> Output {
    command
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for variable in INHERITED {
        command.env_remove(variable);
    }

    let mut child = command.spawn().expect("cannot run agentbus");
    let mut stdin = child.stdin.take().expect("no stdin");
    stdin
        .write_all(screen.as_bytes())
        .expect("cannot write the screen");
    drop(stdin);
    child.wait_with_output().expect("cannot wait for agentbus")
}

/// What a command printed on stdout.
fn out(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout was not text")
}

/// What it printed on stderr.
fn err(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr was not text")
}

/// Everything it said, for a failure message.
fn said(output: &Output) -> String {
    format!(
        "exited with {}\nstdout: {}\nstderr: {}",
        output.status,
        out(output),
        err(output)
    )
}

#[test]
fn a_blocked_screen_becomes_a_claim_anybody_watching_the_bus_can_see() {
    let bus = Bus::start();
    let mut subscriber = bus.attach();
    subscriber.snapshot();

    let output = detect(
        &bus,
        &["--agent", AGENT, "--emit", "--correlation", SLOT],
        BLOCKED,
    );

    // A command that sends says nothing: whoever is running it in a loop is
    // reading the bus, not its scrollback.
    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    assert_eq!(out(&output), "");
    assert_eq!(err(&output), "");

    let claim = subscriber.assertion("the claim");
    assert_eq!(claim.assert, AssertedState::Blocked);
    assert_eq!(claim.correlation, SLOT);
    assert_eq!(claim.agent.as_str(), AGENT);
    // The prompt is on the screen as the claim is made, which is what gives it
    // the standing to be shown over an agent's own quieter account of itself.
    assert!(claim.visible, "{claim:?}");
    assert_eq!(
        claim
            .detail
            .as_ref()
            .and_then(|detail| detail.get("rule"))
            .and_then(|rule| rule.as_str()),
        Some(BLOCKED_RULE),
        "{claim:?}"
    );

    // And the bus's own picture of that slot has moved, which is the whole
    // point of having sent it.
    let snapshot = bus.wait_for(OBSERVED, SessionStatus::Blocked);
    let session = common::session_of(&snapshot, OBSERVED);
    assert_eq!(session.correlation.as_deref(), Some(SLOT));
    assert_eq!(session.agent.as_str(), AGENT);
}

#[test]
fn the_environment_names_the_slot_when_the_command_line_does_not() {
    let bus = Bus::start();
    let mut subscriber = bus.attach();
    subscriber.snapshot();

    for variable in ["AGENTBUS_PANE", "LC_AGENTBUS_PANE"] {
        let slot = format!("{SLOT}:{variable}");
        let home = TempDir::new().expect("a temporary directory");
        let mut command = bus.command(&["detect"]);
        command.env(variable, &slot);
        let output = run(command, home.path(), &["--agent", AGENT, "--emit"], BLOCKED);

        assert_eq!(output.status.code(), Some(0), "{}", said(&output));
        let claim = subscriber.assertion("the claim");
        assert_eq!(claim.correlation, slot);
    }
}

#[test]
fn a_claim_nothing_can_be_attributed_to_is_not_sent_at_all() {
    let bus = Bus::start();
    let mut subscriber = bus.attach();
    subscriber.snapshot();

    let output = detect(&bus, &["--agent", AGENT, "--emit"], BLOCKED);

    // The same status as a screen nobody could be named for: this ran, and
    // there is no claim to be made from it.
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    assert_eq!(out(&output), "");
    assert!(err(&output).contains("--correlation"), "{}", said(&output));
    assert_eq!(subscriber.quiet_for(SILENCE), None);
}

#[test]
fn a_screen_showing_the_past_says_nothing_about_now() {
    let bus = Bus::start();
    let mut subscriber = bus.attach();
    subscriber.snapshot();

    // The screen is read and understood — the manifest has a rule for exactly
    // this — and what it is understood to say is nothing about the present.
    let word = detect(&bus, &["--agent", AGENT], TRANSCRIPT);
    assert_eq!(out(&word), "unknown\n", "{}", said(&word));

    let output = detect(
        &bus,
        &["--agent", AGENT, "--emit", "--correlation", SLOT],
        TRANSCRIPT,
    );

    // A successful non-answer: nothing was sent, and nothing went wrong.
    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    assert_eq!(err(&output), "");
    assert_eq!(subscriber.quiet_for(SILENCE), None);
}

#[test]
fn a_screen_nobody_can_be_named_for_is_not_claimed_about_either() {
    let bus = Bus::start();
    let mut subscriber = bus.attach();
    subscriber.snapshot();

    let output = detect(
        &bus,
        &[
            "--agent",
            "an-agent-nobody-has-heard-of",
            "--emit",
            "--correlation",
            SLOT,
        ],
        BLOCKED,
    );

    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    assert_eq!(out(&output), "");
    assert_eq!(subscriber.quiet_for(SILENCE), None);
}

#[test]
fn a_bus_that_is_not_running_costs_an_observer_nothing() {
    let output = detect_without_a_bus(
        &["--agent", AGENT, "--emit", "--correlation", SLOT],
        BLOCKED,
    );

    // A loop left running across a restart of the daemon must not turn into a
    // loop reporting a failure every second or two.
    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    assert_eq!(out(&output), "");
    assert_eq!(err(&output), "");
}

#[test]
fn the_claim_carries_the_reasoning_and_none_of_the_screen() {
    let bus = Bus::start();
    let mut subscriber = bus.attach();
    subscriber.snapshot();
    let secret = "sk-do-not-put-this-on-a-bus";

    let output = detect(
        &bus,
        &["--agent", AGENT, "--emit", "--correlation", SLOT],
        &format!("{BLOCKED}{secret}\n"),
    );
    assert_eq!(output.status.code(), Some(0), "{}", said(&output));

    // The evidence never leaves the process that read the screen: what the
    // observer sends is dropped on the way through the daemon, and what a
    // subscriber is handed is the conclusion and which rule reached it.
    let claim = subscriber.assertion("the claim");
    let published = serde_json::to_string(&claim).expect("a published claim is serializable");
    assert!(!published.contains(secret), "{published}");
    assert_eq!(claim.assert, AssertedState::Blocked);
}

/// Nothing about a session an agent is speaking for, because none of these
/// send one: what is checked is that the two claims about one slot are one
/// session on the bus rather than two.
#[test]
fn repeating_a_claim_says_one_thing_rather_than_several() {
    let bus = Bus::start();

    for _ in 0..3 {
        let output = detect(
            &bus,
            &["--agent", AGENT, "--emit", "--correlation", SLOT],
            BLOCKED,
        );
        assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    }

    let snapshot = bus.wait_for(OBSERVED, SessionStatus::Blocked);
    let claimed: Vec<&_> = snapshot
        .sessions
        .iter()
        .filter(|entry| entry.correlation.as_deref() == Some(SLOT))
        .collect();
    assert_eq!(claimed.len(), 1, "{:?}", snapshot.sessions);
}
