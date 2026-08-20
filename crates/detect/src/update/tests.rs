use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use tempfile::TempDir;

use super::*;
use crate::hooks::schema::HOOKS_ENGINE_VERSION;
use crate::screen::region::ScreenInput;
use crate::screen::schema::SCREEN_ENGINE_VERSION;

/// An agent the bundled corpus describes in both families, and the versions it
/// describes it at. Written out rather than read back out of the corpus,
/// because these tests are about what a fetched copy is compared against and
/// one that derived the comparison from the same place the code does could not
/// tell whether the comparison happened.
const AGENT: &str = "claude";
const BUNDLED_SCREEN_VERSION: &str = "2026.08.19.1";
const BUNDLED_HOOK_VERSION: &str = "2026.08.20.1";

/// The bundled sources themselves, for the one test that publishes exactly what
/// is already compiled in.
const BUNDLED_SCREEN: &str = include_str!("../../manifests/screen/claude.toml");

/// A version far beyond anything the corpus will carry, so that a fetched copy
/// declaring it is unambiguously newer.
const NEWER: &str = "2999.01.01.1";

/// A screen no bundled manifest recognizes, so that a verdict on it can only
/// have come from a manifest one of these tests published.
const SCREEN: &str = "a screen with the marker on it\n";

/// The rule id a published screen manifest names itself with.
const RULE: &str = "published_marker";

/// A published screen manifest whose one rule fires on [`SCREEN`].
fn screen_manifest(id: &str, version: Option<&str>, engine: Option<u32>) -> String {
    format!(
        "id = \"{id}\"\n{}{}\n[identify]\nnames = [\"{id}\"]\n\n\
         [[rules]]\nid = \"{RULE}\"\nstate = \"blocked\"\npriority = 500\n\
         visible_blocker = true\ncontains = [\"the marker\"]\n",
        version_line(version),
        engine_line(engine),
    )
}

/// A published hook mapping that maps one distinctive event name.
fn hook_manifest(id: &str, version: Option<&str>, engine: Option<u32>) -> String {
    format!(
        "id = \"{id}\"\n{}{}\n[payload]\nevent = \"hook_event_name\"\nsession = [\"session_id\"]\n\n\
         [[events]]\nname = \"PublishedEvent\"\nkind = \"blocked\"\n",
        version_line(version),
        engine_line(engine),
    )
}

/// The optional `version` line, when a manifest declares one.
fn version_line(version: Option<&str>) -> String {
    version.map_or_else(String::new, |version| format!("version = \"{version}\"\n"))
}

/// The optional `min_engine_version` line, when a manifest declares one.
fn engine_line(engine: Option<u32>) -> String {
    engine.map_or_else(String::new, |engine| {
        format!("min_engine_version = {engine}\n")
    })
}

/// A catalog listing the given `(family, id, path)` entries.
fn catalog(schema_version: u32, entries: &[(&str, &str, &str)]) -> String {
    let mut content = format!("schema_version = {schema_version}\n");
    for (family, id, path) in entries {
        content.push_str(&format!(
            "\n[[manifests]]\nid = \"{id}\"\nfamily = \"{family}\"\npath = \"{path}\"\n"
        ));
    }
    content
}

/// A directory of files behind an http server on a loopback port.
///
/// A real server rather than a seam, so that the size cap, the timeouts and the
/// url joining are exercised as themselves. Paths with directories in them are
/// served, because a catalog's entries name one.
struct Site {
    dir: TempDir,
    server: Arc<tiny_http::Server>,
    thread: Option<JoinHandle<()>>,
}

impl Site {
    /// An empty site, already listening.
    fn new() -> Self {
        let dir = TempDir::new().expect("a temporary directory");
        let server = Arc::new(
            tiny_http::Server::http("127.0.0.1:0").expect("cannot listen on the loopback"),
        );
        let thread = std::thread::spawn({
            let server = Arc::clone(&server);
            let root = dir.path().to_owned();
            move || serve(&server, &root)
        });
        Self {
            dir,
            server,
            thread: Some(thread),
        }
    }

