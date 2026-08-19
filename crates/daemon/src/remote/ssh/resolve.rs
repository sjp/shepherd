//! What `ssh` makes of the words somebody typed.
//!
//! A declaration is an argument vector, kept exactly as it was written:
//! `fileserver`, `vscode@fileserver`, `-p 2222 -o StrictHostKeyChecking=no
//! bob@fs.example.net`, `-J bastion.example.com deep@inner`. Working out which
//! machine one of those names would mean evaluating every `Host` block, `Match`
//! block and `Include` in somebody's configuration, and resolving `ProxyJump`
//! chains, certificates, `IdentityAgent`, FIDO keys and
//! `CanonicalizeHostname` on the way through. That is ssh's own language, and
//! writing a dialect of it is a well-known way to be subtly wrong for years.
//!
//! So it is not written. `ssh -G` evaluates the configuration and prints what it
//! arrived at without connecting to anything, and everything here is that
//! command being run and its output being read. The daemon therefore agrees with
//! the `ssh` on the machine it is running on by construction, including about the
//! parts of it nobody here has heard of.
//!
//! # Three things follow from `-G` being a real ssh
//!
//! **It validates for free.** A flag this version of ssh does not have, or a
//! destination that is not there, ends in exit status 255 before anything has
//! been connected to. That is a declaration somebody has to correct, which is a
//! different thing from a machine that is down and may be back later, and it is
//! reported differently.
//!
//! **It must not be asked twice.** A `Match exec` block genuinely runs its
//! command while the configuration is being evaluated, so resolving on a timer
//! would be running somebody's shell command on a timer. Every answer is
//! remembered against the exact words that produced it, and forgotten when the
//! configuration underneath it changes.
//!
//! **A trailing word is a command.** ssh's grammar is `ssh [options] destination
//! [command...]`, so a declaration with two positional arguments in it would run
//! the second one on the far end in place of what this program means to run
//! there — `ssh host vim` becomes `ssh host vim agentbus subscribe`. Refusing
//! such a declaration is the only safe reading of it, and it happens before ssh
//! is run at all.

use std::collections::BTreeMap;
use std::ffi::{CStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use thiserror::Error;

use crate::remote::targets::HOME_VAR;

#[cfg(test)]
mod tests;

/// The command this drives when nothing else is supplied.
pub const DEFAULT_BIN: &str = "ssh";

/// The flag that asks ssh to evaluate the configuration, print the result and
/// connect to nothing.
pub const DUMP: &str = "-G";

/// The port ssh uses when nothing has said otherwise, and so the port a
/// resolution that mentions none is talking about.
pub const DEFAULT_PORT: u16 = 22;

/// The status ssh exits with when it is ssh itself that failed, as opposed to a
/// command at the far end. For a resolution — which never reaches a far end —
/// it means the declaration is not one ssh can make sense of.
const INVALID: i32 = 255;

/// The user's own configuration, below their home directory.
const USER_CONFIG: &str = ".ssh/config";

/// The configuration that applies to everyone on this machine.
const SYSTEM_CONFIG: &str = "/etc/ssh/ssh_config";

/// The short options OpenSSH's `ssh` reads a separate argument after.
///
/// This mirrors the option string in `ssh`'s own usage line, and it is the only
/// part of ssh's grammar restated here — enough to tell an option's value from
/// a positional argument, and nothing more. A later OpenSSH that adds an option
/// taking an argument needs this list extended; until it is, that option's value
/// looks like a positional argument and the declaration carrying it is refused
/// with a message saying so. That is the right way round to be wrong: a false
/// refusal is visible and correctable, and the alternative is silently running a
/// word somebody typed as a command on their machine.
const TAKES_ARGUMENT: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// The character every option starts with.
const DASH: char = '-';

/// The argument that ends the options and leaves everything after it positional.
const END_OF_OPTIONS: &str = "--";

/// Why a declaration could not be turned into what ssh makes of it.
///
/// The first two are decided here, before ssh is run; the rest are ssh's own
/// answers. Every one of them carries the words that were declared, because
/// whoever reads this is holding several declarations and needs to know which
/// of them is being complained about.
#[derive(Debug, Error)]
pub enum Error {
    /// The declaration has a command in it as well as a destination.
    #[error("{} carries a command to run, which a target may not", spoken(argv))]
    RemoteCommandNotAllowed {
        /// What was declared.
        argv: Vec<String>,
    },
    /// The declaration names no machine.
    #[error("{} names no destination", spoken(argv))]
    DestinationMissing {
        /// What was declared.
        argv: Vec<String>,
    },
    /// ssh could not be started at all.
    #[error("cannot run {DEFAULT_BIN} {DUMP} to resolve {}", spoken(argv))]
    Run {
        /// What was declared.
        argv: Vec<String>,
        /// What went wrong locally.
        #[source]
        source: io::Error,
    },
    /// ssh read the declaration and would not have it.
    ///
    /// A bad flag, a destination that resolves to nothing, a configuration that
    /// does not parse. Nothing was connected to and nothing will be: this is
    /// for whoever wrote the declaration, so it carries ssh's own words rather
    /// than a paraphrase of them.
    #[error("the target {} is invalid; ssh said: {}", spoken(argv), said(stderr))]
    TargetInvalid {
        /// What was declared.
        argv: Vec<String>,
        /// What ssh complained, verbatim.
        stderr: String,
    },
    /// ssh failed in some way that is not a verdict on the declaration.
    #[error(
        "cannot resolve {}: ssh exited with {status}; {}",
        spoken(argv),
        said(stderr)
    )]
    ResolveFailed {
        /// What was declared.
        argv: Vec<String>,
        /// How ssh ended.
        status: ExitStatus,
        /// What ssh complained, verbatim.
        stderr: String,
    },
}

