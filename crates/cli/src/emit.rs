//! `agentbus emit`: the client that runs inside somebody else's coding agent.
//!
//! Every other command in this binary is run by a person, or by something that
//! wants an answer out of it. This one is run by a coding agent, as a hook, on
//! every tool call, and that agent reads what the hook prints and what it exits
//! with as instructions about the user's own session: a byte on stdout can
//! rewrite what the agent believes, and a non-zero exit can deny the tool call
//! the user just asked for. The contract here is therefore not "work well", it
//! is "be impossible to notice".
//!
//! - **Nothing reaches stdout.** Not on success, not on failure, not from a
//!   panic. There are diagnostics, but only when [`LOG_VAR`] asks for them, and
//!   they go to stderr or to a file. The one exception is the usage text, which
//!   goes where a person asking for it by hand expects to find it; no hook's
//!   command line asks for it.
//! - **The process always exits 0.** An unreadable payload, an agent this build
//!   has no adapter for, no daemon running, a daemon that is running but wedged,
//!   a panic in the middle of any of it: all one outcome — nothing is sent,
//!   nothing is said, the status is zero. A command line that merely *names*
//!   this command cannot fail either; that part is in [`crate::run`].
//! - **It finishes inside [`BUDGET`].** Enforced against each of the three
//!   things that could wait — reading the payload, connecting, writing — rather
//!   than hoped for. The agent's own hook timeout is not a safety net: Claude
//!   Code's default is measured in minutes.
//! - **No daemon costs nothing.** A machine where the bus is not running has to
//!   run its agents exactly as if none of this were installed, so the absent
//!   socket is one `stat` and an immediate return.
//!
//! Losing an event is the acceptable failure and delaying the agent is not,
//! which is why nothing below retries, queues or spools. It is also why this
//! path uses blocking sockets with explicit deadlines and starts no runtime: a
//! process that lives for a few milliseconds cannot afford to build one.

use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::time::{Duration, Instant};

use agentbus_daemon::SocketPaths;
use agentbus_protocol::{Source, UnstampedEvent};
use serde_json::Value;
use socket2::{Domain, SockAddr, Socket, Type};

use crate::adapters;
use crate::{EMIT, LOG_VAR};

/// The environment variable whose value becomes the event's `correlation`.
///
/// Whatever set the variable decides what its value means; this client copies it
/// into the event verbatim. It is never split, matched, validated or interpreted
/// here, and nothing may depend on its shape: two of these are equal or they are
/// not, and that is the whole of the contract.
pub const PANE_VAR: &str = "AGENTBUS_PANE";

/// The environment variable that sends diagnostics to a file instead of stderr.
///
/// Worth having because of where this runs: an agent frequently discards its
/// hooks' stderr, so the one place a person would look for an explanation is the
/// one place they cannot see. Only consulted when [`LOG_VAR`] has already turned
/// diagnostics on.
pub const LOG_FILE_VAR: &str = "AGENTBUS_LOG_FILE";

/// The value of [`LOG_VAR`] that means "nothing, thank you".
///
/// The daemon reads that variable as a filter and this command reads it as a
/// switch, so the one spelling both understand as *off* has to be honoured here
/// too — otherwise turning the daemon's logging off would turn this on.
const LOG_OFF: &str = "off";

/// The most of a payload this will read, after which the payload is not an
/// event.
///
/// A hook payload carries the agent's own JSON — a whole tool result, at times —
/// so the bound is generous. It is here to put a ceiling on what one invocation
/// can cost rather than to police the size of an honest payload, and the daemon
/// applies a bound of its own to what it will accept.
pub const MAX_PAYLOAD: usize = 1024 * 1024;

/// Everything one invocation is allowed to spend, from the moment this process
/// got control.
///
/// Deliberately shorter than any agent's own hook timeout. The number that
/// matters to a user is not this one but the one they never see: a tool call
/// that takes a tenth of a second longer than it used to is invisible, and one
/// that takes a second longer is the reason they uninstall this.
pub const BUDGET: Duration = Duration::from_millis(100);

