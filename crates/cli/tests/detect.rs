//! What `agentbus detect` makes of a screen somebody piped into it.
//!
//! Every one of these runs the shipped binary with a screen on its stdin,
//! because that is the whole interface: a caller that has terminal text and no
//! way to link a Rust library. The screens are written here rather than
//! captured, so that what each test asserts is a rule in the manifest that
//! ships with the binary and not an accident of somebody's terminal on the day
//! it was recorded.
//!
//! The manifests these read are the bundled ones. Each command is given a home
//! directory of its own with nothing in it, so a copy on the machine running
//! the tests — an override somebody wrote, something fetched — cannot change
//! the answer.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

use tempfile::TempDir;

/// A screen the bundled claude manifest reads as blocked: a permission prompt
/// for a shell command, with the choices under it.
const BLOCKED: &str =
    "Bash command\n  rm -rf build\n\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n";

/// The rule in that manifest which says so.
const BLOCKED_RULE: &str = "bash_permission_prompt";

/// The agent the bundled corpus describes those rules for.
const AGENT: &str = "claude";

/// The words a screen is described in here. None of them may appear in what the
/// command says about itself: this reads text somebody captured, and it has no
/// business knowing what they captured it from.
const NOT_ITS_BUSINESS: [&str; 4] = ["pane", "tab", "workspace", "window"];

/// The variables that would otherwise let the machine running these tests
/// decide which manifests answer.
const INHERITED: [&str; 2] = ["XDG_CONFIG_HOME", "XDG_STATE_HOME"];

/// Runs `agentbus detect` with `screen` on its stdin, over the bundled
/// manifests and nothing else.
fn detect(args: &[&str], screen: &str) -> Output {
    let home = TempDir::new().expect("a temporary directory");
    detect_at(home.path(), args, screen)
}