/// A declaration as it is quoted back to whoever wrote it.
fn spoken(argv: &[String]) -> String {
    match argv.is_empty() {
        true => "an empty declaration".to_owned(),
        false => format!("`{}`", argv.join(" ")),
    }
}

/// What ssh complained, or that it did not.
fn said(stderr: &str) -> &str {
    match stderr.trim() {
        "" => "it said nothing",
        said => said,
    }
}

/// A way of running `ssh`.
///
/// Injectable for one reason: a resolution is the only part of reaching a
/// machine over ssh that can be exercised without one, and it stops being that
/// the moment the tests need the `ssh` on the machine they run on, whose
/// version and whose configuration they do not choose.
pub trait Runner: Send + Sync {
    /// Runs ssh with exactly `argv` and collects everything it said.
    fn run(&self, argv: &[String]) -> io::Result<Output>;
}

impl<F> Runner for F
where
    F: Fn(&[String]) -> io::Result<Output> + Send + Sync,
{
    fn run(&self, argv: &[String]) -> io::Result<Output> {
        self(argv)
    }
}

/// The `ssh` on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ssh {
    binary: PathBuf,
}

impl Ssh {
    /// The command found on the path.
    pub fn new() -> Self {
        Self::named(DEFAULT_BIN)
    }

    /// The command at a path or name the caller chose.
    pub fn named(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    /// What is being run, for saying so.
    pub fn binary(&self) -> &Path {
        &self.binary
    }
}

impl Default for Ssh {
    fn default() -> Self {
        Self::new()
    }
}

impl Runner for Ssh {
    /// Runs it with nothing on its standard input.
    ///
    /// `-G` neither reads nor asks for anything, but a `Match exec` block runs a
    /// command of somebody's choosing, and a command that inherits this
    /// process's standard input could sit there waiting on it. There is nothing
    /// to answer it with, so there is nothing to read.
    fn run(&self, argv: &[String]) -> io::Result<Output> {
        Command::new(&self.binary)
            .args(argv)
            .stdin(Stdio::null())
            .output()
    }
}

/// What ssh printed, read back.
///
/// Everything ssh said is kept, including the keys this program has never heard
/// of: the output is a whole configuration, it grows with every OpenSSH release,
/// and a reader that only kept what it recognized would quietly discard the part
/// that turned out to matter. Nothing here is validated either — a key with a
/// value this cannot make sense of is a key whose typed accessor answers
/// nothing, not an error, because the caller wanted a machine to attach to and
/// not an opinion about somebody's configuration file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    values: BTreeMap<String, Vec<String>>,
}

impl Resolved {
    /// Reads one `key value` pair per line.
    ///
    /// Keys are lowercased on the way in. ssh very nearly does that itself, and
    /// the exception is the reason this does not take its word for it: at least
    /// one key is printed with a capital in the middle of it
    /// (`canonicalizePermittedcnames`), so a lookup that trusted the output
    /// would miss it depending on the version. Values are kept as they were
    /// printed, spaces and all, because several of them are lists.
    ///
    /// A key printed more than once — `identityfile` and `sendenv` are the
    /// ordinary cases — keeps every value it was given, in the order they
    /// arrived.
    pub fn read(printed: &str) -> Self {
        let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for line in printed.lines() {
            let line = line.trim_end();
            if line.trim().is_empty() {
                continue;
            }
            let (key, value) = match line.find(char::is_whitespace) {
                Some(at) => (&line[..at], line[at..].trim_start()),
                None => (line, ""),
            };
            values
                .entry(key.to_ascii_lowercase())
                .or_default()
                .push(value.to_owned());
        }
        Self { values }
    }

