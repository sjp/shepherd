//! What `agentbus manifests` reports, fetches and prints.
//!
//! Every one of these runs the shipped binary against a home directory of its
//! own, because that is the whole interface: somebody at a shell asking which
//! copy of a manifest is in force on their machine, taking whatever has been
//! published since, and reading the copy that is answering so they can start
//! editing their own. Nothing here needs a daemon — manifests are files, and
//! the commands that manage them are file commands.
//!
//! The catalog is a real http server on the loopback serving a directory these
//! tests write, so the url joining, the fetch and the refusals happen as
//! themselves rather than through a seam.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::thread::JoinHandle;

use serde_json::Value;
use tempfile::TempDir;

/// An agent the bundled corpus describes, and the version it describes it at.
/// Pinned here rather than read out of the corpus: these tests are about what
/// the command says about a bundled manifest, and one that derived the version
/// from the same place the command does could not tell whether it was reported
/// at all.
const AGENT: &str = "claude";
const BUNDLED_VERSION: &str = "2026.08.19.1";

/// A version far beyond anything the corpus will carry, so that a published
/// copy declaring it is unambiguously newer.
const NEWER: &str = "2999.01.01.1";

/// An agent no bundled manifest describes, for the answers about absence.
const STRANGER: &str = "nobody";

/// The variables that would otherwise let the machine running these tests
/// decide which manifests answer and which catalog is read.
const INHERITED: [&str; 3] = [
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "AGENTBUS_MANIFEST_CATALOG_URL",
];

/// The words a screen is described in. None of them may appear in what these
/// commands say about themselves: this is data about coding agents, and a
/// program that manages it has no business knowing what anybody displays it in.
const NOT_ITS_BUSINESS: [&str; 4] = ["pane", "tab", "workspace", "window"];

/// The words of a piece of text, lowercased and stripped of punctuation, so
/// that a search for one finds the word and not every word containing it.
fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// A machine with a home directory of its own and nothing in it.
struct Machine {
    home: TempDir,
}

impl Machine {
    fn new() -> Self {
        Self {
            home: TempDir::new().expect("a temporary directory"),
        }
    }

    /// Runs `agentbus manifests` with `args`, over this machine's files alone.
    fn run(&self, args: &[&str]) -> Output {
        self.run_with(&[], args)
    }

    /// The same, with the base directory variables this machine is to have.
    fn run_with(&self, environment: &[(&str, &Path)], args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentbus"));
        command.arg("manifests").args(args).env("HOME", self.home());
        for variable in INHERITED {
            command.env_remove(variable);
        }
        for (variable, value) in environment {
            command.env(variable, value);
        }
        command.output().expect("failed to run agentbus")
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    /// Where a copy somebody wrote themselves belongs.
    fn override_file(&self, family: &str, id: &str) -> PathBuf {
        self.home()
            .join(".config/agentbus/manifests")
            .join(family)
            .join(format!("{id}.toml"))
    }

    /// Where a fetched copy is kept.
    fn remote_file(&self, family: &str, id: &str) -> PathBuf {
        self.home()
            .join(".local/state/agentbus/manifests/remote")
            .join(family)
            .join(format!("{id}.toml"))
    }

    /// Writes one of those files.
    fn place(&self, path: &Path, content: &str) {
        fs::create_dir_all(path.parent().expect("a parent")).expect("the directory is made");
        fs::write(path, content).expect("the manifest is written");
    }
}

/// A directory of files behind an http server on a loopback port.
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
        let name = request.url().trim_start_matches('/').to_owned();
        let climbs = name.split('/').any(|part| part == ".." || part.is_empty());
        let _ = match (climbs, fs::read(root.join(&name))) {
            (false, Ok(bytes)) => request.respond(tiny_http::Response::from_data(bytes)),
            _ => request.respond(tiny_http::Response::empty(404)),
        };
    }
}

/// A catalog listing the given `(family, id, path)` entries.
fn catalog(entries: &[(&str, &str, &str)]) -> String {
    let mut content = String::from("schema_version = 1\n");
    for (family, id, path) in entries {
        content.push_str(&format!(
            "\n[[manifests]]\nid = \"{id}\"\nfamily = \"{family}\"\npath = \"{path}\"\n"
        ));
    }
    content
}

