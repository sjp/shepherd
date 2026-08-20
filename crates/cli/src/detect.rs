//! Reading a captured screen and saying what it is evidence of.
//!
//! The detection library answers this question for programs written in the same
//! language as it; this module answers it for everybody else. A screen arrives
//! as plain text on stdin, an answer leaves on stdout, and what captured the
//! screen — a terminal multiplexer's capture command, a script, a log — is none
//! of this command's business.
//!
//! # Three answers to the same question
//!
//! A word is what a shell script wants: one token to compare against. The
//! verdict as JSON is what a program wants, and it is the library's own struct
//! rather than a shape invented here, so a caller that later links the library
//! reads the same fields. The explanation is what a manifest's author wants —
//! every rule that ran, the text it saw, and why the winner won — and it is the
//! reason the loop of capture, ask, adjust and capture again can be run by
//! somebody who is not holding a debugger.
//!
//! # Bounded input
//!
//! Stdin is read to a ceiling and then stopped. A screen is a few kilobytes;
//! anything past a megabyte is a pipe that is not going to end, and a detection
//! over the first megabyte is still a detection. Truncating and saying so is
//! therefore better than either refusing or buffering forever.

use std::io::{self, Read};

use agentbus_detect::{ManifestStore, ProcessInfo, ScreenInput};

/// The most of a screen this reads, after which the rest is dropped.
///
/// Two orders of magnitude above the largest terminal anyone is looking at, and
/// the same ceiling the hook path puts on a payload, so that neither end of
/// this program can be made to hold an unbounded amount of somebody else's
/// text.
pub const MAX_SCREEN: usize = 1024 * 1024;

/// What is said on stderr when there was more on stdin than [`MAX_SCREEN`].
pub const TRUNCATED: &str = "stdin was longer than the screen limit; the rest was ignored";

/// What is said on stderr when nobody could be named.
pub const UNIDENTIFIED: &str = "no agent identified";

/// The evidence one invocation was given.
///
/// Every field is a string the caller obtained however it liked; nothing here
/// inspects the machine. The two process fields are exactly what `agentbus
/// foreground` prints in its own columns of those names, so the two commands
/// compose without anything in between having to reshape them.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// `--agent`: whose screen this is, when the caller already knows.
    pub agent: Option<&'a str>,
    /// `--process`: the short name of the process drawing the screen.
    pub process: &'a str,
    /// `--cmdline`: the command line that process was started with.
    pub cmdline: &'a str,
    /// The screen itself, as plain text.
    pub screen: &'a str,
    /// `--osc-title`: the last title the agent asked the terminal to show.
    pub osc_title: &'a str,
    /// `--osc-progress`: the last progress report it sent the terminal.
    pub osc_progress: &'a str,
}

/// How much of the answer to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// One word: the state and nothing else.
    Word,
    /// The verdict as a JSON object.
    Json,
    /// The whole evaluation as a JSON object.
    Explain,
}

/// An answered detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// What belongs on stdout, ending in a newline.
    pub text: String,
    /// What the store passed over on the way to the manifest that answered — a
    /// copy it could not read, one it declined as older than the bundled one.
    /// Collected only where it is going to be shown: it explains an answer
    /// rather than qualifying it, and a caller that asked for one word asked
    /// for one word.
    pub warnings: Vec<String>,
}

/// What one screen says, in the form asked for, or nothing when there is no
/// agent to read it as.
///
/// Being unable to name the agent is not the same as being unable to classify
/// the screen, and the two must not collapse into one answer: `unknown` is a
/// verdict about a screen that was read, and this is the case where none was.
pub fn answer(store: &ManifestStore, request: &Request<'_>, form: Form) -> Option<Answer> {
    let agent = store.identify(request.agent, request.process())?;
    let input = ScreenInput {
        screen: request.screen,
        osc_title: request.osc_title,
        osc_progress: request.osc_progress,
    };

    Some(match form {
        Form::Word => Answer {
            text: format!("{}\n", store.detect(agent.as_str(), input).state),
            warnings: Vec::new(),
        },
        Form::Json => Answer {
            text: line(&store.detect(agent.as_str(), input)),
            warnings: Vec::new(),
        },
        Form::Explain => {
            let explanation = store.explain(agent.as_str(), input);
            Answer {
                text: line(&explanation),
                warnings: explanation.warnings.clone(),
            }
        }
    })
}

impl Request<'_> {
    /// The process evidence, when there is any.
    ///
    /// Neither field given is different from both given as empty strings: the
    /// first is a caller with nothing to say about the process, and passing it
    /// on as evidence would ask the identifier to compare manifests against
    /// nothing.
    fn process(&self) -> Option<ProcessInfo<'_>> {
        (!self.process.is_empty() || !self.cmdline.is_empty()).then_some(ProcessInfo {
            comm: self.process,
            cmdline: self.cmdline,
        })
    }
}

/// One value as a single line of JSON.
///
/// Compact rather than indented, because both shapes printed here are read by
/// programs first: a person who wants it laid out has a formatter to hand, and
/// a line is what a stream of them would have to be anyway.
fn line(value: &impl serde::Serialize) -> String {
    match serde_json::to_string(value) {
        Ok(json) => format!("{json}\n"),
        // Neither shape holds a map with non-string keys or a float that is not
        // a number, which is all that can fail here. An impossible failure is
        // still not a reason to write half an object into somebody's parser.
        Err(_) => String::new(),
    }
}

/// A screen read from `input`, and whether there was more of it than was kept.
///
/// Invalid UTF-8 is replaced rather than refused. What arrives here has usually
/// been through a capture and an escape-sequence filter already, and half a
/// multi-byte character at the cut is the ordinary way that ends; a rule that
/// wanted the text either side of it still matches.
pub fn read_screen(input: impl Read) -> io::Result<(String, bool)> {
    let mut bytes = Vec::new();
    // One byte past the cap, so that a stream exactly at the cap is not
    // reported as having been cut.
    input.take(MAX_SCREEN as u64 + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > MAX_SCREEN;
    bytes.truncate(MAX_SCREEN);
    Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screen_within_the_cap_is_read_whole() {
        let (screen, truncated) =
            read_screen("two lines\nof screen\n".as_bytes()).expect("a reader that cannot fail");

        assert_eq!(screen, "two lines\nof screen\n");
        assert!(!truncated);
    }

    #[test]
    fn a_screen_exactly_at_the_cap_is_not_reported_as_cut() {
        let whole = "x".repeat(MAX_SCREEN);

        let (screen, truncated) = read_screen(whole.as_bytes()).expect("a reader that cannot fail");

        assert_eq!(screen.len(), MAX_SCREEN);
        assert!(!truncated);
    }

    #[test]
    fn a_longer_screen_is_cut_and_says_so() {
        let flood = "x".repeat(MAX_SCREEN * 2);

        let (screen, truncated) = read_screen(flood.as_bytes()).expect("a reader that cannot fail");

        assert_eq!(screen.len(), MAX_SCREEN);
        assert!(truncated);
    }

    #[test]
    fn process_evidence_is_absent_only_when_both_fields_are() {
        let empty = Request {
            agent: None,
            process: "",
            cmdline: "",
            screen: "",
            osc_title: "",
            osc_progress: "",
        };
        assert_eq!(empty.process(), None);

        let named = Request {
            process: "claude",
            ..empty
        };
        assert_eq!(
            named.process(),
            Some(ProcessInfo {
                comm: "claude",
                cmdline: ""
            })
        );
    }
}