    /// Every value printed for `key`, in the order ssh printed them, and nothing
    /// for a key it did not print.
    pub fn all(&self, key: &str) -> &[String] {
        self.values.get(key).map_or(&[], Vec::as_slice)
    }

    /// The first value printed for `key`.
    ///
    /// ssh prints the effective setting, so for everything except the handful of
    /// keys that are genuinely lists there is only one, and the first is it.
    pub fn first(&self, key: &str) -> Option<&str> {
        self.all(key).first().map(String::as_str)
    }

    /// Every key ssh printed, in order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    /// The destination as ssh understood it — the alias somebody typed, not
    /// what it resolved to.
    ///
    /// Not printed by every version of ssh, which is why the identity below
    /// falls back to it rather than being built on it.
    pub fn host(&self) -> Option<&str> {
        self.first("host")
    }

    /// Who ssh would log in as.
    pub fn user(&self) -> Option<&str> {
        self.first("user")
    }

    /// The name or address ssh would connect to, after any `HostName`.
    pub fn hostname(&self) -> Option<&str> {
        self.first("hostname")
    }

    /// The port ssh would connect to, and nothing for a port that is not a
    /// number ssh could have meant.
    pub fn port(&self) -> Option<u16> {
        self.first("port")?.parse().ok()
    }

    /// Where ssh would look for a multiplexed connection's socket.
    pub fn controlpath(&self) -> Option<&str> {
        self.first("controlpath")
    }

    /// The machines ssh would go through to get there.
    pub fn proxyjump(&self) -> Option<&str> {
        self.first("proxyjump")
    }

    /// The command ssh would talk to instead of a socket.
    pub fn proxycommand(&self) -> Option<&str> {
        self.first("proxycommand")
    }

    /// Whether ssh would refuse to ask a person anything, and nothing where it
    /// answered with a word that is neither yes nor no.
    pub fn batchmode(&self) -> Option<bool> {
        match self.first("batchmode")? {
            "yes" | "true" => Some(true),
            "no" | "false" => Some(false),
            _ => None,
        }
    }

    /// Who, where and on what port — as much of an identity as can be had
    /// without connecting to anything.
    ///
    /// It is provisional in a specific way. `-G` resolves aliases but does not
    /// canonicalize names: two aliases that a `Host` block points at one
    /// `HostName` come out identical here, and two names for one machine that
    /// only DNS could reconcile do not, because ssh has no reason to ask DNS
    /// anything while it is reading a configuration file. So this is worth
    /// exactly what it costs — enough to notice that two declarations obviously
    /// mean the same machine, and never enough to conclude that two of them do
    /// not.
    ///
    /// The user is what ssh would log in as, and where ssh printed none it is
    /// the name this process runs under, which is what ssh would have defaulted
    /// to itself. The host falls back to the destination as it was typed, for a
    /// version of ssh that prints no `hostname`. The port falls back to 22.
    pub fn provisional_identity(&self) -> (String, String, u16) {
        let user = self
            .user()
            .filter(|user| !user.is_empty())
            .map_or_else(login_name, str::to_owned);
        let hostname = self
            .hostname()
            .or_else(|| self.host())
            .unwrap_or_default()
            .to_owned();
        (user, hostname, self.port().unwrap_or(DEFAULT_PORT))
    }
}

/// Asks ssh what a declaration means, once per declaration.
///
/// Holding one of these is holding what ssh has already said, so it is shared
/// rather than made afresh: the whole point of it is that the question is asked
/// as few times as it can be.
pub struct Resolver {
    runner: Box<dyn Runner>,
    configs: Vec<PathBuf>,
    remembered: Mutex<Remembered>,
}

impl Resolver {
    /// The `ssh` on this machine, watching the configuration files it reads.
    pub fn new() -> Self {
        Self {
            runner: Box::new(Ssh::new()),
            configs: well_known(std::env::var_os(HOME_VAR)),
            remembered: Mutex::new(Remembered::default()),
        }
    }

