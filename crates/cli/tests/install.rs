//! `agentbus install` and `agentbus uninstall` against a described machine.
//!
//! These run the real binary with `HOME` and `PATH` pointed into a temporary
//! directory, so what they exercise is the whole path a user's invocation takes:
//! the argument parser, the detection rules reading the environment this process
//! set up, the files written, the agent's own command line being run, and the
//! report.
//!
//! The agent's command line is a stand-in that records what it was asked and
//! answers the way the real one does — including the part that matters most,
//! that installing a plugin takes a *copy* of it. A stand-in that only recorded
//! its arguments would agree with an installer that never noticed the copy had
//! gone stale, which is the failure this is most concerned with catching.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// The variable the stand-in keeps its world under, so that each test gets one
/// of its own.
const WORLD_VAR: &str = "AGENTBUS_TEST_CLAUDE_WORLD";

/// A stand-in for Claude's command line.
///
/// It answers `plugin marketplace list` and `plugin list` from what it has been
/// told, and `plugin install` copies the plugin out of the marketplace exactly
/// as the real one does — which is what makes a second install of changed
/// content a real question rather than a formality.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
set -eu
# The machine's search path holds only the agents being described, so that
# detection cannot find a real one; this needs the ordinary tools as well.
PATH="/bin:/usr/bin:$PATH"
printf '%s\n' "$*" >> "$AGENTBUS_TEST_CLAUDE_WORLD/argv"
marketplace="$AGENTBUS_TEST_CLAUDE_WORLD/marketplace"
installed="$AGENTBUS_TEST_CLAUDE_WORLD/installed"

case "$1 $2 ${3-}" in
"plugin marketplace add")
	printf '%s' "$4" > "$marketplace"
	exit 0
	;;
"plugin marketplace remove")
	rm -f "$marketplace"
	exit 0
	;;
"plugin marketplace list")
	if [ -f "$marketplace" ]; then
		printf '[{"name":"agentbus","source":"directory","path":"%s"}]' "$(cat "$marketplace")"
	else
		printf '[]'
	fi
	exit 0
	;;
esac

case "$1 $2" in
"plugin install")
	[ -f "$marketplace" ] || { printf 'not in any marketplace\n' >&2; exit 1; }
	rm -rf "$installed"
	mkdir -p "$installed"
	cp -R "$(cat "$marketplace")/agentbus/." "$installed/"
	exit 0
	;;
"plugin uninstall")
	rm -rf "$installed"
	exit 0
	;;
"plugin list")
	if [ -d "$installed" ]; then
		printf '[{"id":"agentbus@agentbus","version":"0.1.0","installPath":"%s"}]' "$installed"
	else
		printf '[]'
	fi
	exit 0
	;;
esac
exit 0
"#;

/// A home directory, a search path and a state directory, all under one
/// temporary directory that is removed with the test.
struct Machine {
    _root: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    state: PathBuf,
    world: PathBuf,
}

impl Machine {
    /// A machine with no coding agent on it.
    fn new() -> Self {
        let root = tempfile::tempdir().expect("cannot make a temporary directory");
        let (home, bin, state, world) = (
            root.path().join("home"),
            root.path().join("bin"),
            root.path().join("state"),
            root.path().join("world"),
        );
        for dir in [&home, &bin, &world] {
            fs::create_dir_all(dir).unwrap();
        }
        Self {
            _root: root,
            home,
            bin,
            state,
            world,
        }
    }

    /// Gives the machine a configuration directory for `agent`.
    fn configured(self, agent: &str) -> Self {
        let dir = match agent {
            "opencode" => self.home.join(".config").join(agent),
            _ => self.home.join(format!(".{agent}")),
        };
        fs::create_dir_all(dir).unwrap();
        self
    }

    /// Puts `agent`'s command on the machine's search path, doing nothing.
    fn installed(self, agent: &str) -> Self {
        self.executable(agent, "#!/bin/sh\n")
    }

    /// Puts a Claude on the machine's search path that behaves like one.
    fn with_claude(self) -> Self {
        self.executable("claude", FAKE_CLAUDE)
    }