    /// Publishes `content` at `path`.
    fn put(&self, path: &str, content: &str) -> &Self {
        let file = self.dir.path().join(path);
        fs::create_dir_all(file.parent().expect("a parent")).expect("the directory is made");
        fs::write(file, content).expect("the file is written");
        self
    }

    /// Publishes a catalog at the location [`Site::catalog`] names.
    fn index(&self, content: &str) -> &Self {
        self.put("index.toml", content)
    }

    /// Where the catalog is.
    fn catalog(&self) -> String {
        let port = self
            .server
            .server_addr()
            .to_ip()
            .expect("the server is not on a port")
            .port();
        format!("http://127.0.0.1:{port}/index.toml")
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Answers requests with the file each one names, until the server is unblocked.
fn serve(server: &tiny_http::Server, root: &Path) {
    for request in server.incoming_requests() {
        let name = request
            .url()
            .trim_start_matches('/')
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_owned();
        let climbs = name.split('/').any(|part| part == ".." || part.is_empty());
        let answered = match (climbs, fs::read(root.join(&name))) {
            (false, Ok(bytes)) => request.respond(tiny_http::Response::from_data(bytes)),
            _ => request.respond(tiny_http::Response::empty(404)),
        };
        let _ = answered;
    }
}

/// A machine with nothing on it.
fn machine() -> (TempDir, ManifestStore) {
    let home = TempDir::new().expect("a temporary directory");
    let store = ManifestStore::open(StorePaths::rooted(home.path()));
    (home, store)
}

/// Where a fetched copy of one manifest belongs.
fn cached_path(store: &ManifestStore, family: &str, id: &str) -> PathBuf {
    store
        .paths()
        .remote_file(family, id)
        .expect("a path for the id")
}

/// What is cached for one manifest, if anything is.
fn cached(store: &ManifestStore, family: &str, id: &str) -> Option<String> {
    fs::read_to_string(cached_path(store, family, id)).ok()
}

/// The record for one manifest in the outcome.
fn item<'a>(outcome: &'a UpdateOutcome, family: &str, id: &str) -> &'a ManifestOutcome {
    outcome
        .manifests
        .iter()
        .find(|manifest| manifest.family == family && manifest.id == id)
        .unwrap_or_else(|| panic!("no outcome for {family}/{id}"))
}

/// Why one manifest was refused, panicking if it was not.
fn refusal(outcome: &UpdateOutcome, family: &str, id: &str) -> String {
    match &item(outcome, family, id).result {
        ItemResult::Failed(reason) => reason.clone(),
        other => panic!("{family}/{id} was {other:?}, not a refusal"),
    }
}

/// Why the whole check failed, panicking if it did not.
fn catalog_refusal(outcome: &UpdateOutcome) -> String {
    match &outcome.result {
        CheckResult::Failed(reason) => reason.clone(),
        CheckResult::Checked => panic!("the catalog was accepted"),
    }
}

/// Publishes one screen manifest for [`AGENT`] and checks the catalog listing
/// it, which is the shape most of the refusal tests want.
fn published_screen(store: &ManifestStore, site: &Site, manifest: &str) -> UpdateOutcome {
    site.index(&catalog(1, &[("screen", AGENT, "screen/claude.toml")]))
        .put("screen/claude.toml", manifest);
    update(store, &site.catalog())
}

