//! Reaching a machine by running the `ssh` that is on this one.
//!
//! Nothing here speaks the ssh protocol. The daemon builds a command line and
//! runs it, exactly as a person at a shell would, which is the only way to get
//! `ProxyJump`, certificates, `IdentityAgent`, FIDO keys, GSSAPI, `Match exec`
//! and everything else somebody has configured for free. A library would mean
//! reimplementing a large and subtle program badly, and then disagreeing with
//! the `ssh` the same user runs by hand on the same machine.
//!
//! # What is added to what somebody typed
//!
//! Their words go through untouched, in the order they wrote them, and the
//! command to run over there comes after a `--` so that no word of theirs can be
//! read as it. What goes *before* their words is this daemon's connection
//! policy: a generated configuration, and the multiplexing that makes reaching
//! several hosts affordable. Before rather than after on purpose — ssh takes the
//! first value it is given, so anything they said on their own command line
//! still beats what is here, which is the right way round for a policy nobody
//! asked for.
//!
//! # What is never asked
//!
//! `BatchMode` is forced, so ssh fails rather than prompting. A daemon has no
//! terminal, cannot answer a passphrase or a push notification, and an ssh that
//! sat waiting for one would be an attachment that hung instead of an attachment
//! that said what was wrong. What it says instead is read ([`Trouble`]) and
//! decides whether trying again is worth anything.
//!
//! # What is not asked either
//!
//! Nothing here asks a host what kind of machine it is, because in the ordinary
//! course nothing has to: the script that looks for a copy of this program over
//! there names the machine itself on its way out when it finds none, which is
//! the same round trip. The plain question is still available for a caller that
//! has not run that script — it is the default every transport gets — and it
//! costs an extra command, which is why it is the fallback rather than the
//! first move.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use agentbus_protocol::OriginHop;
use tracing::{debug, warn};

use super::control::{Masters, PERSIST};
use super::resolve::{self, Resolved, Resolver, Ssh};
use super::trouble::Trouble;
use crate::remote::transport::{Backoff, Error, Made, Running, Transport};

/// The word a declaration uses for one of these endpoints.
pub const NAME: &str = crate::remote::targets::SSH;

/// The kind of boundary this transport crosses, as it is stamped on everything
/// relayed through it. The protocol names it, because it is a word subscribers
/// read rather than one this module is free to choose.
pub const KIND: &str = OriginHop::SSH;

/// Where a copy of this program is put on a host that has not got one.
///
/// A throwaway path, versioned, and not the place somebody's own installation
/// would live. Putting a binary somewhere permanent on a machine a person owns
/// is a thing to be asked for rather than done by a daemon that has just
/// connected, and the shared bootstrap looks here last, after every place an
/// installation of theirs plausibly is.
const INSTALL_DIR: &str = "/tmp";

/// The prefix every copy this program writes onto a host shares.
const INSTALL_PREFIX: &str = "agentbus-";

/// The substring every partial write carries, wherever the rest of its name
/// came from.
///
/// A sweep of [`INSTALL_DIR`] uses this to recognize an unfinished copy and
/// leave it alone without knowing which attempt is writing it or how far it
/// has got.
const PARTIAL_MARK: &str = ".tmp.";

/// A name for the file a copy is written under before it is moved into place,
/// unique to this attempt.
///
/// The move is a rename at the far end, which is atomic there, so nothing ever
/// executes a file that is half of one — but two hosts that turn out to be one
/// endpoint, or two daemons provisioning it at once, would otherwise pour into
/// the *same* half-written name and interleave. A process id and a counter
/// that only grows are enough to make that impossible without asking the far
/// end for anything or leaving a lock file behind: a lock is a name that
/// outlives the write it guards whenever the write does not finish, and a
/// later attempt that finds one has no way to tell "somebody is still writing"
/// from "somebody was, and stopped" — which is exactly the deadlock a stale
/// lock produces. A name nobody else will ever generate needs no such
/// judgement call: it is either finished and moved, or abandoned and inert.
fn partial(remote: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let attempt = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{remote}{PARTIAL_MARK}{}.{attempt}", std::process::id())
}

/// How long to wait before reaching a host again after the last attempt broke.
///
/// Slower than a container's, and for a reason that is not politeness: an
/// attempt here is a round trip across a network and an authentication, so
/// retrying hard costs the far end real work and this end nothing but delay. A
/// minute is also about how long the things that break — a reboot, a laptop lid,
/// a link that flapped — take to stop being broken.
const BACKOFF: Backoff = Backoff {
    initial: Duration::from_secs(5),
    max: Duration::from_secs(60),
    multiplier: 2.0,
    jitter: 0.2,
};