    /// Puts a runnable `script` on the machine's search path, called `name`.
    fn executable(self, name: &str, script: &str) -> Self {
        let command = self.bin.join(name);
        fs::write(&command, script).unwrap();
        fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();
        self
    }

    /// Runs the binary on this machine, with nothing inherited from whoever is
    /// running the tests.
    fn run(&self, args: &[&str]) -> Output {
        self.run_binary(Path::new(env!("CARGO_BIN_EXE_agentbus")), args)
    }

    /// Runs a copy of the binary that lives at `binary`.
    ///
    /// The hooks name this program by the path it was run from, so running a
    /// copy from somewhere else is what a user upgrading, or moving where they
    /// keep it, actually does to an installation.
    fn run_binary(&self, binary: &Path, args: &[&str]) -> Output {
        // A binary copied into place a moment ago can still be held open for
        // writing by another test doing the same thing, and the kernel refuses
        // to run a file somebody is writing. It clears itself; nothing about the
        // condition is what any of these tests is about.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let attempt = Command::new(binary)
                .args(args)
                .env_clear()
                .env("HOME", &self.home)
                .env("PATH", &self.bin)
                .env("XDG_STATE_HOME", &self.state)
                .env(WORLD_VAR, &self.world)
                .output();
            match attempt {
                Err(error)
                    if error.kind() == io::ErrorKind::ExecutableFileBusy
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => return result.expect("cannot run agentbus"),
            }
        }
    }

    /// Runs the binary and hands back what it printed, having checked that it
    /// succeeded.
    fn report(&self, args: &[&str]) -> String {
        let output = self.run(args);
        succeeds(&output).to_owned()
    }

    /// Where the marketplace is generated.
    fn marketplace(&self) -> PathBuf {
        self.home.join(".local/share/agentbus/claude-marketplace")
    }

    /// The hooks inside the generated marketplace.
    fn hooks(&self) -> PathBuf {
        self.marketplace().join("agentbus/hooks/hooks.json")
    }

    /// Where Codex's hooks are dropped in.
    fn codex_hooks(&self) -> PathBuf {
        self.home.join(".codex/hooks.json")
    }

    /// The directory OpenCode loads plugins from.
    fn opencode_plugin_dir(&self) -> PathBuf {
        self.home.join(".config/opencode/plugin")
    }

    /// Where OpenCode's plugin is dropped in.
    fn opencode_plugin(&self) -> PathBuf {
        self.opencode_plugin_dir().join("agentbus.js")
    }

    /// What is in a JSON file on this machine now.
    fn document(&self, path: &Path) -> serde_json::Value {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()))
    }

    /// Every copy taken of `path`, newest first.
    fn backups_of(&self, path: &Path) -> Vec<PathBuf> {
        let dir = path.parent().expect("a file with no directory");
        let prefix = format!(
            "{}{}",
            path.file_name().unwrap().to_string_lossy(),
            agentbus_install::file::BACKUP_INFIX
        );
        let mut found: Vec<PathBuf> = fs::read_dir(dir)
            .expect("cannot read the directory")
            .map(|entry| entry.expect("a directory entry").path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
            })
            .collect();
        found.sort();
        found.reverse();
        found
    }

    /// A copy of the binary somewhere else on this machine, as a user who moved
    /// it would have.
    fn moved_binary(&self) -> PathBuf {
        self.binary_at("elsewhere")
    }

    /// A copy of the binary in a directory of this machine called `dir`.
    ///
    /// The name is the caller's to choose, because what a generated file has to
    /// survive is whatever a user called the directory they keep this in.
    fn binary_at(&self, dir: &str) -> PathBuf {
        let elsewhere = self._root.path().join(dir);
        fs::create_dir_all(&elsewhere).unwrap();
        let copy = elsewhere.join("agentbus");
        fs::copy(env!("CARGO_BIN_EXE_agentbus"), &copy).unwrap();
        copy
    }

    /// Everything the stand-in was asked to do, one command line per entry.
    fn asked(&self) -> Vec<String> {
        fs::read_to_string(self.world.join("argv"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Only the commands that change something, which are the ones an
    /// installation is answerable for.
    fn asked_to_change(&self) -> Vec<String> {
        self.asked()
            .into_iter()
            .filter(|line| !line.ends_with("--json"))
            .collect()
    }
}

/// What a run printed on stdout, once it has been checked for having succeeded.
fn succeeds(output: &Output) -> &str {
    assert!(output.status.success(), "{output:?}");
    std::str::from_utf8(&output.stdout).expect("output is not UTF-8")
}

/// Whether anything at all was written below `path`.
fn is_untouched(path: &Path) -> bool {
    !path.exists()
}

#[test]
fn a_machine_with_no_agent_on_it_is_told_so() {
    let machine = Machine::new();

    let report = machine.report(&["install"]);

    assert_eq!(
        report,
        "no coding agent found on this machine\n\
         nothing to install: this build only handles claude, codex and opencode\n"
    );
}

#[test]
fn an_agent_is_found_by_its_configuration_directory() {
    let machine = Machine::new().configured("claude").with_claude();

    let report = machine.report(&["install"]);

    assert!(
        report.starts_with("found claude (configuration directory"),
        "{report}"
    );
    assert!(report.contains(".claude,"), "{report}");
}

#[test]
fn an_agent_is_found_by_its_command() {
    let machine = Machine::new().installed("codex");

    let report = machine.report(&["install"]);

    assert!(report.starts_with("found codex (command "), "{report}");
}

#[test]
fn every_agent_on_the_machine_is_reported() {
    let machine = Machine::new()
        .configured("claude")
        .installed("codex")
        .configured("opencode")
        .installed("opencode")
        .with_claude();

    let report = machine.report(&["uninstall"]);

    assert!(report.contains("found claude"), "{report}");
    assert!(report.contains("found codex"), "{report}");
    assert!(
        report.contains("found opencode (configuration directory"),
        "{report}"
    );
}

#[test]
fn a_run_that_has_nothing_to_do_writes_nothing() {
    let machine = Machine::new();

    machine.run(&["install"]);
    machine.run(&["install", "--dry-run"]);
    machine.run(&["uninstall"]);

    assert!(
        is_untouched(&machine.state),
        "a run with nothing to do left something behind"
    );
}

#[test]
fn a_dry_run_says_what_it_would_do_and_does_none_of_it() {
    let machine = Machine::new().with_claude();

    let report = machine.report(&["install", "--dry-run"]);

    assert!(
        report.contains(&format!("would create {}", machine.hooks().display())),
        "{report}"
    );
    assert!(
        report.contains("would run claude plugin install agentbus@agentbus -s user"),
        "{report}"
    );
    assert!(
        is_untouched(&machine.marketplace()),
        "a dry run wrote files"
    );
    assert!(is_untouched(&machine.state), "a dry run wrote a record");
    assert_eq!(
        machine.asked_to_change(),
        Vec::<String>::new(),
        "a dry run had the agent change something"
    );
}

#[test]
fn an_agent_can_be_named_and_several_can_be_named_at_once() {
    let machine = Machine::new().with_claude();

    let one = machine.report(&["install", "--dry-run", "--agent", "codex"]);
    let both = machine.report(&[
        "install",
        "--dry-run",
        "--agent",
        "claude",
        "--agent",
        "codex",
    ]);

    // The agent named is acted on whether or not it was found; the one not
    // named is left out even though it was.
    assert!(one.contains("\ncodex\n"), "{one}");
    assert!(!one.contains("\nclaude\n"), "{one}");
    assert!(
        both.contains("\nclaude\n") && both.contains("\ncodex\n"),
        "{both}"
    );
}

#[test]
fn a_name_that_is_not_an_agent_is_refused_with_the_names_that_are() {
    let machine = Machine::new();

    let output = machine.run(&["install", "--agent", "emacs"]);

    assert!(!output.status.success(), "{output:?}");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(complaint.contains("emacs"), "{complaint}");
    assert!(complaint.contains("claude"), "{complaint}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn a_run_with_nowhere_to_look_says_which_variable_is_missing() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentbus"))
        .arg("uninstall")
        .env_remove("HOME")
        .env("PATH", "")
        .output()
        .expect("cannot run agentbus");

    assert!(!output.status.success(), "{output:?}");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(
        complaint.starts_with("agentbus uninstall: HOME"),
        "{complaint}"
    );
}

#[test]
fn installing_generates_the_marketplace_and_has_the_agent_install_from_it() {
    let machine = Machine::new().with_claude();

    let report = machine.report(&["install", "--agent", "claude"]);

    let marketplace = machine.marketplace();
    for name in [
        ".claude-plugin/marketplace.json",
        "agentbus/.claude-plugin/plugin.json",
        "agentbus/hooks/hooks.json",
    ] {
        let path = marketplace.join(name);
        assert!(path.is_file(), "{} was not written", path.display());
        assert!(
            report.contains(&format!("created {}", path.display())),
            "{report}"
        );
    }
    assert_eq!(
        machine.asked_to_change(),
        vec![
            format!("plugin marketplace add {}", marketplace.display()),
            "plugin install agentbus@agentbus -s user".to_owned(),
        ]
    );
}

#[test]
fn every_hook_runs_the_binary_by_an_absolute_path_and_never_waits_for_it() {
    let machine = Machine::new().with_claude();

    machine.report(&["install", "--agent", "claude"]);

    let hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(machine.hooks()).unwrap()).unwrap();
    let events = hooks["hooks"]
        .as_object()
        .expect("no hooks were registered");
    assert_eq!(
        events.keys().map(String::as_str).collect::<Vec<&str>>(),
        [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "Notification",
            "Stop",
            "StopFailure",
            "SubagentStart",
            "SubagentStop",
            "PreCompact",
            "SessionEnd",
        ]
    );
    for (event, matchers) in events {
        for entry in matchers.as_array().expect(event) {
            for hook in entry["hooks"].as_array().expect(event) {
                let command = hook["command"].as_str().expect(event);
                assert!(command.starts_with('/'), "{event}: {command}");
                assert!(
                    command.ends_with(" emit --agent claude"),
                    "{event}: {command}"
                );
                assert_eq!(hook["async"], serde_json::Value::Bool(true), "{event}");
            }
        }
    }
    assert_eq!(
        events["PreToolUse"][0]["matcher"],
        serde_json::Value::from("*")
    );
}