/// The most of the budget that may be spent waiting for a payload to arrive.
///
/// An agent writes the payload and closes, so in practice this is never
/// approached. It exists for the agent that spawns a hook, hands it a pipe and
/// then thinks about something else: without a deadline here, that hook waits
/// forever, and a hook that waits forever is the failure this whole module is
/// written to prevent. What is left of the budget afterwards is enough to reach
/// a daemon that is behaving normally.
const READ_BUDGET: Duration = Duration::from_millis(40);

/// The longest a connection attempt may take.
///
/// Connecting to a unix socket either succeeds at once or fails at once, with
/// one exception: a daemon that has stopped accepting leaves its backlog full,
/// and a blocking connect to a full backlog waits for as long as it takes. That
/// is the case this bounds.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(50);

/// What one invocation was asked to do.
///
/// The three fields are the whole of this command's input: two flags and one
/// environment variable. Nothing else is read, and nothing is consulted that
/// could be slow — no configuration file, no directory walk, no lookup that
/// could touch a network.
#[derive(Debug, Clone, Copy, Default)]
pub struct Request<'a> {
    /// `--agent`: the agent whose hook payload is on stdin.
    pub agent: Option<&'a str>,
    /// `--source`: where the claim comes from. Absent means a hook.
    pub source: Option<&'a str>,
    /// The value of [`PANE_VAR`], if it was set to something. Copied into a
    /// hook event verbatim; ignored for an observation, which states its own.
    pub correlation: Option<&'a str>,
}

/// Reads a payload, and sends what it means to the bus if it means anything.
///
/// Returns nothing, because there is nothing a caller could usefully do about
/// any of the ways this can come to nothing, and because the caller's next
/// statement has to be an exit code of zero whatever happened here.
///
/// `started` is when the process got control, which is what [`BUDGET`] is
/// measured from; `stdin` is where the payload comes from, taken as an argument
/// rather than reached for so that this is testable without a process, and
/// deliberately the only stream this function knows about.
pub fn run(request: &Request<'_>, paths: &SocketPaths, started: Instant, stdin: impl Read + AsFd) {
    quiet_panics();
    // `AssertUnwindSafe` because nothing below outlives the call: a panic here
    // abandons one event and the process exits immediately afterwards, so there
    // is no state left for a later observer to find in a broken condition.
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        send(request, paths, started + BUDGET, stdin);
    }));
}

/// The whole of the work, inside the guard.
fn send(request: &Request<'_>, paths: &SocketPaths, deadline: Instant, stdin: impl Read + AsFd) {
    // The payload is read before anything is decided about it, including
    // whether it was worth reading. An agent writes into this pipe expecting
    // somebody to be at the other end of it, and a hook that exits early enough
    // leaves the agent holding a broken pipe — a way of altering the agent, and
    // so out of bounds, however uninteresting the payload turns out to be.
    let Some(payload) = read_payload(stdin, deadline.min(Instant::now() + READ_BUDGET)) else {
        return;
    };
    let raw: Value = match serde_json::from_slice(&payload) {
        Ok(raw) => raw,
        Err(error) => return note(format_args!("stdin was not JSON: {error}")),
    };
    let Some(event) = normalize(request, &raw) else {
        return note(format_args!("nothing to send"));
    };
    let mut line = match serde_json::to_vec(&event) {
        Ok(line) => line,
        Err(error) => return note(format_args!("the event could not be written: {error}")),
    };
    // The daemon reads to a newline or to end of input, so the line ends with
    // one: it lets the daemon act on the event without waiting to see this
    // process close the connection.
    line.push(b'\n');

    if let Err(error) = deliver(&line, paths.emit(), deadline) {
        note(format_args!("the event was not delivered: {error}"));
    }
}

