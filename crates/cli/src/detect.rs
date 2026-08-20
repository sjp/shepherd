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
//! # The fourth thing to do with a verdict, which is not to print it
//!
//! A verdict on stdout is only useful to whoever is holding the pipe. The bus
//! is where a verdict becomes something anybody watching can act on, so a
//! detection can also be sent as a claim about the correlated slot the screen
//! was captured from — see [`claim`] and [`send`]. That is the whole of what
//! this program does with a screen that nobody is reading interactively: the
//! caller loops over its terminals, and the bus arbitrates whatever else it is
//! being told about the same slots.
//!
//! # Bounded input
//!
//! Stdin is read to a ceiling and then stopped. A screen is a few kilobytes;
//! anything past a megabyte is a pipe that is not going to end, and a detection
//! over the first megabyte is still a detection. Truncating and saying so is
//! therefore better than either refusing or buffering forever.

use std::io::{self, Read};
use std::path::Path;
use std::time::Instant;

use agentbus_detect::{Detection, ManifestStore, ProcessInfo, ScreenInput, ScreenState};
use agentbus_protocol::{Agent, AssertedState, StateAssertion};
use serde_json::{Map, Value};

use crate::emit;

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

/// What is said on stderr when there is nothing to attribute a claim to.
pub const UNATTRIBUTABLE: &str =
    "nothing says which slot this screen was captured from; pass --correlation";

/// The key the winning rule is reported under in a claim's normalized extras.
const RULE: &str = "rule";

/// The keys the evidence is reported under. Both name a decision this program
/// made, and neither carries a character of the screen.
const MATCHED_RULE: &str = "matched_rule";
const FALLBACK: &str = "fallback";

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
    let input = request.input();

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

/// Who a claim is about, which is everything the screen itself cannot say.
///
/// A screen is text. What slot it was captured from, which session is drawing
/// it and where that session is working are all facts about the capture rather
/// than about the pixels, so they arrive here from whoever did the capturing
/// and are copied into the claim without being looked at.
#[derive(Debug, Clone, Copy)]
pub struct Attribution<'a> {
    /// The opaque string naming what was observed. Required: see [`claim`].
    pub correlation: &'a str,
    /// `--session`: the agent's own session id, where the caller knows it.
    pub session: Option<&'a str>,
    /// `--cwd`: where that session is working, where the caller knows it.
    pub cwd: Option<&'a str>,
}

/// What one screen is worth telling the bus.
#[derive(Debug, Clone, PartialEq)]
pub enum Claim {
    /// The claim to send. Boxed so that deciding to say nothing does not cost
    /// the several hundred bytes that saying something would have.
    Send(Box<StateAssertion>),
    /// Nothing at all, which is a result rather than a failure. A screen
    /// showing a record of the past says nothing about what is happening now,
    /// and a claim that it did would overwrite something true with something
    /// irrelevant.
    Nothing,
}

/// The claim one screen justifies, or nothing where there is no agent to read
/// it as.
///
/// `None` means what it means in [`answer`]: nobody could be named, so there
/// was no manifest to read the screen with. A screen that *was* read always
/// justifies something, even if that something is [`Claim::Nothing`].
///
/// What travels is the conclusion and the reasoning, never the screen. The
/// state, whether its chrome is live, which rule concluded it and why there was
/// no rule are all decisions this program made about text it was handed; the
/// text itself may hold anything at all its author was working on, and handing
/// that to a bus in exchange for a one-word status would be a poor trade for
/// whoever's terminal it was.
pub fn claim(
    store: &ManifestStore,
    request: &Request<'_>,
    attribution: &Attribution<'_>,
) -> Option<Claim> {
    let agent = store.identify(request.agent, request.process())?;
    let detection = store.detect(agent.as_str(), request.input());
    Some(claimed(agent, &detection, attribution))
}

/// The claim a detection that has already been made justifies.
fn claimed(agent: Agent, detection: &Detection, attribution: &Attribution<'_>) -> Claim {
    if detection.skip {
        return Claim::Nothing;
    }
    let mut assertion =
        StateAssertion::new(agent, attribution.correlation, asserted(detection.state))
            .with_visible(detection.visible)
            .with_raw(evidence(detection));
    if let Some(rule) = &detection.matched_rule {
        assertion = assertion.with_detail_field(RULE, rule.clone());
    }
    if let Some(session) = attribution.session {
        assertion = assertion.with_session(session);
    }
    if let Some(cwd) = attribution.cwd {
        assertion = assertion.with_cwd(cwd);
    }
    Claim::Send(Box::new(assertion))
}

