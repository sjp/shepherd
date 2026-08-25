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
pub mod detect;
pub mod emit;
pub mod ensure;
pub mod foreground;
pub mod hooks;
pub mod install;
pub mod manifests;
pub mod status;
pub mod stream;
pub mod table;
pub mod targets;

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentbus_daemon::remote::attachments::Attachments;
use agentbus_daemon::remote::bootstrap::Bootstrap;
use agentbus_daemon::remote::docker::Docker;
use agentbus_daemon::remote::provision::{Hooks, Provision};
use agentbus_daemon::remote::ssh;
use agentbus_daemon::remote::targets::{CONFIG_DIR_VAR, DOCKER, SSH, Targets};
use agentbus_daemon::remote::transport::Transport;
use agentbus_daemon::{Daemon, Settings, SocketPaths, clock};
use agentbus_detect::{CATALOG_URL_VAR, CheckResult, ManifestStore, Status, catalog_url};
use agentbus_install::{Agent, Environment, Mode, Outcome, Recommendation, UnknownAgent};
use agentbus_paths::DIR_VAR;
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

/// The environment variable behind `--assert-hold-secs`.
const ASSERT_HOLD_SECS_VAR: &str = "AGENTBUS_ASSERT_HOLD_SECS";

/// The environment variable behind `--proc-root`.
const PROC_ROOT_VAR: &str = "AGENTBUS_PROC_ROOT";

/// The environment variable behind `--no-update-manifests`.
///
/// Named for the thing rather than for the flag: a variable spelled as a
/// negation is read as a double negative by whoever has to set it to zero in a
/// unit file. Anything falsey — `0`, `false`, `off`, `no` — turns the checks
/// off, and the flag is the same answer given on the command line.
const UPDATE_MANIFESTS_VAR: &str = "AGENTBUS_UPDATE_MANIFESTS";

/// The status `foreground` exits with when there is nothing to report from: no
/// daemon to ask, or one that is not watching a process table.
///
/// Distinct from the general failure code because this command already uses
/// that one to mean something narrower — a filter that matched nothing — and a
/// caller has to be able to tell "no such correlation" from "nobody is looking".
const NOT_WATCHING: u8 = 2;

/// What is said on stderr when a daemon cannot see a process table.
const UNAVAILABLE: &str = "foreground monitoring unavailable on this daemon";

/// The status `detect` exits with when it cannot say whose screen it was given.
///
/// Distinct from the general failure code because nothing failed: stdin was
/// read and the manifests were consulted, and the outcome is that no agent on
/// this machine answers to the evidence. A caller looping over terminals wants
/// to skip that one and carry on, which it can only do if it can tell this from
/// a screen it could not read at all.
const UNIDENTIFIED: u8 = 2;

/// The status a second daemon exits with when one is already running.
///
/// Distinct from the general failure code because it is often not a failure at
/// all: something whose goal is "a daemon is running here" has got what it
/// wanted, and can say so by treating this one code as success.
const ALREADY_RUNNING: u8 = 3;

/// The status a command line that does not make sense exits with, which is the
/// one the parser itself uses for the same thing.
const USAGE: u8 = 2;

/// What this program is asked at a far end to find out whether the copy there
/// is the one that was wanted.
const VERSION_FLAG: &str = "--version";

/// How each command names itself in what it prints, so that a message read out
/// of a supervisor's log or a shell's scrollback says which one produced it.
const DAEMON: &str = "agentbus daemon";
const EMIT: &str = "agentbus emit";
const SUBSCRIBE: &str = "agentbus subscribe";
const STATUS: &str = "agentbus status";
const FOREGROUND: &str = "agentbus foreground";
const DETECT: &str = "agentbus detect";
const INSTALL: &str = "agentbus install";
const UNINSTALL: &str = "agentbus uninstall";
const ATTACH: &str = "agentbus attach";
const DETACH: &str = "agentbus detach";
const TARGETS: &str = "agentbus targets";
const MANIFESTS: &str = "agentbus manifests";
const HOOKS: &str = "agentbus hooks";

/// What is said after a machine has been provisioned and its agents have been
/// left alone, which is what happens unless somebody asks for the other half.
const UNTOUCHED: &str =
    "the coding agents there were not touched; pass --with-hooks to wire them up";

