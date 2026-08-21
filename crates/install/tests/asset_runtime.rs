//! The installed plugins, run.
//!
//! Most of what this program writes into an agent is a few lines of shell whose
//! whole content is the command it runs, and reading the file is a fair way to
//! know what it does. The plugins are not like that. They are programs: they are
//! loaded by an agent, handed an interface of that agent's own, and have to work
//! out what to say from what they are given, spawn a command, and disappear.
//! Reading one proves that somebody wrote plausible code. Running one proves
//! what it hands over.
//!
//! So each of them is installed onto a described machine, loaded the way its
//! agent loads it, and driven through the callback that reports a session — and
//! the command it hands its payload to is a stand-in that writes down what it
//! was given. Two things are then asked of that recording:
//!
//! * the payload is one the mapping for that agent reads as a session
//!   beginning. The asset and the mapping are written against each other and
//!   nothing else checks that they agree; a field renamed on either side is a
//!   session that never appears, on a machine nobody is testing on.
//! * running it costs the session nothing when the world is broken. The binary
//!   it names may have been removed, or may be wedged, and a plugin that raised
//!   or waited in either case would be a fault in somebody's editor that they
//!   did not ask for and cannot act on.
//!
//! # What the fakes are
//!
//! Each plugin interface is imitated in the driver its interpreter runs, in as
//! few lines as the plugin reads — the shape the agent documents itself as
//! handing a plugin, described beside each imitation and beside the plugin
//! itself. A fake is not evidence about the agent: it is evidence about the
//! plugin, given that the agent behaves as its own documentation says. What
//! catches an agent whose interface has moved is running against the real one,
//! which is a person's job on a real machine.
//!
//! # Interpreters
//!
//! A machine without `node` or without `python3` runs the cases that do not
//! need it and says how many it left out. It never says nothing: a suite that
//! quietly tests less than it claims is worse than one that fails.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use agentbus_detect::store::{ManifestStore, StorePaths};
use agentbus_install::state::State;
use agentbus_install::{Agent, Change, Environment, installers};
use agentbus_protocol::{Kind, UnstampedEvent};
use serde_json::Value;

/// The session the imitated agent says it has open.
///
/// Made up here so that the recording can be checked against it: a session that
/// came out of the plugin unchanged proves it read the interface, and one that
/// came out of the mapping unchanged proves the mapping reads the field the
/// plugin writes it to.
const SESSION: &str = "ses_37f0b1c4";

/// How long the stand-in that wedges stays wedged for.
///
/// Long enough that a plugin waiting for it would be caught by the budget below
/// several times over, and short enough that the one left behind by the test
/// that wanted it is gone soon after.
const HANG: Duration = Duration::from_secs(20);

/// How long a plugin may keep the session it runs in waiting.
///
/// Not a measurement of anything: what is being caught is a plugin that waits
/// for the command it started, which would take the whole of the wedged
/// stand-in's life. Anything under this is a plugin that handed over and let go.
const BUDGET: Duration = Duration::from_secs(8);

/// An interpreter one of the plugins is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runtime {
    Node,
    Python,
}

impl Runtime {
    /// What runs it on this machine.
    fn command(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Python => "python3",
        }
    }

    /// The driver that puts an agent's plugin interface in front of a file and
    /// fires it.
    fn driver(self) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/drivers")
            .join(match self {
                Self::Node => "plugins.mjs",
                Self::Python => "plugins.py",
            })
    }

    /// Whether this machine has it at all.
    fn present(self) -> bool {
        Command::new(self.command())
            .arg("--version")
            .output()
            .is_ok_and(|version| version.status.success())
    }
}

/// One installed plugin, and what it takes to run it.
#[derive(Debug, Clone, Copy)]
struct Case {
    /// The agent it is installed for, which is both what installs it and whose
    /// mapping reads what it hands over.
    agent: Agent,
    /// What the installer calls it, which is the file to load.
    file: &'static str,
    /// What runs it.
    runtime: Runtime,
    /// Which of the driver's imitations to put in front of it.
    shape: &'static str,
}