#[test]
fn takes_a_newer_manifest_of_each_family_and_serves_it_after_a_reload() {
    let (_home, store) = machine();
    let site = Site::new();
    let screen = screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION));
    let hooks = hook_manifest(AGENT, Some(NEWER), Some(HOOKS_ENGINE_VERSION));
    site.index(&catalog(
        1,
        &[
            ("screen", AGENT, "screen/claude.toml"),
            ("hooks", AGENT, "hooks/claude.toml"),
        ],
    ))
    .put("screen/claude.toml", &screen)
    .put("hooks/claude.toml", &hooks);

    let outcome = update(&store, &site.catalog());

    assert_eq!(outcome.result, CheckResult::Checked);
    assert!(outcome.committed());
    assert_eq!(outcome.updated().count(), 2);
    assert_eq!(item(&outcome, "screen", AGENT).result, ItemResult::Updated);
    assert_eq!(item(&outcome, "hooks", AGENT).result, ItemResult::Updated);
    assert_eq!(cached(&store, "screen", AGENT).as_deref(), Some(&*screen));
    assert_eq!(cached(&store, "hooks", AGENT).as_deref(), Some(&*hooks));

    store.reload();
    assert_eq!(
        store
            .detect(AGENT, ScreenInput::from_screen(SCREEN))
            .matched_rule
            .as_deref(),
        Some(RULE),
        "the fetched screen manifest is the one in force",
    );
    let event = store
        .normalize_hook(
            AGENT,
            &serde_json::json!({"hook_event_name": "PublishedEvent", "session_id": "s"}),
        )
        .expect("the fetched hook mapping maps its event");
    assert_eq!(event.kind, agentbus_protocol::Kind::Blocked);
}

#[test]
fn records_what_it_took_in_the_status_file() {
    let (_home, store) = machine();
    let site = Site::new();
    let outcome = published_screen(
        &store,
        &site,
        &screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION)),
    );
    assert_eq!(item(&outcome, "screen", AGENT).result, ItemResult::Updated);

    let status = Status::read(store.paths());
    assert_eq!(status.last_checked_unix, Some(outcome.checked_at));
    assert_eq!(status.last_result.as_deref(), Some("checked"));
    let record = status
        .record("screen", AGENT)
        .expect("a record for the agent");
    assert_eq!(record.last_result, "updated");
    assert_eq!(record.attempted_version.as_deref(), Some(NEWER));
    assert_eq!(record.cached_version, None, "nothing was cached before");
    assert_eq!(record.last_error, None);
    assert_eq!(record.last_checked_unix, Some(outcome.checked_at));
}

#[test]
fn a_second_check_of_the_same_version_changes_nothing() {
    let (_home, store) = machine();
    let site = Site::new();
    let manifest = screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION));
    assert_eq!(
        published_screen(&store, &site, &manifest).manifests[0].result,
        ItemResult::Updated,
    );

    let again = update(&store, &site.catalog());
    assert_eq!(item(&again, "screen", AGENT).result, ItemResult::Current);
    assert!(!again.committed());
    assert_eq!(cached(&store, "screen", AGENT).as_deref(), Some(&*manifest));

    let record = Status::read(store.paths())
        .record("screen", AGENT)
        .cloned()
        .expect("a record for the agent");
    assert_eq!(record.last_result, "current");
    assert_eq!(record.cached_version.as_deref(), Some(NEWER));
}

#[test]
fn publishing_exactly_what_is_bundled_caches_nothing() {
    let (_home, store) = machine();
    let site = Site::new();
    let outcome = published_screen(&store, &site, BUNDLED_SCREEN);

    assert_eq!(item(&outcome, "screen", AGENT).result, ItemResult::Current);
    assert_eq!(
        cached(&store, "screen", AGENT),
        None,
        "a copy of what is already compiled in is not worth keeping",
    );
}

#[test]
fn refuses_a_version_older_than_the_bundled_one() {
    let (_home, store) = machine();
    let site = Site::new();
    let outcome = published_screen(
        &store,
        &site,
        &screen_manifest(AGENT, Some("1"), Some(SCREEN_ENGINE_VERSION)),
    );

    let reason = refusal(&outcome, "screen", AGENT);
    assert!(reason.contains("older"), "{reason}");
    assert!(reason.contains(BUNDLED_SCREEN_VERSION), "{reason}");
    assert_eq!(cached(&store, "screen", AGENT), None);
}