/// What is said after a failure nothing will retry its way out of.
///
/// Every connection this makes is asked for with `BatchMode`, so ssh fails
/// rather than prompting: a command that sat waiting for a passphrase nobody is
/// there to type would be worse than one that says so. What to do about it is
/// the same thing whichever credential is missing — open the connection by hand
/// once, and let the multiplexed connection that leaves behind carry this.
const ATTENTION: &str = "if that machine wants a passphrase or a confirmation, \
                         connect to it by hand once with the same words and run this again";

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
    /// Read a captured screen on stdin and say what state it is evidence of.
    ///
    /// The screen is plain text: escape sequences already removed, one line per
    /// row, and the rows the agent is drawing now rather than a scrolled-back
    /// view of what it drew earlier. Stripping the escape sequences is the
    /// caller's job, because whatever captured the screen is what knows how.
    ///
    /// One word is printed — working, idle, blocked or unknown — and the status
    /// is 0. A screen no agent on this machine answers for prints nothing and
    /// exits 2; that is a different answer from unknown, which is a verdict on a
    /// screen that was read.
    ///
    /// The rules come from the manifest in force for that agent: a copy the
    /// operator wrote first, then one fetched into this machine's state
    /// directory, then the copy inside this binary. Pass --explain to be told
    /// which copy answered and what it made of every rule.
    ///
    /// Pass --emit to send the verdict to the bus as a claim about the slot the
    /// screen came from instead of printing it. A caller doing that is a loop,
    /// and the cadence that suits the bus is: send when the verdict changes, and
    /// send the same claim again every second or two for as long as it is still
    /// on the screen. The bus stops preferring a claim nobody is repeating, so
    /// an observer that says something once and goes quiet is treated as one
    /// that has stopped looking — which, from where the bus is standing, it has.
    Detect(DetectArgs),
    /// Read a payload on stdin, normalize it, and send it to the bus.
    Emit(EmitArgs),
    /// Put the hooks that emit events into the coding agents on this machine, or put this program somewhere that is not it.
    Install(InstallArgs),
    /// Take those hooks back out again.
    Uninstall(InstallArgs),
    /// Report on the hooks this program has put into the coding agents on this machine.
    #[command(subcommand)]
    Hooks(HooksCommand),
    /// Declare another endpoint whose events should be merged into this bus.
    Attach(DeclarationArgs),
    /// Stop wanting one attached.
    Detach(DeclarationArgs),
    /// Print what has been declared and what the bus is doing about it, once.
    Targets(TargetsArgs),
    /// Report on the manifests this machine reads screens and hook payloads with, and fetch newer ones.
    #[command(subcommand)]
    Manifests(ManifestsCommand),
}

/// What to ask about the hooks installed on this machine.
///
/// A group of its own rather than a flag on `status`, because `status` is
/// already the question of what the sessions on the bus are doing. These are
/// questions about files on a disk, which is a different subject with the same
/// name.
#[derive(Debug, Subcommand)]
enum HooksCommand {
    /// Print what each agent's hooks are: what this build writes, older than it, in need of repair, or absent.
    ///
    /// Every agent this program knows is reported on, including the ones that
    /// are not on this machine, so that the list is the same length whatever
    /// the answers are. One that is here and has no hooks is told which command
    /// would give it some.
    ///
    /// The answers are read from the files themselves rather than from this
    /// program's record of what it wrote, so a machine whose configuration was
    /// copied from somewhere else, or edited by hand, is reported as it now is.
    Status(HooksStatusArgs),
}

/// What to do about the manifests on this machine.
///
/// Three copies of one agent's manifest can sit here at once — the copy inside
/// this binary, a copy fetched from a catalog, and a copy somebody wrote — and
/// these are the commands for finding out which of them is in force, taking
/// whatever has been published since, and reading the one that is answering.
#[derive(Debug, Subcommand)]
enum ManifestsCommand {
    /// Print which copy of each manifest is in force, and what the last check made of it.
    List(ListArgs),
    /// Fetch whatever the catalog publishes that is newer than what is held here.
    ///
    /// Every manifest listed is checked. One that cannot be fetched, does not
    /// validate, or is not newer than what is already here costs itself and
    /// nothing else: the rest are still taken, and the reason is printed and
    /// recorded. The status is non-zero only when the catalog itself could not
    /// be read, because that is the one outcome where nothing was checked at
    /// all.
    Update(UpdateArgs),
    /// Print the manifest that is in force for one agent, exactly as it is written.
    ///
    /// The copy that answers goes to stdout and everything about where it came
    /// from goes to stderr, so redirecting stdout into the override directory
    /// is the way to start editing one:
    ///
    ///   agentbus manifests show claude > ~/.config/agentbus/manifests/screen/claude.toml
    Show(ShowArgs),
}

/// What to report about the hooks here, and how.
#[derive(Debug, Args)]
struct HooksStatusArgs {
    /// Print only the agents whose hooks are older than this build's, or do not run
    #[arg(long)]
    outdated_only: bool,

    /// Print the report as JSON instead of as lines
    #[arg(long)]
    json: bool,
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

/// Where the declared endpoints are kept.
///
/// Separate from [`Location`] because the two directories answer different
/// questions and are resolved by different rules: one holds a running daemon's
/// sockets and is as short-lived as the session, and this one holds what
/// somebody has asked for and outlives every daemon that reads it.
#[derive(Debug, Args)]
struct Config {
    /// Directory holding the declared endpoints [default: the user's configuration directory]
    #[arg(long, value_name = "PATH", env = CONFIG_DIR_VAR)]
    config_dir: Option<PathBuf>,
}

impl Config {
    /// Where the declarations are.
    fn targets(&self) -> Targets {
        match self
            .config_dir
            .as_ref()
            .filter(|dir| !dir.as_os_str().is_empty())
        {
            Some(dir) => Targets::in_dir(dir),
            None => Targets::resolve(),
        }
    }
}

/// How to run the bus.
#[derive(Debug, Args)]
struct DaemonArgs {
    #[command(flatten)]
    location: Location,

    #[command(flatten)]
    config: Config,

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

