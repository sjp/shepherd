//! The bus as an application in its own right.
//!
//! `agentbus` is not a library that something else is expected to make useful.
//! One binary, started by hand, is the whole installation: a daemon, hooks
//! reporting into it, and a command that says what every session is doing. This
//! file runs that from end to end and then asserts the claim underneath it —
//! that the binary is built out of the bus's own crates and nothing else in the
//! repository. Today that is true by construction. The test is here so that it
//! stays true once there is something else in the repository to fail it.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use common::Bus;

/// The agent whose recorded session is replayed.
const AGENT: &str = "claude";

/// The correlation the payloads are emitted with, which the table has to show
/// back untouched.
const PANE: &str = "w1:p1";

/// The crates the binary is allowed to be built from.
const BUS_CRATES: [&str; 5] = [
    "agentbus-protocol",
    "agentbus-detect",
    "agentbus-daemon",
    "agentbus-install",
    "agentbus-cli",
];

#[test]
fn a_session_is_reported_by_the_binary_alone() {
    let bus = Bus::start();
    let mut watching = bus.attach();
    watching.snapshot();

    let recording = common::recording(AGENT);
    for step in &recording.steps {
        bus.emit(&recording.agent, Some(PANE), &step.read());
        watching.event(&step.name());
    }

    // What a person sees, and what a program reading the same bus sees, are the
    // same facts about the same session.
    let snapshot = bus.snapshot();
    let [session] = snapshot.sessions.as_slice() else {
        panic!("one session was reported as {:?}", snapshot.sessions);
    };
    let table = String::from_utf8(bus.run(&["status"]).stdout).expect("not text");
    for shown in [
        session.session.as_str(),
        session.agent.as_str(),
        session.status.as_str(),
        session.cwd.as_deref().expect("no working directory"),
        session.correlation.as_deref().expect("no correlation"),
    ] {
        assert!(table.contains(shown), "{shown:?} is missing from {table}");
    }
    assert_eq!(session.correlation.as_deref(), Some(PANE));

    // And a subscriber arriving now is told exactly what the command printed,
    // because both are the same snapshot from the same daemon.
    assert_eq!(bus.attach().snapshot().sessions, snapshot.sessions);
}

#[test]
fn the_binary_is_built_from_the_bus_crates_and_nothing_else() {
    let packages = workspace_packages();
    let mut reached = BTreeSet::new();
    let mut pending = vec!["agentbus-cli".to_owned()];

    while let Some(name) = pending.pop() {
        if !reached.insert(name.clone()) {
            continue;
        }
        let package = packages
            .iter()
            .find(|package| package["name"] == name.as_str())
            .unwrap_or_else(|| panic!("{name} is not a crate in this workspace"));
        let dependencies = package["dependencies"]
            .as_array()
            .expect("a package with no dependencies array");
        for dependency in dependencies {
            // Anything with a path is a crate in this repository; anything
            // without one came from a registry and is somebody else's. A
            // dependency needed only to test the crate is not part of what was
            // built, which is what the harness these tests are written with is.
            let is_in_repo = dependency.get("path").is_some();
            let only_for_tests = dependency["kind"] == "dev";
            if is_in_repo && !only_for_tests {
                let name = dependency["name"].as_str().expect("a nameless dependency");
                pending.push(name.to_owned());
            }
        }
    }

    let expected: BTreeSet<String> = BUS_CRATES.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(
        reached, expected,
        "the binary is no longer built from the bus crates alone"
    );
}

/// Every crate in this workspace, as the build system describes it.
///
/// Asking the build system is the only answer that cannot drift: a manifest read
/// by hand would have to reimplement how a dependency is declared, and would go
/// on passing the moment somebody declared one a different way.
fn workspace_packages() -> Vec<Value> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let output = Command::new(PathBuf::from(env!("CARGO")))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .arg("--manifest-path")
        .arg(&manifest)
        .output()
        .expect("cannot run cargo");
    assert!(
        output.status.success(),
        "cargo metadata exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("that was not metadata");
    metadata["packages"]
        .as_array()
        .expect("no packages")
        .clone()
}