#[test]
fn installing_twice_changes_nothing_the_second_time() {
    let machine = Machine::new().with_claude();
    machine.report(&["install", "--agent", "claude"]);
    let first = machine.asked_to_change();

    let report = machine.report(&["install", "--agent", "claude"]);

    assert!(report.contains("  already installed\n"), "{report}");
    assert!(
        report.contains("already run claude plugin install agentbus@agentbus -s user"),
        "{report}"
    );
    assert_eq!(
        machine.asked_to_change(),
        first,
        "the second run had the agent do something again"
    );
}

#[test]
fn a_binary_that_moved_has_the_agent_take_the_plugin_again() {
    let machine = Machine::new().with_claude();
    machine.report(&["install", "--agent", "claude"]);
    let before = machine.asked_to_change().len();
    let moved = machine.moved_binary();

    // The version has not changed, so the agent would keep serving the copy it
    // already took — which names a binary that is no longer where it was.
    let output = machine.run_binary(&moved, &["install", "--agent", "claude"]);
    let report = succeeds(&output);

    assert!(
        report.contains(&format!("updated {}", machine.hooks().display())),
        "{report}"
    );
    assert_eq!(
        machine.asked_to_change()[before..],
        [
            "plugin uninstall agentbus@agentbus -s user".to_owned(),
            "plugin install agentbus@agentbus -s user".to_owned(),
        ],
        "a stale copy was not taken again"
    );
    let hooks = fs::read_to_string(machine.hooks()).unwrap();
    assert!(
        hooks.contains(&moved.display().to_string()),
        "the hooks do not name the binary that wrote them: {hooks}"
    );
}