    /// Seconds an observer's claim stands before it has to be made again
    #[arg(
        long,
        value_name = "SECS",
        env = ASSERT_HOLD_SECS_VAR,
        default_value_t = Settings::default().assert_hold.as_secs(),
    )]
    assert_hold_secs: u64,

    /// How much to say on stderr: a level — off, error, warn, info, debug, trace — or a filter naming targets
    #[arg(
        long,
        value_name = "LEVEL",
        env = LOG_VAR,
        default_value = "info",
        value_parser = filter,
    )]
    log_level: String,

    /// Do not take newer detection manifests from the published catalog
    #[arg(
        long = "no-update-manifests",
        action = clap::ArgAction::SetFalse,
        env = UPDATE_MANIFESTS_VAR,
        value_parser = clap::builder::BoolishValueParser::new(),
        default_value_t = Settings::default().update_manifests,
    )]
    update_manifests: bool,

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
            assert_hold: Duration::from_secs(self.assert_hold_secs),
            proc_root: self.proc_root.clone(),
            update_manifests: self.update_manifests,
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

/// Which screen to read, whose it is, and how much to say about it.
///
/// Everything the answer depends on arrives here: the text on stdin and these
/// flags. Nothing is read off the machine this runs on except the manifests
/// themselves, so the same screen and the same flags give the same answer
/// wherever they are run, which is what makes a captured screen worth keeping
/// as a test case.
#[derive(Debug, Args)]
struct DetectArgs {
    #[command(flatten)]
    location: Location,

    /// The agent whose screen is on stdin, when the caller already knows
    #[arg(long, value_name = "ID")]
    agent: Option<String>,

    /// The name of the process drawing the screen, used to work out the agent
    #[arg(long, value_name = "COMM")]
    process: Option<String>,

    /// The command line that process was started with, used the same way
    #[arg(long, value_name = "TEXT")]
    cmdline: Option<String>,

    /// The last title the agent asked the terminal to show
    #[arg(long, value_name = "TEXT")]
    osc_title: Option<String>,

    /// The last progress report the agent sent the terminal
    #[arg(long, value_name = "TEXT")]
    osc_progress: Option<String>,

    /// Print the whole verdict as one JSON object instead of one word
    #[arg(long, conflicts_with = "explain")]
    json: bool,

    /// Print every rule in the agent's manifest that ran, what it saw and why the winner won, as JSON
    #[arg(long)]
    explain: bool,

    /// Send the verdict to the bus as a claim about --correlation instead of printing it
    ///
    /// Nothing is printed and the status is 0 whether or not a bus was
    /// listening, so a loop is safe to leave running across a restart of it.
    /// What is sent is the state, whether its chrome is live on the screen, and
    /// which rule concluded it — never any of the screen.
    #[arg(long, conflicts_with_all = ["json", "explain"])]
    emit: bool,

    /// What the claim is about, copied verbatim [default: the correlation in this process's environment]
    #[arg(long, value_name = "TEXT", requires = "emit")]
    correlation: Option<String>,

    /// The agent's own session id, where the caller knows it
    #[arg(long, value_name = "ID", requires = "emit")]
    session: Option<String>,

    /// The directory that session is working in, where the caller knows it
    #[arg(long, value_name = "DIR", requires = "emit")]
    cwd: Option<String>,
}

impl DetectArgs {
    /// How much of the answer was asked for.
    fn form(&self) -> detect::Form {
        match (self.json, self.explain) {
            (_, true) => detect::Form::Explain,
            (true, _) => detect::Form::Json,
            _ => detect::Form::Word,
        }
    }
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

    /// Which of the agent's events the payload is about, for an agent whose payload does not say
    #[arg(long, value_name = "EVENT")]
    event: Option<String>,

    /// Where the claim comes from: hook, the default, or observed
    #[arg(long, value_name = "SOURCE")]
    source: Option<String>,
}

/// Which agents to act on, and whether to act at all — or, instead, which
/// endpoint that is not this machine to act on.
#[derive(Debug, Args)]
struct InstallArgs {
    #[command(subcommand)]
    endpoint: Option<Endpoint>,

    /// The agent to act on; repeat to name several [default: every one detected]
    #[arg(long, value_name = "NAME", value_parser = agent)]
    agent: Vec<Agent>,

    /// Say what would change, and change nothing
    #[arg(long)]
    dry_run: bool,
}

/// Somewhere other than this machine to put this program, or take it back from.
#[derive(Debug, Subcommand)]
enum Endpoint {
    /// A container on this machine: put a copy of this program in it, wire up
    /// whichever agents are in there, and keep it attached
    Docker(ContainerArgs),
    /// A machine reached over ssh: install a copy of this program on it, or
    /// take it back off, leaving everything else on it alone unless asked
    Ssh(HostArgs),
}

/// Which machine, and how much of it to touch.
#[derive(Debug, Args)]
struct HostArgs {
    #[command(flatten)]
    config: Config,

    /// Also wire up the coding agents on that machine, or take that wiring back out
    #[arg(long)]
    with_hooks: bool,

    /// The words that would reach the machine with ssh, after `--`
    #[arg(
        value_name = "ARGS",
        required = true,
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
    )]
    args: Vec<String>,
}

impl HostArgs {
    /// The words to reach the machine with, without the separator that kept
    /// this command's own options away from them.
    fn words(&self) -> &[String] {
        match self.args.first().map(String::as_str) {
            Some("--") => &self.args[1..],
            _ => &self.args,
        }
    }