/// Every plugin this build installs that is run rather than executed.
///
/// The shells are not here: what a wrapper does is the line it runs, the tests
/// beside each agent check that line, and installing one and running it would
/// be testing the machine's shell.
const CASES: [Case; 6] = [
    Case {
        agent: Agent::Hermes,
        file: "__init__.py",
        runtime: Runtime::Python,
        shape: "plugin-callbacks",
    },
    Case {
        agent: Agent::Kilo,
        file: "agentbus.js",
        runtime: Runtime::Node,
        shape: "plugin-event",
    },
    Case {
        agent: Agent::Omp,
        file: "agentbus-omp.ts",
        runtime: Runtime::Node,
        shape: "extension-subscribe",
    },
    Case {
        agent: Agent::OpenCode,
        file: "agentbus.js",
        runtime: Runtime::Node,
        shape: "plugin-event",
    },
    Case {
        agent: Agent::OpenCode,
        file: "agentbus-tui.js",
        runtime: Runtime::Node,
        shape: "tui-session",
    },
    Case {
        agent: Agent::Pi,
        file: "agentbus.ts",
        runtime: Runtime::Node,
        shape: "extension-subscribe",
    },
];

/// What the command a plugin hands its payload to is like on the machine a case
/// is run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bus {
    /// It is there, and writes down what it was given.
    Records,
    /// It is there, takes the payload, and then never finishes.
    Wedged,
    /// It is not there at all, which is what an installation outlives its
    /// binary looks like.
    Gone,
}

/// One invocation of the command a plugin handed its payload to.
#[derive(Debug)]
struct Recording {
    /// What it was run with, one argument per entry.
    argv: Vec<String>,
    /// What it was given on its standard input.
    stdin: Vec<u8>,
}

/// What running one plugin once did.
#[derive(Debug)]
struct Run {
    /// Where the imitated agent was working, as the plugin will have reported
    /// it: resolved, because a plugin asks the machine where it is and gets
    /// back a path with no links left in it.
    work: String,
    /// What the host it was loaded by saw when it finished.
    output: Output,
    /// How long that host was kept waiting.
    took: Duration,
    /// Every invocation of the command the plugin handed its payload to.
    recordings: Vec<Recording>,
}

#[test]
fn every_plugin_hands_over_a_payload_its_mapping_reads_as_a_session() {
    each(|case| {
        let run = run(case, Bus::Records);

        run.finished_quietly(case);
        let recording = run.only_recording(case);
        assert_eq!(
            recording.argv,
            ["emit", "--agent", case.agent.name()],
            "{case:?} handed its payload to something else",
        );

        let payload: Value = serde_json::from_slice(&recording.stdin)
            .unwrap_or_else(|why| panic!("{case:?} wrote something that is not JSON: {why}"));
        let event = normalized(case.agent, &payload).unwrap_or_else(|| {
            panic!("{case:?} wrote a payload its mapping says nothing about: {payload}")
        });

        assert_eq!(event.agent.as_str(), case.agent.name(), "{case:?}");
        assert_eq!(
            event.session, SESSION,
            "{case:?} lost the session it was told"
        );
        // Every plugin here is driven through the one callback it has that says
        // a session exists, whatever its agent calls that, so this is the same
        // answer for all of them: the identity of a session, which is the whole
        // reason any of these files is installed.
        assert_eq!(event.kind, Kind::SessionStart, "{case:?}");
        assert_eq!(
            event.cwd.as_deref(),
            Some(run.work.as_str()),
            "{case:?} lost the directory it was working in",
        );
    });
}

#[test]
fn every_plugin_is_harmless_when_the_binary_it_names_is_not_there() {
    each(|case| {
        let run = run(case, Bus::Gone);

        run.finished_quietly(case);
        assert!(
            run.recordings.is_empty(),
            "{case:?} reached something, on a machine where there is nothing to reach",
        );
    });
}

