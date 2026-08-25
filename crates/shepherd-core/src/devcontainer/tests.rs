//! What the container command is asked, and what happens when it is not there.
//!
//! Two kinds of test, for two different questions. Most of them go through a
//! stand-in that records what it was asked and answers what the test told it
//! to, because the question is which arguments this code produces and in what
//! order, and a real container runtime would answer that question no better
//! while making it unaskable on a machine without one.
//!
//! The rest run a real command through a real terminal, with a directory of
//! this test's own standing in for the machine's `PATH` and a script in it
//! standing in for the container tool — see [`command_in`], which has something
//! to say about writing a program and then running it.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::thread;
use std::time::{Duration, Instant};

use crate::ids::{ShellId, WorkspaceId};
use crate::terminal::{CORRELATION_VAR, DEFAULT_TERM, ShellSize, TERM_VAR};

use super::*;

/// How long a test waits for a real process to print something before it
/// concludes it never will.
const PATIENCE: Duration = Duration::from_secs(20);

/// How often it looks while waiting.
const GLANCE: Duration = Duration::from_millis(10);

/// The address every test uses, and the correlation it is expected to produce.
fn address() -> ShellAddress {
    ShellAddress::new(WorkspaceId::from_raw(9), ShellId::from_raw(3))
}

/// The correlation for [`address`].
const CORRELATION: &str = "w9:s3";

/// A container command that is never run: it records what it was asked and
/// answers what the test told it to, or agrees if the test said nothing.
#[derive(Debug)]
struct Stand {
    command: Option<PathBuf>,
    asked: RefCell<Vec<Vec<String>>>,
    answers: RefCell<VecDeque<Outcome>>,
}

impl Stand {
    /// One that is installed and agrees with everything.
    fn installed() -> Self {
        Self {
            command: Some(PathBuf::from("/usr/local/bin/devcontainer")),
            asked: RefCell::new(Vec::new()),
            answers: RefCell::new(VecDeque::new()),
        }
    }

    /// One that is not on the machine at all.
    fn absent() -> Self {
        Self {
            command: None,
            ..Self::installed()
        }
    }

    /// The same, answering `answers` to its first runs in turn.
    fn answering(self, answers: impl IntoIterator<Item = Outcome>) -> Self {
        *self.answers.borrow_mut() = answers.into_iter().collect();
        self
    }

    /// Everything it has been asked to run, in order.
    fn asked(&self) -> Vec<Vec<String>> {
        self.asked.borrow().clone()
    }

    /// How many times it has been run.
    fn runs(&self) -> usize {
        self.asked.borrow().len()
    }
}

impl Containers for Stand {
    fn command(&self) -> Result<PathBuf, ContainerError> {
        self.command.clone().ok_or(ContainerError::NotInstalled)
    }

    fn run(&self, args: &[String]) -> Result<Outcome, ContainerError> {
        self.command()?;
        self.asked.borrow_mut().push(args.to_vec());
        Ok(self
            .answers
            .borrow_mut()
            .pop_front()
            .unwrap_or(Outcome::Succeeded))
    }
}

/// A refusal with something to say.
fn refused() -> Outcome {
    Outcome::Refused {
        status: Some(1),
        said: "the container is not running".to_owned(),
    }
}

/// The words that bring the container in `folder` up.
fn up(folder: &Path) -> Vec<String> {
    vec![
        "up".to_owned(),
        "--workspace-folder".to_owned(),
        folder.display().to_string(),
    ]
}

/// The words that ask whether there is a container in `folder` to run in.
fn probe(folder: &Path) -> Vec<String> {
    vec![
        "exec".to_owned(),
        "--workspace-folder".to_owned(),
        folder.display().to_string(),
        "--".to_owned(),
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        ":".to_owned(),
    ]
}

/// A workspace on `folder`, with its shells set to run in a container or not.
fn workspace(folder: &Path, devcontainer: bool) -> Workspace {
    let mut workspace = Workspace::new(WorkspaceId::from_raw(9), folder);
    workspace.settings_mut().devcontainer = devcontainer;
    workspace
}

