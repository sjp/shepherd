//! What a running bus says about the processes in front of correlated shells,
//! and what `agentbus foreground` makes of it.
//!
//! The process table these drive is a directory of files the test wrote, for the
//! reason the reader that walks one was built to take a root: every state worth
//! asserting — a command replacing another, a process ending, a shell with
//! nothing in front of it — lasts a few milliseconds on a real machine and
//! cannot be asked for on purpose. Against files they are all just writes, and
//! the daemon under test is the shipped binary reading what it is pointed at,
//! which is exactly what it does with `/proc`.
//!
//! The correlation values here are strings a person could have typed into
//! `AGENTBUS_PANE` at a shell, because that is all the bus knows them to be.

mod common;

use std::time::Duration;

use agentbus_protocol::{ForegroundEntry, ForegroundState, SessionStatus, StreamLine};
use common::tree::{Tree, connected_shell, running, shell};
use common::{Bus, foreground_of};

/// How long a test waits to be sure nothing more is coming.
///
/// Several times the daemon's own polling interval, so that a bus which had
/// something to say would have said it, and short enough to be a test.
const SILENCE: Duration = Duration::from_secs(3);

/// A table with one shell carrying `w9:p3` and `claude` running in front of it.
fn one_shell_running_claude() -> Tree {
    let tree = Tree::new();
    tree.write(&shell(100, "w9:p3", 200));
    tree.write(&running(200, "claude", vec!["claude", "--resume"]));
    tree
}

/// The rows `agentbus foreground` printed, without the headings.
fn rows(text: &str) -> Vec<&str> {
    text.lines().skip(1).collect()
}

#[test]
fn what_is_running_in_front_of_a_correlated_shell_is_in_the_snapshot() {
    let tree = one_shell_running_claude();
    let bus = Bus::watching(&tree.root());

    let snapshot = bus.wait_for_foreground("w9:p3");

    let entry = foreground_of(&snapshot, "w9:p3");
    assert_eq!(entry.pid, 200);
    assert_eq!(entry.process, "claude");
    assert_eq!(entry.cmdline, "claude --resume");
    assert_eq!(entry.state, Some(ForegroundState::Foreground));
    // Nothing crossed a boundary to get here, and nothing has matched this
    // observation against one made anywhere else.
    assert_eq!(entry.origin, Vec::new());
    assert_eq!(entry.ssh_client_port, None);
    assert!(!entry.since.as_str().is_empty());
}

#[test]
fn a_command_replacing_another_is_one_line_on_the_stream() {
    let tree = one_shell_running_claude();
    let bus = Bus::watching(&tree.root());
    bus.wait_for_foreground("w9:p3");
    let mut subscriber = bus.subscribe();
    subscriber.snapshot();

    tree.write(&running(300, "vim", vec!["vim", "notes.md"]));
    tree.write(&shell(100, "w9:p3", 300));

    let change = subscriber.foreground_change("the new foreground process");
    let entry = change
        .foreground
        .expect("a change with no observation in it");
    assert_eq!(entry.correlation.as_deref(), Some("w9:p3"));
    assert_eq!(entry.pid, 300);
    assert_eq!(entry.process, "vim");
    assert_eq!(entry.cmdline, "vim notes.md");
    assert_eq!(
        subscriber.quiet_for(SILENCE),
        None,
        "the change was repeated"
    );
}

#[test]
fn a_foreground_that_ends_is_withdrawn_rather_than_left_standing() {
    let tree = one_shell_running_claude();
    let bus = Bus::watching(&tree.root());
    bus.wait_for_foreground("w9:p3");
    let mut subscriber = bus.subscribe();
    subscriber.snapshot();

    tree.remove(200);

    let change = subscriber.foreground_change("the withdrawal");
    assert_eq!(change.foreground, None);
    // There is no entry to read the correlation off, so the line carries it.
    assert_eq!(change.correlation.as_deref(), Some("w9:p3"));
    assert_eq!(
        bus.snapshot().foreground,
        Some(Vec::new()),
        "the observation outlived the process"
    );
}