#[test]
fn uninstalling_takes_the_plugin_back_out_and_leaves_nothing_behind() {
    let machine = Machine::new().with_claude();
    machine.report(&["install", "--agent", "claude"]);
    let installed = machine.asked_to_change().len();

    let report = machine.report(&["uninstall", "--agent", "claude"]);

    assert_eq!(
        machine.asked_to_change()[installed..],
        [
            "plugin uninstall agentbus@agentbus -s user".to_owned(),
            "plugin marketplace remove agentbus".to_owned(),
        ]
    );
    assert!(
        report.contains(&format!("removed {}", machine.marketplace().display())),
        "{report}"
    );
    assert!(
        !machine.marketplace().exists(),
        "{} was left behind",
        machine.marketplace().display()
    );
    assert!(
        !machine.home.join(".local/share/agentbus").exists(),
        "the directory the marketplace was in was left behind"
    );
}

#[test]
fn uninstalling_what_was_never_installed_changes_nothing() {
    let machine = Machine::new().with_claude();

    let report = machine.report(&["uninstall", "--agent", "claude"]);

    assert!(report.contains("  nothing of ours is there\n"), "{report}");
    assert_eq!(machine.asked_to_change(), Vec::<String>::new());
    assert!(is_untouched(&machine.state));
}

#[test]
fn the_agents_own_settings_file_is_never_touched() {
    let machine = Machine::new().configured("claude").with_claude();
    let settings = machine.home.join(".claude/settings.json");
    let before = "{\n  \"model\": \"whatever the user chose\"\n}\n";
    fs::write(&settings, before).unwrap();

    machine.report(&["install", "--agent", "claude"]);
    machine.report(&["uninstall", "--agent", "claude"]);

    assert_eq!(fs::read_to_string(&settings).unwrap(), before);
}

