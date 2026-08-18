//! Adding this program's entries to a JSON file somebody else owns, and taking
//! them out again.
//!
//! Every test here is about the same promise from a different angle: the file
//! belongs to the user, this program borrows a few entries in it, and whatever
//! happens the user's own content comes back unchanged — including when the file
//! turns out to be something this program cannot safely rewrite, and including
//! when the user edits it between an install and the uninstall.

use std::error::Error as _;
use std::fs;
use std::path::{Path, PathBuf};

use agentbus_install::state::{Ownership, State};
use agentbus_install::{Agent, Change, Error, Placement, file, merge, sentinel};
use serde_json::{Map, Value, json};

/// The agent the entries in these tests are pretended to be for. Which one it
/// is makes no difference to any of this; the merge is the same for all of them.
const AGENT: Agent = Agent::Codex;

/// Where a hook entry goes in the files these tests describe.
fn placement(event: &str, command: &str) -> Placement {
    let Value::Object(entry) = json!({"type": "command", "command": command}) else {
        unreachable!("a JSON object literal is an object")
    };
    Placement::new(["hooks", event], entry)
}

/// The placements a pretend agent would be installed with.
fn ours() -> Vec<Placement> {
    vec![
        placement("SessionStart", "/opt/bin/agentbus emit --agent codex"),
        placement("Stop", "/opt/bin/agentbus emit --agent codex"),
    ]
}

/// A file inside a directory that lasts as long as the test.
fn target(existing: Option<&str>) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("cannot make a temporary directory");
    let path = dir.path().join("hooks.json");
    if let Some(contents) = existing {
        fs::write(&path, contents).unwrap();
    }
    (dir, path)
}

/// Installs into `path` for real, and answers with what it did.
fn install(path: &Path, state: &mut State) -> Change {
    let change = merge::plan_install(path, &ours()).expect("planning failed");
    change.apply(AGENT, state).expect("applying failed");
    change
}

/// Uninstalls from `path` for real, and answers with what it did.
fn uninstall(path: &Path, state: &mut State) -> Change {
    let change = merge::plan_uninstall(path, state).expect("planning failed");
    change.apply(AGENT, state).expect("applying failed");
    change
}

/// What is in the file now.
fn document(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// How many of the commands in the file are this program's.
fn ours_in(document: &Value) -> usize {
    match document {
        Value::Object(entries) => entries.values().map(ours_in).sum(),
        Value::Array(items) => items
            .iter()
            .map(|item| usize::from(sentinel::is_marked(item)) + ours_in(item))
            .sum(),
        _ => 0,
    }
}

#[test]
fn a_file_that_is_not_there_is_written_from_nothing() {
    let (_dir, path) = target(None);
    let mut state = State::default();

    let change = install(&path, &mut state);

    assert!(matches!(change, Change::Create { .. }), "{change:?}");
    assert_eq!(ours_in(&document(&path)), 2);
    assert_eq!(state.ownership(&path), Some(Ownership::Created));
    assert!(fs::read_to_string(&path).unwrap().ends_with("}\n"));
}

#[test]
fn what_the_user_wrote_survives_the_merge() {
    let theirs = r#"{
  "hooks": {
    "Stop": [
      {"type": "command", "command": "notify-send done"}
    ]
  },
  "unrelated": {"setting": true}
}
"#;
    let (_dir, path) = target(Some(theirs));
    let mut state = State::default();

    let change = install(&path, &mut state);

    assert!(matches!(change, Change::Rewrite { .. }), "{change:?}");
    let after = document(&path);
    assert_eq!(after["unrelated"], json!({"setting": true}));
    assert_eq!(
        after["hooks"]["Stop"][0],
        json!({"type": "command", "command": "notify-send done"})
    );
    assert_eq!(ours_in(&after), 2);
    assert_eq!(state.ownership(&path), Some(Ownership::Merged));
}