#[test]
fn a_foreground_that_does_not_change_is_never_mentioned_again() {
    let tree = one_shell_running_claude();
    let bus = Bus::watching(&tree.root());
    bus.wait_for_foreground("w9:p3");

    let mut subscriber = bus.subscribe();
    subscriber.snapshot();

    assert_eq!(
        subscriber.quiet_for(SILENCE),
        None,
        "a quiet foreground was reported anyway"
    );
}

#[test]
fn the_command_prints_a_row_for_what_it_can_see() {
    let tree = one_shell_running_claude();
    let bus = Bus::watching(&tree.root());
    bus.wait_for_foreground("w9:p3");

    let printed = bus.run(&["foreground"]);

    let text = String::from_utf8(printed.stdout).expect("not text");
    let headings: Vec<&str> = text
        .lines()
        .next()
        .expect("no headings")
        .split_whitespace()
        .collect();
    assert_eq!(
        headings,
        [
            "CORRELATION",
            "CONNECTION",
            "PID",
            "STATE",
            "PROCESS",
            "CMDLINE",
            "ORIGIN",
            "SINCE"
        ]
    );
    let rows = rows(&text);
    assert_eq!(rows.len(), 1, "{text}");
    for value in ["w9:p3", "200", "foreground", "claude", "claude --resume"] {
        assert!(rows[0].contains(value), "{value} is missing from {text}");
    }
}

#[test]
fn a_correlation_is_filtered_by_exactly_the_string_it_was_given() {
    let tree = one_shell_running_claude();
    tree.write(&shell(101, "w9:p4", 201));
    tree.write(&running(201, "vim", vec!["vim"]));
    let bus = Bus::watching(&tree.root());
    bus.wait_for_foreground("w9:p3");
    bus.wait_for_foreground("w9:p4");

    let printed = bus.run(&["foreground", "--correlation", "w9:p3"]);

    let text = String::from_utf8(printed.stdout).expect("not text");
    let rows = rows(&text);
    assert_eq!(rows.len(), 1, "{text}");
    assert!(rows[0].contains("w9:p3"), "{text}");
    assert!(!text.contains("w9:p4"), "{text}");
}

#[test]
fn a_correlation_nothing_is_running_in_is_reported_by_the_exit_code_alone() {
    let tree = one_shell_running_claude();
    let bus = Bus::watching(&tree.root());
    bus.wait_for_foreground("w9:p3");

    let printed = bus
        .command(&["foreground", "--correlation", "w9:p3 "])
        .output()
        .expect("cannot run agentbus");

    assert_eq!(printed.status.code(), Some(1));
    assert!(printed.stdout.is_empty(), "{printed:?}");
}

#[test]
fn the_json_is_the_entries_one_to_a_line() {
    let tree = one_shell_running_claude();
    let bus = Bus::watching(&tree.root());
    bus.wait_for_foreground("w9:p3");

    let printed = bus.run(&["foreground", "--json"]);

    let text = String::from_utf8(printed.stdout).expect("not text");
    let entries: Vec<ForegroundEntry> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error} in {line}")))
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].correlation.as_deref(), Some("w9:p3"));
    assert_eq!(entries[0].pid, 200);
}

