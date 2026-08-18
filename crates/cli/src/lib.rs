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

use std::ffi::OsString;
use std::process::ExitCode;

use agentbus_daemon::{Daemon, SocketPaths};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// The environment variable that turns logging on and sets its verbosity.
const LOG_VAR: &str = "RUST_LOG";

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
    Daemon,
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
            Command::Daemon => daemon(),
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

/// Runs the bus until the process is killed.
///
/// There is one code path here whatever the daemon is started by: nothing about
/// being run by hand, by a supervisor or by another program changes what it
/// does, so there is nothing to select between.
fn daemon() -> ExitCode {
    init_logging();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(&error),
    };
    runtime.block_on(async {
        match Daemon::bind(SocketPaths::resolve()) {
            // Only ever returns if the daemon stops serving, which it does not.
            Ok(daemon) => {
                daemon.run().await;
                ExitCode::SUCCESS
            }
            Err(error) => fail(&error),
        }
    })
}

/// Reports a failure on stderr and gives the shell a non-zero status.
fn fail(error: &dyn std::error::Error) -> ExitCode {
    eprintln!("agentbus: {error}");
    let mut cause = error.source();
    while let Some(error) = cause {
        eprintln!("  caused by: {error}");
        cause = error.source();
    }
    ExitCode::FAILURE
}

/// Sends diagnostics to stderr, at the verbosity the environment asks for.
///
/// Silent unless asked: this binary's output is read by programs, and a daemon
/// that narrates itself by default is noise in somebody's log for every run in
/// which nothing was wrong.
fn init_logging() {
    let filter = EnvFilter::try_from_env(LOG_VAR).unwrap_or_else(|_| EnvFilter::new("off"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
