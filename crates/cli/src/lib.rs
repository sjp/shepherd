//! The command-line front end of the bus. All of its behaviour lives in this
//! library rather than in the binary so that argument handling, exit codes and
//! output can be exercised by tests without spawning a process; the binary is a
//! single call into [`run`].
//!
//! Two properties of this crate are contracts rather than conveniences. Stdout
//! belongs to machine-readable output only, so diagnostics always go to stderr.
//! And `--version` prints exactly `agentbus <semver>` and nothing else: a host
//! that has copied this binary onto another machine compares that line
//! byte-for-byte to decide whether the copy is the build it expects, so any
//! decoration added here would break provisioning somewhere else.
//!
//! One command has a contract stricter than either, because it does not run
//! where the others do: `emit` is run by somebody else's coding agent, on every
//! tool call, and that agent reads what its hooks print and what they exit with
//! as decisions about its user's work. It writes nothing, exits zero whatever
//! happens to it, and holds itself to a wall-clock budget. See [`emit`].
//!
//! Every option a running bus takes is a flag, with an environment variable of
//! the same meaning behind it. The flags are for people; the variables are for
//! whatever supervises the process, which often cannot choose the argument
//! vector but can always choose the environment.

pub mod adapters;
pub mod emit;
pub mod ensure;
pub mod foreground;
pub mod install;
pub mod status;
pub mod stream;
pub mod table;

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use agentbus_daemon::{Daemon, Settings, SocketPaths, clock, paths::DIR_VAR};
use agentbus_install::{Agent, Environment, Mode, UnknownAgent};
use agentbus_protocol::ForegroundEntry;
use clap::{Args, Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// The environment variable that sets how much the daemon says about itself,
/// and the one that turns `emit`'s diagnostics on at all.
pub const LOG_VAR: &str = "AGENTBUS_LOG";

/// The environment variable behind `--stale-secs`.
const STALE_SECS_VAR: &str = "AGENTBUS_STALE_SECS";

/// The environment variable behind `--done-retention-secs`.
const DONE_RETENTION_SECS_VAR: &str = "AGENTBUS_DONE_RETENTION_SECS";

/// The environment variable behind `--proc-root`.
const PROC_ROOT_VAR: &str = "AGENTBUS_PROC_ROOT";

/// The status `foreground` exits with when there is nothing to report from: no
/// daemon to ask, or one that is not watching a process table.
///
/// Distinct from the general failure code because this command already uses
/// that one to mean something narrower — a filter that matched nothing — and a
/// caller has to be able to tell "no such correlation" from "nobody is looking".
const NOT_WATCHING: u8 = 2;

/// What is said on stderr when a daemon cannot see a process table.
const UNAVAILABLE: &str = "foreground monitoring unavailable on this daemon";

/// The status a second daemon exits with when one is already running.
///
/// Distinct from the general failure code because it is often not a failure at
/// all: something whose goal is "a daemon is running here" has got what it
/// wanted, and can say so by treating this one code as success.
const ALREADY_RUNNING: u8 = 3;

/// How each command names itself in what it prints, so that a message read out
/// of a supervisor's log or a shell's scrollback says which one produced it.
const DAEMON: &str = "agentbus daemon";
const EMIT: &str = "agentbus emit";
const SUBSCRIBE: &str = "agentbus subscribe";
const STATUS: &str = "agentbus status";
const FOREGROUND: &str = "agentbus foreground";
const INSTALL: &str = "agentbus install";
const UNINSTALL: &str = "agentbus uninstall";

/// How long `--recent` follows the stream when it is given no number.
///
/// Long enough to catch the event someone is waiting to see, short enough that
/// the command still feels like a one-shot rather than something they have to
/// escape from.
const DEFAULT_TAIL_SECS: &str = "2";

/// The `agentbus` command line.
#[derive(Debug, Parser)]
#[command(name = "agentbus", version, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// What the user asked for.
#[derive(Debug, Subcommand)]
enum Command {
    /// Run the bus in the foreground until it is killed.
    Daemon(DaemonArgs),
    /// Print the event stream on stdout as newline-delimited JSON, until the bus stops.
    Subscribe(SubscribeArgs),
    /// Print what every agent session is doing, once.
    Status(StatusArgs),
    /// Print what is running in front of each correlated shell, once.
    Foreground(ForegroundArgs),
    /// Read a payload on stdin, normalize it, and send it to the bus.
    Emit(EmitArgs),
    /// Put the hooks that emit events into the coding agents on this machine.
    Install(InstallArgs),
    /// Take those hooks back out again.
    Uninstall(InstallArgs),
}

/// Which bus a command is about.
///
/// Every command that talks to a bus takes this, and takes it the same way, so
/// that pointing one of them somewhere else is the same gesture as pointing any
/// other.
#[derive(Debug, Args)]
struct Location {
    /// Directory holding the bus's sockets [default: the session's runtime directory]
    #[arg(long, value_name = "PATH", env = DIR_VAR)]
    dir: Option<PathBuf>,
}

impl Location {
    /// Where the sockets are.
    ///
    /// An empty value names no directory, so it falls through to the rest of the
    /// precedence rules rather than pointing at a relative path in whatever the
    /// working directory happened to be.
    fn paths(&self) -> SocketPaths {
        match self.dir.as_ref().filter(|dir| !dir.as_os_str().is_empty()) {
            Some(dir) => SocketPaths::in_dir(dir),
            None => SocketPaths::resolve(),
        }
    }
}

/// How to run the bus.
#[derive(Debug, Args)]
struct DaemonArgs {
    #[command(flatten)]
    location: Location,

    /// Seconds a session may go quiet before it is reported stale
    #[arg(
        long,
        value_name = "SECS",
        env = STALE_SECS_VAR,
        default_value_t = Settings::default().stale_after.as_secs(),
    )]
    stale_secs: u64,

    /// Seconds a finished session is kept before it is forgotten
    #[arg(
        long,
        value_name = "SECS",
        env = DONE_RETENTION_SECS_VAR,
        default_value_t = Settings::default().done_retention.as_secs(),
    )]
    done_retention_secs: u64,

    /// How much to say on stderr: a level — off, error, warn, info, debug, trace — or a filter naming targets
    #[arg(
        long,
        value_name = "LEVEL",
        env = LOG_VAR,
        default_value = "info",
        value_parser = filter,
    )]
    log_level: String,

    /// Where the process table is read from. For tests: a machine has exactly
    /// one and it is the default. Hidden from the usage text for that reason,
    /// and left as a flag rather than a constant so that a test can put a
    /// daemon in front of a process table it wrote itself and hold it still.
    #[arg(
        long,
        value_name = "PATH",
        env = PROC_ROOT_VAR,
        hide = true,
        default_value_os_t = Settings::default().proc_root,
    )]
    proc_root: PathBuf,
}

