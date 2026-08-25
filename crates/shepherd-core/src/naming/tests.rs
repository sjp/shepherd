//! Two kinds of test, because there are two questions.
//!
//! Whether a real shell running a real command is called after it can only be
//! asked of a real shell running a real command, so those tests start one and
//! type into it. Everything else — what wins over what, when a look is worth
//! taking, what an answer of "nothing" leaves behind — is about the rules rather
//! than about the platform, and those tests say what is running instead of
//! running it. Both need a terminal device, and the only way to have one is to
//! have a shell, so both start one.

use std::cell::{Cell, RefCell};
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::ids::{ShellAddress, ShellId, WorkspaceId};
use crate::terminal::{Program, Shell, ShellOptions};

/// How long a test waits for something a real process has to do before it
/// concludes the process is never going to do it.
const PATIENCE: Duration = Duration::from_secs(20);

/// How often it looks while waiting.
const GLANCE: Duration = Duration::from_millis(10);

/// A shell running the one program every unix has, at a prompt that prints
/// nothing.
fn shell() -> Shell {
    let options = ShellOptions::new()
        .program(Program::new("/bin/sh"))
        .env("PS1", "")
        .env("ENV", "");
    Shell::spawn(
        ShellAddress::new(WorkspaceId::from_raw(9), ShellId::from_raw(3)),
        &options,
    )
    .expect("a shell to start")
}

/// Types a command and the return that runs it.
fn run(shell: &mut Shell, command: &str) {
    shell.write(format!("{command}\n"));
}

/// Keeps looking at what is running until `ready`, or fails saying what it was
/// waiting for, what the shell is called and what is on its screen.
fn wait_for(shell: &mut Shell, expectation: &str, mut ready: impl FnMut(&Shell) -> bool) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        shell.poll_name();
        if ready(shell) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "waited {PATIENCE:?} for {expectation}; the shell is called {:?}, \
             {:?} is running in it, and the screen said:\n{}",
            shell.name(),
            shell.naming().foreground(),
            shell.screen().join("\n")
        );
        thread::sleep(GLANCE);
    }
}

/// A stand-in for the kernel: says whatever it has been told to say, and counts
/// how often it is asked.
#[derive(Debug, Default)]
struct Pretend {
    running: RefCell<Option<Foreground>>,
    looks: Cell<usize>,
}

impl Pretend {
    /// From now on, this is what is in front of every terminal.
    fn running(&self, name: &str) {
        *self.running.borrow_mut() = Some(Foreground::new(4471, name));
    }

    /// From now on, nothing is.
    fn nothing(&self) {
        *self.running.borrow_mut() = None;
    }

    /// How many times it has been asked.
    fn looks(&self) -> usize {
        self.looks.get()
    }
}

impl ForegroundProcess for Pretend {
    fn foreground(&self, device: &Device) -> Option<Foreground> {
        let _ = device;
        self.looks.set(self.looks.get() + 1);
        self.running.borrow().clone()
    }
}

#[test]
fn a_shell_is_called_after_what_is_running_in_it() {
    let mut shell = shell();
    assert_eq!(
        shell.name(),
        None,
        "a shell nothing has looked at yet claims a name"
    );

    wait_for(&mut shell, "a shell to be looked at", |shell| {
        shell.name().is_some()
    });
    let at_a_prompt = shell.name().expect("a name to have been found").to_owned();

    // `cat` with nothing to read sits in the terminal's foreground until it is
    // told to stop, which is what makes it visible to a look.
    run(&mut shell, "cat");
    wait_for(&mut shell, "the running command's name", |shell| {
        shell.name() == Some("cat")
    });

    // End-of-file, which is what a person pressing ctrl-D sends.
    shell.write([0x04]);
    wait_for(&mut shell, "the shell's own name to come back", |shell| {
        shell.name() == Some(at_a_prompt.as_str())
    });
}

#[test]
fn what_is_running_is_a_process_group_that_is_really_there() {
    let mut shell = shell();
    run(&mut shell, "cat");
    wait_for(&mut shell, "a command to be looked at", |shell| {
        shell.name() == Some("cat")
    });

    let foreground = shell
        .naming()
        .foreground()
        .expect("something to be running")
        .clone();
    assert_eq!(foreground.name(), "cat");
    assert!(
        foreground.group() > 0,
        "a process group of {}",
        foreground.group()
    );
    // Sending nothing to a process group asks the kernel whether it exists.
    // Safe by construction: `kill` with a signal of zero reads kernel state and
    // touches no memory this process owns.
    let alive = unsafe { libc::kill(-foreground.group(), 0) };
    assert_eq!(alive, 0, "the group named is not running");
}

