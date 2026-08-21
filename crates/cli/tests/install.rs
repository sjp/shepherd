//! `agentbus install` and `agentbus uninstall` against a described machine.
//!
//! These run the real binary with `HOME` and `PATH` pointed into a temporary
//! directory, so what they exercise is the whole path a user's invocation takes:
//! the argument parser, the detection rules reading the environment this process
//! set up, the files written, the agent's own command line being run, and the
//! report.
//!
//! One agent's command line is a stand-in, because one of them still has to be
//! run: an installation made the way an earlier build made them is registered
//! inside Claude rather than on disk, and clearing that away means asking Claude
//! about it and then telling it. The stand-in records what it was asked and
//! answers from a world of its own, so a test can describe a machine that
//! carries such an installation and a machine that never did, and see that only
//! one of them is asked anything at all.

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
/// It answers `plugin list` and `plugin marketplace list` from what it has been
/// told, and forgets what it is asked to forget. Nothing else is needed of it:
/// installing no longer goes through Claude at all, and what is left is the
/// retirement of what did.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
set -eu
# The machine's search path holds only the agents being described, so that
# detection cannot find a real one; this needs the ordinary tools as well.
PATH="/bin:/usr/bin:$PATH"
printf '%s\n' "$*" >> "$AGENTBUS_TEST_CLAUDE_WORLD/argv"
marketplace="$AGENTBUS_TEST_CLAUDE_WORLD/marketplace"
installed="$AGENTBUS_TEST_CLAUDE_WORLD/installed"

case "$1 $2 ${3-}" in
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
"plugin uninstall")
	rm -f "$installed"
	exit 0
	;;