impl DaemonArgs {
    /// What to start the bus with.
    fn settings(&self) -> Settings {
        Settings {
            stale_after: Duration::from_secs(self.stale_secs),
            done_retention: Duration::from_secs(self.done_retention_secs),
            proc_root: self.proc_root.clone(),
            ..Settings::default()
        }
    }
}

/// How to follow the stream.
#[derive(Debug, Args)]
struct SubscribeArgs {
    #[command(flatten)]
    location: Location,

    /// Start a daemon first if none is running here, and leave it running
    #[arg(long)]
    ensure_daemon: bool,
}

/// How to report what the bus knows.
#[derive(Debug, Args)]
struct StatusArgs {
    #[command(flatten)]
    location: Location,

    /// Print the snapshot as the daemon sent it instead of as a table
    #[arg(long)]
    json: bool,

    /// After the table, follow the stream for a few seconds [default: 2]
    #[arg(
        long,
        value_name = "SECS",
        num_args = 0..=1,
        default_missing_value = DEFAULT_TAIL_SECS,
    )]
    recent: Option<u64>,
}

/// What to report about the foreground, and how.
#[derive(Debug, Args)]
struct ForegroundArgs {
    #[command(flatten)]
    location: Location,

    /// Print only what is running in this correlation, compared exactly
    #[arg(long, value_name = "CORRELATION")]
    correlation: Option<String>,

    /// Print the observations as newline-delimited JSON instead of as a table
    #[arg(long)]
    json: bool,
}

/// What to send, and what it is.
///
/// Both flags take a bare string rather than a closed set of values, because
/// clap refuses a value it does not recognize by printing usage and exiting
/// non-zero, and this is the one command in the binary that must do neither.
/// An unrecognized value is a payload that means nothing instead, which is
/// already the ordinary outcome here.
#[derive(Debug, Args)]
struct EmitArgs {
    #[command(flatten)]
    location: Location,

    /// The agent whose hook payload is on stdin
    #[arg(long, value_name = "NAME")]
    agent: Option<String>,

    /// Where the claim comes from: hook, the default, or observed
    #[arg(long, value_name = "SOURCE")]
    source: Option<String>,
}

