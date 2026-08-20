//! The compiled adapters and the manifest-driven mappings, over the same
//! payloads.
//!
//! Two implementations of one job exist while the second replaces the first,
//! and this file is the whole reason that is safe: every recorded payload goes
//! through both, and what comes out has to be identical — the same kind, the
//! same session, the same directory, the same detail, the same payload carried
//! along, and the same silence where a payload produces nothing at all.
//!
//! It proves two implementations agree, so it lives exactly as long as there
//! are two: it is scaffolding for the move to data, and it is deleted with the
//! implementation it is holding still.

use std::fs;
use std::path::PathBuf;

use agentbus_cli::adapters::{claude, codex, opencode};
use agentbus_detect::{ManifestStore, StorePaths};
use agentbus_protocol::UnstampedEvent;
use serde_json::Value;
use tempfile::TempDir;

/// One agent's compiled normalizer, as the module that holds it exports it.
type Adapter = fn(&Value) -> Option<UnstampedEvent>;

/// The compiled function for each agent whose payloads are recorded, under the
/// name its directory and its mapping are both filed by.
const ADAPTERS: &[(&str, Adapter)] = &[
    ("claude", claude::normalize),
    ("codex", codex::normalize),
    ("opencode", opencode::normalize),
];

/// A store reading nothing but the mappings that ship inside the library, under
/// a home directory of its own so that the machine running the tests cannot
/// answer for an agent here.
fn bundled() -> (TempDir, ManifestStore) {
    let home = TempDir::new().expect("a temporary directory");
    let store = ManifestStore::open(StorePaths::rooted(home.path()));
    (home, store)
}

/// Every payload recorded for one agent, by file name, in a stable order.
fn payloads(agent: &str) -> Vec<(String, Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks")
        .join(agent);
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no payloads in {}", dir.display());
    paths
        .into_iter()
        .map(|path| {
            let text =
                fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let payload =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                payload,
            )
        })
        .collect()
}

#[test]
fn every_recorded_payload_normalizes_the_same_way_through_both() {
    let (_home, store) = bundled();
    let mut events = 0;
    let mut silences = 0;

    for (agent, adapter) in ADAPTERS {
        for (name, payload) in payloads(agent) {
            let compiled = adapter(&payload);
            let from_data = store.normalize_hook(agent, &payload);
            assert_eq!(compiled, from_data, "{agent} {name}");
            match compiled {
                Some(_) => events += 1,
                None => silences += 1,
            }
        }
    }

    // A comparison of two functions that both said nothing agrees about
    // nothing, so both answers have to have been seen for the run to have
    // proved anything.
    assert!(events > 0, "no payload produced an event through either");
    assert!(silences > 0, "no payload was passed over by either");
}

#[test]
fn the_two_agree_about_the_payloads_neither_was_written_for() {
    let (_home, store) = bundled();
    for (agent, adapter) in ADAPTERS {
        for payload in [
            serde_json::json!(null),
            serde_json::json!("SessionStart"),
            serde_json::json!([1, 2, 3]),
            serde_json::json!(7),
            serde_json::json!({}),
        ] {
            assert_eq!(
                adapter(&payload),
                store.normalize_hook(agent, &payload),
                "{agent} {payload}",
            );
        }
    }
}