    /// The same resolver, running ssh some other way.
    #[must_use]
    pub fn running(mut self, runner: impl Runner + 'static) -> Self {
        self.runner = Box::new(runner);
        self
    }

    /// The same resolver, forgetting what it knows when any of `configs`
    /// changes and when nothing else does.
    #[must_use]
    pub fn watching(mut self, configs: impl IntoIterator<Item = PathBuf>) -> Self {
        self.configs = configs.into_iter().collect();
        self
    }

    /// The files a change to which empties this resolver.
    pub fn configs(&self) -> &[PathBuf] {
        &self.configs
    }

    /// What ssh makes of `argv`, asking it only if it has not been asked this
    /// before.
    ///
    /// `argv` is everything somebody wrote after `--`, and it goes to ssh in
    /// that order with nothing added to it and nothing taken away. It is also
    /// the whole of the key this is remembered under: two declarations that
    /// differ only in the order of their options are two declarations, because
    /// deciding they were one would mean knowing which of ssh's options commute,
    /// which is knowing ssh's grammar.
    pub fn resolve(&self, argv: &[String]) -> Result<Arc<Resolved>, Error> {
        one_destination(argv)?;
        // Held across the run, which serializes resolutions. That is the
        // intended shape rather than a compromise: evaluating a configuration
        // may run a `Match exec` command, and two threads that both want the
        // same declaration should cost one of those between them, not one each.
        let mut remembered = self
            .remembered
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        remembered.forget_if_changed(&self.configs);
        if let Some(resolved) = remembered.resolved.get(argv) {
            return Ok(Arc::clone(resolved));
        }
        let resolved = Arc::new(self.ask(argv)?);
        remembered
            .resolved
            .insert(argv.to_vec(), Arc::clone(&resolved));
        Ok(resolved)
    }

    /// Forgets everything, so that the next question goes to ssh.
    ///
    /// For whoever wants an answer that is certainly current: what is watched
    /// for a change is the two configuration files ssh reads first, and a file
    /// one of those pulls in with `Include` can change without either of them
    /// being touched.
    pub fn invalidate_all(&self) {
        let mut remembered = self
            .remembered
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        remembered.resolved.clear();
        remembered.stamps.clear();
    }

    /// Runs `ssh -G` over the declaration and reads what came back.
    fn ask(&self, argv: &[String]) -> Result<Resolved, Error> {
        let mut asked = Vec::with_capacity(argv.len() + 1);
        asked.push(DUMP.to_owned());
        asked.extend_from_slice(argv);
        let output = self.runner.run(&asked).map_err(|source| Error::Run {
            argv: argv.to_vec(),
            source,
        })?;
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        match output.status.code() {
            Some(0) => Ok(Resolved::read(&String::from_utf8_lossy(&output.stdout))),
            Some(INVALID) => Err(Error::TargetInvalid {
                argv: argv.to_vec(),
                stderr,
            }),
            _ => Err(Error::ResolveFailed {
                argv: argv.to_vec(),
                status: output.status,
                stderr,
            }),
        }
    }
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver")
            .field("configs", &self.configs)
            .finish_non_exhaustive()
    }
}

/// What ssh has already said, and what was true of the configuration when it
/// said it.
#[derive(Debug, Default)]
struct Remembered {
    stamps: Vec<Option<SystemTime>>,
    resolved: BTreeMap<Vec<String>, Arc<Resolved>>,
}

impl Remembered {
    /// Empties this if any of `configs` has changed since it was filled.
    ///
    /// A file that is not there has no time, and a file appearing where there
    /// was none is as much of a change as an edit to one that was already there
    /// — somebody writing their first `~/.ssh/config` has changed what every
    /// declaration means.
    fn forget_if_changed(&mut self, configs: &[PathBuf]) {
        let stamps: Vec<Option<SystemTime>> = configs
            .iter()
            .map(|path| modified(path.as_path()))
            .collect();
        if stamps != self.stamps {
            self.resolved.clear();
            self.stamps = stamps;
        }
    }
}

/// When a file was last written, and nothing for one that is not there or
/// cannot be looked at.
fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|file| file.modified())
        .ok()
}

/// The configuration files ssh reads that this program can watch: the user's own
/// and the machine's.
///
/// Not every file that goes into a resolution — an `Include` may pull in
/// anything, and following those would mean parsing the configuration this
/// module exists to avoid parsing. These two are where a change is nearly always
/// made, and [`Resolver::invalidate_all`] covers the rest.
fn well_known(home: Option<OsString>) -> Vec<PathBuf> {
    let mut configs = Vec::new();
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        configs.push(PathBuf::from(home).join(USER_CONFIG));
    }
    configs.push(PathBuf::from(SYSTEM_CONFIG));
    configs
}

