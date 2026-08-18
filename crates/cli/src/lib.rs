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

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use agentbus_daemon::{Daemon, Settings, SocketPaths, paths::DIR_VAR};
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

/// How the daemon names itself in what it prints.
const DAEMON: &str = "agentbus daemon";

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
}

/// How to run the bus.
#[derive(Debug, Args)]
struct DaemonArgs {
    /// Directory to hold the bus's sockets [default: the session's runtime directory]
    #[arg(long, value_name = "PATH", env = DIR_VAR)]
    dir: Option<PathBuf>,

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
        }
    }

    /// Where to put the sockets.
    ///
    /// An empty value names no directory, so it falls through to the rest of the
    /// precedence rules rather than putting the sockets at a relative path in
    /// whatever the working directory happened to be.
    fn paths(&self) -> SocketPaths {
        match self.dir.as_ref().filter(|dir| !dir.as_os_str().is_empty()) {
            Some(dir) => SocketPaths::in_dir(dir),
            None => SocketPaths::resolve(),
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
    match Cli::try_parse_from(args) {
        Ok(cli) => match cli.command {
            Command::Daemon(args) => daemon(&args),
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
    let paths = args.paths();
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