/// A directory holding a command called `devcontainer` that does `body`, and a
/// machine that looks for commands only there.
///
/// Written rather than borrowed from the machine, because what these tests want
/// to see is what the command was *asked*, and nothing a machine ships reports
/// that usefully. Every `body` here answers being asked to bring a container up
/// straight away, which is what the wait below relies on.
///
/// That wait is not superstition. A file this process has open for writing
/// cannot be executed, and a sibling test that starts a process in that window
/// inherits the handle and keeps the file unrunnable until it execs something
/// of its own — a failure with nothing to do with what is being tested. Running
/// the script once, patiently, until the machine agrees it is a program is what
/// closes that window rather than merely narrowing it.
#[cfg(unix)]
fn command_in(dir: &Path, body: &str) -> Machine {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(COMMAND);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("a script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("a script anybody may run");

    let deadline = Instant::now() + PATIENCE;
    while let Err(problem) = process::Command::new(&path)
        .arg(UP)
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .status()
    {
        assert!(
            Instant::now() < deadline,
            "the command written for this test never became runnable: {problem}"
        );
        thread::sleep(GLANCE);
    }
    Machine::searching([dir])
}

/// Waits until `ready`, or fails saying what it was waiting for and what the
/// shell had printed instead.
fn wait_for(shell: &Shell, expectation: &str, mut ready: impl FnMut(&Shell) -> bool) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if ready(shell) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "waited {PATIENCE:?} for {expectation}; state {:?}, revision {}; the screen said:\n{}",
            shell.state(),
            shell.revision(),
            shell.screen().join("\n")
        );
        thread::sleep(GLANCE);
    }
}

/// Waits until the shell has printed `text` somewhere, and answers with
/// everything it printed.
fn wait_for_text(shell: &Shell, text: &str) -> String {
    let printed = |shell: &Shell| shell.buffer().join(" ");
    wait_for(shell, &format!("`{text}` to be printed"), |shell| {
        printed(shell).contains(text)
    });
    printed(shell)
}

#[test]
fn a_folder_says_whether_it_describes_a_development_container() {
    let neither = tempfile::tempdir().expect("a temporary directory");
    assert!(!described(neither.path()));

    let directory = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir(directory.path().join(CONFIG_DIR)).expect("a directory");
    assert!(described(directory.path()));

    let file = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(file.path().join(CONFIG_FILE), "{}").expect("a file");
    assert!(described(file.path()));

    // The description is a file where the other shape is a directory, and the
    // other way round. Neither is a folder that describes a container.
    let wrong_way_round = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(wrong_way_round.path().join(CONFIG_DIR), "").expect("a file");
    std::fs::create_dir(wrong_way_round.path().join(CONFIG_FILE)).expect("a directory");
    assert!(!described(wrong_way_round.path()));
}

#[test]
fn a_container_is_brought_up_before_the_first_shell_and_only_checked_before_the_next() {
    let folder = Path::new("/home/someone/projects/thing");
    let stand = Stand::installed();
    let mut container = Devcontainer::at(folder);
    assert!(!container.is_up());

    container.ensure_up(&stand).expect("the container comes up");
    assert!(container.is_up());
    container.ensure_up(&stand).expect("it is still up");
    container.ensure_up(&stand).expect("and still up");

    assert_eq!(
        stand.asked(),
        vec![up(folder), probe(folder), probe(folder)],
        "the container was brought up more than once, or never checked again"
    );
}

#[test]
fn a_container_that_has_stopped_is_brought_up_again_rather_than_assumed() {
    let folder = Path::new("/home/someone/projects/thing");
    // Up, then still up, then gone — and the run after that is what puts it
    // back.
    let stand = Stand::installed().answering([
        Outcome::Succeeded,
        Outcome::Succeeded,
        refused(),
        Outcome::Succeeded,
    ]);
    let mut container = Devcontainer::at(folder);

    for _ in 0..3 {
        container.ensure_up(&stand).expect("a container to run in");
    }

    assert_eq!(
        stand.asked(),
        vec![up(folder), probe(folder), probe(folder), up(folder)],
        "a stopped container was not brought back up"
    );
    assert!(container.is_up());
}