#[test]
fn refuses_a_version_older_than_the_cached_one() {
    let (_home, store) = machine();
    let site = Site::new();
    published_screen(
        &store,
        &site,
        &screen_manifest(AGENT, Some("2999.02.01.1"), Some(SCREEN_ENGINE_VERSION)),
    );
    let kept = cached(&store, "screen", AGENT).expect("the newer copy was cached");

    let outcome = published_screen(
        &store,
        &site,
        &screen_manifest(AGENT, Some("2999.01.01.1"), Some(SCREEN_ENGINE_VERSION)),
    );

    let reason = refusal(&outcome, "screen", AGENT);
    assert!(reason.contains("the cached copy"), "{reason}");
    assert_eq!(cached(&store, "screen", AGENT), Some(kept));
}

#[test]
fn refuses_the_same_version_with_different_content() {
    let (_home, store) = machine();
    let site = Site::new();
    published_screen(
        &store,
        &site,
        &screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION)),
    );
    let kept = cached(&store, "screen", AGENT).expect("the first copy was cached");

    let mut edited = screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION));
    edited.push_str("\n[[rules]]\nid = \"another\"\ncontains = [\"anything\"]\n");
    let outcome = published_screen(&store, &site, &edited);

    let reason = refusal(&outcome, "screen", AGENT);
    assert!(
        reason.contains("changed content without a version bump"),
        "{reason}",
    );
    assert_eq!(cached(&store, "screen", AGENT), Some(kept));
}

#[test]
fn refuses_a_manifest_that_declares_no_version() {
    let (_home, store) = machine();
    let site = Site::new();
    let outcome = published_screen(
        &store,
        &site,
        &screen_manifest(AGENT, None, Some(SCREEN_ENGINE_VERSION)),
    );

    assert!(refusal(&outcome, "screen", AGENT).contains("no version"));
    assert_eq!(cached(&store, "screen", AGENT), None);
    assert_eq!(item(&outcome, "screen", AGENT).attempted_version, None);
}

#[test]
fn refuses_a_manifest_that_declares_no_engine_floor() {
    let (_home, store) = machine();
    let site = Site::new();
    let outcome = published_screen(&store, &site, &screen_manifest(AGENT, Some(NEWER), None));

    assert!(refusal(&outcome, "screen", AGENT).contains("no min_engine_version"));
    assert_eq!(cached(&store, "screen", AGENT), None);
    assert_eq!(
        item(&outcome, "screen", AGENT).attempted_version.as_ref(),
        Some(&ManifestVersion::parse(NEWER).expect("a version")),
        "what it offered is reported even though it was turned down",
    );
}

#[test]
fn refuses_a_manifest_that_needs_a_later_engine() {
    let (_home, store) = machine();
    let site = Site::new();
    let outcome = published_screen(
        &store,
        &site,
        &screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION + 1)),
    );

    let reason = refusal(&outcome, "screen", AGENT);
    assert!(
        reason.contains(&(SCREEN_ENGINE_VERSION + 1).to_string()),
        "{reason}"
    );
    assert_eq!(cached(&store, "screen", AGENT), None);
}

#[test]
fn refuses_a_manifest_about_another_agent() {
    let (_home, store) = machine();
    let site = Site::new();
    site.index(&catalog(1, &[("screen", "codex", "screen/codex.toml")]))
        .put(
            "screen/codex.toml",
            &screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION)),
        );

    let outcome = update(&store, &site.catalog());

    let reason = refusal(&outcome, "screen", "codex");
    assert!(
        reason.contains("codex") && reason.contains(AGENT),
        "{reason}"
    );
    assert_eq!(cached(&store, "screen", "codex"), None);
    assert_eq!(cached(&store, "screen", AGENT), None);
}

#[test]
fn refuses_a_body_over_the_cap() {
    let (_home, store) = machine();
    let site = Site::new();
    let mut huge = screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION));
    huge.push_str(&format!(
        "\n# {}\n",
        "padding ".repeat(MAX_FETCH_BYTES as usize / 8),
    ));
    assert!(huge.len() as u64 > MAX_FETCH_BYTES);

    let outcome = published_screen(&store, &site, &huge);

    assert!(matches!(
        item(&outcome, "screen", AGENT).result,
        ItemResult::Failed(_),
    ));
    assert_eq!(cached(&store, "screen", AGENT), None);
}