#[test]
fn an_agent_that_cannot_be_run_leaves_the_files_and_says_what_to_run() {
    let machine = Machine::new().configured("claude");

    let output = machine.run(&["install", "--agent", "claude"]);

    assert!(!output.status.success(), "{output:?}");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(
        complaint.contains("claude plugin marketplace add"),
        "{complaint}"
    );
    assert!(complaint.contains("by hand"), "{complaint}");
    assert!(
        machine.hooks().is_file(),
        "the files a user was told to register were not written"
    );
    assert!(
        !machine.home.join(".claude/settings.json").exists(),
        "a failed run wrote into the agent's own settings"
    );
}

#[test]
fn a_run_that_stopped_partway_still_remembers_what_it_wrote() {
    let machine = Machine::new().configured("claude");
    machine.run(&["install", "--agent", "claude"]);

    let record = fs::read_to_string(machine.state.join("agentbus/installed.json"))
        .expect("nothing was remembered about a run that wrote files");

    assert!(
        record.contains("hooks.json"),
        "a file that was written is not in the record: {record}"
    );
}

#[test]
fn installing_for_codex_drops_a_file_in_where_there_was_none() {
    let machine = Machine::new().installed("codex");

    let report = machine.report(&["install", "--agent", "codex"]);

    let path = machine.codex_hooks();
    assert!(
        report.contains(&format!("created {}", path.display())),
        "{report}"
    );
    let events = machine.document(&path)["hooks"]
        .as_object()
        .expect("no hooks were registered")
        .clone();
    assert_eq!(
        events.keys().map(String::as_str).collect::<Vec<&str>>(),
        [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "PostToolUse",
            "SubagentStart",
            "SubagentStop",
            "PreCompact",
            "PostCompact",
            "Stop",
            "SessionEnd",
        ]
    );
    for (event, entries) in &events {
        let entries = entries.as_array().expect(event);
        assert_eq!(entries.len(), 1, "{event}");
        // Marked, because that is the whole of how an upgrade and an uninstall
        // later find this program's own work in somebody else's file.
        assert!(
            agentbus_install::sentinel::is_marked(&entries[0]),
            "{event} was left unmarked"
        );
        for hook in entries[0]["hooks"].as_array().expect(event) {
            let command = hook["command"].as_str().expect(event);
            assert!(command.starts_with('/'), "{event}: {command}");
            assert!(
                command.ends_with(" emit --agent codex"),
                "{event}: {command}"
            );
            assert_eq!(hook["async"], serde_json::Value::Bool(true), "{event}");
            assert_eq!(hook["timeout"], serde_json::Value::from(5), "{event}");
        }
    }
    for matched in ["PreToolUse", "PermissionRequest", "PostToolUse"] {
        assert_eq!(
            events[matched][0]["matcher"],
            serde_json::Value::from("*"),
            "{matched}"
        );
    }
    // A file this program wrote from nothing is not a merge, so nothing was
    // there to copy.
    assert!(machine.backups_of(&path).is_empty());
}