    /// Whether the agents on that machine are part of what was asked for.
    fn hooks(&self) -> Hooks {
        match self.with_hooks {
            true => Hooks::Included,
            false => Hooks::Untouched,
        }
    }
}

/// Which container, and where the declaration about it is kept.
#[derive(Debug, Args)]
struct ContainerArgs {
    /// The container: its name, or as much of its id as is unambiguous
    #[arg(value_name = "CONTAINER")]
    container: String,

    #[command(flatten)]
    config: Config,
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

/// Which endpoint is being declared, or undeclared.
///
/// The words are taken as they were typed and handed on unexamined: what they
/// mean is the business of the transport that will be given them, and this
/// command has no way of knowing what a version of `ssh` on another machine
/// accepts. So the only thing checked here is that there are some.
#[derive(Debug, Args)]
struct DeclarationArgs {
    #[command(flatten)]
    config: Config,

    /// The transport and what it needs: `docker <container>`, or ssh arguments after `--`
    #[arg(
        value_name = "ARGS",
        required = true,
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
    )]
    args: Vec<String>,
}

/// What to report about the declared endpoints, and how.
#[derive(Debug, Args)]
struct TargetsArgs {
    #[command(flatten)]
    location: Location,

    #[command(flatten)]
    config: Config,

    /// Print the merged structure as JSON instead of as a table
    #[arg(long)]
    json: bool,
}

/// What to report about the manifests here, and how.
#[derive(Debug, Args)]
struct ListArgs {
    /// Print the list as JSON instead of as a table
    #[arg(long)]
    json: bool,
}

/// Where to check, and how to report what came of it.
#[derive(Debug, Args)]
struct UpdateArgs {
    /// The catalog to read [default: the one this build was published alongside]
    #[arg(long, value_name = "URL", env = CATALOG_URL_VAR)]
    catalog: Option<String>,

    /// Print what happened as JSON instead of as a table
    #[arg(long)]
    json: bool,
}

impl UpdateArgs {
    /// The catalog to read. An empty value names none, so it falls through to
    /// the default rather than being checked as a url with nothing in it.
    fn url(&self) -> String {
        self.catalog
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map_or_else(catalog_url, ToOwned::to_owned)
    }
}

/// Which manifest to print.
#[derive(Debug, Args)]
struct ShowArgs {
    /// The agent it describes
    #[arg(value_name = "AGENT")]
    agent: String,

