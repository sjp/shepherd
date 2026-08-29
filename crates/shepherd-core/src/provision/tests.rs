//! Which containers the bus is put into, and how often.
//!
//! Most of these go through a stand-in that records the containers it was asked
//! about, because the question is the bookkeeping — one installation per
//! container, a restarted container counted as a new one, a refusal recorded
//! rather than thrown away — and a real container would answer it no better
//! while making it unaskable on a machine without one.
//!
//! The rest run a real command, with a directory of this test's own standing in
//! for the machine's `PATH` and a script in it standing in for the bus. See
//! [`bus_in`], which has something to say about writing a program and then
//! running it.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use super::*;

/// How long a test waits for a script it has just written to become runnable.
const PATIENCE: Duration = Duration::from_secs(20);

/// How often it looks while waiting.
const GLANCE: Duration = Duration::from_millis(10);

/// The workspace every bookkeeping test is about.
fn workspace() -> WorkspaceId {
    WorkspaceId::from_raw(1)
}

/// And a second one, for the tests about two workspaces at once.
fn other() -> WorkspaceId {
    WorkspaceId::from_raw(2)
}

/// A bus that is never run: it records which containers it was asked to install
/// into, and answers what the test told it to.
#[derive(Debug, Default)]
struct Stand {
    asked: RefCell<Vec<String>>,
    answers: RefCell<VecDeque<Provisioned>>,
}

impl Stand {
    /// One that agrees with everything.
    fn agreeable() -> Self {
        Self::default()
    }

    /// The same, answering `answers` to its first runs in turn.
    fn answering(self, answers: impl IntoIterator<Item = Provisioned>) -> Self {
        *self.answers.borrow_mut() = answers.into_iter().collect();
        self
    }