/// The same state, in the vocabulary the bus speaks.
///
/// Two enums with the same four members, kept apart on purpose: the detection
/// library's is what a screen is evidence *of*, and the protocol's is what an
/// observer is prepared to *claim*. Nothing but a program doing both at once
/// needs them joined, so the join lives here rather than in either of them.
///
/// `unknown` is the one that carries a consequence. On the wire it withdraws
/// whatever this observer said before, which is the right reading of a screen
/// somebody's rules explicitly decline to interpret: the last confident answer
/// should stop standing rather than go on being repeated.
fn asserted(state: ScreenState) -> AssertedState {
    match state {
        ScreenState::Idle => AssertedState::Idle,
        ScreenState::Working => AssertedState::Working,
        ScreenState::Blocked => AssertedState::Blocked,
        ScreenState::Unknown => AssertedState::Unknown,
    }
}

/// Why the claim says what it says, in as few fields as will carry it.
fn evidence(detection: &Detection) -> Value {
    let mut evidence = Map::new();
    if let Some(rule) = &detection.matched_rule {
        evidence.insert(MATCHED_RULE.to_owned(), Value::from(rule.clone()));
    }
    if let Some(fallback) = detection.fallback {
        evidence.insert(FALLBACK.to_owned(), Value::from(fallback));
    }
    Value::Object(evidence)
}

/// Writes one claim to the bus at `socket`, if there is a bus there.
///
/// A caller of this is a loop: it captures a screen every second or so, for as
/// long as somebody leaves it running, which is longer than any one daemon
/// lives. So a bus that is not there is not an error and says nothing — the
/// loop is meant to survive the daemon being restarted under it, and a loop
/// that complained once per capture in the meantime would be worse than one
/// that said nothing at all.
///
/// The deadlines are the hook client's, and so is the code that applies them.
/// This is not a hook and nobody's editor is waiting on it, but a claim about
/// what a screen looks like now is worthless in ten seconds' time, and a loop
/// blocked on a wedged daemon is a loop that has stopped observing.
pub fn send(assertion: &StateAssertion, socket: &Path) -> io::Result<()> {
    // Asked before the line is built, for the same reason the hook client asks
    // it before it parses anything: the machine with no bus running is the
    // ordinary machine, and what it is owed is one `stat`.
    if !socket.exists() {
        return Ok(());
    }
    let mut line = serde_json::to_vec(assertion).map_err(io::Error::other)?;
    // The daemon reads to a newline or to end of input, so ending the line lets
    // it act without waiting for this process to close the connection.
    line.push(b'\n');
    match emit::deliver(&line, socket, Instant::now() + emit::BUDGET) {
        // The socket was there a moment ago and is not now, which is the same
        // situation as its never having been there.
        Err(error) if vanished(&error) => Ok(()),
        result => result,
    }
}