/// The one destination in `argv`, or why there is not exactly one.
///
/// This is the whole of the checking done before ssh is run, and it exists
/// because of what the extra word would be if there were one. `ssh host vim` is
/// a valid thing to type and means "run vim over there"; a declaration is a
/// machine to attach to, and this program appends the command it means to run
/// there itself. Silently gluing the two together would run something nobody
/// asked for.
fn one_destination(argv: &[String]) -> Result<&str, Error> {
    match positionals(argv).as_slice() {
        [destination] if !destination.is_empty() => Ok(destination),
        [] | [_] => Err(Error::DestinationMissing {
            argv: argv.to_vec(),
        }),
        _ => Err(Error::RemoteCommandNotAllowed {
            argv: argv.to_vec(),
        }),
    }
}

/// Everything in `argv` that is not an option or an option's value, in order.
///
/// Walked left to right the way ssh's own option parsing walks it: `--` ends the
/// options and leaves everything after it positional, a word beginning with `-`
/// is options, and an option that reads a separate argument takes the next word
/// with it unless its value is already stuck to it.
fn positionals(argv: &[String]) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = argv.iter();
    while let Some(token) = rest.next() {
        if token == END_OF_OPTIONS {
            found.extend(rest.map(String::as_str));
            break;
        }
        match reads_next_word(token) {
            Some(true) => {
                rest.next();
            }
            Some(false) => {}
            None => found.push(token),
        }
    }
    found
}

/// Whether `token` is options at all, and if it is, whether the word after it
/// belongs to it.
///
/// Options cluster — `-vp 2222` is `-v` and then `-p 2222` — so this reads the
/// letters in order and stops at the first one that wants a value. That letter
/// takes the following word only if it is the last letter in the token;
/// otherwise the rest of the token is the value, as in `-p2222` and `-oFoo=bar`.
/// A letter this does not know about is assumed to want nothing, which is what
/// makes an unfamiliar option's value look positional and the declaration
/// carrying it refusable rather than dangerous.
fn reads_next_word(token: &str) -> Option<bool> {
    let letters = token.strip_prefix(DASH)?;
    if letters.is_empty() {
        return None;
    }
    let mut rest = letters.chars();
    while let Some(letter) = rest.next() {
        if TAKES_ARGUMENT.contains(letter) {
            return Some(rest.next().is_none());
        }
    }
    Some(false)
}

/// The name this process runs under, for the one thing `-G` might not say.
///
/// Asked of the password database first, because that is where ssh asks and the
/// answer has to be the one ssh would have defaulted to. The environment is the
/// fallback for a machine that has no entry for this user at all, which happens
/// in containers built by hand.
fn login_name() -> String {
    // Safe by construction: `geteuid` reads one field of the calling process,
    // takes nothing and cannot fail.
    let uid = unsafe { libc::geteuid() };
    passwd_name(uid)
        .or_else(|| named_by("LOGNAME"))
        .or_else(|| named_by("USER"))
        .unwrap_or_default()
}

/// What a variable says this user is called, ignoring one that says nothing.
fn named_by(variable: &str) -> Option<String> {
    std::env::var(variable).ok().filter(|name| !name.is_empty())
}

/// What the password database calls `uid`.
///
/// The reentrant call rather than the plain one: this runs inside a daemon with
/// several threads, and `getpwuid` answers out of one buffer shared by all of
/// them.
fn passwd_name(uid: libc::uid_t) -> Option<String> {
    // Long enough for any name; the second size is for a database that answers
    // with a great deal else besides, since the buffer holds the whole record.
    for size in [1024, 16384] {
        let mut buffer = vec![0 as libc::c_char; size];
        let mut record: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // Safe by construction: the record and the buffer outlive the call, the
        // length passed is the buffer's own, and nothing is read back unless the
        // call reports success and points at the record it was given.
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                &raw mut record,
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut found,
            )
        };
        if code == libc::ERANGE {
            continue;
        }
        if code != 0 || found.is_null() {
            return None;
        }
        // Safe by construction: a record the call filled in has a name in it,
        // and it points into the buffer above, which is still alive here.
        let name = unsafe { CStr::from_ptr(record.pw_name) };
        return name
            .to_str()
            .ok()
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
    }
    None
}
