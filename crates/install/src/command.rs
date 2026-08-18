//! Running a coding agent's own tool.
//!
//! Some agents will not accept a plugin by having one written into a directory:
//! the registration lives somewhere this program has no business writing, and
//! the only supported way to change it is to run the command the agent ships for
//! exactly that purpose. So an installation is not always a set of files, and
//! this is the other half of it.
//!
//! Two kinds of run happen here and they fail differently. A command that
//! *changes* something is part of the installation, and a failure is the
//! installation's failure — reported with the command line in it, so that a user
//! can see what to run themselves. A command that only *asks* something is used
//! to work out whether a step is needed, and an unanswered question is not a
//! failure: every step this asks about is safe to take again, so not knowing
//! means taking it.

use std::ffi::OsStr;
use std::fmt;
use std::path::PathBuf;
use std::process;

use crate::Error;

/// One run of another program, with its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    program: PathBuf,
    args: Vec<String>,
}

impl Invocation {
    /// A run of `program` with `args`.
    ///
    /// `program` is the path a command was found at rather than its bare name,
    /// wherever the caller has one, so that what runs is the program that was
    /// detected rather than whatever a `PATH` resolves to a moment later.
    pub fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// Runs it, and fails unless it reports success.
    ///
    /// Whatever it printed goes wherever this process's own output goes. These
    /// are the agent's own commands reporting on the agent's own state, and a
    /// user who has just been told that an installation ran one is entitled to
    /// read what it said.
    pub fn run(&self) -> Result<(), Error> {
        let status = process::Command::new(&self.program)
            .args(&self.args)
            .status()
            .map_err(|source| Error::CannotRun {
                command: self.to_string(),
                source,
            })?;
        match status.success() {
            true => Ok(()),
            false => Err(Error::CommandFailed {
                command: self.to_string(),
                status: status.code(),
            }),
        }
    }

    /// Runs it and answers with what it printed, or with nothing if it could not
    /// be run or did not report success.
    ///
    /// Its own diagnostics are discarded and it is given no input, because this
    /// is a question asked while working out a plan: nothing has happened yet,
    /// and a report about a command the user did not ask for would be a report
    /// about nothing.
    pub fn ask(&self) -> Option<String> {
        let output = process::Command::new(&self.program)
            .args(&self.args)
            .stdin(process::Stdio::null())
            .stderr(process::Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).ok())
            .flatten()
    }
}

impl fmt::Display for Invocation {
    /// How the command would be typed, so that a message about one that failed
    /// is something a user can copy.
    ///
    /// The program is named the way it would be run rather than by the path it
    /// was found at, because the point of showing it is that somebody may have
    /// to run it themselves.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self
            .program
            .file_name()
            .unwrap_or_else(|| OsStr::new("?"))
            .to_string_lossy();
        f.write_str(&name)?;
        for arg in &self.args {
            write!(f, " {arg}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;

    /// A script at `path` that does `body`, runnable.
    fn script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A run of `program` with no arguments, spelled so the item type is known.
    fn bare(program: impl Into<PathBuf>) -> Invocation {
        Invocation::new(program, Vec::<String>::new())
    }

    #[test]
    fn a_command_is_shown_the_way_it_would_be_typed() {
        let invocation = Invocation::new("/usr/local/bin/claude", ["plugin", "list", "--json"]);

        assert_eq!(invocation.to_string(), "claude plugin list --json");
    }

    #[test]
    fn a_command_that_cannot_be_found_names_itself_in_the_failure() {
        // Named after nothing that could be on the machine running this: the
        // point is what happens when the program is absent, and a name that
        // resolved would be testing the opposite case.
        let invocation = Invocation::new("no-such-agent-anywhere", ["plugin", "install"]);

        let Err(Error::CannotRun { command, .. }) = invocation.run() else {
            panic!("running a command that is not there should have failed");
        };
        assert_eq!(command, "no-such-agent-anywhere plugin install");
    }

    #[test]
    fn a_command_that_refuses_is_a_failure_and_one_that_agrees_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let (good, bad) = (dir.path().join("good"), dir.path().join("bad"));
        script(&good, "exit 0");
        script(&bad, "exit 1");

        assert!(bare(&good).run().is_ok());
        assert!(matches!(
            bare(&bad).run(),
            Err(Error::CommandFailed {
                status: Some(1),
                ..
            })
        ));
    }

    #[test]
    fn a_question_is_answered_by_what_the_command_printed() {
        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("say");
        script(&command, "printf '%s' \"$1\"");

        assert_eq!(
            Invocation::new(&command, ["hello"]).ask(),
            Some("hello".to_owned())
        );
    }

    #[test]
    fn a_question_the_command_cannot_answer_is_left_unanswered() {
        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("refuse");
        script(&command, "printf 'partial'; exit 2");

        assert_eq!(bare(&command).ask(), None);
        assert_eq!(bare("no-such-command-anywhere").ask(), None);
    }
}