#[test]
fn refuses_a_manifest_too_large_for_the_store_to_read_back() {
    let (_home, store) = machine();
    let site = Site::new();
    let mut large = screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION));
    large.push_str(&format!(
        "\n# {}\n",
        "padding ".repeat(MAX_MANIFEST_BYTES as usize / 8),
    ));
    assert!(large.len() as u64 > MAX_MANIFEST_BYTES);
    assert!((large.len() as u64) < MAX_FETCH_BYTES, "it arrives whole");

    let outcome = published_screen(&store, &site, &large);

    let reason = refusal(&outcome, "screen", AGENT);
    assert!(reason.contains(&MAX_MANIFEST_BYTES.to_string()), "{reason}");
    assert_eq!(cached(&store, "screen", AGENT), None);
}

#[test]
fn refuses_a_manifest_that_is_not_published() {
    let (_home, store) = machine();
    let site = Site::new();
    site.index(&catalog(1, &[("screen", AGENT, "screen/claude.toml")]));

    let outcome = update(&store, &site.catalog());

    assert!(matches!(
        item(&outcome, "screen", AGENT).result,
        ItemResult::Failed(_),
    ));
    assert_eq!(outcome.result, CheckResult::Checked, "the catalog was fine");
    assert_eq!(cached(&store, "screen", AGENT), None);
}

#[test]
fn refuses_a_whole_catalog_that_climbs_out_of_its_own_directory() {
    for path in [
        "../elsewhere/claude.toml",
        "/etc/claude.toml",
        "https://elsewhere.invalid/claude.toml",
    ] {
        let (_home, store) = machine();
        let site = Site::new();
        site.index(&catalog(1, &[("screen", AGENT, path)])).put(
            "screen/claude.toml",
            &screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION)),
        );

        let outcome = update(&store, &site.catalog());

        let reason = catalog_refusal(&outcome);
        assert!(
            reason.contains("catalog's own directory"),
            "{path}: {reason}"
        );
        assert!(outcome.manifests.is_empty(), "{path}: nothing was fetched");
        assert_eq!(cached(&store, "screen", AGENT), None, "{path}");
    }
}

#[test]
fn refuses_a_whole_catalog_with_an_empty_path() {
    let (_home, store) = machine();
    let site = Site::new();
    site.index(&catalog(1, &[("screen", AGENT, "  ")]));

    assert!(
        catalog_refusal(&update(&store, &site.catalog()))
            .contains("says nothing about where it is"),
    );
}

#[test]
fn refuses_a_whole_catalog_that_lists_one_manifest_twice() {
    let (_home, store) = machine();
    let site = Site::new();
    site.index(&catalog(
        1,
        &[
            ("screen", AGENT, "screen/claude.toml"),
            ("screen", "Claude", "screen/claude-again.toml"),
        ],
    ));

    assert!(catalog_refusal(&update(&store, &site.catalog())).contains("more than once"));
}

#[test]
fn the_same_id_in_two_families_is_not_a_duplicate() {
    let (_home, store) = machine();
    let site = Site::new();
    site.index(&catalog(
        1,
        &[
            ("screen", AGENT, "screen/claude.toml"),
            ("hooks", AGENT, "hooks/claude.toml"),
        ],
    ))
    .put(
        "screen/claude.toml",
        &screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION)),
    )
    .put(
        "hooks/claude.toml",
        &hook_manifest(AGENT, Some(NEWER), Some(HOOKS_ENGINE_VERSION)),
    );

    let outcome = update(&store, &site.catalog());

    assert_eq!(outcome.result, CheckResult::Checked);
    assert_eq!(outcome.updated().count(), 2);
}

#[test]
fn refuses_a_whole_catalog_naming_an_id_that_could_not_be_a_file() {
    let (_home, store) = machine();
    let site = Site::new();
    site.index(&catalog(
        1,
        &[("screen", "../claude", "screen/claude.toml")],
    ));

    assert!(
        catalog_refusal(&update(&store, &site.catalog()))
            .contains("not a name a manifest can be kept under"),
    );
}