/// Which agents to act on, and whether to act at all.
#[derive(Debug, Args)]
struct InstallArgs {
    /// The agent to act on; repeat to name several [default: every one detected]
    #[arg(long, value_name = "NAME", value_parser = agent)]
    agent: Vec<Agent>,

    /// Say what would change, and change nothing
    #[arg(long)]
    dry_run: bool,
}

impl InstallArgs {
    /// Whether this run is allowed to touch anything.
    fn mode(&self) -> Mode {
        match self.dry_run {
            true => Mode::DryRun,
            false => Mode::Apply,
        }
    }
}

/// Parses `args` (including the program name at position zero) and runs the
/// requested command, returning the process exit code.
///
/// Nothing is written to stdout except a command's own output; usage and error
/// messages go to stderr.
pub fn run<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    // Taken before anything else happens, because `emit` is held to a
    // wall-clock budget and the honest place to start counting is the first
    // moment this code has control of the process.
    let started = Instant::now();
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    match Cli::try_parse_from(&args) {
        Ok(cli) => match cli.command {
            Command::Daemon(args) => daemon(&args),
            Command::Subscribe(args) => subscribe(&args),
            Command::Status(args) => status(&args),
            Command::Foreground(args) => foreground(&args),
            Command::Emit(args) => emit(&args, started),
            Command::Install(args) => hooks(&args, install::Direction::Install),
            Command::Uninstall(args) => hooks(&args, install::Direction::Uninstall),
        },
        // `--version` and `--help` arrive here too: clap reports them as errors
        // that carry the rendered text, a zero exit code and stdout as their
        // destination. Letting clap print keeps that routing in one place.
        Err(err) => {
            let _ = err.print();
            match err.use_stderr() && names_emit(&args) {
                // A command line that names `emit` and is then refused is a
                // misconfigured hook, which is to say: a coding agent that is
                // about to read this process's exit code as a decision about
                // its user's work. Usage on stderr is as much as may be said
                // about it, and zero is the only status that may be returned.
                true => ExitCode::SUCCESS,
                false => ExitCode::from(u8::try_from(err.exit_code()).unwrap_or(2)),
            }
        }
    }
}

/// Whether a command line asks for the emit command.
///
/// Answered by looking at the words rather than by asking the parser, because
/// the case it exists for is the one where the parser has already refused to
/// understand them. The rule is the first word that is not a flag, which is
/// where every subcommand this binary has appears.
fn names_emit(args: &[OsString]) -> bool {
    args.iter()
        .skip(1)
        .find(|arg| !arg.as_encoded_bytes().starts_with(b"-"))
        .is_some_and(|arg| arg == "emit")
}

/// Runs the bus until it is signalled.
///
/// There is one code path here whatever the daemon is started by: nothing about
/// being run by hand, by a supervisor or by another program changes what it
/// does, so there is nothing to select between.
fn daemon(args: &DaemonArgs) -> ExitCode {
    init_logging(&args.log_level);
    let paths = args.location.paths();
    info!(
        version = agentbus_daemon::VERSION,
        dir = %paths.dir().display(),
        stale_secs = args.stale_secs,
        done_retention_secs = args.done_retention_secs,
        log_level = args.log_level,
        "starting"
    );

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(DAEMON, &error),
    };
    runtime.block_on(async {
        match Daemon::bind(paths, args.settings()) {
            Ok(daemon) => {
                let stopped = daemon.run().await;
                info!(signal = %stopped, "exiting");
                ExitCode::SUCCESS
            }
            Err(agentbus_daemon::Error::AlreadyRunning { dir }) => {
                // Not reported through `fail`: this one is a state of the world
                // rather than something that went wrong, and the caller is told
                // apart from a real failure by the exit code.
                eprintln!(
                    "{DAEMON}: another daemon is already running in {}",
                    dir.display()
                );
                ExitCode::from(ALREADY_RUNNING)
            }
            Err(error) => fail(DAEMON, &error),
        }
    })
}

