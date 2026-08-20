//! Replaying captured screens through the manifests that ship in this library.
//!
//! The unit tests prove the engine does what its rules say. This one proves the
//! rules still describe the agents: every screen here came off a real session,
//! and an agent that redraws its permission prompt breaks this test on the next
//! capture rather than quietly reporting the wrong state to everyone watching.
//!
//! Fixtures arrive by hand, from someone sitting at a live session, so an empty
//! tree is a state this test has to survive. It reports the count either way —
//! a suite that passes because it found nothing to run is the one failure mode
//! a regression alarm cannot have.

use std::fs;
use std::path::{Path, PathBuf};

use agentbus_detect::{
    Detection, ManifestStore, ScreenInput, ScreenState, StorePaths, UNKNOWN_AGENT_FALLBACK,
};

/// The tree of captures, relative to this crate.
const SCREENS: &str = "tests/fixtures/screens";

/// One capture and what it should be read as.
struct Fixture {
    /// The agent whose manifest answers for it, from the directory name.
    agent: String,
    /// How the failure names it: `<agent>/<case>`.
    label: String,
    /// The screen itself.
    screen: String,
    /// The title the terminal was showing, empty when none was captured.
    title: String,
    /// The verdict the sidecar asks for.
    expected: Expected,
}

/// What a `.expected` sidecar asserts.
#[derive(Debug, PartialEq, Eq)]
struct Expected {
    state: ScreenState,
    visible: bool,
    skip: bool,
}

impl Expected {
    /// Reads one sidecar, or says what is wrong with it.
    ///
    /// A malformed sidecar is an error rather than a skip: it was written by a
    /// person to record what they saw, and the one outcome worth refusing is
    /// silently not checking it.
    fn parse(text: &str) -> Result<Self, String> {
        let line = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .ok_or_else(|| "no verdict line".to_owned())?;

        let mut words = line.split_whitespace();
        let state = match words.next().expect("a non-empty line has a first word") {
            "working" => ScreenState::Working,
            "idle" => ScreenState::Idle,
            "blocked" => ScreenState::Blocked,
            "unknown" => ScreenState::Unknown,
            other => return Err(format!("{other:?} is not a state")),
        };

        let mut expected = Self {
            state,
            visible: false,
            skip: false,
        };
        for word in words {
            match word {
                "visible" => expected.visible = true,
                "skip" => expected.skip = true,
                other => return Err(format!("{other:?} is not a marker")),
            }
        }
        Ok(expected)
    }
}

impl From<&Detection> for Expected {
    fn from(detection: &Detection) -> Self {
        Self {
            state: detection.state,
            visible: detection.visible,
            skip: detection.skip,
        }
    }
}

/// The captures on disk, in a stable order so a failing run names the same
/// fixture as the one before it.
fn fixtures() -> Vec<Fixture> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SCREENS);
    let mut agents: Vec<PathBuf> = read_dir(&root)
        .into_iter()
        .filter(|entry| entry.is_dir())
        .collect();
    agents.sort();

    let mut fixtures = Vec::new();
    for dir in agents {
        let agent = file_name(&dir);
        let mut captures: Vec<PathBuf> = read_dir(&dir)
            .into_iter()
            .filter(|entry| entry.extension().is_some_and(|ext| ext == "txt"))
            .collect();
        captures.sort();

        for capture in captures {
            let case = capture
                .file_stem()
                .expect("a file with an extension has a stem")
                .to_string_lossy()
                .into_owned();
            let label = format!("{agent}/{case}");

            let sidecar = capture.with_extension("expected");
            let expected = read(&sidecar)
                .unwrap_or_else(|| panic!("{label} has no {} beside it", file_name(&sidecar)));
            let expected = Expected::parse(&expected)
                .unwrap_or_else(|why| panic!("{label}: {why} in its .expected"));

            fixtures.push(Fixture {
                agent: agent.clone(),
                label,
                screen: read(&capture).expect("the capture was just listed"),
                title: read(&capture.with_extension("title")).unwrap_or_default(),
                expected,
            });
        }
    }
    fixtures
}

/// A directory's entries, or nothing at all when it does not exist — the tree
/// is populated by hand and may legitimately be missing.
fn read_dir(path: &Path) -> Vec<PathBuf> {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect()
}

/// A file's contents, or nothing when it is not there.
fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// The last component of a path, as a string.
fn file_name(path: &Path) -> String {
    path.file_name()
        .expect("every path here was built from a directory entry")
        .to_string_lossy()
        .into_owned()
}

/// A store that can only answer from the manifests inside this binary.
///
/// Rooting it at a directory that exists but holds nothing leaves the override
/// and fetched tiers empty, so the bundled copy answers. That matters more here
/// than anywhere else: this test asks whether the manifests *this library
/// ships* still describe these agents, and a copy in the developer's own config
/// directory would answer a different question on every machine.
fn bundled_only() -> (tempfile::TempDir, ManifestStore) {
    let home = tempfile::tempdir().expect("a temporary directory");
    let store = ManifestStore::open(StorePaths::rooted(home.path()));
    (home, store)
}

#[test]
fn captured_screens_still_read_the_way_they_were_captured() {
    let fixtures = fixtures();
    let (_home, store) = bundled_only();

    let mut wrong = Vec::new();
    for fixture in &fixtures {
        let detection = store.detect(
            &fixture.agent,
            ScreenInput {
                screen: &fixture.screen,
                osc_title: fixture.title.trim_end_matches('\n'),
                osc_progress: "",
            },
        );
        let got = Expected::from(&detection);
        if got != fixture.expected {
            wrong.push(format!(
                "{}: expected {:?}, got {:?} (rule {:?}, fallback {:?})",
                fixture.label, fixture.expected, got, detection.matched_rule, detection.fallback,
            ));
        }
    }

    assert!(
        wrong.is_empty(),
        "{} of {} captured screens no longer read as recorded:\n  {}",
        wrong.len(),
        fixtures.len(),
        wrong.join("\n  "),
    );

    // Said out loud on the way past, because "0 fixtures, all passing" and
    // "every agent still behaves" print the same green tick otherwise. Run with
    // --nocapture to see it.
    println!("{} captured screens replayed", fixtures.len());
}

#[test]
fn every_capture_names_an_agent_the_library_describes() {
    let (_home, store) = bundled_only();

    for fixture in fixtures() {
        let detection = store.detect(&fixture.agent, ScreenInput::from_screen(""));
        assert!(
            detection.matched_rule.is_some() || detection.fallback != Some(UNKNOWN_AGENT_FALLBACK),
            "{} sits under a directory no bundled manifest answers for",
            fixture.label,
        );
    }
}