#[test]
fn refuses_a_whole_catalog_in_a_schema_it_does_not_know() {
    let (_home, store) = machine();
    let site = Site::new();
    site.index(&catalog(2, &[("screen", AGENT, "screen/claude.toml")]))
        .put(
            "screen/claude.toml",
            &screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION)),
        );

    let outcome = update(&store, &site.catalog());

    let reason = catalog_refusal(&outcome);
    assert!(reason.contains("schema 2"), "{reason}");
    assert!(outcome.manifests.is_empty());
    assert_eq!(cached(&store, "screen", AGENT), None);
}

#[test]
fn refuses_a_catalog_that_is_not_there() {
    let (_home, store) = machine();
    let site = Site::new();

    let outcome = update(&store, &site.catalog());

    assert!(matches!(outcome.result, CheckResult::Failed(_)));
    let status = Status::read(store.paths());
    assert!(status.last_checked_unix.is_some(), "the check is dated");
    assert!(
        status
            .last_result
            .as_deref()
            .is_some_and(|result| result.starts_with("failed:")),
        "{status:?}",
    );
}

#[test]
fn skips_a_family_this_build_has_never_heard_of() {
    let (_home, store) = machine();
    let site = Site::new();
    site.index(&catalog(
        1,
        &[
            ("colours", AGENT, "colours/claude.toml"),
            ("screen", AGENT, "screen/claude.toml"),
        ],
    ))
    .put(
        "screen/claude.toml",
        &screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION)),
    );

    let outcome = update(&store, &site.catalog());

    assert_eq!(outcome.result, CheckResult::Checked);
    assert_eq!(
        outcome.manifests.len(),
        1,
        "the unknown family is not acted on"
    );
    assert_eq!(item(&outcome, "screen", AGENT).result, ItemResult::Updated);
}

#[test]
fn one_bad_entry_does_not_cost_the_others() {
    let (_home, store) = machine();
    let site = Site::new();
    site.index(&catalog(
        1,
        &[
            ("screen", "codex", "screen/codex.toml"),
            ("screen", AGENT, "screen/claude.toml"),
            ("hooks", AGENT, "hooks/claude.toml"),
        ],
    ))
    .put(
        "screen/codex.toml",
        &screen_manifest("codex", None, Some(SCREEN_ENGINE_VERSION)),
    )
    .put(
        "screen/claude.toml",
        &screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION)),
    )
    .put(
        "hooks/claude.toml",
        &hook_manifest(AGENT, Some(NEWER), Some(HOOKS_ENGINE_VERSION)),
    );

    let outcome = update(&store, &site.catalog());

    assert_eq!(outcome.result, CheckResult::Checked);
    assert!(refusal(&outcome, "screen", "codex").contains("no version"));
    assert_eq!(item(&outcome, "screen", AGENT).result, ItemResult::Updated);
    assert_eq!(item(&outcome, "hooks", AGENT).result, ItemResult::Updated);
    assert_eq!(cached(&store, "screen", "codex"), None);
    assert!(cached(&store, "screen", AGENT).is_some());
    assert!(cached(&store, "hooks", AGENT).is_some());

    let status = Status::read(store.paths());
    assert_eq!(
        status
            .record("screen", "codex")
            .map(|record| record.last_result.as_str()),
        Some("failed"),
    );
    assert!(
        status
            .record("screen", "codex")
            .is_some_and(|record| record.last_error.is_some()),
    );
}

#[test]
fn a_failed_check_keeps_what_earlier_checks_recorded() {
    let (_home, store) = machine();
    let good = Site::new();
    published_screen(
        &store,
        &good,
        &screen_manifest(AGENT, Some(NEWER), Some(SCREEN_ENGINE_VERSION)),
    );

    let empty = Site::new();
    let outcome = update(&store, &empty.catalog());
    assert!(matches!(outcome.result, CheckResult::Failed(_)));

    let record = Status::read(store.paths())
        .record("screen", AGENT)
        .cloned()
        .expect("the earlier record survives");
    assert_eq!(record.last_result, "updated");
    assert_eq!(record.attempted_version.as_deref(), Some(NEWER));
}