#[test]
fn a_name_somebody_chose_outlives_everything_that_runs_under_it() {
    let mut shell = shell();
    shell.set_name("deploy");

    run(&mut shell, "cat");
    wait_for(&mut shell, "a command to be noticed", |shell| {
        shell.naming().foreground().map(Foreground::name) == Some("cat")
    });

    assert_eq!(
        shell.name(),
        Some("deploy"),
        "a shell somebody named was renamed by what it is running"
    );
    assert!(shell.naming().is_chosen());
    assert_eq!(shell.naming().chosen(), Some("deploy"));

    // Giving the chosen name up shows what is running now, which was being
    // watched all along.
    shell.clear_name();
    assert!(!shell.naming().is_chosen());
    assert_eq!(shell.name(), Some("cat"));
}

#[test]
fn a_shell_nobody_has_named_and_nothing_has_looked_at_has_no_name() {
    let name = ShellName::new();

    assert_eq!(name.name(), None);
    assert_eq!(name.chosen(), None);
    assert_eq!(name.foreground(), None);
    assert!(!name.is_chosen());
    assert_eq!(name, ShellName::default());
}

#[test]
fn nothing_is_looked_at_twice_inside_one_interval() {
    let shell = shell();
    let kernel = Pretend::default();
    kernel.running("vim");
    let mut name = ShellName::new();

    assert!(
        name.poll(shell.device(), &kernel),
        "the first look renamed it"
    );
    assert_eq!(name.name(), Some("vim"));
    assert_eq!(kernel.looks(), 1);

    kernel.running("cargo");
    assert!(!name.poll(shell.device(), &kernel));
    assert_eq!(
        name.name(),
        Some("vim"),
        "a look was taken before the interval was up"
    );
    assert_eq!(kernel.looks(), 1);

    thread::sleep(FOREGROUND_INTERVAL);

    assert!(name.poll(shell.device(), &kernel));
    assert_eq!(name.name(), Some("cargo"));
    assert_eq!(kernel.looks(), 2);
}

#[test]
fn a_look_that_finds_nothing_leaves_a_shell_with_no_name() {
    let shell = shell();
    let kernel = Pretend::default();
    let mut name = ShellName::new();

    assert!(
        !name.poll(shell.device(), &kernel),
        "nothing where there was nothing is not a change"
    );
    assert_eq!(name.name(), None);
    assert_eq!(name.foreground(), None);

    kernel.running("cat");
    thread::sleep(FOREGROUND_INTERVAL);
    assert!(name.poll(shell.device(), &kernel));
    assert_eq!(name.name(), Some("cat"));

    // A terminal with nothing in front of it is a real answer, and it is not
    // answered with the name of something that has gone.
    kernel.nothing();
    thread::sleep(FOREGROUND_INTERVAL);
    assert!(name.poll(shell.device(), &kernel));
    assert_eq!(name.name(), None);
    assert_eq!(name.foreground(), None);
}

#[test]
fn a_chosen_name_is_never_a_change_however_much_moves_under_it() {
    let shell = shell();
    let kernel = Pretend::default();
    kernel.running("vim");
    let mut name = ShellName::new();
    name.set("deploy");

    assert!(
        !name.poll(shell.device(), &kernel),
        "a shell named by hand reported being renamed"
    );
    assert_eq!(name.name(), Some("deploy"));
    assert_eq!(
        name.foreground().map(Foreground::name),
        Some("vim"),
        "what is running stopped being watched while a chosen name stood"
    );

    kernel.running("cargo");
    thread::sleep(FOREGROUND_INTERVAL);
    assert!(!name.poll(shell.device(), &kernel));
    assert_eq!(name.name(), Some("deploy"));

    // What takes over is what is running now, not what was running when the
    // chosen name was set.
    name.clear();
    assert_eq!(name.name(), Some("cargo"));
}

#[test]
fn a_foreground_process_is_a_group_and_a_name() {
    let foreground = Foreground::new(4471, "claude");

    assert_eq!(foreground.group(), 4471);
    assert_eq!(foreground.name(), "claude");
    assert_eq!(foreground, Foreground::new(4471, "claude".to_owned()));
    assert_ne!(foreground, Foreground::new(4472, "claude"));
}