#[test]
fn no_plugin_waits_for_the_binary_it_handed_its_payload_to() {
    each(|case| {
        let run = run(case, Bus::Wedged);

        run.finished_quietly(case);
        assert!(
            run.took < BUDGET,
            "{case:?} kept the session waiting {:?} for a command that had not finished",
            run.took,
        );
        // The payload still arrived. Without this the test would pass just as
        // well for a plugin that had handed nothing over at all.
        run.only_recording(case);
    });
}

impl Run {
    /// Fails unless the host was left with nothing to complain about.
    ///
    /// Both halves matter and neither implies the other: a plugin that raised
    /// is a fault in somebody's session, and one that printed is an instruction
    /// to an agent that reads what its hooks print.
    fn finished_quietly(&self, case: &Case) {
        assert!(
            self.output.status.success(),
            "{case:?} failed in its host: {}",
            String::from_utf8_lossy(&self.output.stderr),
        );
        assert!(
            self.output.stdout.is_empty(),
            "{case:?} said `{}` to the session it ran in",
            String::from_utf8_lossy(&self.output.stdout),
        );
    }

    /// The one invocation there should have been.
    fn only_recording(&self, case: &Case) -> &Recording {
        assert_eq!(
            self.recordings.len(),
            1,
            "{case:?} ran the command {} times",
            self.recordings.len(),
        );
        &self.recordings[0]
    }
}

/// Runs `case` on a machine where the bus is as `bus` says, and answers with
/// everything that happened.
fn run(case: &Case, bus: Bus) -> Run {
    let root = tempfile::tempdir().expect("cannot make a temporary directory");
    let home = root.path().join("home");
    let work = root.path().join("work");
    let recordings = root.path().join("recordings");
    let binary = root.path().join("bin/agentbus");
    for dir in [&home, &work, &recordings] {
        fs::create_dir_all(dir).expect("cannot make a directory");
    }
    if bus != Bus::Gone {
        stub(&binary, &recordings, bus == Bus::Wedged);
    }

    let module = install(case.agent, &home, &binary, case.file);
    let started = Instant::now();
    let output = Command::new(case.runtime.command())
        .arg(case.runtime.driver())
        .arg(case.shape)
        .arg(&module)
        .arg(SESSION)
        // The plugins that are told nothing about where the agent is working
        // read it from the process they are loaded in, so the process is put
        // where the imitated agent is supposed to be.
        .current_dir(&work)
        .output()
        .unwrap_or_else(|why| panic!("cannot run {}: {why}", case.runtime.command()));
    let took = started.elapsed();

    Run {
        work: resolved(&work),
        output,
        took,
        recordings: read_recordings(&recordings, bus),
    }
}

/// Installs `agent`'s hooks onto a machine whose home directory is `home`, with
/// `binary` as the path of the bus, and answers with the installed file called
/// `file`.
///
/// Through the agent's own installer rather than by copying the asset, so that
/// what is run is what a real installation puts on a real machine — the path
/// written in, the file named as the agent names it, in the directory the agent
/// reads.
fn install(agent: Agent, home: &Path, binary: &Path, file: &str) -> PathBuf {
    let env = Environment::rooted(home);
    // The agent's own directory is never made by an installer, which refuses
    // rather than guessing at the layout of a program that has not run here.
    fs::create_dir_all(agent.config_dir(&env)).expect("cannot make the agent's directory");

    let installer = installers()
        .into_iter()
        .find(|installer| installer.agent() == agent)
        .unwrap_or_else(|| panic!("{agent} has no installer"));
    let changes = installer
        .plan_install(&env, &State::default(), binary)
        .unwrap_or_else(|why| panic!("planning {agent}'s installation failed: {why}"));

    let mut state = State::default();
    for change in &changes {
        change
            .apply(agent, &mut state)
            .unwrap_or_else(|why| panic!("installing for {agent} failed: {why}"));
    }

    changes
        .iter()
        .filter_map(Change::path)
        .find(|path| path.file_name().is_some_and(|name| name == file))
        .unwrap_or_else(|| panic!("installing for {agent} writes no {file}: {changes:?}"))
        .to_owned()
}

