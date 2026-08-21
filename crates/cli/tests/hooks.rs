//! `agentbus hooks status` against a described machine.
//!
//! These run the real binary with `HOME` and `PATH` pointed into a temporary
//! directory, so what they exercise is the whole path a user's invocation
//! takes: the argument parser, the detection rules reading the environment this
//! process set up, the files on disk being read, and the lines printed.
//!
//! Every state but "current" is made by installing for real and then doing to
//! the files what time does to them — a generation rolls forward, a mark
//! predates the marking, an entry somewhere else goes missing. That way the
//! machine being reported on is one this program could actually have left
//! behind, rather than one a test invented and nothing would ever produce.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// The mark an installed file carries saying which generation it is.
const MARKER: &str = "AGENTBUS_HOOK_VERSION=";

/// The status a command line that does not make sense exits with.
const USAGE: i32 = 2;

/// A home directory, a search path and a state directory, all under one
/// temporary directory that is removed with the test.
struct Machine {
    root: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    state: PathBuf,
}

impl Machine {
    /// A machine with no coding agent on it.
    fn new() -> Self {
        let root = tempfile::tempdir().expect("cannot make a temporary directory");
        let (home, bin, state) = (
            root.path().join("home"),
            root.path().join("bin"),
            root.path().join("state"),
        );
        for dir in [&home, &bin] {
            fs::create_dir_all(dir).unwrap();
        }
        Self {
            root,
            home,
            bin,
            state,
        }
    }

    /// Gives the machine a configuration directory for `agent`, which is one of
    /// the two things that make an agent count as being here.
    fn configured(self, agent: &str) -> Self {
        fs::create_dir_all(self.config_dir(agent)).unwrap();
        self
    }

    /// Installs this program's hooks for `agent`, as a user would.
    fn installed(self, agent: &str) -> Self {
        let output = self.run(&["install", "--agent", agent]);
        assert!(output.status.success(), "{output:?}");
        self
    }

    /// Where `agent` keeps its configuration on this machine.
    fn config_dir(&self, agent: &str) -> PathBuf {
        self.home.join(format!(".{agent}"))
    }

    /// The file whose mark says which generation `agent`'s hooks are.
    fn wrapper(&self, agent: &str) -> PathBuf {
        self.config_dir(agent).join("hooks").join("agentbus.sh")
    }

    /// Makes `agent`'s installed hooks look like the ones an earlier build
    /// wrote, by rolling the generation in the file back.
    fn aged(self, agent: &str) -> Self {
        let path = self.wrapper(agent);
        let text = fs::read_to_string(&path).unwrap();
        let older = text.replace(
            &format!("{MARKER}{}", self.generation(agent)),
            &format!("{MARKER}0"),
        );
        fs::write(&path, older).unwrap();
        self
    }

    /// Makes them look like the ones written before this program marked its
    /// work at all, by taking the mark out.
    fn unmarked(self, agent: &str) -> Self {
        let path = self.wrapper(agent);
        let text = fs::read_to_string(&path).unwrap();
        let stripped: Vec<&str> = text.lines().filter(|line| !line.contains(MARKER)).collect();
        fs::write(&path, stripped.join("\n")).unwrap();
        self
    }

    /// Takes away the file that points at `agent`'s wrapper, leaving a current
    /// file nothing ever runs.
    fn unwired(self, agent: &str, entry: &str) -> Self {
        fs::remove_file(self.config_dir(agent).join(entry)).unwrap();
        self
    }

    /// Which generation `agent`'s installed file says it is.
    fn generation(&self, agent: &str) -> u32 {
        let text = fs::read_to_string(self.wrapper(agent)).unwrap();
        text.lines()
            .find_map(|line| line.split_once(MARKER))
            .map(|(_, version)| version.trim())
            .expect("the installed file carries no mark")
            .parse()
            .expect("the mark is not a number")
    }