    /// Which kind of manifest to print
    #[arg(long, value_name = "FAMILY", default_value = "screen")]
    family: manifests::Family,
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
            Command::Detect(args) => detect(&args),
            Command::Emit(args) => emit(&args, started),
            Command::Install(args) => elsewhere(&args, install::Direction::Install),
            Command::Uninstall(args) => elsewhere(&args, install::Direction::Uninstall),
            Command::Hooks(command) => match command {
                HooksCommand::Status(args) => hooks_status(&args),
            },
            Command::Attach(args) => attach(&args),
            Command::Detach(args) => detach(&args),
            Command::Targets(args) => targets(&args),
            Command::Manifests(command) => match command {
                ManifestsCommand::List(args) => list_manifests(&args),
                ManifestsCommand::Update(args) => update_manifests(&args),
                ManifestsCommand::Show(args) => show_manifest(&args),
            },
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
        assert_hold_secs = args.assert_hold_secs,
        update_manifests = args.update_manifests,
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
        match Daemon::bind(paths, args.settings())
            .map(|daemon| daemon.declaring(args.config.targets()))
        {
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
                .is_none_or(|correlation| entry.correlation.as_ref() == Some(correlation))
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

/// Says what a captured screen is evidence of.
///
/// The three exit codes separate three different things, and the middle one is
/// the reason they cannot collapse. `unknown` on stdout is an answer — the
/// screen was read against somebody's manifest and says nothing either way —
/// while [`UNIDENTIFIED`] is the case where there was no manifest to read it
/// with, and the general failure code stays with the failures: stdin that could
/// not be read, stdout that could not be written.
///
/// The manifests come from the environment, which is where a person's own
/// copies and any that have been fetched already live. A screen that reads
/// oddly is usually a manifest question, so `--explain` says which copy
/// answered and repeats on stderr whatever the store passed over to reach it.
///
/// The screen is read before anything else is decided, `--emit` included.
/// Whatever captured it is holding the other end of the pipe, and a process
/// that exits before draining it leaves that end broken — a way of interfering
/// with somebody's capture command, and no way to answer a question about a
/// correlation this was never given.
fn detect(args: &DetectArgs) -> ExitCode {
    let (screen, truncated) = match detect::read_screen(std::io::stdin().lock()) {
        Ok(read) => read,
        Err(error) => return fail(DETECT, &error),
    };
    if truncated {
        eprintln!("{DETECT}: {}", detect::TRUNCATED);
    }

    let request = detect::Request {
        agent: args.agent.as_deref(),
        process: args.process.as_deref().unwrap_or_default(),
        cmdline: args.cmdline.as_deref().unwrap_or_default(),
        screen: &screen,
        osc_title: args.osc_title.as_deref().unwrap_or_default(),
        osc_progress: args.osc_progress.as_deref().unwrap_or_default(),
    };
    if args.emit {
        return assert_detection(args, &request);
    }

    let form = args.form();
    let Some(answer) = detect::answer(&ManifestStore::from_env(), &request, form) else {
        eprintln!("{DETECT}: {}", detect::UNIDENTIFIED);
        return ExitCode::from(UNIDENTIFIED);
    };
    if form == detect::Form::Explain {
        for warning in &answer.warnings {
            eprintln!("{DETECT}: {warning}");
        }
    }

    let mut out = std::io::stdout().lock();
    match out
        .write_all(answer.text.as_bytes())
        .and_then(|()| out.flush())
    {
        Ok(()) => ExitCode::SUCCESS,
        // Whoever was reading this has finished, which is how a pipe is
        // supposed to end rather than something to complain about.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => fail(DETECT, &error),
    }
}

/// Sends what the screen was evidence of to the bus, rather than printing it.
///
/// Three of the four ways out are zero. A claim that went to a listening bus, a
/// screen whose verdict was that it says nothing about now, and a machine with
/// no bus running are all this command working: the caller asked for the screen
/// to be reported and the screen has been reported as fully as it can be.
/// Exiting non-zero on the last of them would make an observer loop noisy for
/// as long as the bus is down, which is exactly when nobody is reading it.
///
/// The fourth is the pair of things this cannot report about: a screen nobody
/// can be named for, and a claim nothing can be attributed to. Both leave with
/// [`UNIDENTIFIED`], because both are the same shape of answer — this ran, and
/// there is no claim to be made — and a caller sweeping terminals wants to skip
/// that one and carry on rather than treat it as a failure of the command.
fn assert_detection(args: &DetectArgs, request: &detect::Request<'_>) -> ExitCode {
    // A claim is about something, and nothing here says what: the bus would
    // have nowhere to file it, so it is not sent. Asked before the manifests
    // are consulted because it is the caller's own mistake and cannot become
    // anything else however the screen reads.
    let Some(correlation) = correlation(args) else {
        eprintln!("{DETECT}: {}", detect::UNATTRIBUTABLE);
        return ExitCode::from(UNIDENTIFIED);
    };
    let attribution = detect::Attribution {
        correlation: &correlation,
        session: args.session.as_deref(),
        cwd: args.cwd.as_deref(),
    };
    let store = ManifestStore::from_env();
    let Some(claim) = detect::claim(&store, request, &attribution) else {
        eprintln!("{DETECT}: {}", detect::UNIDENTIFIED);
        return ExitCode::from(UNIDENTIFIED);
    };
    let detect::Claim::Send(assertion) = claim else {
        return ExitCode::SUCCESS;
    };
    match detect::send(&assertion, args.location.paths().emit()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(DETECT, &error),
    }
}

/// What a claim about this screen is about: the flag, then the environment.
///
/// The two variables are the ones the hook client reads for the same purpose
/// and are read the same way — copied, never looked at, and an empty value
/// treated as no value, because a value that says nothing cannot tie anything
/// to anything. The flag comes first because a program capturing somebody
/// else's terminal knows which slot it captured, and its own environment is
/// about the shell it is running in rather than the one it is looking at.
fn correlation(args: &DetectArgs) -> Option<String> {
    let named = args.correlation.clone();
    let pane = std::env::var(emit::PANE_VAR).ok();
    let fallback = std::env::var(emit::PANE_FALLBACK_VAR).ok();
    [named, pane, fallback]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())
}

/// Sends one event to the bus, and says nothing whatever about how it went.
///
/// This command runs inside somebody else's coding agent, which reads what its
/// hooks print and what they exit with as instructions about the user's own
/// session. So there is one exit code here and it is zero, there is no output,
/// and every decision that could go either way is made inside [`emit::run`],
/// where it is made silently. The environment is read here and nowhere below:
/// each value is whatever its variable holds, copied without being looked at,
/// and an empty value is the same as an unset one because a value that says
/// nothing cannot tie anything to anything.
///
/// Three variables and three `getenv` calls, which is what this path can
/// afford. The second correlation name is read because a shell reached over a
/// connection inherits only what the far end let through, and the connection
/// itself is read because a shell that inherited neither name is still worth
/// placing.
fn emit(args: &EmitArgs, started: Instant) -> ExitCode {
    let pane = std::env::var_os(emit::PANE_VAR);
    let fallback = std::env::var_os(emit::PANE_FALLBACK_VAR);
    let connection = std::env::var_os(emit::SSH_CONNECTION_VAR);
    let request = emit::Request {
        agent: args.agent.as_deref(),
        event: args.event.as_deref(),
        source: args.source.as_deref(),
        correlation: stated(&pane).or_else(|| stated(&fallback)),
        ssh_connection: stated(&connection),
    };
    emit::run(&request, &args.location.paths(), started, std::io::stdin());
    ExitCode::SUCCESS
}

/// What an environment variable says, where it says anything at all.
///
/// A variable that is unset, one set to nothing, and one holding bytes that are
/// not text are the same answer: this process was told nothing.
fn stated(value: &Option<std::ffi::OsString>) -> Option<&str> {
    value
        .as_deref()
        .and_then(std::ffi::OsStr::to_str)
        .filter(|value| !value.is_empty())
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
fn here(args: &InstallArgs, direction: install::Direction) -> ExitCode {
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
    // An agent this build has no installer for is passed over in a run that
    // asked about the whole machine — there is nothing to say about it that the
    // report does not already say by leaving it out — but refused in a run that
    // named it, which is somebody asking a question that deserves an answer.
    let unhandled: Vec<Agent> = args
        .agent
        .iter()
        .copied()
        .filter(|agent| !agentbus_install::supported().contains(agent))
        .collect();
    if !unhandled.is_empty() {
        eprintln!("{context}: {}", install::unhandled(&unhandled, direction));
        return ExitCode::from(USAGE);
    }
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
    if let Err(error) = std::io::stdout().write_all(report.as_bytes()) {
        return fail(context, &error);
    }
    if direction == install::Direction::Install {
        if let Some(sentence) = install::also_behind(&others_behind(&env, &outcomes)) {
            eprintln!("{context}: {sentence}");
        }
    }
    ExitCode::SUCCESS
}

/// The agents this run was not about whose hooks are older than what this build
/// writes, or are no longer run by anything.
///
/// Somebody who installs for one agent has told this program which agent they
/// care about, not that the rest of their machine has stopped mattering — and
/// the moment they are already thinking about their hooks is the cheapest one
/// they will ever get to hear that the others are behind. A machine that cannot
/// be read for this says nothing: the run being reported on succeeded, and a
/// remark beside it is no reason to say otherwise.
fn others_behind(env: &Environment, outcomes: &[Outcome]) -> Vec<Agent> {
    let Ok(reported) = agentbus_install::recommendations(env) else {
        return Vec::new();
    };
    hooks::behind(&reported)
        .into_iter()
        .filter(|agent| !outcomes.iter().any(|outcome| outcome.agent == *agent))
        .collect()
}

/// Prints what this program's hooks are in each agent on this machine.
///
/// Nothing has to be installed for this to answer, and nothing about it changes
/// the machine: it is the report a user reads before deciding whether to run
/// anything at all, so it says what is there and stops.
fn hooks_status(args: &HooksStatusArgs) -> ExitCode {
    let env = match Environment::from_env() {
        Ok(env) => env,
        Err(error) => return fail(HOOKS, &error),
    };
    let reported = match agentbus_install::recommendations(&env) {
        Ok(reported) => reported,
        Err(error) => return fail(HOOKS, &error),
    };
    let reported: Vec<&Recommendation> = reported
        .iter()
        .filter(|one| !args.outdated_only || hooks::is_behind(one))
        .collect();

    let text = match args.json {
        true => hooks::json(&reported),
        false => hooks::render(&reported),
    };
    wrote(HOOKS, &text)
}

/// Acts on this machine, or on the endpoint the command line named instead.
///
/// The two flags choose which of *this* machine's agents to act on and whether
/// to act at all, and neither means anything about a container: what gets
/// installed in there is whatever turns out to be in there, and there is no
/// halfway version of putting a binary on a machine. So a command line carrying
/// both is refused rather than quietly doing half of what it says.
fn elsewhere(args: &InstallArgs, direction: install::Direction) -> ExitCode {
    let Some(endpoint) = &args.endpoint else {
        return here(args, direction);
    };
    let context = direction.context();
    if !args.agent.is_empty() || args.dry_run {
        eprintln!(
            "{context}: --agent and --dry-run are about the agents on this \
             machine, and say nothing about an endpoint that is not it"
        );
        return ExitCode::from(USAGE);
    }
    match (endpoint, direction) {
        (Endpoint::Docker(container), install::Direction::Install) => provision(container),
        (Endpoint::Docker(container), install::Direction::Uninstall) => strip(container),
        (Endpoint::Ssh(host), install::Direction::Install) => settle(host),
        (Endpoint::Ssh(host), install::Direction::Uninstall) => unsettle(host),
    }
}

/// Puts a copy of this program into a container, wires up the agents in there,
/// and declares the container so that it stays attached.
///
/// Only the last of those three is this process's own doing. Establishing the
/// binary is the same provisioning a daemon does when it attaches, asked for by
/// hand here; wiring up the agents inside is what a container is told when that
/// has worked. Which means this command is worth having for exactly one reason:
/// a container that carries no devcontainer label is one nothing would ever
/// find, and this is how somebody says they want it anyway.
fn provision(args: &ContainerArgs) -> ExitCode {
    let container = Docker::resolve().container(&args.container);
    let mut running =
        match Bootstrap::new(agentbus_daemon::VERSION).run(&container, &[VERSION_FLAG]) {
            Ok(running) => running,
            Err(error) => return fail(INSTALL, &error),
        };
    let mut answered = String::new();
    if let Err(error) = running.stdout().read_line(&mut answered) {
        return fail(INSTALL, &error);
    }
    match running.wait() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!(
                "{INSTALL}: the copy in {} would not say what it is: {status}",
                args.container
            );
            return ExitCode::FAILURE;
        }
        Err(error) => return fail(INSTALL, &error),
    }