/// How much of what ssh complained is kept.
///
/// Enough to hold every message ssh has ever printed about a connection several
/// times over, and bounded because the thing on the other end of that pipe is a
/// program on somebody else's machine and what it prints is not this daemon's to
/// decide.
const SAID: usize = 4096;

/// The argument that ends ssh's own options and its destination.
const END_OF_OPTIONS: &str = "--";

/// The option ssh reads a whole configuration from, in place of the user's.
const CONFIG: &str = "-F";

/// The option ssh takes a single setting after.
const SETTING: &str = "-o";

/// The option that asks a running master to do something rather than starting a
/// command.
const CONTROL: &str = "-O";

/// Asks whether a master for this endpoint is alive.
const CHECK: &str = "check";

/// Asks a master for this endpoint to close.
const EXIT: &str = "exit";

/// The flag that makes ssh print the configuration it arrived at and connect to
/// nothing.
const DUMP: &str = resolve::DUMP;

/// A way of running `ssh`, so that what is under test is the command line rather
/// than somebody's network.
///
/// Two ways, because there are two kinds of command. Most of them answer and
/// finish — is the master alive, is the copy that was sent runnable — and the
/// whole of their output is wanted at once. One does not: the subscription at
/// the far end lasts as long as the attachment does, and is read a line at a
/// time for days.
pub trait Driver: fmt::Debug + Send + Sync {
    /// Starts ssh with exactly `argv`, with `stdin` written to it, and hands it
    /// back still running.
    fn start(&self, argv: &[String], stdin: Option<&str>) -> io::Result<Running>;

    /// Runs ssh with exactly `argv`, pouring `stdin` into it, and collects
    /// everything it said.
    fn collect(&self, argv: &[String], stdin: Option<&mut dyn Read>) -> io::Result<Output>;
}

impl Driver for Ssh {
    fn start(&self, argv: &[String], stdin: Option<&str>) -> io::Result<Running> {
        let mut command = Command::new(self.binary());
        command.args(argv);
        Running::spawn(&mut command, stdin)
    }

