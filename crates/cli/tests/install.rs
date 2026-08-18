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
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        Command::new(binary)
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", &self.bin)
            .env("XDG_STATE_HOME", &self.state)
            .env(WORLD_VAR, &self.world)
            .output()
            .expect("cannot run agentbus")
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

    /// A copy of the binary somewhere else on this machine, as a user who moved
    /// it would have.
    fn moved_binary(&self) -> PathBuf {
        let elsewhere = self._root.path().join("elsewhere");
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
         nothing to install: this build only handles claude\n"
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

    let report = machine.report(&["install", "--agent", "claude", "--agent", "codex"]);

    assert!(
        report.contains("would create") || report.contains("created"),
        "{report}"
    );
    assert!(!report.contains("codex\n"), "{report}");
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