/// Copies the daemon's stream to stdout, one line at a time, until the daemon
/// stops.
///
/// Nothing is buffered across lines and nothing is reconnected. This command is
/// a pipe: whatever is on the other end of it — `jq`, a file, another program
/// entirely — should see each line at the moment the daemon produced it, and
/// should be able to tell the end of the stream from a pause in it. Restarting
/// after the bus goes away is a decision for whoever ran this, not for it.
///
/// `--ensure-daemon` is the exception, and only to the first connection: it
/// turns "there is no bus here" from something to report into something to fix,
/// which is what anything arriving on a machine where nobody is going to start
/// one by hand needs. The daemon it starts is left running afterwards.
fn subscribe(args: &SubscribeArgs) -> ExitCode {
    let paths = args.location.paths();
    let mut stream = match stream::connect(&paths) {
        Ok(stream) => stream,
        Err(stream::Error::NoDaemon { .. }) if args.ensure_daemon => {
            if let Err(error) = ensure::daemon(&paths) {
                return fail(SUBSCRIBE, &error);
            }
            match stream::connect(&paths) {
                Ok(stream) => stream,
                Err(error) => return fail(SUBSCRIBE, &error),
            }
        }
        Err(error) => return fail(SUBSCRIBE, &error),
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        match stream.line() {
            Ok(None) => return ExitCode::SUCCESS,
            Ok(Some(line)) => {
                if let Err(error) = out.write_all(&line).and_then(|()| out.flush()) {
                    return match error.kind() {
                        // Whoever was reading this has finished — `| head`, a
                        // pipeline that ended. That is how a pipe is supposed to
                        // end, not something to complain about.
                        std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
                        _ => fail(SUBSCRIBE, &error),
                    };
                }
            }
            Err(error) => return fail(SUBSCRIBE, &error),
        }
    }
}

/// Prints what the bus knows right now, and optionally a little of what happens
/// next.
///
/// The snapshot is the whole answer, which is why this connects, reads one line
/// and is done: a command someone runs to find out whether their agent is
/// waiting for them should not outlive the question. `--recent` is the exception,
/// and it is a tail rather than a replay — the daemon's own history of recent
/// events is its business, and asking for it would need a request protocol on a
/// socket that deliberately has none.
fn status(args: &StatusArgs) -> ExitCode {
    let mut stream = match stream::connect(&args.location.paths()) {
        Ok(stream) => stream,
        Err(error) => return fail(STATUS, &error),
    };
    let (line, snapshot) = match stream.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => return fail(STATUS, &error),
    };

    let stdout = std::io::stdout();
    let styled = stdout.is_terminal();
    let mut out = stdout.lock();
    let written = match args.json {
        true => out.write_all(&line),
        false => out.write_all(status::render(&snapshot, &clock::now(), styled).as_bytes()),
    };
    if let Err(error) = written.and_then(|()| out.flush()) {
        return fail(STATUS, &error);
    }

    match args.recent {
        None => ExitCode::SUCCESS,
        Some(seconds) => tail(&mut stream, Duration::from_secs(seconds), &mut out),
    }
}

/// Prints what the process table says is in front of each correlated shell.
///
/// The snapshot carries the answer, so this is the same one connection and one
/// line that `status` is, and the same reason: somebody asking what is running
/// in a terminal should get an answer rather than a stream.
///
/// The three exit codes are three different answers, and the middle one is why
/// they cannot collapse. Nothing matched the filter is *news* — the correlation
/// is real and nothing is running in it — and a script has to be able to tell it
/// from the daemon not being there or not being able to look, which is why
/// every way of failing to get an answer at all shares the other code.
fn foreground(args: &ForegroundArgs) -> ExitCode {
    let snapshot = match stream::connect(&args.location.paths())
        .and_then(|mut stream| stream.snapshot().map(|(_, snapshot)| snapshot))
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            report(FOREGROUND, &error);
            return ExitCode::from(NOT_WATCHING);
        }
    };
    let Some(entries) = snapshot.foreground else {
        eprintln!("{FOREGROUND}: {UNAVAILABLE}");
        return ExitCode::from(NOT_WATCHING);
    };

    let wanted: Vec<&ForegroundEntry> = entries
        .iter()
        .filter(|entry| {
            args.correlation
                .as_ref()
                .is_none_or(|correlation| &entry.correlation == correlation)
        })
        .collect();
    if wanted.is_empty() && args.correlation.is_some() {
        return ExitCode::FAILURE;
    }

    let text = match args.json {
        true => foreground::json(&wanted),
        false => foreground::render(&wanted, &clock::now()),
    };
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        // Whoever was reading this has finished — `| head`, a pipeline that
        // ended — which is how a pipe is supposed to end rather than something
        // to complain about.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => fail(FOREGROUND, &error),
    }
}