/// Turns a payload into the event it means, if it means one.
///
/// The dispatch is a match on the agent's name, so that an agent whose adapter
/// this build does not have is the same non-event as a payload the adapter had
/// nothing to say about, and so that adding an agent is adding an arm.
fn normalize(request: &Request<'_>, raw: &Value) -> Option<UnstampedEvent> {
    // Spelled with the protocol's own wire strings rather than with literals of
    // this module's own: there is one vocabulary and this is not where a second
    // one starts.
    let source = match request.source {
        None => Source::Hook,
        Some(named) if named == Source::Hook.as_str() => Source::Hook,
        Some(named) if named == Source::Observed.as_str() => Source::Observed,
        Some(_) => return None,
    };
    match (source, request.agent) {
        (Source::Hook, Some(agent)) => {
            let event = match agent {
                "claude" => adapters::claude::normalize(raw)?,
                // Debug builds carry one agent that is not an agent: it panics,
                // so that the guarantee this module exists for can be tested
                // against a real process rather than against a closure standing
                // in for one. A released build does not contain this arm.
                #[cfg(debug_assertions)]
                PANIC_AGENT => panic!("{PANIC_AGENT}"),
                _ => return None,
            };
            match request.correlation {
                Some(correlation) => Some(event.with_correlation(correlation)),
                None => Some(event),
            }
        }
        // An observation states its own correlation, which is why the
        // environment's is not consulted here: the program that made it knows
        // what it was watching, and it need not have been watching this process.
        (Source::Observed, None) => adapters::observed::normalize(raw),
        // Either an observation attributed to an agent's hook, or a hook payload
        // with no agent to read it. Both are contradictions rather than events.
        _ => None,
    }
}

/// The agent name that panics, in a build that has debug assertions on.
#[cfg(debug_assertions)]
const PANIC_AGENT: &str = "panic-on-purpose";

/// Reads the payload, bounded in both size and time.
///
/// `None` means there is no payload to speak of: nothing arrived at all, the
/// stream could not be read, or more arrived than [`MAX_PAYLOAD`] allows. All
/// three are the same to the caller.
///
/// What did arrive before the deadline is kept and handed back. A payload
/// normally ends with the writer closing the pipe, but a writer that sends its
/// JSON and then holds the pipe open has still said everything it had to say,
/// and throwing that away to be strict about how it ended would drop an event
/// this could act on. What is incomplete instead of merely unterminated fails
/// to parse a moment later, which is the same outcome as not having it.
fn read_payload(mut stdin: impl Read + AsFd, deadline: Instant) -> Option<Vec<u8>> {
    let fd = stdin.as_fd().as_raw_fd();
    let mut payload = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        if !readable(fd, deadline) {
            if payload.is_empty() {
                note(format_args!("no payload arrived in time"));
                return None;
            }
            note(format_args!("the payload did not end; taking what arrived"));
            return Some(payload);
        }
        match stdin.read(&mut chunk) {
            Ok(0) => return Some(payload),
            Ok(read) => payload.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                note(format_args!("stdin could not be read: {error}"));
                return None;
            }
        }
        if payload.len() > MAX_PAYLOAD {
            note(format_args!("the payload is over {MAX_PAYLOAD} bytes"));
            return None;
        }
    }
}

/// Waits until `fd` has something to say, or until the deadline.
///
/// The descriptor is left exactly as it was found: it belongs to whoever started
/// this process, who is entitled to assume that handing a pipe to a child does
/// not come back changed.
fn readable(fd: RawFd, deadline: Instant) -> bool {
    loop {
        let Some(remaining) = remaining(deadline) else {
            return false;
        };
        let mut watched = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let milliseconds = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
        // Safe: `watched` is one initialised `pollfd` and the count says so.
        let ready = unsafe { libc::poll(&raw mut watched, 1, milliseconds) };
        match ready {
            1 => return true,
            0 => return false,
            _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            _ => return false,
        }
    }
}

/// Connects to the socket and writes the line, all inside the deadline.
///
/// The exchange is one-directional by design: this writes its line, closes, and
/// never reads a byte back. There is nothing the daemon could say that would
/// change what happens next, and waiting to be told it would be the one thing a
/// hook must not do.
fn deliver(line: &[u8], socket: &Path, deadline: Instant) -> io::Result<()> {
    // A machine with no bus running is the ordinary case, not an error, and it
    // is answered with one `stat` rather than with a connection attempt. A
    // socket that exists but has nobody behind it fails on connect below, and
    // the two are the same outcome.
    if !socket.exists() {
        note(format_args!("no bus is listening at {}", socket.display()));
        return Ok(());
    }
    let mut stream = connect(socket, deadline)?;
    write_within(&mut stream, line, deadline)
}

