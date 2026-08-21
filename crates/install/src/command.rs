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
//!
//! The other thing built here is a command this program never runs: the line an
//! agent is given to run a wrapper with. It belongs beside the two above because
//! it is the same problem — one program naming another to a shell — and because
//! getting the quoting of it wrong fails in the worst possible place, months
//! later, inside somebody's session, silently.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process;

use crate::Error;
use crate::agent::Agent;
use crate::paths::Platform;

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

/// What a machine that runs its scripts by extension is asked to run one with.
///
/// Its own profile is left out, because a hook has to behave the same for every
/// user on the machine, and the execution policy is set aside for this one run,
/// because a wrapper this program wrote is not the sort of downloaded script
/// that policy exists to stop.
const POWERSHELL: &str = "powershell -NoProfile -ExecutionPolicy Bypass";

/// How `agent` is told to run the wrapper installed at `script`, on a machine
/// of `platform`.
///
/// The interpreter is named, rather than left to the file to say for itself,
/// because the file is written by this program and read by somebody else's:
/// naming it means the wrapper needs no first line about itself, needs no
/// executable bit on a machine that has one, and cannot be run by whatever a
/// user's login shell happens to be.
///
/// An `argument` is passed on for the agents whose payload does not say which
/// event it is about, so that the wrapper can be told. Whether an agent needs
/// one is a question about that agent.
///
/// A path that is not text is refused rather than approximated. The lossy
/// spelling of it would install, and produce hooks that run a command which is
/// not there — a failure that shows up as an agent quietly emitting nothing,
/// which is the hardest kind to attribute to its cause.
pub fn hook_command(
    agent: Agent,
    platform: Platform,
    script: &Path,
    argument: Option<&str>,
) -> Result<String, Error> {
    let path = text(script)?;
    let command = match platform {
        Platform::Unix => format!("{} {}", interpreter(agent), single_quoted(path)),
        Platform::Windows => format!("{POWERSHELL} -File {}", double_quoted(path)),
    };
    Ok(with(command, argument))
}

/// The same command, spelled so that a hook runner cannot mangle it.
///
/// Some agents put what they are given through a shell of their own before
/// running it, and a path with a space or a quote in it does not survive that.
/// A machine that runs its scripts by extension will take the whole command
/// encoded instead, where there is nothing left for anything to reinterpret.
/// The other kind of machine has no such spelling and does not need one: the
/// quoting there is the shell's own, and it survives being read twice.
pub fn encoded_hook_command(
    agent: Agent,
    platform: Platform,
    script: &Path,
    argument: Option<&str>,
) -> Result<String, Error> {
    if platform == Platform::Unix {
        return hook_command(agent, platform, script, argument);
    }
    // Doubling is how a literal quote is written inside a quoted string there,
    // and the call operator is how a path that has been quoted is run rather
    // than printed.
    let quoted = text(script)?.replace('\'', "''");
    let script = with(format!("& '{quoted}'"), argument);
    Ok(format!(
        "{POWERSHELL} -EncodedCommand {}",
        base64(&utf16(&script))
    ))
}

/// A command with the argument that says what it is about, if it has one.
fn with(mut command: String, argument: Option<&str>) -> String {
    if let Some(argument) = argument {
        command.push(' ');
        command.push_str(argument);
    }
    command
}

/// What runs `agent`'s wrapper on a machine whose commands are files with the
/// executable bit set.
///
/// One agent's wrapper stays inside the older shell standard and is named to
/// the shell every such machine has, so that it runs on machines with no `bash`
/// on them at all. The rest use what `bash` adds and are named to it.
fn interpreter(agent: Agent) -> &'static str {
    match agent {
        Agent::Grok => "sh",
        _ => "bash",
    }
}