#[test]
fn a_container_that_will_not_come_up_is_reported_with_what_the_command_said() {
    let folder = Path::new("/home/someone/projects/thing");
    let stand = Stand::installed().answering([refused()]);
    let mut container = Devcontainer::at(folder);

    let Err(ContainerError::NotUp {
        folder: named,
        command,
        status,
        said,
    }) = container.ensure_up(&stand)
    else {
        panic!("a container that refused to come up was taken to have come up");
    };

    assert_eq!(named, folder);
    assert_eq!(
        command,
        "devcontainer up --workspace-folder /home/someone/projects/thing"
    );
    assert_eq!(status, Some(1));
    assert_eq!(said, "the container is not running");
    assert!(
        !container.is_up(),
        "a container that refused was remembered"
    );
}

#[test]
fn a_shell_in_a_container_is_run_by_the_command_with_its_environment_as_arguments() {
    let folder = Path::new("/home/someone/projects/thing");
    let stand = Stand::installed();
    let container = Devcontainer::at(folder);

    let options = ShellOptions::new().env("EDITOR", "vi");
    let inside = container
        .options(address(), &options, &stand)
        .expect("options for a shell in the container");

    let program = inside.chosen_program().expect("the command runs the shell");
    assert_eq!(program.program(), "/usr/local/bin/devcontainer");
    assert_eq!(
        program.args(),
        [
            "exec",
            "--workspace-folder",
            "/home/someone/projects/thing",
            "--remote-env",
            "AGENTBUS_PANE=w9:s3",
            "--remote-env",
            "EDITOR=vi",
            "--remote-env",
            "TERM=xterm-256color",
            "--",
            "/bin/sh",
        ]
    );
    assert_eq!(stand.runs(), 0, "asking what to run ran something");
}

#[test]
fn a_shell_in_a_container_runs_what_was_asked_for_rather_than_the_default() {
    let stand = Stand::installed();
    let container = Devcontainer::at("/w");
    let options = ShellOptions::new().program(Program::new("/usr/bin/fish").with_args(["-l"]));

    let inside = container
        .options(address(), &options, &stand)
        .expect("options for a shell in the container");

    let args = inside.chosen_program().expect("a program").args();
    let (_, command) = args.split_at(args.iter().position(|arg| arg == "--").expect("an end") + 1);
    assert_eq!(command, ["/usr/bin/fish", "-l"]);
}

#[test]
fn the_command_being_absent_is_said_plainly_rather_than_worked_around() {
    let stand = Stand::absent();
    let mut container = Devcontainer::at("/home/someone/projects/thing");

    assert!(matches!(
        container.ensure_up(&stand),
        Err(ContainerError::NotInstalled)
    ));
    assert!(matches!(
        container.options(address(), &ShellOptions::new(), &stand),
        Err(ContainerError::NotInstalled)
    ));
    assert!(matches!(
        container.spawn(address(), &ShellOptions::new(), &stand),
        Err(StartError::Container(ContainerError::NotInstalled))
    ));
    assert_eq!(
        ContainerError::NotInstalled.to_string(),
        "`devcontainer` is not installed on this machine, and a workspace whose shells run in a development container needs it"
    );
}

#[test]
fn a_machine_with_nothing_on_its_path_has_no_command() {
    assert!(matches!(
        Machine::searching(Vec::<PathBuf>::new()).command(),
        Err(ContainerError::NotInstalled)
    ));

    // A file of the right name that nobody can run is not a command either.
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(dir.path().join(COMMAND), "").expect("a file");
    assert!(matches!(
        Machine::searching([dir.path()]).command(),
        Err(ContainerError::NotInstalled)
    ));
}