/// A publishable screen manifest for `id` at `version`, with one rule that
/// names itself.
fn screen_manifest(id: &str, version: &str) -> String {
    format!(
        "id = \"{id}\"\nversion = \"{version}\"\nmin_engine_version = 1\n\n\
         [identify]\nnames = [\"{id}\"]\n\n\
         [[rules]]\nid = \"published_marker\"\nstate = \"blocked\"\npriority = 500\n\
         visible_blocker = true\ncontains = [\"the marker\"]\n"
    )
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

/// The row of a table whose cells begin with `cells`, as its cells.
fn row(text: &str, cells: &[&str]) -> Vec<String> {
    text.lines()
        .map(|line| {
            line.split_whitespace()
                .map(ToOwned::to_owned)
                .collect::<Vec<String>>()
        })
        .find(|line| cells.iter().zip(line).all(|(wanted, cell)| wanted == cell))
        .unwrap_or_else(|| panic!("no row for {cells:?} in\n{text}"))
}

/// What a `--json` command printed, parsed.
fn json(output: &Output) -> Value {
    serde_json::from_str(&out(output)).unwrap_or_else(|error| panic!("{error}: {}", said(output)))
}

/// The listed manifest for one family and id.
fn listed<'a>(document: &'a Value, family: &str, id: &str) -> &'a Value {
    document["manifests"]
        .as_array()
        .expect("the manifests are an array")
        .iter()
        .find(|manifest| manifest["family"] == family && manifest["id"] == id)
        .unwrap_or_else(|| panic!("no {family} manifest for {id} in {document}"))
}

#[test]
fn a_machine_with_nothing_on_it_reads_every_screen_with_the_bundled_copies() {
    let machine = Machine::new();

    let output = machine.run(&["list"]);

    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    let text = out(&output);
    assert!(text.contains("no check"), "{text}");
    let claude = row(&text, &["screen", AGENT]);
    assert_eq!(claude[2], "bundled");
    assert_eq!(claude[3], BUNDLED_VERSION);
    assert_eq!(claude[4..7], ["-", "-", "-"], "{text}");
    assert!(
        text.lines().skip(2).all(|line| line.contains("bundled")),
        "{text}",
    );
}

#[test]
fn both_families_are_listed_because_a_machine_holds_both() {
    let machine = Machine::new();

    let text = out(&machine.run(&["list"]));

    assert_eq!(row(&text, &["screen", AGENT])[2], "bundled");
    assert_eq!(row(&text, &["hooks", AGENT])[2], "bundled");
}

#[test]
fn a_check_takes_what_is_published_and_the_list_then_says_so() {
    let machine = Machine::new();
    let site = Site::new();
    let published = screen_manifest(AGENT, NEWER);
    site.put(
        "index.toml",
        &catalog(&[("screen", AGENT, "screen/claude.toml")]),
    )
    .put("screen/claude.toml", &published);

    let update = machine.run(&["update", "--catalog", &site.catalog()]);

    assert_eq!(update.status.code(), Some(0), "{}", said(&update));
    let text = out(&update);
    assert!(text.contains(&site.catalog()), "{text}");
    let acted = row(&text, &["screen", AGENT]);
    assert_eq!(acted[2], "updated");
    assert_eq!(acted[3], "-", "nothing was cached before: {text}");
    assert_eq!(acted[4], NEWER);
    assert_eq!(
        fs::read_to_string(machine.remote_file("screen", AGENT)).expect("the fetched copy"),
        published,
    );

    let listed = out(&machine.run(&["list"]));
    assert!(listed.contains("last checked"), "{listed}");
    let claude = row(&listed, &["screen", AGENT]);
    assert_eq!(claude[2], "remote");
    assert_eq!(claude[3], BUNDLED_VERSION);
    assert_eq!(claude[4], NEWER, "the fetched version: {listed}");
    assert_eq!(claude[7], "updated", "the last result: {listed}");
}

#[test]
fn checking_again_finds_nothing_to_do() {
    let machine = Machine::new();
    let site = Site::new();
    site.put(
        "index.toml",
        &catalog(&[("screen", AGENT, "screen/claude.toml")]),
    )
    .put("screen/claude.toml", &screen_manifest(AGENT, NEWER));

    machine.run(&["update", "--catalog", &site.catalog()]);
    let again = machine.run(&["update", "--catalog", &site.catalog()]);

    assert_eq!(again.status.code(), Some(0), "{}", said(&again));
    assert_eq!(row(&out(&again), &["screen", AGENT])[2], "current");
}

#[test]
fn one_published_manifest_that_is_refused_costs_only_itself() {
    let machine = Machine::new();
    let site = Site::new();
    site.put(
        "index.toml",
        &catalog(&[
            ("screen", AGENT, "screen/claude.toml"),
            ("screen", "codex", "screen/codex.toml"),
        ]),
    )
    .put("screen/claude.toml", &screen_manifest(AGENT, NEWER))
    .put("screen/codex.toml", "this is not a manifest at all\n");

    let output = machine.run(&["update", "--catalog", &site.catalog()]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "one entry failing is not the command failing: {}",
        said(&output),
    );
    let text = out(&output);
    assert_eq!(row(&text, &["screen", AGENT])[2], "updated");
    let refused = row(&text, &["screen", "codex"]);
    assert_eq!(refused[2], "failed");
    assert!(refused[5..].join(" ").len() > 1, "no reason given: {text}");
    assert!(!machine.remote_file("screen", "codex").exists());
}