#[test]
fn installing_again_changes_nothing_at_all() {
    let theirs =
        "{\n  \"hooks\": {\n    \"Stop\": [\n      {\"command\": \"theirs\"}\n    ]\n  }\n}\n";
    for existing in [None, Some(theirs)] {
        let (_dir, path) = target(existing);
        let mut state = State::default();

        install(&path, &mut state);
        let after_first = fs::read_to_string(&path).unwrap();
        let backups = file::backups_of(&path).unwrap().len();

        let change = install(&path, &mut state);

        assert!(matches!(change, Change::Keep { .. }), "{change:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), after_first);
        assert_eq!(file::backups_of(&path).unwrap().len(), backups);
    }
}

#[test]
fn upgrading_replaces_what_an_earlier_build_wrote() {
    let (_dir, path) = target(None);
    let mut state = State::default();
    install(&path, &mut state);

    let newer = vec![placement(
        "SessionStart",
        "/opt/bin/agentbus emit --agent codex",
    )];
    merge::plan_install(&path, &newer)
        .unwrap()
        .apply(AGENT, &mut state)
        .unwrap();

    assert_eq!(ours_in(&document(&path)), 1);
}

#[test]
fn the_indentation_of_the_file_is_kept() {
    let (_dir, path) = target(Some("{\n    \"unrelated\": true\n}\n"));

    install(&path, &mut State::default());

    let after = fs::read_to_string(&path).unwrap();
    assert!(after.contains("\n    \"hooks\""), "{after}");
    assert!(!after.contains("\n  \"hooks\""), "{after}");
}

#[test]
fn a_file_that_is_not_json_is_left_exactly_as_it_was() {
    let theirs = "{ this was never JSON\n";
    let (_dir, path) = target(Some(theirs));

    let refusal = merge::plan_install(&path, &ours()).unwrap_err();

    assert!(
        matches!(refusal, Error::NotRewritable { .. }),
        "{refusal:?}"
    );
    assert!(refusal.to_string().contains("hooks.json"), "{refusal}");
    assert!(refusal.source().is_some(), "the reason is not reported");
    assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
}

#[test]
fn a_file_with_a_repeated_key_is_left_exactly_as_it_was() {
    let theirs = "{\"hooks\": {\"Stop\": []}, \"hooks\": {\"Stop\": []}}\n";
    let (_dir, path) = target(Some(theirs));

    let refusal = merge::plan_install(&path, &ours()).unwrap_err();

    assert!(
        matches!(refusal, Error::NotRewritable { .. }),
        "{refusal:?}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
}

#[test]
fn a_file_shaped_wrongly_where_the_entries_go_is_left_exactly_as_it_was() {
    let theirs = "{\"hooks\": \"all of them, please\"}\n";
    let (_dir, path) = target(Some(theirs));

    let refusal = merge::plan_install(&path, &ours()).unwrap_err();

    let Error::Conflict { at, needed, .. } = &refusal else {
        panic!("{refusal:?}");
    };
    assert_eq!((at.as_str(), *needed), ("hooks", "an object"));
    assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
}

#[test]
fn nothing_is_written_until_a_change_is_applied() {
    let (_dir, path) = target(None);

    let planned = merge::plan_install(&path, &ours()).unwrap();

    assert!(matches!(planned, Change::Create { .. }), "{planned:?}");
    assert!(!path.exists(), "planning wrote the file");
}

#[test]
fn uninstalling_takes_out_ours_and_leaves_theirs() {
    let theirs = r#"{"hooks": {"Stop": [{"command": "theirs"}]}, "unrelated": 1}"#;
    let (_dir, path) = target(Some(theirs));
    let mut state = State::default();
    install(&path, &mut state);

    let change = uninstall(&path, &mut state);

    assert!(matches!(change, Change::Rewrite { .. }), "{change:?}");
    assert_eq!(
        document(&path),
        serde_json::from_str::<Value>(theirs).unwrap()
    );
    assert!(path.exists(), "a file this program merged into was removed");
}

#[test]
fn something_the_user_added_afterwards_survives_the_uninstall() {
    let (_dir, path) = target(None);
    let mut state = State::default();
    install(&path, &mut state);

    let mut after = document(&path);
    after["hooks"]["Stop"]
        .as_array_mut()
        .unwrap()
        .push(json!({"command": "theirs"}));
    after["mine"] = json!("do not touch");
    fs::write(&path, serde_json::to_string_pretty(&after).unwrap()).unwrap();

    uninstall(&path, &mut state);

    let left = document(&path);
    assert_eq!(left["hooks"]["Stop"], json!([{"command": "theirs"}]));
    assert_eq!(left["mine"], json!("do not touch"));
    assert_eq!(ours_in(&left), 0);
}

#[test]
fn a_file_this_program_made_goes_away_when_it_is_emptied() {
    let (_dir, path) = target(None);
    let mut state = State::default();
    install(&path, &mut state);

    let change = uninstall(&path, &mut state);

    assert!(matches!(change, Change::Delete { .. }), "{change:?}");
    assert!(!path.exists());
    assert_eq!(state.ownership(&path), None);
    assert!(
        file::backups_of(&path).unwrap().is_empty(),
        "copies of a file this program made were left behind"
    );
}

#[test]
fn a_file_this_program_made_stays_when_the_user_has_put_something_in_it() {
    let (_dir, path) = target(None);
    let mut state = State::default();
    install(&path, &mut state);
    let mut after = document(&path);
    after["theirs"] = json!(1);
    fs::write(&path, serde_json::to_string_pretty(&after).unwrap()).unwrap();

    uninstall(&path, &mut state);

    assert_eq!(document(&path), json!({"theirs": 1}));
}

#[test]
fn uninstalling_twice_is_the_same_as_uninstalling_once() {
    let theirs = r#"{"hooks": {"Stop": [{"command": "theirs"}]}}"#;
    let (_dir, path) = target(Some(theirs));
    let mut state = State::default();
    install(&path, &mut state);
    uninstall(&path, &mut state);
    let after_first = fs::read_to_string(&path).unwrap();

    let change = uninstall(&path, &mut state);

    assert!(matches!(change, Change::Keep { .. }), "{change:?}");
    assert_eq!(fs::read_to_string(&path).unwrap(), after_first);
}

#[test]
fn uninstalling_from_a_file_that_is_not_there_does_nothing() {
    let (_dir, path) = target(None);

    let change = merge::plan_uninstall(&path, &State::default()).unwrap();

    assert!(matches!(change, Change::Keep { .. }), "{change:?}");
    assert!(!path.exists());
}

#[test]
fn a_file_this_program_never_touched_is_left_alone() {
    let theirs = "{\"hooks\": {\"Stop\": [{\"command\": \"theirs\"}]}}\n";
    let (_dir, path) = target(Some(theirs));

    uninstall(&path, &mut State::default());

    assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
}

#[test]
fn every_write_over_a_file_leaves_a_copy_of_what_was_there() {
    let (_dir, path) = target(Some("{\"round\": 0}\n"));
    let mut state = State::default();

    for round in 1..6 {
        merge::plan_install(&path, &ours())
            .unwrap()
            .apply(AGENT, &mut state)
            .unwrap();
        fs::write(&path, format!("{{\"round\": {round}}}\n")).unwrap();
    }

    assert_eq!(file::backups_of(&path).unwrap().len(), file::BACKUPS_KEPT);
}

#[test]
fn an_entry_can_be_placed_at_the_top_level() {
    let (_dir, path) = target(None);
    let Value::Object(entry) = json!({"command": "/opt/bin/agentbus emit"}) else {
        unreachable!("a JSON object literal is an object")
    };
    let placements = vec![Placement::new(Vec::<String>::new(), entry)];

    merge::plan_install(&path, &placements)
        .unwrap()
        .apply(AGENT, &mut State::default())
        .unwrap();

    assert_eq!(ours_in(&document(&path)), 1);
}

#[test]
fn an_entry_carries_what_it_was_given_and_the_mark() {
    let (_dir, path) = target(None);
    let mut entry = Map::new();
    entry.insert("command".to_owned(), Value::from("/opt/bin/agentbus emit"));
    let placements = vec![Placement::new(["hooks", "Stop"], entry)];

    merge::plan_install(&path, &placements)
        .unwrap()
        .apply(AGENT, &mut State::default())
        .unwrap();

    let written = &document(&path)["hooks"]["Stop"][0];
    assert_eq!(written["command"], json!("/opt/bin/agentbus emit"));
    assert_eq!(written[sentinel::KEY], json!({"v": sentinel::VERSION}));
}