/// Whether a failure to deliver means the bus has gone rather than that
/// something went wrong.
///
/// The window is between the check above and the connect below it — a daemon
/// shutting down inside a few microseconds of being looked for. Rare, and
/// exactly the case the check exists to forgive, so it is forgiven here too
/// rather than reported as the one capture in a thousand that failed.
fn vanished(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

impl Request<'_> {
    /// The screen and its side channels, as the detection library takes them.
    fn input(&self) -> ScreenInput<'_> {
        ScreenInput {
            screen: self.screen,
            osc_title: self.osc_title,
            osc_progress: self.osc_progress,
        }
    }

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

    /// Every state a screen can be read as. The library does not enumerate
    /// them, so this does; [`asserted`] matches on all of them, which is what
    /// stops a state added there from being quietly left out of the claim.
    const SCREEN_STATES: [ScreenState; 4] = [
        ScreenState::Idle,
        ScreenState::Working,
        ScreenState::Blocked,
        ScreenState::Unknown,
    ];

    /// The slot every claim below is about.
    const SLOT: &str = "w9:p3";

    /// A detection with nothing decided, for a test to set one field of.
    fn detection(state: ScreenState) -> Detection {
        Detection {
            state,
            visible: false,
            skip: false,
            matched_rule: None,
            fallback: None,
        }
    }

    /// The slot and nothing else, which is all most of these know.
    fn about(correlation: &str) -> Attribution<'_> {
        Attribution {
            correlation,
            session: None,
            cwd: None,
        }
    }

    /// An agent id from a literal, which is what every one of these is.
    fn agent(name: &str) -> Agent {
        Agent::new(name).expect("a test's own agent id is a valid one")
    }

    /// The claim in `claimed`, failing the test if it decided not to make one.
    fn sent(claim: Claim) -> StateAssertion {
        match claim {
            Claim::Send(assertion) => *assertion,
            Claim::Nothing => panic!("nothing was claimed"),
        }
    }

    #[test]
    fn every_screen_state_is_exactly_one_state_the_bus_understands() {
        let mapped: Vec<AssertedState> = SCREEN_STATES.into_iter().map(asserted).collect();

        assert_eq!(
            mapped,
            vec![
                AssertedState::Idle,
                AssertedState::Working,
                AssertedState::Blocked,
                AssertedState::Unknown,
            ]
        );
        // Onto, and one-for-one: every state the protocol has is reached, and
        // none of them twice. With the mapping itself exhaustive over the
        // library's enum, that is the whole of the correspondence.
        for state in AssertedState::ALL {
            assert_eq!(
                mapped.iter().filter(|reached| **reached == state).count(),
                1,
                "{state} is not reached exactly once by {mapped:?}"
            );
        }
    }

    #[test]
    fn a_verdict_becomes_a_claim_about_the_slot_it_was_captured_from() {
        let winner = Detection {
            visible: true,
            matched_rule: Some("bash_permission_prompt".to_owned()),
            ..detection(ScreenState::Blocked)
        };

        let assertion = sent(claimed(agent("claude"), &winner, &about(SLOT)));

        assert_eq!(assertion.assert, AssertedState::Blocked);
        assert!(assertion.visible);
        assert_eq!(assertion.agent, agent("claude"));
        assert_eq!(assertion.correlation, SLOT);
        assert_eq!(
            assertion
                .detail
                .as_ref()
                .and_then(|detail| detail.get(RULE)),
            Some(&Value::from("bash_permission_prompt"))
        );
        assert_eq!(
            assertion.raw,
            Some(serde_json::json!({"matched_rule": "bash_permission_prompt"}))
        );
        // Neither is known here, and neither is guessed at.
        assert_eq!(assertion.session, None);
        assert_eq!(assertion.cwd, None);
    }

    #[test]
    fn what_the_caller_knows_about_the_session_travels_with_the_claim() {
        let attribution = Attribution {
            correlation: SLOT,
            session: Some("abc123"),
            cwd: Some("/x"),
        };

        let assertion = sent(claimed(
            agent("claude"),
            &detection(ScreenState::Working),
            &attribution,
        ));

        assert_eq!(assertion.session.as_deref(), Some("abc123"));
        assert_eq!(assertion.cwd.as_deref(), Some("/x"));
    }

    #[test]
    fn a_screen_showing_the_past_is_not_claimed_about_at_all() {
        let transcript = Detection {
            skip: true,
            // A state it would have claimed, if the screen were of now.
            matched_rule: Some("transcript_viewer".to_owned()),
            ..detection(ScreenState::Blocked)
        };

        assert_eq!(
            claimed(agent("claude"), &transcript, &about(SLOT)),
            Claim::Nothing
        );
    }

    #[test]
    fn a_screen_no_rule_recognized_says_why_there_was_no_rule() {
        let fell_back = Detection {
            fallback: Some("default_known_agent_idle_fallback"),
            ..detection(ScreenState::Idle)
        };

        let assertion = sent(claimed(agent("claude"), &fell_back, &about(SLOT)));

        assert_eq!(assertion.detail, None);
        assert_eq!(
            assertion.raw,
            Some(serde_json::json!({"fallback": "default_known_agent_idle_fallback"}))
        );
    }

    #[test]
    fn nothing_a_claim_carries_came_off_the_screen() {
        let store = ManifestStore::open(agentbus_detect::StorePaths::rooted(
            tempfile::tempdir().expect("a temporary directory").path(),
        ));
        let secret = "sk-do-not-put-this-on-a-bus";
        let request = Request {
            agent: Some("claude"),
            process: "",
            cmdline: "",
            screen: &format!("Do you want to proceed?\n{secret}\n"),
            osc_title: secret,
            osc_progress: secret,
        };

        let claim = claim(&store, &request, &about(SLOT)).expect("claude is a described agent");

        let written = serde_json::to_string(&sent(claim)).expect("a claim is serializable");
        assert!(!written.contains(secret), "{written}");
    }

    #[test]
    fn a_screen_with_nobody_to_read_it_justifies_no_claim() {
        let store = ManifestStore::open(agentbus_detect::StorePaths::rooted(
            tempfile::tempdir().expect("a temporary directory").path(),
        ));
        let request = Request {
            agent: Some("an-agent-nobody-has-heard-of"),
            process: "",
            cmdline: "",
            screen: "",
            osc_title: "",
            osc_progress: "",
        };

        assert_eq!(claim(&store, &request, &about(SLOT)), None);
    }

    #[test]
    fn sending_where_no_bus_is_listening_is_not_a_failure() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let assertion = StateAssertion::new(agent("claude"), SLOT, AssertedState::Blocked);

        assert!(send(&assertion, &dir.path().join("emit.sock")).is_ok());
    }

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