#[test]
fn a_catalog_that_cannot_be_read_is_the_one_failure_worth_a_status() {
    let machine = Machine::new();
    let site = Site::new();

    let output = machine.run(&["update", "--catalog", &site.catalog()]);

    assert_eq!(output.status.code(), Some(1), "{}", said(&output));
    assert_eq!(out(&output), "", "nothing was checked, so nothing to show");
    assert!(err(&output).contains(&site.catalog()), "{}", err(&output));
}

#[test]
fn a_refusal_is_recorded_where_the_next_list_will_find_it() {
    let machine = Machine::new();
    let site = Site::new();
    site.put(
        "index.toml",
        &catalog(&[("screen", AGENT, "screen/claude.toml")]),
    )
    .put(
        "screen/claude.toml",
        &screen_manifest(AGENT, "1999.01.01.1"),
    );

    machine.run(&["update", "--catalog", &site.catalog()]);

    let text = out(&machine.run(&["list"]));
    let claude = row(&text, &["screen", AGENT]);
    assert_eq!(claude[2], "bundled", "the older copy was not taken: {text}");
    assert_eq!(claude[7], "failed");
    assert!(text.contains("1999.01.01.1"), "{text}");
}

#[test]
fn an_override_takes_over_and_the_list_says_what_it_shadowed() {
    let machine = Machine::new();
    machine.place(
        &machine.remote_file("screen", AGENT),
        &screen_manifest(AGENT, NEWER),
    );
    machine.place(
        &machine.override_file("screen", AGENT),
        &screen_manifest(AGENT, "2999.02.02.1"),
    );

    let text = out(&machine.run(&["list"]));

    let claude = row(&text, &["screen", AGENT]);
    assert_eq!(claude[2], "override");
    assert_eq!(claude[5], "2999.02.02.1");
    let notes = text
        .lines()
        .find(|line| line.starts_with("screen") && line.contains(AGENT))
        .expect("the row");
    assert!(notes.contains("shadowed"), "{text}");
    assert!(
        notes.contains(&machine.remote_file("screen", AGENT).display().to_string()),
        "the warning should name the file it passed over: {text}",
    );
}

#[test]
fn show_prints_exactly_the_bytes_that_are_in_force() {
    let machine = Machine::new();
    let written = screen_manifest(AGENT, NEWER);
    machine.place(&machine.override_file("screen", AGENT), &written);

    let output = machine.run(&["show", AGENT]);

    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    assert_eq!(out(&output), written);
    let commentary = err(&output);
    assert!(commentary.contains("override"), "{commentary}");
    assert!(
        commentary.contains(&machine.override_file("screen", AGENT).display().to_string()),
        "{commentary}",
    );
}

#[test]
fn what_show_prints_is_an_override_that_needs_no_editing_to_take_effect() {
    let machine = Machine::new();

    let shown = out(&machine.run(&["show", AGENT]));
    machine.place(&machine.override_file("screen", AGENT), &shown);

    let text = out(&machine.run(&["list"]));
    let claude = row(&text, &["screen", AGENT]);
    assert_eq!(claude[2], "override");
    assert_eq!(claude[5], BUNDLED_VERSION, "{text}");
    assert_eq!(out(&machine.run(&["show", AGENT])), shown);
}

#[test]
fn the_two_families_are_shown_separately() {
    let machine = Machine::new();

    let screen = out(&machine.run(&["show", AGENT]));
    let hooks = out(&machine.run(&["show", AGENT, "--family", "hooks"]));

    assert!(screen.contains("[[rules]]"), "{screen}");
    assert!(hooks.contains("[[events]]"), "{hooks}");
    assert_ne!(screen, hooks);
}

#[test]
fn an_agent_nothing_describes_is_not_an_answer() {
    let machine = Machine::new();

    let output = machine.run(&["show", STRANGER]);

    assert_eq!(output.status.code(), Some(1), "{}", said(&output));
    assert_eq!(out(&output), "");
    assert!(err(&output).contains(STRANGER), "{}", err(&output));
}

#[test]
fn the_json_list_carries_what_the_table_carries() {
    let machine = Machine::new();
    machine.place(
        &machine.remote_file("screen", AGENT),
        &screen_manifest(AGENT, NEWER),
    );
    machine.place(
        &machine.override_file("screen", AGENT),
        &screen_manifest(AGENT, "2999.02.02.1"),
    );

    let output = machine.run(&["list", "--json"]);

    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    let document = json(&output);
    assert_eq!(document["v"], 1);
    assert_eq!(document["check"], Value::Null);
    let claude = listed(&document, "screen", AGENT);
    assert_eq!(claude["source"]["kind"], "override");
    assert_eq!(claude["bundled_version"], BUNDLED_VERSION);
    assert_eq!(claude["remote_version"], NEWER);
    assert_eq!(claude["override_version"], "2999.02.02.1");
    assert!(
        claude["warnings"]
            .as_array()
            .expect("the warnings are an array")
            .iter()
            .any(|warning| warning.as_str().is_some_and(|w| w.contains("shadowed"))),
        "{claude}",
    );
}