/// Connects, without waiting longer than the deadline allows.
///
/// A unix socket connects immediately or not at all, except when the daemon has
/// stopped accepting and its backlog has filled: a blocking connect then waits
/// on a process that may never come back. Hence a non-blocking connect with a
/// deadline. A full backlog reports itself as "would block" and costs this
/// invocation its event, which is the right trade — the alternative is a queue,
/// and a queue in a process this short-lived is a queue nobody empties.
fn connect(socket: &Path, deadline: Instant) -> io::Result<UnixStream> {
    let timeout = remaining(deadline)
        .map(|left| left.min(CONNECT_TIMEOUT))
        .ok_or_else(expired)?;
    let address = SockAddr::unix(socket)?;
    let endpoint = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    endpoint.connect_timeout(&address, timeout)?;
    Ok(UnixStream::from(endpoint))
}

/// Writes all of `line`, or gives up when the deadline passes.
///
/// The timeout is reapplied before every write rather than set once, because a
/// socket timeout bounds one call and this may take several: a receiver that
/// reads slowly enough could otherwise hold this process for a multiple of the
/// budget without ever exceeding it on any single write.
fn write_within(stream: &mut UnixStream, mut line: &[u8], deadline: Instant) -> io::Result<()> {
    while !line.is_empty() {
        let left = remaining(deadline).ok_or_else(expired)?;
        stream.set_write_timeout(Some(left))?;
        match stream.write(line) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(written) => line = &line[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// How much of the budget is left, or `None` if it is spent.
fn remaining(deadline: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    (!left.is_zero()).then_some(left)
}

/// The failure a spent budget is reported as.
fn expired() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "the budget for this event is spent",
    )
}

/// Stops a panic from announcing itself.
///
/// The default hook writes to stderr, which is safe but not quiet, and an agent
/// that shows its hooks' stderr to the user would be showing them a crash report
/// for something they did not run and cannot act on. So a panic says what it has
/// to say through the same channel as every other diagnostic here: only when
/// asked, and never on stdout.
fn quiet_panics() {
    std::panic::set_hook(Box::new(|panicked| note(format_args!("{panicked}"))));
}

/// Says something, if anybody asked to be told.
///
/// Silence is the default and the reason for it is the same as everywhere else
/// in this module: this process is a guest in somebody's editor. [`LOG_VAR`]
/// turns diagnostics on and [`LOG_FILE_VAR`] chooses somewhere they will survive
/// an agent that discards its hooks' stderr.
fn note(message: fmt::Arguments<'_>) {
    if !wanted(std::env::var_os(LOG_VAR).as_deref()) {
        return;
    }
    match std::env::var_os(LOG_FILE_VAR).filter(|path| !path.is_empty()) {
        Some(path) => {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{EMIT}: {message}");
            }
        }
        None => eprintln!("{EMIT}: {message}"),
    }
}