/// A path as a shell reads it: quoted, with the one character that ends the
/// quoting escaped.
fn single_quoted(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

/// A path as a machine that runs its scripts by extension reads it.
fn double_quoted(path: &str) -> String {
    format!("\"{}\"", path.replace('"', "\\\""))
}

/// A path as text, or a refusal if it is not.
fn text(path: &Path) -> Result<&str, Error> {
    path.to_str().ok_or_else(|| Error::Unwritable {
        path: path.to_owned(),
    })
}

/// `text` in the encoding a command handed over encoded is spelled in.
fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// `bytes` in the encoding of RFC 4648 §4.
///
/// Written out rather than taken from elsewhere: it is one table and one loop,
/// used in one place, and a dependency for it would be more to keep an eye on
/// than to keep.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let mut whole = [0_u8; 3];
        whole[..group.len()].copy_from_slice(group);
        let bits = (u32::from(whole[0]) << 16) | (u32::from(whole[1]) << 8) | u32::from(whole[2]);
        for (place, index) in [bits >> 18, (bits >> 12) & 63, (bits >> 6) & 63, bits & 63]
            .into_iter()
            .enumerate()
        {
            // Every group is four characters wide however few bytes went into
            // it; the places no byte reached are filled rather than left out.
            encoded.push(match place > group.len() {
                true => '=',
                false => char::from(ALPHABET[index as usize]),
            });
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of the shell that does `body`, with `args` as its arguments.
    ///
    /// These tests need programs that exit and print to order, and the shell is
    /// asked to be them rather than a script being written for the occasion.
    /// That is not squeamishness about temporary files: a file this process
    /// holds open for writing cannot be executed, and a *sibling* thread that
    /// forks in that window inherits the handle and keeps the file unrunnable
    /// until it execs something of its own. A test suite that writes a program
    /// and immediately runs it is therefore intermittently unable to run it, for
    /// reasons that have nothing to do with what is being tested.
    fn shell(body: &str, args: &[&str]) -> Invocation {
        // Everything after the command is positional, starting with the name the
        // shell reports itself by, so the caller's arguments arrive as `$1`
        // onwards exactly as they would for a script.
        let mut arguments = vec!["-c".to_owned(), body.to_owned(), "sh".to_owned()];
        arguments.extend(args.iter().map(|argument| (*argument).to_owned()));
        Invocation::new("/bin/sh", arguments)
    }

    /// A run of `program` with no arguments, spelled so the item type is known.
    fn bare(program: impl Into<PathBuf>) -> Invocation {
        Invocation::new(program, Vec::<String>::new())
    }

    /// Where a wrapper sits on a machine that keeps its files under a name with
    /// nothing in it needing care.
    fn script() -> PathBuf {
        PathBuf::from("/home/u/.local/share/agentbus/claude/agentbus-hook.sh")
    }

    #[test]
    fn a_wrapper_is_named_to_the_interpreter_that_runs_it() {
        assert_eq!(
            hook_command(Agent::Claude, Platform::Unix, &script(), None).unwrap(),
            format!("bash '{}'", script().display())
        );
        assert_eq!(
            hook_command(Agent::Grok, Platform::Unix, &script(), None).unwrap(),
            format!("sh '{}'", script().display()),
            "the one agent whose wrapper stays inside the older standard"
        );
    }

    #[test]
    fn a_wrapper_can_be_told_which_event_it_is_about() {
        assert_eq!(
            hook_command(Agent::Grok, Platform::Unix, &script(), Some("session")).unwrap(),
            format!("sh '{}' session", script().display())
        );
    }

    #[test]
    fn a_path_a_shell_could_read_twice_is_quoted_so_that_it_cannot() {
        let awkward = PathBuf::from("/home/o'brien/a b/agentbus-hook.sh");

        assert_eq!(
            hook_command(Agent::Claude, Platform::Unix, &awkward, None).unwrap(),
            "bash '/home/o'\\''brien/a b/agentbus-hook.sh'"
        );
    }

    #[test]
    fn a_machine_that_runs_its_scripts_by_extension_is_told_what_runs_them() {
        let script = PathBuf::from(r"C:\Users\u\agentbus\agentbus-hook.ps1");

        assert_eq!(
            hook_command(Agent::Claude, Platform::Windows, &script, Some("session")).unwrap(),
            format!(
                "powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\" session",
                script.display()
            )
        );
    }

    #[test]
    fn a_command_handed_over_encoded_says_the_same_thing() {
        let script = PathBuf::from(r"C:\Users\u\a b\agentbus-hook.ps1");

        let command = encoded_hook_command(
            Agent::Mastracode,
            Platform::Windows,
            &script,
            Some("session"),
        )
        .unwrap();

        let encoded = command
            .strip_prefix("powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand ")
            .unwrap_or_else(|| panic!("{command}"));
        assert_eq!(
            decoded(encoded),
            format!("& '{}' session", script.display())
        );
    }

    #[test]
    fn there_is_nothing_to_encode_where_a_shell_reads_the_command() {
        assert_eq!(
            encoded_hook_command(Agent::Mastracode, Platform::Unix, &script(), None).unwrap(),
            hook_command(Agent::Mastracode, Platform::Unix, &script(), None).unwrap()
        );
    }

    #[test]
    fn what_is_encoded_is_what_the_standard_says_it_is() {
        // The examples from RFC 4648 section 10, which exercise each of the
        // three ways a group can end.
        for (plain, encoded) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v")] {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
    }

    #[test]
    fn a_path_that_is_not_text_is_refused_rather_than_approximated() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = PathBuf::from(OsStr::from_bytes(b"/home/\xff/agentbus-hook.sh"));

        assert!(matches!(
            hook_command(Agent::Claude, Platform::Unix, &path, None),
            Err(Error::Unwritable { .. })
        ));
        assert!(matches!(
            encoded_hook_command(Agent::Claude, Platform::Windows, &path, None),
            Err(Error::Unwritable { .. })
        ));
    }

    /// What `encoded` was made from, read back the way the machine reading it
    /// would.
    fn decoded(encoded: &str) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

        let mut bits = Vec::new();
        for character in encoded.bytes().filter(|byte| *byte != b'=') {
            let index = ALPHABET
                .iter()
                .position(|letter| *letter == character)
                .expect("not one of the letters the encoding uses");
            for shift in (0..6).rev() {
                bits.push((index >> shift) & 1 == 1);
            }
        }
        let bytes: Vec<u8> = bits
            .chunks_exact(8)
            .map(|byte| {
                byte.iter()
                    .fold(0_u8, |whole, bit| (whole << 1) | u8::from(*bit))
            })
            .collect();
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units).expect("not what was encoded")
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
        assert!(shell("exit 0", &[]).run().is_ok());
        assert!(matches!(
            shell("exit 1", &[]).run(),
            Err(Error::CommandFailed {
                status: Some(1),
                ..
            })
        ));
    }

    #[test]
    fn a_question_is_answered_by_what_the_command_printed() {
        assert_eq!(
            shell("printf '%s' \"$1\"", &["hello"]).ask(),
            Some("hello".to_owned())
        );
    }

    #[test]
    fn a_question_the_command_cannot_answer_is_left_unanswered() {
        assert_eq!(shell("printf 'partial'; exit 2", &[]).ask(), None);
        assert_eq!(bare("no-such-command-anywhere").ask(), None);
    }
}