    /// Every container it was asked about, in order.
    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

impl Provisions for Stand {
    fn provision(&self, container: &str) -> Result<Provisioned, ProvisionError> {
        self.asked.borrow_mut().push(container.to_owned());
        Ok(self
            .answers
            .borrow_mut()
            .pop_front()
            .unwrap_or(Provisioned::Installed))
    }
}

/// Does for `container` whatever a caller would: asks whether the bus has to go
/// in, puts it in if so, and records what became of that.
fn provision(
    provisioning: &mut Provisioning,
    bus: &impl Provisions,
    workspace: WorkspaceId,
    container: &str,
) {
    if !provisioning.using(workspace, container) {
        return;
    }
    let installed = bus
        .provision(container)
        .is_ok_and(|provisioned| provisioned.installed());
    provisioning.provisioned(container, installed);
}

/// A directory holding a command called `agentbus` that does `body`, and a bus
/// that looks for commands only there.
///
/// Written rather than borrowed from the machine, because what these tests want
/// to see is what the command was *asked*, and a real installation would answer
/// that by changing a container.
///
/// The wait is not superstition. A file this process has open for writing
/// cannot be executed, and a sibling test that starts a process in that window
/// inherits the handle and keeps the file unrunnable until it execs something
/// of its own — a failure with nothing to do with what is being tested. Running
/// the script once, patiently, until the machine agrees it is a program is what
/// closes that window rather than merely narrowing it.
#[cfg(unix)]
fn bus_in(dir: &Path, body: &str) -> Bus {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(COMMAND);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("a script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("a script anybody may run");

    let deadline = Instant::now() + PATIENCE;
    while let Err(problem) = Command::new(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        assert!(
            Instant::now() < deadline,
            "the command written for this test never became runnable: {problem}"
        );
        thread::sleep(GLANCE);
    }
    Bus::searching([dir])
}

#[test]
fn the_bus_goes_into_a_container_once() {
    let bus = Stand::agreeable();
    let mut provisioning = Provisioning::new();

    for _ in 0..3 {
        provision(&mut provisioning, &bus, workspace(), "brave_kepler");
    }

    assert_eq!(
        bus.asked(),
        vec!["brave_kepler".to_owned()],
        "the bus was put into one container more than once, and the command is idempotent"
    );
    assert_eq!(provisioning.of(workspace()), Some(Standing::Ready));
}

#[test]
fn a_container_that_has_been_restarted_is_a_container_the_bus_goes_into_again() {
    let bus = Stand::agreeable();
    let mut provisioning = Provisioning::new();

    provision(&mut provisioning, &bus, workspace(), "brave_kepler");
    // What a restart looks like from here: the command brought a container up
    // and named a different one.
    provision(&mut provisioning, &bus, workspace(), "eager_mclean");

    assert_eq!(
        bus.asked(),
        vec!["brave_kepler".to_owned(), "eager_mclean".to_owned()],
        "the bus was not put into the container that replaced the one it was in"
    );
}

#[test]
fn two_workspaces_sharing_a_container_are_one_installation() {
    let bus = Stand::agreeable();
    let mut provisioning = Provisioning::new();

    provision(&mut provisioning, &bus, workspace(), "brave_kepler");
    provision(&mut provisioning, &bus, other(), "brave_kepler");

    assert_eq!(bus.asked(), vec!["brave_kepler".to_owned()]);
    assert_eq!(provisioning.of(workspace()), Some(Standing::Ready));
    assert_eq!(provisioning.of(other()), Some(Standing::Ready));
}

#[test]
fn a_container_being_provisioned_is_not_provisioned_again_underneath() {
    let mut provisioning = Provisioning::new();

    assert!(provisioning.using(workspace(), "brave_kepler"));
    assert_eq!(
        provisioning.of(workspace()),
        Some(Standing::UnderWay),
        "a container nothing has finished with says nothing about whether agents in it are reported"
    );
    assert!(
        !provisioning.using(other(), "brave_kepler"),
        "a second workspace asked while the first is still installing would install twice over"
    );
}

#[test]
fn a_refusal_is_a_container_whose_agents_will_not_be_reported() {
    let bus = Stand::agreeable().answering([Provisioned::Refused {
        status: Some(1),
        said: "no such container".to_owned(),
    }]);
    let mut provisioning = Provisioning::new();

    provision(&mut provisioning, &bus, workspace(), "brave_kepler");

    assert_eq!(provisioning.of(workspace()), Some(Standing::Unreported));
}

#[test]
fn a_workspace_using_no_container_has_nothing_to_say() {
    let provisioning = Provisioning::new();
    assert_eq!(provisioning.of(workspace()), None);
}

#[test]
fn a_closed_workspace_is_forgotten_and_its_container_is_not() {
    let bus = Stand::agreeable();
    let mut provisioning = Provisioning::new();
    provision(&mut provisioning, &bus, workspace(), "brave_kepler");

    provisioning.forget(workspace());

    assert_eq!(provisioning.of(workspace()), None);
    provision(&mut provisioning, &bus, other(), "brave_kepler");
    assert_eq!(
        bus.asked(),
        vec!["brave_kepler".to_owned()],
        "the container was installed into again for a workspace that arrived at the same one"
    );
}

#[test]
#[cfg(unix)]
fn the_command_is_asked_to_install_into_the_container_by_name() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let bus = bus_in(dir.path(), r#"echo "$@" > "$(dirname "$0")/asked""#);

    assert_eq!(
        bus.provision("brave_kepler").expect("a run"),
        Provisioned::Installed
    );

    let asked = std::fs::read_to_string(dir.path().join("asked")).expect("what it was asked");
    assert_eq!(asked.trim(), "install docker brave_kepler");
}

#[test]
#[cfg(unix)]
fn a_command_that_refuses_is_carried_back_with_what_it_said() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let bus = bus_in(dir.path(), "echo 'no such container' >&2; exit 4");

    assert_eq!(
        bus.provision("brave_kepler").expect("a run"),
        Provisioned::Refused {
            status: Some(4),
            said: "no such container".to_owned(),
        }
    );
}

#[test]
#[cfg(unix)]
fn a_command_that_refuses_without_saying_why_still_says_something() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let bus = bus_in(dir.path(), "exit 1");

    assert_eq!(
        bus.provision("brave_kepler").expect("a run"),
        Provisioned::Refused {
            status: Some(1),
            said: UNEXPLAINED.to_owned(),
        }
    );
}

#[test]
fn a_machine_without_the_bus_says_so_rather_than_running_something_else() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let bus = Bus::searching([dir.path()]);

    assert!(matches!(
        bus.provision("brave_kepler"),
        Err(ProvisionError::NotInstalled)
    ));
}