#[cfg(unix)]
#[test]
fn a_machine_runs_the_command_it_finds_and_carries_what_it_said() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let machine = command_in(dir.path(), "echo 'no runtime is running' >&2\nexit 3");

    assert_eq!(
        machine.command().expect("a command"),
        dir.path().join(COMMAND)
    );
    assert_eq!(
        machine
            .run(&up(Path::new("/w")))
            .expect("a command that ran"),
        Outcome::Refused {
            status: Some(3),
            said: "no runtime is running".to_owned(),
        }
    );

    let silent = tempfile::tempdir().expect("a temporary directory");
    assert_eq!(
        command_in(silent.path(), "exit 1")
            .run(&up(Path::new("/w")))
            .expect("a command that ran"),
        Outcome::Refused {
            status: Some(1),
            said: UNEXPLAINED.to_owned(),
        },
        "a refusal with nothing to say was left looking like something else"
    );
}

#[test]
fn a_workspace_that_does_not_use_a_container_starts_its_shells_here() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let stand = Stand::absent();
    let mut shells = Shells::for_workspace(&workspace(dir.path(), false));

    assert_eq!(shells, Shells::ThisMachine);
    assert!(shells.container().is_none());

    let options = ShellOptions::new()
        .program(Program::new("/bin/sh"))
        .env("PS1", "")
        .env("ENV", "");
    let mut shell = shells
        .start(address(), &options, &stand)
        .expect("a shell on this machine");

    shell.write("printf 'here=%s\\n' \"$AGENTBUS_PANE\"\n");
    wait_for_text(&shell, &format!("here={CORRELATION}"));
    assert_eq!(
        stand.runs(),
        0,
        "an ordinary workspace asked about a container"
    );
}

#[cfg(unix)]
#[test]
fn a_workspace_that_uses_a_container_carries_the_correlation_across_the_boundary() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let folder = tempfile::tempdir().expect("a temporary directory");
    // Asked to bring a container up it agrees and says nothing; asked to run
    // something it prints what it was asked and then stays running, exactly as
    // a shell in a container would. Staying is the point: what a process prints
    // immediately before it exits can be lost with the terminal it printed to,
    // and a test that raced that would fail for a reason of its own.
    let machine = command_in(
        dir.path(),
        "if [ \"$1\" = up ]; then exit 0; fi\nprintf '%s\\n' \"$*\"\nsleep 30",
    );
    let mut shells = Shells::for_workspace(&workspace(folder.path(), true));

    assert!(matches!(shells, Shells::Container(_)));
    let options = ShellOptions::new()
        // Wide enough that the whole command line lands on one row, so that
        // looking for part of it cannot be defeated by a wrap.
        .size(ShellSize::new(400, 24));
    let shell = shells
        .start(address(), &options, &machine)
        .expect("a shell in the container");

    let printed = wait_for_text(&shell, &format!("{CORRELATION_VAR}={CORRELATION}"));
    assert!(
        printed.contains(&format!("{REMOTE_ENV} {CORRELATION_VAR}={CORRELATION}")),
        "the correlation did not cross as an argument; the shell printed:\n{printed}"
    );
    assert!(
        printed.contains(&format!("{REMOTE_ENV} {TERM_VAR}={DEFAULT_TERM}")),
        "the terminal did not describe itself across the boundary:\n{printed}"
    );
    assert!(
        printed.contains(&format!(
            "{EXEC} {WORKSPACE_FOLDER} {}",
            folder.path().display()
        )),
        "the shell was not run in the workspace's own container:\n{printed}"
    );
    assert!(
        printed.contains(&format!("{END_OF_OPTIONS} {DEFAULT_PROGRAM}")),
        "the container was not asked to run a shell:\n{printed}"
    );

    assert!(
        shells
            .container()
            .expect("a workspace that uses a container")
            .is_up(),
        "the container was not brought up before the shell"
    );
}