    /// Writes the input from this thread rather than from one of its own.
    ///
    /// What is poured in this way is a file being copied to the far end, which
    /// may be megabytes, so the pipe will fill and this will block until ssh has
    /// carried it. That is the point: nothing else here has anything to do
    /// meanwhile, and what ssh says while it works is a line or two, which its
    /// own pipe holds until it is read below.
    fn collect(&self, argv: &[String], stdin: Option<&mut dyn Read>) -> io::Result<Output> {
        let mut command = Command::new(self.binary());
        command
            .args(argv)
            .stdin(match stdin {
                Some(_) => Stdio::piped(),
                None => Stdio::null(),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        if let Some(bytes) = stdin {
            let mut pipe = child.stdin.take().expect("stdin was asked for");
            io::copy(bytes, &mut pipe)?;
        }
        child.wait_with_output()
    }
}

/// One host, and the way in to it.
#[derive(Debug)]
pub struct Host {
    driver: Arc<dyn Driver>,
    /// The words somebody declared, verbatim and in their order.
    argv: Vec<String>,
    /// This daemon's own options, ready to have the words after them.
    options: Vec<String>,
    /// What it is called when telling somebody about it: what they typed.
    label: String,
    /// Who, where and on what port, as far as the configuration goes.
    way_in: String,
    /// Whether the master has been asked after since this daemon started.
    checked: Mutex<bool>,
    /// Whether it has been told to close.
    closed: AtomicBool,
    /// Whether superseded copies at [`INSTALL_DIR`] have been cleared since
    /// this host was made.
    swept: AtomicBool,
}

impl Host {
    /// The host `argv` declares, or why those words do not declare one.
    ///
    /// Everything that can be settled without touching the network is settled
    /// here: ssh reads the words and says what they mean, and a declaration it
    /// will not have is refused now rather than by a connection attempt that
    /// could not have worked. So the words are validated by the same program
    /// that will later be given them, which is the only validator guaranteed to
    /// agree.
    pub fn declared(argv: &[String], resolver: &Resolver, masters: &Masters) -> Made {
        Self::built(argv, resolver, masters, Arc::new(Ssh::new()))
    }

    /// The same, running ssh some other way.
    pub fn built(
        argv: &[String],
        resolver: &Resolver,
        masters: &Masters,
        driver: Arc<dyn Driver>,
    ) -> Made {
        if argv.is_empty() {
            return Err("a host is reached by the words that would reach it with ssh".to_owned());
        }
        let resolved = resolver.resolve(argv).map_err(|error| error.to_string())?;
        masters
            .prepare()
            .map_err(|error| format!("cannot make {}: {error}", masters.dir().display()))?;
        let config = masters
            .config_for(argv)
            .map_err(|error| format!("cannot write a configuration for ssh: {error}"))?;
        Ok(Arc::new(Self::new(argv, &resolved, masters, &config, driver)) as Arc<dyn Transport>)
    }

    /// One, with everything it needs already worked out.
    fn new(
        argv: &[String],
        resolved: &Resolved,
        masters: &Masters,
        config: &Path,
        driver: Arc<dyn Driver>,
    ) -> Self {
        let (user, hostname, port) = resolved.provisional_identity();
        Self {
            driver,
            argv: argv.to_vec(),
            options: options(config, &masters.socket()),
            label: argv.join(" "),
            way_in: format!("{user}@{hostname}:{port}"),
            checked: Mutex::new(false),
            closed: AtomicBool::new(false),
            swept: AtomicBool::new(false),
        }
    }

    /// Closes the connection this daemon has been holding open, if it has not
    /// been closed already.
    ///
    /// Once and only once: a master left behind would keep a login to somebody's
    /// machine open for as long as ssh's own patience lasts, and asking a master
    /// that has already gone to go again is a command that fails for no reason
    /// anybody needs to see.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut argv = self.options.clone();
        argv.extend([CONTROL.to_owned(), EXIT.to_owned()]);
        argv.extend(self.argv.iter().cloned());
        // Nothing is done about a failure, because there is nothing that could
        // be: a master that will not close on being asked has either gone
        // already or belongs to a connection this daemon no longer has.
        match self.driver.collect(&argv, None) {
            Ok(output) => {
                debug!(host = self.label, status = %output.status, "closed the connection")
            }
            Err(error) => debug!(host = self.label, %error, "cannot close the connection"),
        }
    }

    /// Makes sure the connection this daemon is about to use is one ssh can
    /// still use, the first time anything wants it.
    ///
    /// A socket left by a daemon that was killed outlives the process that was
    /// listening on it, and ssh finding one is ssh trying to talk to nobody.
    /// Asking first costs one local command; not asking costs a connection
    /// attempt that fails in a way that looks like the host being down.
    fn check(&self) {
        let mut checked = self.checked.lock().unwrap_or_else(PoisonError::into_inner);
        if *checked {
            return;
        }
        *checked = true;
        let mut argv = self.options.clone();
        argv.extend([CONTROL.to_owned(), CHECK.to_owned()]);
        argv.extend(self.argv.iter().cloned());
        match self.driver.collect(&argv, None) {
            Ok(output) if output.status.success() => {
                debug!(host = self.label, "there is a connection to reuse")
            }
            // Either there was never one or what is there is dead. Only the
            // second leaves anything behind, and clearing it is what lets the
            // next command make a fresh one instead of finding this one.
            Ok(_) => self.clear(),
            Err(error) => warn!(
                host = self.label,
                %error, "cannot ask whether there is a connection to reuse"
            ),
        }
    }

    /// Removes the socket of a master that is not answering.
    ///
    /// Where it is has to be asked rather than worked out: the name is ssh's own
    /// hash of the endpoint it resolved, and computing it here would mean
    /// knowing ssh's hash and ssh's resolution, which is the thing this whole
    /// module exists not to do. So ssh is asked to print the path it would use,
    /// and only a path it prints in full is acted on — one still carrying the
    /// template is a version that does not expand it, and names no file.
    fn clear(&self) {
        let Some(path) = self.socket() else {
            return;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => debug!(
                host = self.label,
                path, "cleared a connection that had died"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => warn!(host = self.label, path, %error, "cannot clear a dead connection"),
        }
    }

    /// Where ssh says the master socket for this host would be.
    fn socket(&self) -> Option<String> {
        let mut argv = self.options.clone();
        argv.push(DUMP.to_owned());
        argv.extend(self.argv.iter().cloned());
        let output = self.driver.collect(&argv, None).ok()?;
        if !output.status.success() {
            return None;
        }
        let printed = String::from_utf8_lossy(&output.stdout);
        Resolved::read(&printed)
            .controlpath()
            .filter(|path| !path.contains('%'))
            .map(str::to_owned)
    }

    /// The whole command line for running `words` at the far end.
    fn asking(&self, words: &[&str]) -> Vec<String> {
        let mut argv = self.options.clone();
        argv.extend(self.argv.iter().cloned());
        argv.push(END_OF_OPTIONS.to_owned());
        argv.extend(words.iter().map(|word| (*word).to_owned()));
        argv
    }

    /// Runs one command that answers and finishes, and complains in this
    /// transport's terms if it did not.
    fn ran(&self, words: &[&str], stdin: Option<&mut dyn Read>) -> Result<(), Error> {
        let argv = self.asking(words);
        let said = words.join(" ");
        let output = self
            .driver
            .collect(&argv, stdin)
            .map_err(|source| Error::Run {
                label: self.label.clone(),
                command: said.clone(),
                source,
            })?;
        match output.status.success() {
            true => Ok(()),
            false => Err(Error::Failed {
                label: self.label.clone(),
                command: said,
                status: output.status,
                said: tail(&output.stderr),
            }),
        }
    }
}

impl Transport for Host {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    /// Nothing: an address is not an identity, and this end has not been told
    /// one.
    ///
    /// What ssh was asked to reach says where to go and not what is there. Two
    /// names may be one machine, one name may be two machines on two days, and
    /// the same machine reached as two users is two of the daemons this is
    /// looking for, because their sockets are per-user. The party that knows is
    /// the daemon at the far end, which says so on every snapshot, so this end
    /// waits to be told rather than concluding anything from what somebody
    /// typed.
    fn identity(&self) -> Option<String> {
        None
    }

    /// Who ssh would log in as, where, and on what port — which is also what it
    /// builds the name of a multiplexed connection's socket out of.
    ///
    /// That coincidence is the useful part, and it is not a coincidence: ssh
    /// derives `%C` from the local host and the resolved user, host and port, so
    /// two declarations that answer alike here are two declarations ssh itself
    /// considers one endpoint, and they are already sharing one connection
    /// rather than opening two. Nothing tries to reproduce that hash; what is
    /// compared is the same resolution ssh made it from.
    ///
    /// Worth what it costs and no more. Two declarations that a `Host` block
    /// points at one machine come out identical here, which is enough to notice
    /// they are one endpoint; two names for one machine that only DNS could
    /// reconcile do not, because nothing has asked DNS anything. So this is good
    /// for spotting sameness and never for concluding difference.
    fn way_in(&self) -> Option<String> {
        Some(self.way_in.clone())
    }

    /// Leaves the connection alone when this host is let go, because something
    /// else is still using it.
    ///
    /// Two declarations that ssh considers one endpoint share one master, and
    /// the ordinary way of letting go — asking that master to close — would take
    /// the other one's stream down with it. Saying it is already closed is what
    /// makes dropping this a local matter.
    fn keep_open(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            debug!(
                host = self.label,
                "letting go of this host without closing the connection something else is using"
            );
        }
    }

    fn install_path(&self, version: &str) -> String {
        format!("{INSTALL_DIR}/{INSTALL_PREFIX}{version}")
    }

    fn run(&self, command: &str, args: &[&str], stdin: Option<&str>) -> Result<Running, Error> {
        self.check();
        let mut words = vec![command];
        words.extend_from_slice(args);
        let argv = self.asking(&words);
        self.driver
            .start(&argv, stdin)
            .map_err(|source| Error::Run {
                label: self.label.clone(),
                command: command.to_owned(),
                source,
            })
    }

    /// Pours the file down the connection that is already open.
    ///
    /// `cat` rather than `scp` or `sftp` because both of those are a subsystem
    /// the far end may not have enabled, and a shell that can run `cat` is the
    /// least this transport can already assume — it is about to run a daemon
    /// over there. It is also one thing that can time out instead of two: a
    /// transfer riding the connection this daemon is already holding open has
    /// nothing separate left to hang, whereas a second subsystem is a second
    /// place for a slow or half-open link to leave a copy stuck partway with
    /// no sign of whether it is still coming. The write lands beside the final
    /// name and is moved onto it, so the path a hook may execute at any moment
    /// is either the previous copy or the whole new one and never part of one.
    ///
    /// The far end's shell parses this, so the paths are quoted and reach it
    /// exactly as they are written: nothing in them is expanded, and a `~` in
    /// one would stay a `~`.
    fn copy_in(&self, local: &Path, remote: &str) -> Result<(), Error> {
        self.check();
        let mut file = File::open(local).map_err(|source| Error::Copy {
            label: self.label.clone(),
            local: local.to_owned(),
            remote: remote.to_owned(),
            source,
        })?;
        let partial = partial(remote);
        let script = format!(
            "mkdir -p {} && cat > {} && chmod +x {} && mv -f {} {}",
            quoted(holding(remote)),
            quoted(&partial),
            quoted(&partial),
            quoted(&partial),
            quoted(remote)
        );
        self.ran(&[&script], Some(&mut file))?;
        // Asked of the far end rather than assumed from the commands above
        // having succeeded, because what matters is that the path is runnable
        // now, and only the machine it is on can say so.
        self.ran(&[&format!("test -x {}", quoted(remote))], None)
    }

    /// Clears every copy at `/tmp` this program put there that is not
    /// `version`.
    ///
    /// A container is thrown away and rebuilt, so nothing left in it ever
    /// matters again; a host reached over ssh is not, and is often one a
    /// person merely has an account on rather than owns outright, so a copy
    /// from every upgrade this daemon has ever made left sitting in `/tmp`
    /// is exactly the kind of thing somebody notices against a disk quota
    /// and cannot explain. Once per host is enough — nothing that happens
    /// between one attach and the next changes what counts as superseded —
    /// so a sweep that has already run is not asked to run again.
    fn established(&self, version: &str) {
        if self.swept.swap(true, Ordering::SeqCst) {
            return;
        }
        let keep = self.install_path(version);
        let script = format!(
            "for f in {}/{}*; do case \"$f\" in \
             {}) continue ;; \
             *{}*) continue ;; \
             esac; [ -e \"$f\" ] || continue; rm -f -- \"$f\"; done",
            INSTALL_DIR,
            INSTALL_PREFIX,
            quoted(&keep),
            PARTIAL_MARK,
        );
        // Housekeeping, not the thing that was asked for: an attachment that
        // came up fine has nothing to gain from failing over a directory it
        // could not tidy, so trouble here is logged and left at that.
        if let Err(error) = self.ran(&[&script], None) {
            debug!(host = self.label, %error, "cannot sweep superseded copies");
        }
    }

    fn backoff(&self) -> Backoff {
        BACKOFF
    }

    /// Reads what ssh complained about and says whether another attempt could
    /// possibly go differently.
    fn recoverable(&self, failure: &dyn std::error::Error) -> bool {
        let said = failure.to_string();
        let trouble = Trouble::of(&said);
        debug!(host = self.label, ?trouble, said, "why it did not connect");
        trouble.retries()
    }
}

impl Drop for Host {
    /// Lets go of the connection as well as of the handle.
    ///
    /// Nothing else would: `ControlPersist` keeps a master alive after the last
    /// command using it has finished, which is exactly what makes it worth
    /// having and exactly why detaching has to say so.
    fn drop(&mut self) {
        self.close();
    }
}

/// This daemon's own options, in the order ssh is given them.
///
/// The configuration file carries the same multiplexing settings as the `-o`
/// arguments beside it. That is not redundancy for its own sake: the file is
/// what carries the keepalives, and repeating the rest on the command line means
/// a reader of a process listing can see the whole policy without going to look
/// for a file.
fn options(config: &Path, socket: &str) -> Vec<String> {
    vec![
        CONFIG.to_owned(),
        config.display().to_string(),
        SETTING.to_owned(),
        "ControlMaster=auto".to_owned(),
        SETTING.to_owned(),
        format!("ControlPath={socket}"),
        SETTING.to_owned(),
        format!("ControlPersist={PERSIST}"),
        SETTING.to_owned(),
        "BatchMode=yes".to_owned(),
    ]
}

/// The directory a path is in, as the far end's shell will read it.
///
/// String work rather than [`Path`] work, and deliberately: this is a path on a
/// machine whose filesystem this process cannot see, and treating it as a local
/// one is how code that tries to look at it gets written.
fn holding(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some(("", _)) => "/",
        Some((dir, _)) => dir,
        None => ".",
    }
}

/// One word for a shell, whatever is in it.
///
/// Single quotes take everything literally, so the only thing that needs saying
/// is what to do about a single quote: leave the quoting, say one, and start
/// again.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The end of what a command complained, which is the part that says what went
/// wrong.
fn tail(said: &[u8]) -> String {
    let from = said.len().saturating_sub(SAID);
    String::from_utf8_lossy(&said[from..]).into_owned()
}