#[test]
fn installing_for_codex_twice_changes_nothing_the_second_time() {
    let machine = Machine::new().installed("codex");
    machine.report(&["install", "--agent", "codex"]);
    let path = machine.codex_hooks();
    let after_one = fs::read_to_string(&path).unwrap();

    let report = machine.report(&["install", "--agent", "codex"]);

    assert!(report.contains("  already installed\n"), "{report}");
    assert!(
        report.contains(&format!("unchanged {}", path.display())),
        "{report}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), after_one);
    assert!(
        machine.backups_of(&path).is_empty(),
        "a run that changed nothing still copied the file"
    );
}

#[test]
fn what_the_user_had_in_their_codex_hooks_survives_being_installed_around() {
    let machine = Machine::new().configured("codex");
    let path = machine.codex_hooks();
    let theirs = serde_json::json!({
        "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "notify-send done"}]}]
        },
        "somethingElse": {"they": "set"}
    });
    fs::write(&path, format!("{theirs:#}\n")).unwrap();

    machine.report(&["install", "--agent", "codex"]);

    let merged = machine.document(&path);
    assert_eq!(merged["somethingElse"], theirs["somethingElse"]);
    assert_eq!(merged["hooks"]["Stop"][0], theirs["hooks"]["Stop"][0]);
    assert_eq!(
        merged["hooks"]["Stop"].as_array().unwrap().len(),
        2,
        "ours was not added beside theirs"
    );
    assert_eq!(
        machine.backups_of(&path).len(),
        1,
        "the file was changed without a copy being taken first"
    );

    machine.report(&["uninstall", "--agent", "codex"]);

    assert_eq!(
        machine.document(&path),
        theirs,
        "the user's own file did not come back as it went in"
    );
}

#[test]
fn a_codex_hooks_file_that_cannot_be_rewritten_is_left_exactly_as_it_was() {
    let machine = Machine::new().configured("codex");
    let path = machine.codex_hooks();
    // Reads perfectly well, and writing it back out would silently drop the
    // first of the two.
    let theirs = "{\n  \"hooks\": {},\n  \"hooks\": {\"Stop\": []}\n}\n";
    fs::write(&path, theirs).unwrap();

    let output = machine.run(&["install", "--agent", "codex"]);

    assert!(!output.status.success(), "{output:?}");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(
        complaint.contains(&path.display().to_string()),
        "{complaint}"
    );
    assert!(complaint.contains("left as it was"), "{complaint}");
    assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
    assert!(machine.backups_of(&path).is_empty());
}

#[test]
fn uninstalling_for_codex_takes_away_the_file_it_dropped_in() {
    let machine = Machine::new().installed("codex");
    machine.report(&["install", "--agent", "codex"]);
    let path = machine.codex_hooks();

    let report = machine.report(&["uninstall", "--agent", "codex"]);

    assert!(
        report.contains(&format!("removed {}", path.display())),
        "{report}"
    );
    assert!(!path.exists(), "{} was left behind", path.display());
    assert!(
        machine.backups_of(&path).is_empty(),
        "copies of a file this program made were left behind"
    );
    assert!(
        machine.home.join(".codex").is_dir(),
        "the agent's own configuration directory was removed"
    );
}

#[test]
fn uninstalling_for_codex_when_nothing_was_installed_changes_nothing() {
    let machine = Machine::new().installed("codex");

    let report = machine.report(&["uninstall", "--agent", "codex"]);

    assert!(report.contains("  nothing of ours is there\n"), "{report}");
    assert!(!machine.codex_hooks().exists());
    assert!(is_untouched(&machine.state));
}