    let words = vec![args.container.clone()];
    let there = format!("{} in {}", answered.trim(), args.container);
    match args.config.targets().declare(DOCKER, &words, &clock::now()) {
        Ok(true) => said(
            INSTALL,
            &format!("{there}\ndeclared: docker {}", args.container),
        ),
        Ok(false) => said(INSTALL, &format!("{there}\nalready declared")),
        Err(error) => fail(INSTALL, &error),
    }
}

/// Takes this program back out of a container and stops wanting it attached.
///
/// The container is left running and everything else in it is left alone: what
/// is removed is the hooks this program wrote and every copy of it that was put
/// there, which between them are the whole of what it left behind.
///
/// This takes back a declaration; it does not make a container invisible. One
/// that carries a devcontainer label is found by looking, and a daemon that is
/// running will attach to it again and put the hooks back — which is what being
/// found automatically means. This command is how a container that would never
/// have been found is let go of.
fn strip(args: &ContainerArgs) -> ExitCode {
    let container = Docker::resolve().container(&args.container);
    if let Err(error) = container.uninstall(agentbus_daemon::VERSION) {
        return fail(UNINSTALL, &error);
    }
    let words = vec![args.container.clone()];
    let there = format!("taken out of {}", args.container);
    match args.config.targets().undeclare(DOCKER, &words) {
        Ok(true) => said(UNINSTALL, &format!("{there}\nno longer declared")),
        Ok(false) => said(UNINSTALL, &there),
        Err(error) => fail(UNINSTALL, &error),
    }
}