#[test]
fn the_json_list_reports_the_check_that_ran() {
    let machine = Machine::new();
    let site = Site::new();
    site.put(
        "index.toml",
        &catalog(&[("screen", AGENT, "screen/claude.toml")]),
    )
    .put("screen/claude.toml", &screen_manifest(AGENT, NEWER));
    machine.run(&["update", "--catalog", &site.catalog()]);

    let document = json(&machine.run(&["list", "--json"]));

    assert_eq!(document["check"]["result"], "checked");
    assert!(
        document["check"]["at"]
            .as_str()
            .is_some_and(|at| at.ends_with('Z')),
        "{document}",
    );
    let claude = listed(&document, "screen", AGENT);
    assert_eq!(claude["source"]["kind"], "remote");
    assert_eq!(claude["checked"]["result"], "updated");
    assert_eq!(claude["checked"]["attempted_version"], NEWER);
}

#[test]
fn the_json_check_carries_what_the_table_carries() {
    let machine = Machine::new();
    let site = Site::new();
    site.put(
        "index.toml",
        &catalog(&[
            ("screen", AGENT, "screen/claude.toml"),
            ("screen", "codex", "screen/codex.toml"),
        ]),
    )
    .put("screen/claude.toml", &screen_manifest(AGENT, NEWER))
    .put("screen/codex.toml", "this is not a manifest at all\n");

    let output = machine.run(&["update", "--json", "--catalog", &site.catalog()]);

    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    let document = json(&output);
    assert_eq!(document["v"], 1);
    assert_eq!(document["catalog"], site.catalog());
    assert_eq!(document["result"], "checked");
    assert_eq!(document["error"], Value::Null);
    let taken = listed(&document, "screen", AGENT);
    assert_eq!(taken["result"], "updated");
    assert_eq!(taken["attempted_version"], NEWER);
    let refused = listed(&document, "screen", "codex");
    assert_eq!(refused["result"], "failed");
    assert!(refused["error"].is_string(), "{refused}");
}

#[test]
fn a_json_check_that_never_got_started_says_so_and_still_fails() {
    let machine = Machine::new();
    let site = Site::new();

    let output = machine.run(&["update", "--json", "--catalog", &site.catalog()]);

    assert_eq!(output.status.code(), Some(1), "{}", said(&output));
    let document = json(&output);
    assert_eq!(document["result"], "failed");
    assert!(document["error"].is_string(), "{document}");
    assert_eq!(document["manifests"].as_array().expect("an array").len(), 0,);
}

#[test]
fn the_base_directory_variables_decide_where_the_copies_are_looked_for() {
    let machine = Machine::new();
    let config = TempDir::new().expect("a temporary directory");
    let state = TempDir::new().expect("a temporary directory");
    let written = screen_manifest(AGENT, NEWER);
    machine.place(
        &config.path().join("agentbus/manifests/screen/claude.toml"),
        &written,
    );
    // Under the home directory as well, where it would be found if the
    // variables were being ignored — and holding a different version, so that
    // the two files cannot be confused for one another.
    machine.place(
        &machine.override_file("screen", AGENT),
        &screen_manifest(AGENT, "2999.02.02.1"),
    );
    let environment = [
        ("XDG_CONFIG_HOME", config.path()),
        ("XDG_STATE_HOME", state.path()),
    ];

    let text = out(&machine.run_with(&environment, &["list"]));

    let claude = row(&text, &["screen", AGENT]);
    assert_eq!(claude[2], "override");
    assert_eq!(claude[5], NEWER, "{text}");
    assert_eq!(
        out(&machine.run_with(&environment, &["show", AGENT])),
        written
    );
}

#[test]
fn nothing_here_knows_what_anybody_displays_a_manifest_in() {
    let machine = Machine::new();
    let commands: [&[&str]; 4] = [
        &["--help"],
        &["list", "--help"],
        &["update", "--help"],
        &["show", "--help"],
    ];

    for args in commands {
        let text = out(&machine.run(args));
        assert!(!text.is_empty(), "no help for {args:?}");
        let said = words(&text);
        for word in NOT_ITS_BUSINESS {
            assert!(
                !said.iter().any(|spoken| spoken == word),
                "{args:?} help mentions {word:?}:\n{text}",
            );
        }
    }
}
