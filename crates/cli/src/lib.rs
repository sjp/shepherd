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
//! Every option a running bus takes is a flag, with an environment variable of
//! the same meaning behind it. The flags are for people; the variables are for
//! whatever supervises the process, which often cannot choose the argument
//! vector but can always choose the environment.

pub mod status;
pub mod stream;

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use agentbus_daemon::{Daemon, Settings, SocketPaths, clock, paths::DIR_VAR};
use clap::{Args, Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// The environment variable that sets how much the daemon says about itself.
const LOG_VAR: &str = "AGENTBUS_LOG";

/// The environment variable behind `--stale-secs`.
const STALE_SECS_VAR: &str = "AGENTBUS_STALE_SECS";

/// The environment variable behind `--done-retention-secs`.
const DONE_RETENTION_SECS_VAR: &str = "AGENTBUS_DONE_RETENTION_SECS";

/// The status a second daemon exits with when one is already running.
///
/// Distinct from the general failure code because it is often not a failure at
/// all: something whose goal is "a daemon is running here" has got what it
/// wanted, and can say so by treating this one code as success.
const ALREADY_RUNNING: u8 = 3;

/// How each command names itself in what it prints, so that a message read out
/// of a supervisor's log or a shell's scrollback says which one produced it.
const DAEMON: &str = "agentbus daemon";
const SUBSCRIBE: &str = "agentbus subscribe";
const STATUS: &str = "agentbus status";

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
}

impl DaemonArgs {
    /// The timings to start the bus with.
    fn settings(&self) -> Settings {
        Settings {
            stale_after: Duration::from_secs(self.stale_secs),
            done_retention: Duration::from_secs(self.done_retention_secs),
            ..Settings::default()
        }
    }
}

/// How to follow the stream.
#[derive(Debug, Args)]
struct SubscribeArgs {
    #[command(flatten)]
    location: Location,
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
    match Cli::try_parse_from(args) {
        Ok(cli) => match cli.command {
            Command::Daemon(args) => daemon(&args),
            Command::Subscribe(args) => subscribe(&args),
            Command::Status(args) => status(&args),
        },
        // `--version` and `--help` arrive here too: clap reports them as errors
        // that carry the rendered text, a zero exit code and stdout as their
        // destination. Letting clap print keeps that routing in one place.
        Err(err) => {
            let _ = err.print();
            ExitCode::from(u8::try_from(err.exit_code()).unwrap_or(2))
        }
    }
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
fn subscribe(args: &SubscribeArgs) -> ExitCode {
    let mut stream = match stream::connect(&args.location.paths()) {
        Ok(stream) => stream,
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

/// Reports a failure on stderr and gives the shell a non-zero status.
///
/// `context` names the command that failed, so that a message read out of a
/// supervisor's log says which of them produced it.
fn fail(context: &str, error: &dyn std::error::Error) -> ExitCode {
    eprintln!("{context}: {error}");
    let mut cause = error.source();
    while let Some(error) = cause {
        eprintln!("  caused by: {error}");
        cause = error.source();
    }
    ExitCode::FAILURE
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