/// Prints what arrives on the stream for a while, then stops.
fn tail(stream: &mut stream::Stream, of: Duration, out: &mut impl Write) -> ExitCode {
    let deadline = Instant::now() + of;
    loop {
        match stream.line_before(deadline) {
            Ok(None) => return ExitCode::SUCCESS,
            Ok(Some(line)) if stream::is_heartbeat(&line) => {}
            Ok(Some(line)) => {
                if let Err(error) = out.write_all(&line).and_then(|()| out.flush()) {
                    return match error.kind() {
                        std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
                        _ => fail(STATUS, &error),
                    };
                }
            }
            Err(error) => return fail(STATUS, &error),
        }
    }
}

/// Sends one event to the bus, and says nothing whatever about how it went.
///
/// This command runs inside somebody else's coding agent, which reads what its
/// hooks print and what they exit with as instructions about the user's own
/// session. So there is one exit code here and it is zero, there is no output,
/// and every decision that could go either way is made inside [`emit::run`],
/// where it is made silently. The environment is read here and nowhere below:
/// the correlation is whatever the variable holds, copied without being looked
/// at, and an empty value is the same as an unset one because a correlation
/// that says nothing cannot tie anything to anything.
fn emit(args: &EmitArgs, started: Instant) -> ExitCode {
    let pane = std::env::var_os(emit::PANE_VAR);
    let request = emit::Request {
        agent: args.agent.as_deref(),
        source: args.source.as_deref(),
        correlation: pane
            .as_deref()
            .and_then(std::ffi::OsStr::to_str)
            .filter(|value| !value.is_empty()),
    };
    emit::run(&request, &args.location.paths(), started, std::io::stdin());
    ExitCode::SUCCESS
}

/// Puts this program's hooks into the coding agents on this machine, or takes
/// them out.
///
/// What was found is printed whichever way the run goes, and before whatever
/// happened to it, because a user whose agent was not installed for needs to
/// know whether this program failed to install or failed to find their agent at
/// all — and those look identical from a report that only lists what it did.
///
/// Naming an agent explicitly acts on it whether or not it was detected. The
/// detection rules are two guesses about a machine, and somebody who knows their
/// own machine better than this does should not have to argue with them.
fn hooks(args: &InstallArgs, direction: install::Direction) -> ExitCode {
    let context = direction.context();
    let env = match Environment::from_env() {
        Ok(env) => env,
        Err(error) => return fail(context, &error),
    };
    let found = agentbus_install::detect(&env);
    let chosen: Vec<Agent> = match args.agent.is_empty() {
        true => found.iter().map(|found| found.agent).collect(),
        false => args.agent.clone(),
    };
    let mode = args.mode();
    let outcomes = match direction {
        install::Direction::Install => agentbus_install::install(&env, &chosen, mode),
        install::Direction::Uninstall => agentbus_install::uninstall(&env, &chosen, mode),
    };
    let outcomes = match outcomes {
        Ok(outcomes) => outcomes,
        Err(error) => return fail(context, &error),
    };

    let report = install::render(&found, &outcomes, direction, mode);
    match std::io::stdout().write_all(report.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(context, &error),
    }
}

/// Accepts the name of an agent, for the command line.
fn agent(name: &str) -> Result<Agent, UnknownAgent> {
    name.parse()
}

/// Reports a failure on stderr and gives the shell a non-zero status.
///
/// `context` names the command that failed, so that a message read out of a
/// supervisor's log says which of them produced it.
fn fail(context: &str, error: &dyn std::error::Error) -> ExitCode {
    report(context, error);
    ExitCode::FAILURE
}

/// Says on stderr what went wrong, and what was underneath it, for a caller
/// that has its own idea of what to exit with.
fn report(context: &str, error: &dyn std::error::Error) {
    eprintln!("{context}: {error}");
    let mut cause = error.source();
    while let Some(error) = cause {
        eprintln!("  caused by: {error}");
        cause = error.source();
    }
}

/// Accepts a verbosity the log filter understands, and hands it back unchanged.
///
/// Validating here rather than when the filter is built means an unusable value
/// is refused the way every other bad argument is — usage on stderr, before
/// anything has been created — instead of after the daemon has started.
fn filter(level: &str) -> Result<String, String> {
    EnvFilter::try_new(level)
        .map(|_| level.to_owned())
        .map_err(|error| error.to_string())
}

/// Sends diagnostics to stderr, at the verbosity asked for.
///
/// A daemon is a process someone starts and then has to reason about later, so
/// by default it says what it was started as and why it stopped, and no more.
/// Stdout is left alone: it carries machine-readable output.
fn init_logging(level: &str) {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        // A daemon's stderr is nearly always going somewhere it will be read
        // later — a log file, a journal, a supervisor's capture — where colour
        // codes are noise in the middle of every field.
        .with_ansi(false)
        .init();
}