/// Whether a value of [`LOG_VAR`] asks for diagnostics.
fn wanted(level: Option<&std::ffi::OsStr>) -> bool {
    level.is_some_and(|level| !level.is_empty() && level != LOG_OFF)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Seek;

    use agentbus_protocol::{Agent, Kind};
    use serde_json::json;

    use super::*;

    /// A readable stream holding `bytes`, on a real descriptor because that is
    /// what the payload arrives on.
    fn stdin(bytes: &[u8]) -> File {
        let mut file = tempfile::tempfile().expect("cannot make a temporary file");
        file.write_all(bytes).expect("cannot write");
        file.rewind().expect("cannot rewind");
        file
    }

    /// A payload the Claude adapter has something to say about.
    fn hook_payload() -> Value {
        json!({
            "session_id": "abc123",
            "hook_event_name": "UserPromptSubmit",
            "cwd": "/srv/project",
        })
    }

    #[test]
    #[cfg(debug_assertions)]
    fn a_panic_anywhere_below_is_swallowed() {
        let paths = SocketPaths::in_dir("/nonexistent");
        let request = Request {
            agent: Some(PANIC_AGENT),
            ..Request::default()
        };
        run(
            &request,
            &paths,
            Instant::now(),
            stdin(hook_payload().to_string().as_bytes()),
        );
    }

    #[test]
    fn a_hook_event_carries_the_correlation_it_was_given_verbatim() {
        let request = Request {
            agent: Some("claude"),
            correlation: Some("  w9:p3 / anything at all  "),
            ..Request::default()
        };
        let event = normalize(&request, &hook_payload()).expect("that should have been an event");
        assert_eq!(event.agent, Agent::Claude);
        assert_eq!(event.kind, Kind::TurnStart);
        assert_eq!(event.source, Source::Hook);
        assert_eq!(
            event.correlation.as_deref(),
            Some("  w9:p3 / anything at all  ")
        );
    }

    #[test]
    fn an_agent_with_no_adapter_produces_nothing() {
        let request = Request {
            agent: Some("an-agent-from-the-future"),
            ..Request::default()
        };
        assert!(normalize(&request, &hook_payload()).is_none());
    }

    #[test]
    fn naming_an_agent_and_an_observation_at_once_produces_nothing() {
        let request = Request {
            agent: Some("claude"),
            source: Some("observed"),
            correlation: None,
        };
        assert!(normalize(&request, &hook_payload()).is_none());
    }

    #[test]
    fn a_source_nobody_defined_produces_nothing() {
        let request = Request {
            agent: Some("claude"),
            source: Some("guessed"),
            correlation: None,
        };
        assert!(normalize(&request, &hook_payload()).is_none());
    }

    #[test]
    fn the_default_source_is_a_hook_and_may_be_said_out_loud() {
        for source in [None, Some("hook")] {
            let request = Request {
                agent: Some("claude"),
                source,
                correlation: None,
            };
            assert!(normalize(&request, &hook_payload()).is_some());
        }
    }

    #[test]
    fn an_observation_ignores_the_environments_correlation() {
        let request = Request {
            agent: None,
            source: Some("observed"),
            correlation: Some("from-the-environment"),
        };
        let payload = json!({"kind": "blocked", "correlation": "stated-by-the-observer"});
        let event = normalize(&request, &payload).expect("that should have been an event");
        assert_eq!(event.source, Source::Observed);
        assert_eq!(event.correlation.as_deref(), Some("stated-by-the-observer"));
    }

    #[test]
    fn a_payload_at_the_bound_is_read_and_one_over_it_is_not() {
        let at = vec![b'x'; MAX_PAYLOAD];
        let deadline = Instant::now() + BUDGET;
        assert_eq!(
            read_payload(stdin(&at), deadline).map(|read| read.len()),
            Some(MAX_PAYLOAD)
        );

        let over = vec![b'x'; MAX_PAYLOAD + 1];
        assert!(read_payload(stdin(&over), deadline).is_none());
    }

    #[test]
    fn a_payload_that_never_arrives_is_given_up_on() {
        let (reader, _writer) = pipe();
        let deadline = Instant::now() + Duration::from_millis(20);
        assert!(read_payload(reader, deadline).is_none());
        assert!(Instant::now() < deadline + Duration::from_millis(100));
    }

    /// A pipe nobody is writing to, as an agent that has not got round to
    /// sending its payload would leave one.
    fn pipe() -> (File, File) {
        let mut ends = [0; 2];
        // Safe: `ends` is two descriptors' worth of space, which is what `pipe`
        // fills in.
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
        use std::os::fd::FromRawFd;
        // Safe: both descriptors are freshly made and owned by nothing else.
        unsafe { (File::from_raw_fd(ends[0]), File::from_raw_fd(ends[1])) }
    }

    #[test]
    fn a_spent_budget_stops_the_work() {
        assert!(remaining(Instant::now() - Duration::from_secs(1)).is_none());
        assert!(remaining(Instant::now() + Duration::from_secs(1)).is_some());
    }

    #[test]
    fn diagnostics_are_off_unless_asked_for() {
        use std::ffi::OsStr;
        assert!(!wanted(None));
        assert!(!wanted(Some(OsStr::new(""))));
        assert!(!wanted(Some(OsStr::new("off"))));
        assert!(wanted(Some(OsStr::new("info"))));
        assert!(wanted(Some(OsStr::new("1"))));
    }
}