"plugin list")
	if [ -f "$installed" ]; then
		printf '[{"id":"agentbus@agentbus","version":"0.1.0","installPath":"%s"}]' "$(cat "$installed")"
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

    /// Puts on the machine what an installation made the way an earlier build
    /// made them left behind: the generated marketplace, and a Claude that says
    /// it has the plugin from it installed.
    fn with_the_old_install(self) -> Self {
        let root = self.marketplace();
        for name in [
            ".claude-plugin/marketplace.json",
            "agentbus/.claude-plugin/plugin.json",
            "agentbus/hooks/hooks.json",
        ] {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "{}\n").unwrap();
        }
        fs::write(self.world.join("marketplace"), root.to_str().unwrap()).unwrap();
        fs::write(
            self.world.join("installed"),
            root.join("agentbus").to_str().unwrap(),
        )
        .unwrap();
        self
    }

    /// Where an earlier build generated its marketplace.
    fn marketplace(&self) -> PathBuf {
        self.home.join(".local/share/agentbus/claude-marketplace")
    }

    /// The wrapper Claude is told to run.
    fn claude_wrapper(&self) -> PathBuf {
        self.home.join(".claude/hooks/agentbus.sh")
    }

    /// The settings file the entry that runs it goes into.
    fn claude_settings(&self) -> PathBuf {
        self.home.join(".claude/settings.json")
    }

    /// The wrapper Codex is told to run.
    fn codex_wrapper(&self) -> PathBuf {
        self.home.join(".codex/hooks/agentbus.sh")
    }

    /// Where Codex's hooks are dropped in.
    fn codex_hooks(&self) -> PathBuf {
        self.home.join(".codex/hooks.json")
    }

    /// The file holding the setting that makes Codex read them at all.
    fn codex_config(&self) -> PathBuf {
        self.home.join(".codex/config.toml")
    }

    /// The directory OpenCode loads plugins from.
    fn opencode_plugin_dir(&self) -> PathBuf {
        self.home.join(".config/opencode/plugin")
    }

    /// Where OpenCode's plugin is dropped in.
    fn opencode_plugin(&self) -> PathBuf {
        self.opencode_plugin_dir().join("agentbus.js")
    }

    /// Where the plugin OpenCode's terminal interface loads by name is written.
    fn opencode_tui_plugin(&self) -> PathBuf {
        self.home.join(".config/opencode/agentbus-tui.js")
    }

    /// The file that terminal interface reads its own settings from.
    fn opencode_tui_config(&self) -> PathBuf {
        self.home.join(".config/opencode/tui.jsonc")
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
         nothing to install: this build only handles antigravity, claude, codex, \
         cursor, devin, droid, github-copilot, grok, mastracode, opencode, qodercli \
         and qwen\n"
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

    for path in [machine.claude_wrapper(), machine.claude_settings()] {
        assert!(
            report.contains(&format!("would create {}", path.display())),
            "{report}"
        );
    }
    assert!(
        is_untouched(&machine.home.join(".claude")),
        "a dry run wrote files"
    );
    assert!(is_untouched(&machine.state), "a dry run wrote a record");
    assert_eq!(
        machine.asked(),
        Vec::<String>::new(),
        "a dry run went to the agent's own command line"
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
fn installing_drops_a_wrapper_in_and_points_the_settings_at_it() {
    let machine = Machine::new().with_claude();

    let report = machine.report(&["install", "--agent", "claude"]);

    let wrapper = machine.claude_wrapper();
    assert!(
        report.contains(&format!("created {}", wrapper.display())),
        "{report}"
    );
    let script = fs::read_to_string(&wrapper).expect("no wrapper was written");
    // Marked, because that is the whole of how an upgrade and an uninstall later
    // tell this program's own file from one that merely shares its name.
    assert!(
        agentbus_install::sentinel::is_generated(&script),
        "the wrapper was left unmarked: {script}"
    );
    assert!(
        script.contains(&format!("'{}'", env!("CARGO_BIN_EXE_agentbus"))),
        "the wrapper does not name the binary that wrote it: {script}"
    );
    assert!(script.contains("emit --agent claude"), "{script}");
    assert!(!script.contains('@'), "a placeholder was left in: {script}");
    // A script the agent is told to run is a script the machine has to let it
    // run.
    assert_eq!(
        fs::metadata(&wrapper).unwrap().permissions().mode() & 0o777,
        0o755
    );

    let settings = machine.claude_settings();
    assert!(
        report.contains(&format!("created {}", settings.display())),
        "{report}"
    );
    let entries = machine.document(&settings)["hooks"]["SessionStart"]
        .as_array()
        .cloned()
        .expect("no entry was written");
    assert_eq!(entries.len(), 1);
    assert!(
        agentbus_install::sentinel::is_marked(&entries[0]),
        "the entry was left unmarked"
    );
    assert_eq!(entries[0]["matcher"], serde_json::Value::from("*"));

    // Nothing on this machine was ever installed the old way, so the agent's
    // own command line was never troubled.
    assert_eq!(machine.asked(), Vec::<String>::new());
}

#[test]
fn the_entry_runs_the_wrapper_by_an_absolute_path_and_never_waits_for_it() {
    let machine = Machine::new().with_claude();

    machine.report(&["install", "--agent", "claude"]);

    let settings = machine.document(&machine.claude_settings());
    let events = settings["hooks"]
        .as_object()
        .expect("no hooks were registered");
    // One event, and it is the one that says which session this is. What every
    // other event means is worked out from the payload this one's hook forwards.
    assert_eq!(
        events.keys().map(String::as_str).collect::<Vec<&str>>(),
        ["SessionStart"]
    );
    for hook in events["SessionStart"][0]["hooks"]
        .as_array()
        .expect("no hook was written")
    {
        assert_eq!(hook["type"], serde_json::Value::from("command"));
        assert_eq!(
            hook["command"],
            serde_json::Value::from(format!("bash '{}'", machine.claude_wrapper().display()))
        );
        assert_eq!(hook["async"], serde_json::Value::Bool(true));
        assert_eq!(hook["timeout"], serde_json::Value::from(5));
    }
}

#[test]
fn installing_twice_changes_nothing_the_second_time() {
    let machine = Machine::new().with_claude();
    machine.report(&["install", "--agent", "claude"]);
    let (wrapper, settings) = (machine.claude_wrapper(), machine.claude_settings());
    let after_one = (
        fs::read_to_string(&wrapper).unwrap(),
        fs::read_to_string(&settings).unwrap(),
    );

    let report = machine.report(&["install", "--agent", "claude"]);

    assert!(report.contains("  already installed\n"), "{report}");
    for path in [&wrapper, &settings] {
        assert!(
            report.contains(&format!("unchanged {}", path.display())),
            "{report}"
        );
    }
    assert_eq!(
        (
            fs::read_to_string(&wrapper).unwrap(),
            fs::read_to_string(&settings).unwrap()
        ),
        after_one
    );
    assert!(
        machine.backups_of(&settings).is_empty(),
        "a run that changed nothing still copied the file"
    );
}

#[test]
fn a_binary_that_moved_is_written_into_the_wrapper_again() {
    let machine = Machine::new().with_claude();
    machine.report(&["install", "--agent", "claude"]);
    let moved = machine.moved_binary();

    let output = machine.run_binary(&moved, &["install", "--agent", "claude"]);
    let report = succeeds(&output);

    let wrapper = machine.claude_wrapper();
    assert!(
        report.contains(&format!("updated {}", wrapper.display())),
        "{report}"
    );
    let script = fs::read_to_string(&wrapper).unwrap();
    assert!(
        script.contains(&moved.display().to_string()),
        "the wrapper does not name the binary that wrote it: {script}"
    );
    // The entry names the wrapper, and the wrapper has not moved, so the file
    // the user maintains was not written again on account of it.
    assert!(
        report.contains(&format!(
            "unchanged {}",
            machine.claude_settings().display()
        )),
        "{report}"
    );
}

#[test]
fn uninstalling_takes_the_entry_and_the_wrapper_and_leaves_nothing_behind() {
    let machine = Machine::new().with_claude();
    machine.report(&["install", "--agent", "claude"]);

    let report = machine.report(&["uninstall", "--agent", "claude"]);

    let wrapper = machine.claude_wrapper();
    assert!(
        report.contains(&format!("removed {}", wrapper.display())),
        "{report}"
    );
    assert!(!wrapper.exists(), "{} was left behind", wrapper.display());
    assert!(
        !machine.claude_settings().exists(),
        "a settings file this program made was left behind"
    );
    // Nothing of this program's is left, down to the directories it made — and
    // the agent's own command line was never asked about any of it.
    assert!(
        !machine.home.join(".claude").exists(),
        "a directory this program made was left behind"
    );
    assert_eq!(machine.asked(), Vec::<String>::new());
}

#[test]
fn a_configuration_directory_the_user_already_had_survives_an_uninstall() {
    let machine = Machine::new().configured("claude").with_claude();
    machine.report(&["install", "--agent", "claude"]);

    machine.report(&["uninstall", "--agent", "claude"]);

    assert!(
        machine.home.join(".claude").is_dir(),
        "a directory this program did not make was removed"
    );
    assert!(!machine.home.join(".claude/hooks").exists());
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
fn what_the_user_had_in_their_settings_survives_being_installed_around() {
    let machine = Machine::new().configured("claude").with_claude();
    let settings = machine.claude_settings();
    // Written the way somebody keeps a file by hand: tabs, one hook of their
    // own, and the line endings of the machine they wrote it on.
    let theirs = "{\r\n\t\"model\": \"whatever the user chose\",\r\n\t\"hooks\": {\r\n\t\t\"Stop\": [\r\n\t\t\t{ \"hooks\": [{ \"type\": \"command\", \"command\": \"notify-send done\" }] }\r\n\t\t]\r\n\t}\r\n}\r\n";
    fs::write(&settings, theirs).unwrap();

    machine.report(&["install", "--agent", "claude"]);

    let after = fs::read_to_string(&settings).unwrap();
    assert!(
        after.starts_with("{\r\n\t\"model\": \"whatever the user chose\",\r\n"),
        "{after}"
    );
    assert!(after.contains("notify-send done"), "{after}");
    let merged = machine.document(&settings);
    assert_eq!(merged["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert_eq!(
        merged["hooks"]["SessionStart"].as_array().unwrap().len(),
        1,
        "ours was not added beside theirs"
    );
    assert_eq!(
        machine.backups_of(&settings).len(),
        1,
        "the file was changed without a copy being taken first"
    );

    machine.report(&["uninstall", "--agent", "claude"]);

    assert_eq!(
        fs::read_to_string(&settings).unwrap(),
        theirs,
        "the user's own file did not come back as it went in"
    );
}

#[test]
fn a_settings_file_that_cannot_be_rewritten_is_left_exactly_as_it_was() {
    let machine = Machine::new().configured("claude").with_claude();
    let settings = machine.claude_settings();
    // Reads perfectly well, and writing it back out would silently drop the
    // first of the two.
    let theirs = "{\n  \"hooks\": {},\n  \"hooks\": {\"SessionStart\": []}\n}\n";
    fs::write(&settings, theirs).unwrap();

    let output = machine.run(&["install", "--agent", "claude"]);

    assert!(!output.status.success(), "{output:?}");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(
        complaint.contains(&settings.display().to_string()),
        "{complaint}"
    );
    assert!(complaint.contains("left as it was"), "{complaint}");
    assert_eq!(fs::read_to_string(&settings).unwrap(), theirs);
    assert!(machine.backups_of(&settings).is_empty());
    assert!(
        !machine.claude_wrapper().exists(),
        "a plan that was refused still wrote a file"
    );
}

#[test]
fn an_installation_made_the_old_way_is_taken_away_when_the_new_one_goes_in() {
    let machine = Machine::new()
        .configured("claude")
        .with_claude()
        .with_the_old_install();

    let report = machine.report(&["install", "--agent", "claude"]);

    assert_eq!(
        machine.asked_to_change(),
        vec![
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
        machine.claude_wrapper().is_file(),
        "the installation that replaces it did not land"
    );
}

#[test]
fn an_installation_made_the_old_way_is_taken_away_by_an_uninstall_too() {
    let machine = Machine::new()
        .configured("claude")
        .with_claude()
        .with_the_old_install();

    machine.report(&["uninstall", "--agent", "claude"]);

    assert_eq!(
        machine.asked_to_change(),
        vec![
            "plugin uninstall agentbus@agentbus -s user".to_owned(),
            "plugin marketplace remove agentbus".to_owned(),
        ]
    );
    assert!(!machine.marketplace().exists());
    assert!(!machine.claude_wrapper().exists());
}

#[test]
fn an_installation_made_the_old_way_goes_even_where_the_agent_cannot_be_run() {
    // Its configuration directory is there and its command is not, which is what
    // a machine whose agent has been removed since looks like.
    let machine = Machine::new().configured("claude").with_the_old_install();

    let report = machine.report(&["uninstall", "--agent", "claude"]);

    // Nothing could be asked, so nothing is claimed to have been done about the
    // registration — and the files go regardless, which is the half of it this
    // program can do by itself.
    assert!(
        report.contains("already run claude plugin uninstall agentbus@agentbus -s user"),
        "{report}"
    );
    assert!(
        !machine.marketplace().exists(),
        "{} was left behind",
        machine.marketplace().display()
    );
}

#[test]
fn a_record_left_by_an_earlier_build_is_cleared_when_the_new_installation_goes_in() {
    let machine = Machine::new().configured("claude").with_claude();
    // A record in the shape an earlier build wrote it, naming a file of a
    // marketplace whose directory somebody has since deleted by hand — the one
    // trace nothing on disk could still find.
    let record = machine.state.join("agentbus/installed.json");
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    let stale = machine.marketplace().join("agentbus/hooks/hooks.json");
    fs::write(
        &record,
        format!(
            "{{\n  \"version\": 1,\n  \"files\": {{\n    \"{}\": {{\n      \"agent\": \"claude\",\n      \"ownership\": \"created\"\n    }}\n  }}\n}}\n",
            stale.display()
        ),
    )
    .unwrap();

    let report = machine.report(&["install", "--agent", "claude"]);

    assert!(
        report.contains(&format!("removed {}", stale.display())),
        "{report}"
    );
    let after = machine.document(&record);
    assert!(
        after["files"]
            .as_object()
            .expect("no files in the record")
            .keys()
            .all(|path| !path.contains("claude-marketplace")),
        "{after}"
    );
}

#[test]
fn uninstalling_a_machine_that_carries_both_installations_leaves_no_record_of_either() {
    let machine = Machine::new()
        .configured("claude")
        .with_claude()
        .with_the_old_install();
    machine.report(&["install", "--agent", "claude"]);

    machine.report(&["uninstall", "--agent", "claude"]);

    assert!(!machine.marketplace().exists());
    assert!(!machine.claude_wrapper().exists());
    assert!(!machine.claude_settings().exists());
    let record = machine.document(&machine.state.join("agentbus/installed.json"));
    assert_eq!(
        record["files"].as_object().map(serde_json::Map::len),
        Some(0),
        "a file this program wrote is still in the record: {record}"
    );
    assert_eq!(
        record["agents"].as_object().map(serde_json::Map::len),
        Some(0),
        "an agent with nothing installed is still remembered as having something: {record}"
    );
}

#[test]
fn a_run_that_stopped_partway_still_remembers_what_it_wrote() {
    let machine = Machine::new().with_claude().configured("opencode");
    // A plugin of the user's own under the name this program uses, which stops
    // the plan for the agent that comes after claude.
    let plugin = machine.opencode_plugin();
    fs::create_dir_all(plugin.parent().unwrap()).unwrap();
    fs::write(&plugin, "// mine\n").unwrap();

    let output = machine.run(&["install", "--agent", "claude", "--agent", "opencode"]);

    assert!(!output.status.success(), "{output:?}");
    let record = fs::read_to_string(machine.state.join("agentbus/installed.json"))
        .expect("nothing was remembered about a run that wrote files");

    assert!(
        record.contains("agentbus.sh"),
        "a file that was written is not in the record: {record}"
    );
}

#[test]
fn what_an_agent_had_installed_is_remembered_and_forgotten_again() {
    let machine = Machine::new().installed("codex");

    machine.report(&["install", "--agent", "codex"]);
    let record = machine.document(&machine.state.join("agentbus/installed.json"));

    let installed = record["agents"]["codex"]
        .as_object()
        .unwrap_or_else(|| panic!("nothing was remembered about the install: {record}"));
    assert_eq!(
        installed["assets"],
        serde_json::json!([
            machine.codex_wrapper().to_str().unwrap(),
            machine.codex_hooks().to_str().unwrap(),
            machine.codex_config().to_str().unwrap(),
        ]),
        "{record}"
    );
    assert_eq!(
        installed["version"],
        serde_json::Value::from(agentbus_install::expected_version(
            agentbus_install::Agent::Codex
        )),
        "{record}"
    );

    machine.report(&["uninstall", "--agent", "codex"]);
    let record = machine.document(&machine.state.join("agentbus/installed.json"));

    assert_eq!(
        record["agents"].as_object().map(serde_json::Map::len),
        Some(0),
        "an agent with nothing installed is still remembered as having something: {record}"
    );
}

#[test]
fn installing_for_codex_drops_a_wrapper_in_and_points_its_hooks_at_it() {
    let machine = Machine::new().installed("codex");

    let report = machine.report(&["install", "--agent", "codex"]);

    let wrapper = machine.codex_wrapper();
    assert!(
        report.contains(&format!("created {}", wrapper.display())),
        "{report}"
    );
    let script = fs::read_to_string(&wrapper).expect("no wrapper was written");
    // Marked, because that is the whole of how an upgrade and an uninstall later
    // tell this program's own file from one that merely shares its name.
    assert!(
        agentbus_install::sentinel::is_generated(&script),
        "the wrapper was left unmarked: {script}"
    );
    assert!(
        script.contains(&format!("'{}'", env!("CARGO_BIN_EXE_agentbus"))),
        "the wrapper does not name the binary that wrote it: {script}"
    );
    assert!(script.contains("emit --agent codex"), "{script}");
    assert!(!script.contains('@'), "a placeholder was left in: {script}");
    // A script the agent is told to run is a script the machine has to let it
    // run.
    assert_eq!(
        fs::metadata(&wrapper).unwrap().permissions().mode() & 0o777,
        0o755
    );

    let path = machine.codex_hooks();
    assert!(
        report.contains(&format!("created {}", path.display())),
        "{report}"
    );
    let events = machine.document(&path)["hooks"]
        .as_object()
        .expect("no hooks were registered")
        .clone();
    // One event, and it is the one that says which session this is. What every
    // other event means is worked out from the payload this one's hook forwards.
    assert_eq!(
        events.keys().map(String::as_str).collect::<Vec<&str>>(),
        ["SessionStart"]
    );
    let entries = events["SessionStart"].as_array().expect("no entry");
    assert_eq!(entries.len(), 1);
    assert!(
        agentbus_install::sentinel::is_marked(&entries[0]),
        "the entry was left unmarked"
    );
    // The event carries no tool name, so there is nothing for a matcher to say.
    assert_eq!(entries[0].get("matcher"), None, "{entries:?}");
    for hook in entries[0]["hooks"].as_array().expect("no hook was written") {
        assert_eq!(hook["type"], serde_json::Value::from("command"));
        assert_eq!(
            hook["command"],
            serde_json::Value::from(format!("bash '{}'", wrapper.display()))
        );
        assert_eq!(hook["async"], serde_json::Value::Bool(true));
        assert_eq!(hook["timeout"], serde_json::Value::from(5));
    }

    // And the setting without which the agent never reads that file at all.
    let config = machine.codex_config();
    assert!(
        report.contains(&format!("created {}", config.display())),
        "{report}"
    );
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        "[features]\nhooks = true\n"
    );
    // Files this program wrote from nothing are not merges, so nothing was
    // there to copy.
    assert!(machine.backups_of(&path).is_empty());
}

#[test]
fn installing_for_codex_twice_changes_nothing_the_second_time() {
    let machine = Machine::new().installed("codex");
    machine.report(&["install", "--agent", "codex"]);
    let files = [
        machine.codex_wrapper(),
        machine.codex_hooks(),
        machine.codex_config(),
    ];
    let after_one: Vec<String> = files
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();

    let report = machine.report(&["install", "--agent", "codex"]);

    assert!(report.contains("  already installed\n"), "{report}");
    for path in &files {
        assert!(
            report.contains(&format!("unchanged {}", path.display())),
            "{report}"
        );
        assert!(
            machine.backups_of(path).is_empty(),
            "a run that changed nothing still copied {}",
            path.display()
        );
    }
    let after_two: Vec<String> = files
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();
    assert_eq!(after_two, after_one);
}

#[test]
fn a_dry_run_for_codex_says_what_the_real_run_does_and_writes_none_of_it() {
    let machine = Machine::new().installed("codex");

    let planned = machine.report(&["install", "--dry-run", "--agent", "codex"]);

    assert!(!machine.codex_wrapper().exists());
    assert!(!machine.codex_hooks().exists());
    assert!(!machine.codex_config().exists());
    assert!(is_untouched(&machine.state));

    let done = machine.report(&["install", "--agent", "codex"]);

    assert_eq!(planned.replace("would create", "created"), done);
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
    assert_eq!(merged["hooks"]["Stop"], theirs["hooks"]["Stop"]);
    assert_eq!(
        merged["hooks"]["SessionStart"].as_array().unwrap().len(),
        1,
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
fn what_an_earlier_build_registered_for_codex_is_replaced_by_the_one_entry() {
    let machine = Machine::new().configured("codex");
    let path = machine.codex_hooks();
    // What installing used to write: an entry for every event the mapping reads,
    // each running the binary directly rather than a wrapper, all of them marked
    // as this program's.
    let mut events = serde_json::Map::new();
    for event in [
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
    ] {
        events.insert(
            event.to_owned(),
            serde_json::json!([{
                "_agentbus": {"v": 1},
                "hooks": [{
                    "type": "command",
                    "command": "/opt/bin/agentbus emit --agent codex",
                    "async": true,
                    "timeout": 5,
                }],
            }]),
        );
    }
    let old = serde_json::json!({ "hooks": serde_json::Value::Object(events) });
    fs::write(&path, format!("{old:#}\n")).unwrap();

    machine.report(&["install", "--agent", "codex"]);

    let events = machine.document(&path)["hooks"]
        .as_object()
        .expect("no hooks were registered")
        .clone();
    for (event, entries) in &events {
        let entries = entries.as_array().expect(event);
        match event.as_str() {
            "SessionStart" => assert_eq!(entries.len(), 1, "{event}"),
            _ => assert!(entries.is_empty(), "{event} still holds {entries:?}"),
        }
    }
    let command = events["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .expect("the entry runs nothing");
    assert_eq!(
        command,
        format!("bash '{}'", machine.codex_wrapper().display()),
        "the entry still runs the binary directly"
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
fn a_codex_config_that_cannot_be_read_a_line_at_a_time_stops_the_run() {
    let machine = Machine::new().configured("codex");
    let config = machine.codex_config();
    // Two sections of the same name: there is no saying which of them a setting
    // written here would belong to.
    let theirs = "[features]\nhooks = false\n\n[features]\nother = true\n";
    fs::write(&config, theirs).unwrap();

    let output = machine.run(&["install", "--agent", "codex"]);

    assert!(!output.status.success(), "{output:?}");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(
        complaint.contains(&config.display().to_string()),
        "{complaint}"
    );
    assert!(complaint.contains("left as it was"), "{complaint}");
    assert_eq!(fs::read_to_string(&config).unwrap(), theirs);
    // The whole run is worked out before any of it is carried out, so a file
    // refused at the end of the plan means nothing at the start of it was
    // written either.
    assert!(!machine.codex_wrapper().exists());
    assert!(!machine.codex_hooks().exists());
}

#[test]
fn what_the_user_had_in_their_codex_config_survives_being_installed_around() {
    let machine = Machine::new().configured("codex");
    let config = machine.codex_config();
    let theirs = "# mine\nmodel = \"something\"\n\n[features]\n# and this\nother = false\n";
    fs::write(&config, theirs).unwrap();

    machine.report(&["install", "--agent", "codex"]);

    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        "# mine\nmodel = \"something\"\n\n[features]\nhooks = true\n# and this\nother = false\n",
        "one line went in and something else changed with it"
    );

    machine.report(&["uninstall", "--agent", "codex"]);

    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        theirs,
        "the file did not come back as it went in"
    );
}

#[test]
fn a_setting_the_user_had_already_switched_on_is_left_switched_on() {
    let machine = Machine::new().configured("codex");
    let config = machine.codex_config();
    let theirs = "[features]\nhooks = true\n";
    fs::write(&config, theirs).unwrap();

    let report = machine.report(&["install", "--agent", "codex"]);

    assert!(
        report.contains(&format!("unchanged {}", config.display())),
        "{report}"
    );

    machine.report(&["uninstall", "--agent", "codex"]);

    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        theirs,
        "a setting this program never switched on was switched off"
    );
}

#[test]
fn uninstalling_for_codex_takes_away_everything_it_put_there() {
    let machine = Machine::new().installed("codex");
    machine.report(&["install", "--agent", "codex"]);

    let report = machine.report(&["uninstall", "--agent", "codex"]);

    for path in [
        machine.codex_wrapper(),
        machine.codex_hooks(),
        machine.codex_config(),
    ] {
        assert!(
            report.contains(&format!("removed {}", path.display())),
            "{report}"
        );
        assert!(!path.exists(), "{} was left behind", path.display());
    }
    // Nothing of this program's is left, down to the copies it takes before it
    // writes and the directories it made.
    assert!(
        !machine.home.join(".codex").exists(),
        "a directory this program made was left behind"
    );
}

#[test]
fn a_configuration_directory_codex_already_had_survives_an_uninstall() {
    let machine = Machine::new().configured("codex");
    machine.report(&["install", "--agent", "codex"]);

    machine.report(&["uninstall", "--agent", "codex"]);

    let dir = machine.home.join(".codex");
    assert!(
        dir.is_dir(),
        "the agent's own configuration directory was removed"
    );
    let left: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(
        left.is_empty(),
        "an uninstall left something behind: {left:?}"
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
fn installing_for_opencode_drops_both_plugins_in_where_there_were_none() {
    let machine = Machine::new().installed("opencode");

    let report = machine.report(&["install", "--agent", "opencode"]);

    assert!(
        report.contains(&format!(
            "created {}",
            machine.opencode_plugin_dir().display()
        )),
        "{report}"
    );
    for path in [machine.opencode_plugin(), machine.opencode_tui_plugin()] {
        assert!(
            report.contains(&format!("created {}", path.display())),
            "{report}"
        );
        let script = fs::read_to_string(&path).expect("no plugin was written");
        // Marked, because that is the whole of how an upgrade and an uninstall
        // later tell this program's own file from one that merely shares its
        // name.
        assert!(
            agentbus_install::sentinel::is_generated(&script),
            "{} was left unmarked: {script}",
            path.display()
        );
        let quoted = format!("\"{}\"", env!("CARGO_BIN_EXE_agentbus"));
        assert!(
            script.contains(&quoted),
            "{} does not name the binary that wrote it: {script}",
            path.display()
        );
        assert!(
            script.contains(r#""emit", "--agent", "opencode""#),
            "{} does not emit: {script}",
            path.display()
        );
        assert!(
            !script.contains('@'),
            "a placeholder was left in {}: {script}",
            path.display()
        );
    }
}

#[test]
fn installing_for_opencode_tells_the_terminal_interface_to_load_the_second_one() {
    let machine = Machine::new().installed("opencode");

    let report = machine.report(&["install", "--agent", "opencode"]);

    let config = machine.opencode_tui_config();
    assert!(
        report.contains(&format!("created {}", config.display())),
        "{report}"
    );
    // The entry names the file as its own reader resolves it, which is from the
    // directory the configuration file is in.
    assert_eq!(
        machine.document(&config)["plugin"],
        serde_json::json!(["./agentbus-tui.js"])
    );
    assert!(
        machine.opencode_tui_plugin().is_file(),
        "the interface was told to load a file that is not there"
    );
}

#[test]
fn the_plugins_opencode_is_given_are_javascript_that_parses() {
    let machine = Machine::new().installed("opencode");
    machine.report(&["install", "--agent", "opencode"]);

    for path in [machine.opencode_plugin(), machine.opencode_tui_plugin()] {
        match parses(&path) {
            Some(parsed) => assert!(parsed, "{} is not valid JavaScript", path.display()),
            // Nothing to run it with here. The assertions above already pin that
            // the substitution happened and that nothing of the template is
            // left.
            None => assert!(path.is_file()),
        }
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
fn a_dry_run_for_opencode_says_what_the_real_run_does_and_writes_none_of_it() {
    let machine = Machine::new().installed("opencode");

    let planned = machine.report(&["install", "--dry-run", "--agent", "opencode"]);

    assert!(!machine.opencode_plugin().exists());
    assert!(!machine.opencode_tui_plugin().exists());
    assert!(!machine.opencode_tui_config().exists());
    assert!(is_untouched(&machine.state));

    let done = machine.report(&["install", "--agent", "opencode"]);

    assert_eq!(planned.replace("would create", "created"), done);
}

#[test]
fn installing_for_opencode_twice_changes_nothing_the_second_time() {
    let machine = Machine::new().installed("opencode");
    machine.report(&["install", "--agent", "opencode"]);
    let paths = [
        machine.opencode_plugin(),
        machine.opencode_tui_plugin(),
        machine.opencode_tui_config(),
    ];
    let after_one: Vec<String> = paths
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();

    let report = machine.report(&["install", "--agent", "opencode"]);

    assert!(report.contains("  already installed\n"), "{report}");
    for (path, before) in paths.iter().zip(after_one) {
        assert!(
            report.contains(&format!("unchanged {}", path.display())),
            "{report}"
        );
        assert_eq!(fs::read_to_string(path).unwrap(), before);
        assert!(
            machine.backups_of(path).is_empty(),
            "a run that changed nothing still copied {}",
            path.display()
        );
    }
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
fn uninstalling_for_opencode_takes_away_both_plugins_and_the_directories_it_made() {
    let machine = Machine::new().installed("opencode");
    machine.report(&["install", "--agent", "opencode"]);

    let report = machine.report(&["uninstall", "--agent", "opencode"]);

    for path in [machine.opencode_plugin(), machine.opencode_tui_plugin()] {
        assert!(
            report.contains(&format!("removed {}", path.display())),
            "{report}"
        );
        assert!(!path.exists(), "{} was left behind", path.display());
    }
    // Nothing of this program's is left, down to the directories it made —
    // which is also why there is nowhere for a copy of a file to have survived.
    // Both go: this machine had no configuration for the agent at all before
    // the install, so the directory holding it is one this program made.
    assert!(
        !machine.opencode_plugin_dir().exists(),
        "the directory this program made was left behind"
    );
    assert!(
        !machine.home.join(".config/opencode").exists(),
        "a configuration directory this program made was left behind"
    );
}

#[test]
fn uninstalling_for_opencode_puts_a_configuration_of_the_users_own_back_as_it_was() {
    let machine = Machine::new().configured("opencode");
    let config = machine.opencode_tui_config();
    let theirs = "{\n  // the ones I chose myself\n  \"plugin\": [\"./mine.js\"],\n}\n";
    fs::write(&config, theirs).unwrap();
    machine.report(&["install", "--agent", "opencode"]);
    let installed = fs::read_to_string(&config).unwrap();
    assert!(installed.contains("./agentbus-tui.js"), "{installed}");
    assert!(
        installed.contains("// the ones I chose myself"),
        "{installed}"
    );

    machine.report(&["uninstall", "--agent", "opencode"]);

    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        theirs,
        "the file was not put back exactly as it was"
    );
}

#[test]
fn a_terminal_configuration_that_cannot_take_the_entry_stops_the_install() {
    let machine = Machine::new().configured("opencode");
    let config = machine.opencode_tui_config();
    let theirs = "{\n  \"plugin\": \"./mine.js\"\n}\n";
    fs::write(&config, theirs).unwrap();

    let output = machine.run(&["install", "--agent", "opencode"]);

    assert!(!output.status.success(), "{output:?}");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(
        complaint.contains(&config.display().to_string()),
        "{complaint}"
    );
    // Refused while the whole change was still only a plan, so not one file of
    // the agent's was written.
    assert_eq!(fs::read_to_string(&config).unwrap(), theirs);
    assert!(!machine.opencode_plugin().exists());
    assert!(!machine.opencode_tui_plugin().exists());
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

    let output = machine.run(&["install", "--agent", "pi"]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "naming an agent nothing can be done for is a mistake in the command \
         line, not a failure to install"
    );
    let said = String::from_utf8(output.stderr).expect("output is not UTF-8");
    assert!(said.contains("cannot install pi yet"), "{said}");
    assert!(
        said.contains(
            "antigravity, claude, codex, cursor, devin, droid, github-copilot, grok, \
             mastracode, opencode, qodercli and qwen"
        ),
        "{said}"
    );
    assert!(is_untouched(&machine.state));
}

#[test]
fn an_agent_this_build_has_no_installer_for_is_refused_by_the_uninstall_too() {
    let machine = Machine::new();

    let output = machine.run(&["uninstall", "--agent", "pi"]);

    assert_eq!(output.status.code(), Some(2));
    let said = String::from_utf8(output.stderr).expect("output is not UTF-8");
    assert!(said.contains("cannot uninstall pi yet"), "{said}");
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
    let machine = Machine::new().installed("pi");

    let report = machine.report(&["install"]);

    assert!(report.contains("found pi"), "{report}");
    assert!(
        report.contains(
            "nothing to install: this build only handles antigravity, claude, codex, \
         cursor, devin, droid, github-copilot, grok, mastracode, opencode, qodercli \
         and qwen\n"
        ),
        "{report}"
    );
    assert!(is_untouched(&machine.state));
}

#[test]
fn an_agent_run_by_a_name_that_is_not_its_own_is_found_by_that_name() {
    let machine = Machine::new()
        .installed("cursor-agent")
        .configured("cursor");

    let report = machine.report(&["install"]);

    assert!(report.contains("found cursor ("), "{report}");
    assert!(
        report.contains("cursor-agent"),
        "the name it was found under is what a user would recognize: {report}"
    );
}