/// Installs a copy of this program on a machine reached over ssh.
///
/// Everything about this is the opposite of what a container gets, and for one
/// reason: this is somebody's own machine. The copy is permanent, because
/// re-sending it down a link that may be slow every time something reconnects
/// is waste; it goes to one path and over nothing this program did not write;
/// and the agents on that machine are left exactly as they are unless the
/// command line says otherwise in so many words.
///
/// The machine is not declared here either. Declaring is `attach`, it is a
/// separate decision, and somebody may well want a binary on a machine whose
/// events they do not want merged into this bus.
fn settle(args: &HostArgs) -> ExitCode {
    let Some(host) = reached(INSTALL, args.words()) else {
        return ExitCode::FAILURE;
    };
    let bootstrap = Bootstrap::new(agentbus_daemon::VERSION);
    let mut out = std::io::stdout();
    let code = match Provision::new(&bootstrap).install(host.as_ref(), args.hooks(), &mut out) {
        // Said here rather than by the provisioning, because how to ask for the
        // other half is this command line's business and not its.
        Ok(()) if args.hooks() == Hooks::Untouched => said(INSTALL, UNTOUCHED),
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => refused(INSTALL, host.as_ref(), &error),
    };
    shared(host.as_ref());
    code
}

/// Takes this program back off a machine reached over ssh.
fn unsettle(args: &HostArgs) -> ExitCode {
    let Some(host) = reached(UNINSTALL, args.words()) else {
        return ExitCode::FAILURE;
    };
    let bootstrap = Bootstrap::new(agentbus_daemon::VERSION);
    let mut out = std::io::stdout();
    let code = match Provision::new(&bootstrap).uninstall(host.as_ref(), args.hooks(), &mut out) {
        Ok(()) => remaining(args),
        Err(error) => refused(UNINSTALL, host.as_ref(), &error),
    };
    shared(host.as_ref());
    code
}

/// The machine those words reach, or nothing when they reach none — in which
/// case what ssh made of them has been said.
fn reached(context: &str, words: &[String]) -> Option<Arc<dyn Transport>> {
    let resolver = ssh::Resolver::new();
    let masters = ssh::Masters::resolve();
    match ssh::Host::declared(words, &resolver, &masters) {
        Ok(host) => Some(host),
        Err(problem) => {
            eprintln!("{context}: {problem}");
            None
        }
    }
}

/// Lets go of the machine without closing the connection to it.
///
/// A daemon serving this machine keeps its multiplexed connections in the same
/// place under the same names, which is what lets a command like this one reach
/// a host without paying for an authentication. The other side of that bargain
/// is that letting go here must not take down a connection something else is in
/// the middle of using.
fn shared(host: &dyn Transport) {
    host.keep_open();
}

/// Reminds whoever asked that the machine is still declared, which is a
/// different thing from having this program on it.
fn remaining(args: &HostArgs) -> ExitCode {
    let words = args.words();
    let still = match args.config.targets().list() {
        Ok(targets) => targets.iter().any(|target| target.is(SSH, words)),
        // A declarations file that cannot be read says nothing about whether
        // this machine is in it. This is a reminder rather than the thing that
        // was asked for, and what was asked for has already happened.
        Err(_) => false,
    };
    match still {
        true => said(
            UNINSTALL,
            &format!(
                "it is still declared: `agentbus detach -- {}` stops this bus attaching to it",
                words.join(" ")
            ),
        ),
        false => ExitCode::SUCCESS,
    }
}

/// Reports why provisioning did not happen, and says the one thing the failure
/// itself cannot: that this one needs a person.
fn refused(context: &str, host: &dyn Transport, error: &dyn std::error::Error) -> ExitCode {
    report(context, error);
    if !host.recoverable(error) {
        eprintln!("{context}: {ATTENTION}");
    }
    ExitCode::FAILURE
}