/// The same, over whatever manifests `home` holds as well.
fn detect_at(home: &Path, args: &[&str], screen: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
    command
        .arg("detect")
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for variable in INHERITED {
        command.env_remove(variable);
    }

    let mut child = command.spawn().expect("failed to run agentbus");
    let mut input = child.stdin.take().expect("no stdin");
    match input.write_all(screen.as_bytes()) {
        Ok(()) => {}
        // A screen over the command's cap is answered on and the rest of the
        // pipe is dropped, so a test feeding it more than that has the far end
        // close on it. That is the behaviour under test, not a failure of it.
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
        Err(error) => panic!("failed to write the screen: {error}"),
    }
    drop(input);
    child.wait_with_output().expect("failed to wait")
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
fn a_named_agents_blocked_screen_is_one_word_on_stdout() {
    let output = detect(&["--agent", AGENT], BLOCKED);

    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    assert_eq!(out(&output), "blocked\n");
    assert_eq!(err(&output), "");
}

#[test]
fn the_process_alone_names_the_agent() {
    let named = detect(&["--process", AGENT], BLOCKED);
    let from_cmdline = detect(&["--cmdline", "claude --resume"], BLOCKED);

    for output in [&named, &from_cmdline] {
        assert_eq!(output.status.code(), Some(0), "{}", said(output));
        assert_eq!(out(output), "blocked\n");
    }
}

#[test]
fn a_screen_with_nobody_to_read_it_is_not_an_answer() {
    let output = detect(&[], BLOCKED);

    // Two exits, and the difference between them is the point: this is not a
    // verdict of `unknown` on a screen that was read.
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    assert_eq!(out(&output), "");
    assert!(
        err(&output).contains("no agent identified"),
        "{}",
        said(&output)
    );
}

#[test]
fn an_agent_no_manifest_describes_is_no_answer_either() {
    let output = detect(&["--agent", "an-agent-nobody-has-heard-of"], BLOCKED);

    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    assert_eq!(out(&output), "");
}

#[test]
fn the_json_verdict_is_the_word_with_its_working_out() {
    let word = detect(&["--agent", AGENT], BLOCKED);
    let output = detect(&["--agent", AGENT, "--json"], BLOCKED);

    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    let verdict: Value = serde_json::from_str(&out(&output)).expect("that was not JSON");
    assert_eq!(verdict["state"].as_str(), Some(out(&word).trim()));
    assert_eq!(verdict["matched_rule"].as_str(), Some(BLOCKED_RULE));
    assert_eq!(verdict["visible"].as_bool(), Some(true));
    assert_eq!(verdict["skip"].as_bool(), Some(false));
    assert_eq!(verdict["fallback"], Value::Null);
}

#[test]
fn explaining_names_the_rule_that_won_and_the_copy_that_answered() {
    let output = detect(&["--agent", AGENT, "--explain"], BLOCKED);

    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    let explanation: Value = serde_json::from_str(&out(&output)).expect("that was not JSON");
    assert_eq!(explanation["agent"].as_str(), Some(AGENT));
    assert_eq!(explanation["state"].as_str(), Some("blocked"));
    assert_eq!(
        explanation["matched_rule"]["id"].as_str(),
        Some(BLOCKED_RULE)
    );
    // With nothing on this machine outranking it, the copy that answers is the
    // one inside the binary.
    assert_eq!(explanation["source"]["kind"].as_str(), Some("bundled"));

    // Every rule in the manifest is reported, whether or not it matched, which
    // is what makes the output usable for working out why one did not.
    let evaluated = explanation["evaluated"]
        .as_array()
        .expect("no rules were reported");
    assert!(evaluated.len() > 1, "only {} rules ran", evaluated.len());
    assert!(
        evaluated
            .iter()
            .any(|rule| rule["id"].as_str() == Some(BLOCKED_RULE) && rule["matched"] == true)
    );
}

#[test]
fn the_two_json_forms_cannot_be_asked_for_at_once() {
    let output = detect(&["--agent", AGENT, "--json", "--explain"], BLOCKED);

    assert!(!output.status.success(), "{}", said(&output));
    assert_eq!(out(&output), "");
}

#[test]
fn an_empty_screen_from_a_known_agent_falls_back_to_calm() {
    let output = detect(&["--agent", AGENT], "");

    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    assert_eq!(out(&output), "idle\n");

    let explained = detect(&["--agent", AGENT, "--explain"], "");
    let explanation: Value = serde_json::from_str(&out(&explained)).expect("that was not JSON");
    assert_eq!(explanation["matched_rule"], Value::Null);
    assert!(
        explanation["fallback"].as_str().is_some(),
        "nothing said why: {}",
        out(&explained)
    );
}

#[test]
fn a_side_channel_is_evidence_the_screen_is_not() {
    // Nothing is on the screen at all; the title carries the spinner the
    // manifest's highest-priority rule watches for.
    let output = detect(&["--agent", AGENT, "--osc-title", "⠋ building"], "");

    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    assert_eq!(out(&output), "working\n");
}

#[test]
fn more_than_a_megabyte_is_cut_rather_than_swallowed() {
    let flood = "filler\n".repeat(200_000);
    assert!(flood.len() > 1024 * 1024);

    let output = detect(&["--agent", AGENT], &format!("{BLOCKED}{flood}"));

    // The screen was read as far as the cap and answered on, and the caller was
    // told the rest went nowhere.
    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    assert_eq!(out(&output), "blocked\n");
    assert!(err(&output).contains("ignored"), "{}", said(&output));
}

#[test]
fn the_help_describes_screens_and_agents_and_nothing_it_cannot_see() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentbus"))
        .args(["detect", "--help"])
        .output()
        .expect("failed to run agentbus");

    let help = out(&output).to_lowercase();
    for word in NOT_ITS_BUSINESS {
        assert!(!help.contains(word), "the help text says {word:?}: {help}");
    }
    for word in ["screen", "agent", "manifest", "stdin"] {
        assert!(help.contains(word), "the help text omits {word:?}: {help}");
    }
}

#[test]
fn a_manifest_the_store_could_not_use_is_reported_where_it_is_asked_about() {
    let home = TempDir::new().expect("a temporary directory");
    let overrides = home.path().join(".config/agentbus/manifests/screen");
    fs::create_dir_all(&overrides).expect("failed to make the override directory");
    fs::write(overrides.join("claude.toml"), "this is not a manifest\n")
        .expect("failed to write the override");

    // The override cannot be used, so the copy inside the binary answers and
    // the verdict is the same one it would have been.
    let word = detect_at(home.path(), &["--agent", AGENT], BLOCKED);
    assert_eq!(word.status.code(), Some(0), "{}", said(&word));
    assert_eq!(out(&word), "blocked\n");
    // A caller that asked for one word gets one word: this qualifies where the
    // rules came from, and says nothing about the screen.
    assert_eq!(err(&word), "");

    // Asking why is exactly when somebody needs to be told which file was
    // skipped — it is almost always the file they are editing.
    let explained = detect_at(home.path(), &["--agent", AGENT, "--explain"], BLOCKED);
    assert_eq!(explained.status.code(), Some(0), "{}", said(&explained));
    let stderr = err(&explained);
    assert!(
        stderr.contains("claude.toml"),
        "the skipped file is not named: {stderr}"
    );
    let explanation: Value = serde_json::from_str(&out(&explained)).expect("that was not JSON");
    assert_eq!(explanation["source"]["kind"].as_str(), Some("bundled"));
    assert!(
        explanation["warnings"]
            .as_array()
            .expect("no warnings array")
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|it| it.contains("claude.toml"))),
        "the explanation does not carry it either: {}",
        out(&explained)
    );
}