#[test]
fn a_daemon_that_cannot_see_a_process_table_says_so_and_serves_everything_else() {
    let bus = Bus::start();
    bus.emit(
        "claude",
        Some("w9:p3"),
        &common::payload("claude", "PreToolUse"),
    );
    let session = "9f2c1b7a-4d5e-4a91-8c33-1e6b0d7f2a48";
    bus.wait_for(session, SessionStatus::Working);

    let printed = bus
        .command(&["foreground"])
        .output()
        .expect("cannot run agentbus");

    assert_eq!(printed.status.code(), Some(2));
    assert!(printed.stdout.is_empty(), "{printed:?}");
    let said = String::from_utf8_lossy(&printed.stderr);
    assert!(
        said.contains("foreground monitoring unavailable on this daemon"),
        "{said}"
    );
    // The key is absent rather than empty: this daemon does not know what is
    // running anywhere, which is not the same as knowing nothing is. What it
    // does know, it still reports.
    let snapshot = bus.snapshot();
    assert_eq!(snapshot.foreground, None);
    assert_eq!(
        common::session_of(&snapshot, session).status,
        SessionStatus::Working
    );
}

#[test]
fn a_daemon_that_cannot_see_a_process_table_never_mentions_the_foreground() {
    let bus = Bus::start();
    let mut subscriber = bus.subscribe();
    assert_eq!(subscriber.snapshot().foreground, None);

    assert!(
        !matches!(
            subscriber.quiet_for(SILENCE),
            Some(StreamLine::ForegroundChange(_))
        ),
        "a daemon with nothing to observe reported an observation"
    );
}

#[test]
fn there_is_nothing_to_report_from_where_there_is_no_daemon() {
    let dir = tempfile::tempdir().expect("cannot make a temporary directory");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_agentbus"));

    let printed = command
        .args(["foreground", "--dir"])
        .arg(dir.path())
        .output()
        .expect("cannot run agentbus");

    assert_eq!(printed.status.code(), Some(2));
    assert!(printed.stdout.is_empty(), "{printed:?}");
    let said = String::from_utf8_lossy(&printed.stderr);
    assert!(said.contains("no daemon running"), "{said}");
}

/// A connection value in the four fields `sshd` documents.
const CONNECTION: &str = "10.0.0.5 51234 10.0.0.9 22";

#[test]
fn a_shell_that_arrived_over_a_connection_is_observed_without_a_correlation() {
    let tree = Tree::new();
    tree.write(&connected_shell(100, CONNECTION, 200));
    tree.write(&running(200, "claude", vec!["claude", "--resume"]));
    let bus = Bus::watching(&tree.root());

    let snapshot = bus.wait_for_observation(CONNECTION, |entry| {
        entry.ssh_connection.as_deref() == Some(CONNECTION)
    });

    let observed = snapshot.foreground.expect("this daemon is not watching");
    assert_eq!(observed.len(), 1, "{observed:?}");
    let entry = &observed[0];
    assert_eq!(
        entry.correlation, None,
        "nothing labelled that shell, so nothing here may claim it was"
    );
    assert_eq!(entry.pid, 200);
    assert_eq!(entry.process, "claude");

    // And it is a row somebody can read, with the connection where the label
    // would have been if there had been one.
    let printed = bus.run(&["foreground"]);
    let text = String::from_utf8(printed.stdout).expect("not text");
    let rows = rows(&text);
    assert_eq!(rows.len(), 1, "{text}");
    assert!(rows[0].contains(CONNECTION), "{text}");
    assert!(rows[0].starts_with('-'), "{text}");
}

#[test]
fn a_shell_that_arrived_labelled_is_observed_under_that_label_and_reports_both() {
    let tree = Tree::new();
    let mut arrived = connected_shell(100, CONNECTION, 200);
    // What a server that let the second correlation name through leaves behind.
    arrived.environ.push(("LC_AGENTBUS_PANE", "w1".to_owned()));
    tree.write(&arrived);
    tree.write(&running(200, "claude", vec!["claude", "--resume"]));
    let bus = Bus::watching(&tree.root());

    let snapshot = bus.wait_for_foreground("w1");

    let entry = foreground_of(&snapshot, "w1");
    assert_eq!(entry.pid, 200);
    assert_eq!(
        entry.ssh_connection.as_deref(),
        Some(CONNECTION),
        "a shell that was labelled and arrived over a connection carries both"
    );
}