/// Declares an endpoint, so that whichever daemon serves this machine attaches
/// to it and keeps doing so.
///
/// Only the file is touched. A declaration is a thing to remember rather than a
/// thing to do, so it is written whether or not a daemon is running, and a
/// daemon that is running notices within a couple of seconds — which is also
/// what makes this the same operation on a machine somebody is setting up and
/// on one that has been running for a week.
fn attach(args: &DeclarationArgs) -> ExitCode {
    let declaration = match targets::declared(&args.args) {
        Ok(declaration) => declaration,
        Err(problem) => return fail(ATTACH, &problem),
    };
    match args
        .config
        .targets()
        .declare(declaration.transport, declaration.args, &clock::now())
    {
        Ok(true) => said(ATTACH, &format!("declared: {}", declaration.said())),
        // Not a failure: what was asked for is what the file already says, and
        // a second entry would only mean attaching to one endpoint twice.
        Ok(false) => said(ATTACH, "already declared"),
        Err(error) => fail(ATTACH, &error),
    }
}

/// Takes a declaration back, so that the endpoint is no longer attached to.
fn detach(args: &DeclarationArgs) -> ExitCode {
    let declaration = match targets::declared(&args.args) {
        Ok(declaration) => declaration,
        Err(problem) => return fail(DETACH, &problem),
    };
    match args
        .config
        .targets()
        .undeclare(declaration.transport, declaration.args)
    {
        Ok(true) => said(DETACH, "removed"),
        Ok(false) => said(DETACH, "not declared"),
        Err(error) => fail(DETACH, &error),
    }
}

/// Prints what has been declared and what the daemon here is doing about it.
///
/// Both files are read and neither has to be there. A machine with nothing
/// declared and no daemon running is not an error to report — it is the ordinary
/// state of a machine nobody has attached anything to — and a declaration whose
/// daemon is not running is reported as exactly that rather than as an
/// attachment in some unknown state.
fn targets(args: &TargetsArgs) -> ExitCode {
    let declared = match args.config.targets().list() {
        Ok(declared) => declared,
        Err(error) => return fail(TARGETS, &error),
    };
    let attached = match Attachments::in_dir(args.location.paths().dir()).read() {
        Ok(attached) => attached,
        Err(error) => return fail(TARGETS, &error),
    };
    let known = targets::merge(&declared, attached.as_deref());

    let styled = std::io::stdout().is_terminal();
    let text = match args.json {
        true => targets::json(&known, attached.is_some()),
        false => targets::render(&known, &clock::now(), styled),
    };
    wrote(TARGETS, &text)
}

/// Prints which copy of each manifest is in force here.
///
/// Nothing has to exist for this to answer: a machine that has never fetched
/// anything and overridden nothing still reads every screen with the copies
/// inside this binary, and saying so is the answer rather than an absence of
/// one.
fn list_manifests(args: &ListArgs) -> ExitCode {
    let store = ManifestStore::from_env();
    let status = Status::read(store.paths());
    let summaries = store.summaries();
    let listed = manifests::list(&summaries, &status);

    let styled = std::io::stdout().is_terminal();
    let text = match args.json {
        true => manifests::json(&listed, &status),
        false => manifests::render(&listed, &status, &clock::now(), styled),
    };
    wrote(MANIFESTS, &text)
}

/// Takes whatever the catalog publishes that is newer than what is here.
fn update_manifests(args: &UpdateArgs) -> ExitCode {
    let store = ManifestStore::from_env();
    let url = args.url();
    let outcome = agentbus_detect::update(&store, &url);
    if outcome.committed() {
        // The files this store's decisions were made from have just been
        // replaced. Nothing else runs in this process afterwards, but a store
        // that has been told is a store nothing can later read a stale answer
        // out of, and forgetting costs a hash map.
        store.reload();
    }

    let styled = std::io::stdout().is_terminal();
    let text = match args.json {
        true => manifests::outcome_json(&outcome),
        false => manifests::outcome(&outcome, styled),
    };
    let written = wrote(MANIFESTS, &text);
    match &outcome.result {
        CheckResult::Checked => written,
        // The one failure that is a failure of the command: nothing was
        // checked, so nothing can be said about whether this machine is up to
        // date. A manifest that was refused is reported and recorded like any
        // other outcome, and the run it was part of still did its job.
        CheckResult::Failed(reason) => {
            eprintln!("{MANIFESTS}: {url}: {reason}");
            ExitCode::FAILURE
        }
    }
}

/// Prints the manifest one agent is read with, as it is written.
fn show_manifest(args: &ShowArgs) -> ExitCode {
    let store = ManifestStore::from_env();
    let family = args.family;
    let active = match family {
        manifests::Family::Screen => store.screen_source(&args.agent),
        manifests::Family::Hooks => store.hook_source(&args.agent),
    };
    let Some(active) = active else {
        eprintln!(
            "{MANIFESTS}: no {} manifest describes {:?}",
            family.name(),
            args.agent,
        );
        return ExitCode::FAILURE;
    };

    eprintln!(
        "{MANIFESTS}: {}/{} is {}",
        family.name(),
        args.agent,
        manifests::describe(&active.source, store.paths(), family, &args.agent),
    );
    for warning in &active.warnings {
        eprintln!("{MANIFESTS}: {warning}");
    }
    wrote(MANIFESTS, &active.text)
}

/// Writes what a command produced to stdout, or fails saying why it could not.
fn wrote(context: &str, text: &str) -> ExitCode {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        // Whoever was reading this has finished, which is how a pipe is supposed
        // to end rather than something to complain about.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => fail(context, &error),
    }
}

/// Prints one line on stdout and succeeds, or fails saying why it could not.
fn said(context: &str, line: &str) -> ExitCode {
    match writeln!(std::io::stdout(), "{line}") {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
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