    /// Runs the binary on this machine, with nothing inherited from whoever is
    /// running the tests.
    fn run(&self, args: &[&str]) -> Output {
        // A binary copied into place a moment ago can still be held open for
        // writing by another test doing the same thing, and the kernel refuses
        // to run a file somebody is writing. It clears itself; nothing about the
        // condition is what any of these tests is about.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let attempt = Command::new(env!("CARGO_BIN_EXE_agentbus"))
                .args(args)
                .env_clear()
                .env("HOME", &self.home)
                .env("PATH", &self.bin)
                .env("XDG_STATE_HOME", &self.state)
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

    /// What `agentbus hooks status` printed, having checked that it succeeded.
    fn status(&self, flags: &[&str]) -> String {
        let mut args = vec!["hooks", "status"];
        args.extend_from_slice(flags);
        let output = self.run(&args);
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).expect("output is not UTF-8")
    }

    /// The one line of the report about `agent`.
    fn line(&self, agent: &str) -> String {
        let report = self.status(&[]);
        let prefix = format!("{agent}: ");
        report
            .lines()
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| panic!("nothing about {agent} in\n{report}"))
            .to_owned()
    }

    /// What `agentbus hooks status --json` reported, parsed.
    fn json(&self, flags: &[&str]) -> serde_json::Value {
        let mut flags = flags.to_vec();
        flags.push("--json");
        let text = self.status(&flags);
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{error} in {text}"))
    }

    /// Every file under the temporary directory, with its contents, so that a
    /// command can be shown to have changed none of them.
    fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut found = Vec::new();
        walk(self.root.path(), &mut found);
        found.sort_by(|one, other| one.0.cmp(&other.0));
        found
    }
}

/// Reads every file below `dir` into `found`.
fn walk(dir: &Path, found: &mut Vec<(PathBuf, Vec<u8>)>) {
    for entry in fs::read_dir(dir).expect("cannot read a directory") {
        let path = entry.expect("cannot read a directory entry").path();
        match path.is_dir() {
            true => walk(&path, found),
            false => {
                let contents = fs::read(&path).expect("cannot read a file");
                found.push((path, contents));
            }
        }
    }
}

/// The agent whose installation has a second half that can go missing, and the
/// file that is the other half of it.
const WIRED: (&str, &str) = ("claude", "settings.json");

#[test]
fn hooks_of_this_builds_generation_are_current_and_name_the_file_they_are_in() {
    let machine = Machine::new().configured("claude").installed("claude");
    let generation = machine.generation("claude");

    let line = machine.line("claude");

    assert_eq!(
        line,
        format!(
            "claude: current (v{generation}) ({})",
            machine.wrapper("claude").display()
        )
    );
}

#[test]
fn hooks_from_an_earlier_build_are_reported_against_what_this_build_writes() {
    let machine = Machine::new().configured("claude").installed("claude");
    // Read before the file is aged: what a user is being told is that what is
    // there is behind what this build writes, and it is this build's number
    // that the file carried a moment ago.
    let generation = machine.generation("claude");
    let machine = machine.aged("claude");

    let line = machine.line("claude");

    assert_eq!(
        line,
        format!(
            "claude: outdated (v0 < v{generation}) ({})",
            machine.wrapper("claude").display()
        ),
        "the mark rolled back to nought, and nought is what should be reported"
    );
}

#[test]
fn hooks_from_before_this_program_marked_its_work_say_so_in_words() {
    let machine = Machine::new()
        .configured("claude")
        .installed("claude")
        .unmarked("claude");

    let line = machine.line("claude");

    assert_eq!(
        line,
        format!(
            "claude: outdated (pre-versioning) ({})",
            machine.wrapper("claude").display()
        ),
        "there is no earlier generation to name, only the absence of one"
    );
}

#[test]
fn a_current_file_nothing_runs_is_a_repair_rather_than_an_upgrade() {
    let (agent, entry) = WIRED;
    let machine = Machine::new()
        .configured(agent)
        .installed(agent)
        .unwired(agent, entry);
    let generation = machine.generation(agent);

    let line = machine.line(agent);

    assert_eq!(
        line,
        format!(
            "{agent}: needs repair (v{generation}) ({})",
            machine.wrapper(agent).display()
        )
    );
}

#[test]
fn an_agent_that_is_here_with_no_hooks_is_told_which_command_would_give_it_some() {
    let machine = Machine::new().configured("grok");

    assert_eq!(
        machine.line("grok"),
        "grok: not installed — detected; run agentbus install --agent grok"
    );
}

#[test]
fn an_agent_that_is_not_here_is_reported_plainly() {
    let machine = Machine::new().configured("grok");

    assert_eq!(
        machine.line("kimi"),
        "kimi: not installed",
        "nothing is on this machine to install for, so there is nothing to suggest"
    );
}