/// Whether `script` is JavaScript a parser accepts.
///
/// `None` where there is no parser to hand: a machine without one still runs
/// every other assertion about the file, rather than reporting a check it never
/// made as having passed.
fn parses(script: &Path) -> Option<bool> {
    match Command::new("node").arg("--check").arg(script).output() {
        Ok(output) => Some(output.status.success()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("cannot run node: {error}"),
    }
}

#[test]
fn installing_for_opencode_drops_a_plugin_in_where_there_was_none() {
    let machine = Machine::new().installed("opencode");

    let report = machine.report(&["install", "--agent", "opencode"]);

    let path = machine.opencode_plugin();
    assert!(
        report.contains(&format!(
            "created {}",
            machine.opencode_plugin_dir().display()
        )),
        "{report}"
    );
    assert!(
        report.contains(&format!("created {}", path.display())),
        "{report}"
    );
    let script = fs::read_to_string(&path).expect("no plugin was written");
    // Marked, because that is the whole of how an upgrade and an uninstall later
    // tell this program's own file from one that merely shares its name.
    assert!(
        agentbus_install::sentinel::is_generated(&script),
        "the plugin was left unmarked: {script}"
    );
    let quoted = format!("\"{}\"", env!("CARGO_BIN_EXE_agentbus"));
    assert!(
        script.contains(&quoted),
        "the plugin does not name the binary that wrote it: {script}"
    );
    assert!(
        script.contains("emit --agent opencode"),
        "the plugin does not emit: {script}"
    );
    assert!(!script.contains('@'), "a placeholder was left in: {script}");
}

#[test]
fn the_plugin_opencode_is_given_is_javascript_that_parses() {
    let machine = Machine::new().installed("opencode");
    machine.report(&["install", "--agent", "opencode"]);

    let path = machine.opencode_plugin();

    match parses(&path) {
        Some(parsed) => assert!(parsed, "{} is not valid JavaScript", path.display()),
        // Nothing to run it with here. The assertions above already pin that the
        // substitution happened and that nothing of the template is left.
        None => assert!(path.is_file()),
    }
}

#[test]
fn a_path_that_needs_escaping_still_gives_opencode_javascript_that_parses() {
    let machine = Machine::new().installed("opencode");
    let awkward = machine.binary_at("a \"quoted\" name");
    machine.run_binary(&awkward, &["install", "--agent", "opencode"]);

    let script = fs::read_to_string(machine.opencode_plugin()).expect("no plugin was written");

    assert!(
        script.contains(r#"\"quoted\""#),
        "the path was not escaped: {script}"
    );
    if let Some(parsed) = parses(&machine.opencode_plugin()) {
        assert!(parsed, "an escaped path did not survive: {script}");
    }
}

#[test]
fn installing_for_opencode_twice_changes_nothing_the_second_time() {
    let machine = Machine::new().installed("opencode");
    machine.report(&["install", "--agent", "opencode"]);
    let path = machine.opencode_plugin();
    let after_one = fs::read_to_string(&path).unwrap();

    let report = machine.report(&["install", "--agent", "opencode"]);

    assert!(report.contains("  already installed\n"), "{report}");
    assert!(
        report.contains(&format!("unchanged {}", path.display())),
        "{report}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), after_one);
    assert!(
        machine.backups_of(&path).is_empty(),
        "a run that changed nothing still copied the file"
    );
}

#[test]
fn a_binary_that_moved_is_written_into_opencodes_plugin_again() {
    let machine = Machine::new().installed("opencode");
    machine.report(&["install", "--agent", "opencode"]);
    let moved = machine.moved_binary();

    let output = machine.run_binary(&moved, &["install", "--agent", "opencode"]);
    let report = succeeds(&output);

    let path = machine.opencode_plugin();
    assert!(
        report.contains(&format!("updated {}", path.display())),
        "{report}"
    );
    let script = fs::read_to_string(&path).unwrap();
    assert!(
        script.contains(&moved.display().to_string()),
        "the plugin does not name the binary that wrote it: {script}"
    );
}

#[test]
fn a_plugin_of_the_users_own_with_the_same_name_is_left_exactly_as_it_was() {
    let machine = Machine::new().configured("opencode");
    let path = machine.opencode_plugin();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let theirs = "// a plugin I wrote myself, which happens to share a name\n";
    fs::write(&path, theirs).unwrap();

    let output = machine.run(&["install", "--agent", "opencode"]);

    assert!(!output.status.success(), "{output:?}");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(
        complaint.contains(&path.display().to_string()),
        "{complaint}"
    );
    assert!(
        complaint.contains("not written by this program"),
        "{complaint}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
    assert!(machine.backups_of(&path).is_empty());
}

#[test]
fn uninstalling_for_opencode_takes_away_the_plugin_and_the_directory_it_made() {
    let machine = Machine::new().installed("opencode");
    machine.report(&["install", "--agent", "opencode"]);
    let path = machine.opencode_plugin();

    let report = machine.report(&["uninstall", "--agent", "opencode"]);

    assert!(
        report.contains(&format!("removed {}", path.display())),
        "{report}"
    );
    assert!(!path.exists(), "{} was left behind", path.display());
    // Nothing of this program's is left, down to the directory it made — which
    // is also why there is nowhere for a copy of the file to have survived.
    assert!(
        !machine.opencode_plugin_dir().exists(),
        "the directory this program made was left behind"
    );
    assert!(
        machine.home.join(".config/opencode").is_dir(),
        "the agent's own configuration directory was removed"
    );
}

#[test]
fn uninstalling_for_opencode_leaves_a_plugin_directory_that_was_already_there() {
    let machine = Machine::new().configured("opencode");
    let dir = machine.opencode_plugin_dir();
    fs::create_dir_all(&dir).unwrap();
    machine.report(&["install", "--agent", "opencode"]);

    machine.report(&["uninstall", "--agent", "opencode"]);

    assert!(!machine.opencode_plugin().exists());
    assert!(
        dir.is_dir(),
        "a directory this program did not make was removed"
    );
}

#[test]
fn uninstalling_for_opencode_when_nothing_was_installed_changes_nothing() {
    let machine = Machine::new().installed("opencode");

    let report = machine.report(&["uninstall", "--agent", "opencode"]);

    assert!(report.contains("  nothing of ours is there\n"), "{report}");
    assert!(!machine.opencode_plugin().exists());
    assert!(is_untouched(&machine.state));
}

#[test]
fn a_plugin_of_the_users_own_survives_an_uninstall() {
    let machine = Machine::new().configured("opencode");
    let path = machine.opencode_plugin();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let theirs = "// mine\n";
    fs::write(&path, theirs).unwrap();

    machine.report(&["uninstall", "--agent", "opencode"]);

    assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
}

#[test]
fn an_agent_this_build_has_no_installer_for_is_refused_when_it_is_named() {
    let machine = Machine::new();

    let output = machine.run(&["install", "--agent", "devin"]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "naming an agent nothing can be done for is a mistake in the command \
         line, not a failure to install"
    );
    let said = String::from_utf8(output.stderr).expect("output is not UTF-8");
    assert!(said.contains("cannot install devin yet"), "{said}");
    assert!(said.contains("claude, codex and opencode"), "{said}");
    assert!(is_untouched(&machine.state));
}

#[test]
fn an_agent_this_build_has_no_installer_for_is_refused_by_the_uninstall_too() {
    let machine = Machine::new();

    let output = machine.run(&["uninstall", "--agent", "devin"]);

    assert_eq!(output.status.code(), Some(2));
    let said = String::from_utf8(output.stderr).expect("output is not UTF-8");
    assert!(said.contains("cannot uninstall devin yet"), "{said}");
}

#[test]
fn a_name_that_is_no_agent_at_all_is_refused_with_every_agent_there_is() {
    let machine = Machine::new();

    let output = machine.run(&["install", "--agent", "nonesuch"]);

    assert!(!output.status.success(), "{output:?}");
    let said = String::from_utf8(output.stderr).expect("output is not UTF-8");
    assert!(said.contains("nonesuch"), "{said}");
    for agent in ["claude", "cursor", "github-copilot", "qwen"] {
        assert!(said.contains(agent), "{said} omits {agent}");
    }
}

#[test]
fn an_agent_this_build_has_no_installer_for_is_reported_and_passed_over() {
    let machine = Machine::new().installed("devin");

    let report = machine.report(&["install"]);

    assert!(report.contains("found devin"), "{report}");
    assert!(
        report.contains("nothing to install: this build only handles claude, codex and opencode\n"),
        "{report}"
    );
    assert!(is_untouched(&machine.state));
}

#[test]
fn an_agent_run_by_a_name_that_is_not_its_own_is_found_by_that_name() {
    let machine = Machine::new().installed("cursor-agent");

    let report = machine.report(&["install"]);

    assert!(report.contains("found cursor (command "), "{report}");
}