#[test]
fn a_commit_is_never_observable_half_written() {
    let (_home, store) = machine();
    let path = cached_path(&store, "screen", AGENT);
    let first = screen_manifest(AGENT, Some("1"), Some(SCREEN_ENGINE_VERSION));
    let second = screen_manifest(AGENT, Some("2"), Some(SCREEN_ENGINE_VERSION));

    commit(&path, &first).expect("the first commit");
    assert_eq!(fs::read_to_string(&path).expect("the file"), first);
    commit(&path, &second).expect("the second commit");
    assert_eq!(
        fs::read_to_string(&path).expect("the file"),
        second,
        "a replacement is the whole new file, never a mixture",
    );

    let left: BTreeSet<String> = fs::read_dir(path.parent().expect("a directory"))
        .expect("the directory lists")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        left,
        BTreeSet::from(["claude.toml".to_owned()]),
        "nothing is left behind under a temporary name",
    );
}

#[test]
fn a_file_being_written_is_not_a_manifest_the_store_would_read() {
    let (_home, store) = machine();
    let path = cached_path(&store, "screen", AGENT);
    fs::create_dir_all(path.parent().expect("a directory")).expect("the directory is made");
    // The name a commit uses while it is writing, holding what a torn write
    // would leave: half a manifest, which parses as nothing.
    fs::write(
        path.with_file_name(format!("claude.toml.{}.{PARTIAL}", std::process::id())),
        "id = \"claude\"\nversi",
    )
    .expect("the partial file is written");

    let summary = store
        .summaries()
        .into_iter()
        .find(|summary| summary.family == "screen" && summary.id == AGENT)
        .expect("a summary for the agent");
    assert_eq!(summary.remote_version, None);
    assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);
    assert_eq!(
        summary.bundled_version.as_ref().map(ToString::to_string),
        Some(BUNDLED_SCREEN_VERSION.to_owned()),
        "the bundled copy still answers",
    );
}

#[test]
fn a_bundled_hook_mapping_is_compared_against_too() {
    let (_home, store) = machine();
    let site = Site::new();
    site.index(&catalog(1, &[("hooks", AGENT, "hooks/claude.toml")]))
        .put(
            "hooks/claude.toml",
            &hook_manifest(AGENT, Some("1"), Some(HOOKS_ENGINE_VERSION)),
        );

    let outcome = update(&store, &site.catalog());

    let reason = refusal(&outcome, "hooks", AGENT);
    assert!(reason.contains(BUNDLED_HOOK_VERSION), "{reason}");
    assert_eq!(cached(&store, "hooks", AGENT), None);
}

#[test]
fn the_catalog_names_a_directory_its_entries_are_relative_to() {
    assert_eq!(
        base_of("https://example.invalid/a/b/index.toml").expect("a base"),
        "https://example.invalid/a/b",
    );
    assert!(base_of("index.toml").is_err());
}

#[test]
fn a_relative_path_is_one_that_cannot_leave_where_it_started() {
    for good in [
        "claude.toml",
        "screen/claude.toml",
        "a/b/c.toml",
        "a..b/c.toml",
    ] {
        assert!(relative(good), "{good} should have been allowed");
    }
    for bad in [
        "/claude.toml",
        "../claude.toml",
        "screen/../../claude.toml",
        "https://elsewhere.invalid/claude.toml",
        "file:///etc/passwd",
    ] {
        assert!(!relative(bad), "{bad} should have been refused");
    }
}

#[test]
fn the_catalog_is_the_published_one_unless_the_environment_says_otherwise() {
    // Not set here: changing the environment is a change every other test in
    // this process would see. What is checked is that the default is the one
    // documented, and that it is where the release repository publishes.
    assert!(DEFAULT_CATALOG_URL.starts_with("https://"));
    assert!(DEFAULT_CATALOG_URL.ends_with("/index.toml"));
    assert_eq!(catalog_url(), DEFAULT_CATALOG_URL);
}