#[test]
fn every_agent_this_program_knows_is_reported_on() {
    let machine = Machine::new();

    let report = machine.status(&[]);
    let json = machine.json(&[]);

    let agents = json["agents"].as_array().expect("no agents").len();
    assert!(agents >= 17, "only {agents} agents reported");
    assert_eq!(report.lines().count(), agents, "{report}");
}

#[test]
fn only_the_hooks_needing_work_are_kept_when_that_is_what_was_asked_for() {
    let machine = Machine::new()
        .configured("claude")
        .installed("claude")
        .aged("claude")
        .configured("codex")
        .installed("codex")
        .configured("grok");

    let report = machine.status(&["--outdated-only"]);

    assert_eq!(report.lines().count(), 1, "{report}");
    assert!(report.starts_with("claude: outdated"), "{report}");
    assert!(
        !report.contains("codex") && !report.contains("grok"),
        "one is current and the other has nothing installed: {report}"
    );
}

#[test]
fn a_filter_that_matched_nothing_says_so_rather_than_printing_nothing() {
    let machine = Machine::new().configured("codex").installed("codex");

    let report = machine.status(&["--outdated-only"]);

    assert_eq!(
        report, "nothing installed here is behind what this build writes\n",
        "an empty report is the answer, and has to be said out loud"
    );
}

#[test]
fn what_is_written_for_a_program_says_what_the_lines_say() {
    let (wired, entry) = WIRED;
    let machine = Machine::new()
        .configured(wired)
        .installed(wired)
        .unwired(wired, entry)
        .configured("codex")
        .installed("codex")
        .aged("codex")
        .configured("grok");

    let json = machine.json(&[]);

    assert_eq!(json["v"], 1);
    for agent in json["agents"].as_array().expect("no agents") {
        let name = agent["agent"].as_str().expect("an agent with no name");
        let line = machine.line(name);
        let stated = match agent["state"].as_str().expect("an agent with no state") {
            "not-installed" => "not installed".to_owned(),
            "needs-repair" => "needs repair".to_owned(),
            state => state.to_owned(),
        };
        assert!(
            line.starts_with(&format!("{name}: {stated}")),
            "{line} does not say {stated}"
        );
        match agent["path"].as_str() {
            Some(path) => assert!(line.contains(path), "{line} does not name {path}"),
            None => assert!(
                line.starts_with(&format!("{name}: not installed")) && !line.contains(" (/"),
                "{line} names a file for an agent that has none"
            ),
        }
        assert_eq!(
            agent["detected"].as_bool(),
            Some(matches!(name, "claude" | "codex" | "grok")),
            "{name} was described wrongly: {agent}"
        );
    }
    let versions: Vec<&serde_json::Value> = json["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|agent| agent["agent"] == "codex")
        .map(|agent| &agent["installed_version"])
        .collect();
    assert_eq!(versions, vec![&serde_json::json!(0)], "codex was aged");
}

#[test]
fn asking_changes_nothing_on_the_machine() {
    let (wired, entry) = WIRED;
    let machine = Machine::new()
        .configured(wired)
        .installed(wired)
        .unwired(wired, entry)
        .configured("codex")
        .installed("codex")
        .aged("codex")
        .configured("grok");
    let before = machine.snapshot();

    machine.status(&[]);
    machine.status(&["--outdated-only"]);
    machine.status(&["--json"]);

    assert_eq!(
        machine.snapshot(),
        before,
        "reporting on a machine wrote to it"
    );
}

#[test]
fn a_report_is_data_rather_than_a_verdict_to_exit_with() {
    let machine = Machine::new();

    for flags in [vec![], vec!["--outdated-only"], vec!["--json"]] {
        let mut args = vec!["hooks", "status"];
        args.extend_from_slice(&flags);
        let output = machine.run(&args);

        assert_eq!(
            output.status.code(),
            Some(0),
            "{flags:?}: nothing about a machine that is behind is this command failing"
        );
    }
}

#[test]
fn a_flag_that_means_nothing_is_refused_the_way_every_other_command_refuses_one() {
    let machine = Machine::new();

    let output = machine.run(&["hooks", "status", "--everything"]);

    assert_eq!(output.status.code(), Some(USAGE), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}