/// Writes a stand-in for this program's own binary at `path`.
///
/// It records the two things these tests are about — what it was run with, and
/// what it was given — under its own process number, so that a second
/// invocation cannot be mistaken for the first, and under the final names only
/// once each is complete, so that a reader never finds half of one. It is a
/// file rather than anything the bus would recognize because what is under test
/// is the plugin, and a plugin cannot tell the difference.
fn stub(path: &Path, recordings: &Path, wedged: bool) {
    let sleeps = match wedged {
        true => format!("sleep {}\n", HANG.as_secs()),
        false => String::new(),
    };
    let script = format!(
        "#!/bin/sh\n\
         dir='{dir}'\n\
         for arg in \"$@\"; do printf '%s\\n' \"$arg\"; done > \"$dir/$$.argv.part\"\n\
         cat > \"$dir/$$.stdin.part\"\n\
         mv \"$dir/$$.argv.part\" \"$dir/$$.argv\"\n\
         mv \"$dir/$$.stdin.part\" \"$dir/$$.stdin\"\n\
         {sleeps}exit 0\n",
        dir = recordings.display(),
    );
    fs::create_dir_all(path.parent().expect("a file has a directory"))
        .expect("cannot make a directory");
    fs::write(path, script).expect("cannot write the stand-in");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .expect("cannot make the stand-in runnable");
}

/// Everything the stand-in wrote down.
///
/// A plugin lets go of the command it starts and then finishes, so the
/// recording can land after the host it was loaded in has gone. Where one is
/// expected this waits for it; where none is, there is nothing that could ever
/// write one and nothing to wait for.
fn read_recordings(dir: &Path, bus: Bus) -> Vec<Recording> {
    if bus != Bus::Gone {
        let deadline = Instant::now() + BUDGET;
        while Instant::now() < deadline && complete(dir).is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let mut recordings: Vec<Recording> = complete(dir)
        .into_iter()
        .map(|stdin| Recording {
            argv: fs::read_to_string(stdin.with_extension("argv"))
                .expect("cannot read what the stand-in was run with")
                .lines()
                .map(str::to_owned)
                .collect(),
            stdin: fs::read(&stdin).expect("cannot read what the stand-in was given"),
        })
        .collect();
    recordings.sort_by(|one, other| one.stdin.cmp(&other.stdin));
    recordings
}

/// The recordings in `dir` that are finished being written.
fn complete(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .expect("cannot read what the stand-in wrote")
        .map(|entry| entry.expect("cannot read a recording").path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "stdin"))
        .filter(|path| path.with_extension("argv").is_file())
        .collect();
    found.sort();
    found
}

/// What the mapping this build carries for `agent` makes of `payload`.
///
/// Read through a machine with nothing on it, so that the answer is the mapping
/// this build ships rather than a copy that happens to be on the machine the
/// tests are running on.
fn normalized(agent: Agent, payload: &Value) -> Option<UnstampedEvent> {
    let bare = tempfile::tempdir().expect("cannot make a temporary directory");
    let store = ManifestStore::open(StorePaths::rooted(bare.path()));
    store.normalize_hook(agent.name(), payload)
}

/// A directory as whatever is inside it will see it named.
fn resolved(dir: &Path) -> String {
    fs::canonicalize(dir)
        .expect("cannot resolve a directory that was just made")
        .to_str()
        .expect("a temporary directory is text")
        .to_owned()
}

/// Runs `check` over every case whose interpreter this machine has, and says
/// how many were left out and why.
///
/// Never silently: a run that tested four of six is a run whose result means
/// something different, and the person reading it has to be told which.
fn each(check: impl Fn(&Case)) {
    let mut skipped = Vec::new();
    for case in &CASES {
        if !case.runtime.present() {
            skipped.push(format!(
                "{} ({}): no {}",
                case.file,
                case.agent,
                case.runtime.command()
            ));
            continue;
        }
        check(case);
    }
    eprintln!(
        "{} of {} plugins run; {} skipped{}",
        CASES.len() - skipped.len(),
        CASES.len(),
        skipped.len(),
        match skipped.is_empty() {
            true => String::new(),
            false => format!(": {}", skipped.join(", ")),
        },
    );
}
