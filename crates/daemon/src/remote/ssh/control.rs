//! The connections this daemon keeps open to the hosts it reaches, and the
//! configuration that makes ssh keep them.
//!
//! One ssh connection is a handshake, a key exchange and an authentication,
//! possibly across a link that is slow. Paying that for every command would
//! make a daemon that reaches three hosts unusable, so every command goes down
//! a connection that is already there: ssh's own multiplexing, driven from
//! here. The socket that connection lives on belongs to this daemon and to
//! nothing else — a socket somebody's terminal opened lasts as long as that
//! terminal, and an event stream that ends when a window is closed is not an
//! event stream.
//!
//! # Why a generated configuration and not a pile of `-o`
//!
//! Most of what has to be forced can be forced on the command line, and is.
//! Keepalives are the exception worth a file, and a file brings a trap with it:
//! `-F` *replaces* the user's configuration rather than layering on top of it,
//! so a generated one that said only what this program wanted would throw away
//! every `Host` block, `ProxyJump` and identity the user has written. It
//! therefore ends with an `Include` of theirs. And because ssh takes the *first*
//! value it is given for a setting rather than the last, everything this program
//! insists on has to come before that line. Reversed, the file would be a
//! no-op wherever the user had an opinion. What the machine's administrator set
//! in `/etc/ssh/ssh_config` is read afterwards as usual, which is the right way
//! round: it is the lowest priority either way.
//!
//! # Why the directory is chosen rather than fixed
//!
//! A unix socket's path lives in a fixed-size field, and ssh builds this one by
//! expanding a hash into a template of ours. A directory too deep to leave room
//! for the expansion does not produce an error anybody would understand — it
//! produces a daemon that quietly stops sharing connections. So the room is
//! measured first and a shorter directory is used when there is not enough.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::paths::{self, SocketPaths};

/// The directory these live in, inside whichever directory holds them.
pub const DIR_NAME: &str = "ssh";

/// The mode that directory is kept at: a socket in it is a live login to
/// somebody else's machine.
pub const DIR_MODE: u32 = 0o700;

/// The mode the generated configurations are kept at.
pub const FILE_MODE: u32 = 0o600;

/// What every one of this daemon's master sockets is called, before the part
/// ssh fills in.
const PREFIX: &str = "/cm-";

/// What ssh puts after that.
///
/// `%C` is ssh's own hash of the resolved user, host, port and local host, which
/// is what makes two aliases for one machine share a connection exactly when ssh
/// considers them one machine. Deferring to ssh's notion of sameness is cheaper
/// and more correct than inventing one here.
const EXPANDS: &str = "%C";

/// The name every generated configuration starts with.
const CONFIG: &str = "config-";

/// How much of the hash of a declaration goes into a configuration's name.
///
/// Enough that two declarations on one machine will not collide, and short
/// enough to leave the directory's own budget alone.
const NAMED_BY: usize = 16;

/// How many characters to allow for what ssh expands `%C` into.
///
/// It is forty today, being hexadecimal SHA-1. Reserving sixty-four costs a
/// couple of dozen bytes of a budget that is not tight and survives the day
/// somebody changes the hash, which is the cheaper way to be wrong.
const EXPANSION: usize = 64;

/// The longest path a socket may have, which is the size of the field the
/// kernel keeps it in.
const SUN_PATH: usize = 108;

/// The longest a directory may be and still leave room to put a socket in it.
///
/// Short of the field by a margin, because being a few bytes under the limit is
/// not a state worth being in: the whole path is built by somebody else's
/// expansion of a template, and a budget with nothing in hand would turn a
/// slightly longer hash into a silent loss of multiplexing.
const BUDGET: usize = SUN_PATH - 8;

/// The first line of every generated configuration, which says what it is to
/// whoever finds one.
const PREAMBLE: &str = "# generated; overrides come first because ssh takes the first match";

/// How long a connection is kept open after the last command using it has
/// finished, in seconds.
///
/// Long enough that the commands of one attachment attempt share a connection,
/// short enough that a host nobody is talking to does not hold a login open all
/// day.
pub const PERSIST: &str = "60";

/// How often ssh asks a silent connection whether it is still there, in
/// seconds, and how many unanswered asks it takes to give up on it.
///
/// Forced because the far end of an attachment is silent by design for long
/// stretches, and a connection that has died in a way TCP has not noticed looks
/// exactly like one where nothing is happening.
const ALIVE_INTERVAL: &str = "15";

/// How many keepalives may go unanswered before ssh gives up.
const ALIVE_COUNT: &str = "3";

/// The user's own configuration, pulled back in because `-F` replaced it.
const INCLUDE: &str = "~/.ssh/config";

/// Where this daemon keeps the connections it holds open, and the
/// configurations that make them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Masters {
    dir: PathBuf,
}

impl Masters {
    /// The directory this machine's environment implies, or a shorter one when
    /// that leaves no room for a socket.
    ///
    /// Said once, as this is resolved, so that a machine which fell back has
    /// said so somewhere before anybody wonders why.
    pub fn resolve() -> Self {
        Self::beside(SocketPaths::resolve().dir())
    }

    /// The directory to use for a daemon whose own files are in `dir`.
    ///
    /// Beside them where there is room, which keeps everything one daemon owns
    /// in one place; under `/tmp` where there is not, which is the shortest
    /// per-user path this program has anywhere.
    pub fn beside(dir: &Path) -> Self {
        let preferred = dir.join(DIR_NAME);
        if fits(&preferred) {
            info!(dir = %preferred.display(), "keeping ssh connections here");
            return Self::under(preferred);
        }
        let fallback = paths::per_user_dir().join(DIR_NAME);
        warn!(
            wanted = %preferred.display(),
            using = %fallback.display(),
            "that directory is too long to hold a multiplexed connection's socket"
        );
        Self::under(fallback)
    }

    /// The directory a caller chose, whatever its length.
    pub fn under(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Where it all is.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The template ssh is told to build every master socket's path from.
    pub fn socket(&self) -> String {
        format!("{}{PREFIX}{EXPANDS}", self.dir.display())
    }

    /// Creates the directory if it is absent, and puts it out of everyone
    /// else's reach either way.
    pub fn prepare(&self) -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(&self.dir)?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(DIR_MODE))
    }

    /// Writes the configuration `ssh -F` is pointed at for `argv`, and says
    /// where it went.
    ///
    /// One file per declaration, named after it, written afresh every time:
    /// what goes in it is a few hundred bytes derived entirely from this
    /// build, so there is nothing to gain by reading what is there and a stale
    /// file from an older build to lose by keeping it.
    pub fn config_for(&self, argv: &[String]) -> io::Result<PathBuf> {
        let path = self.dir.join(format!("{CONFIG}{}", named(argv)));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(FILE_MODE)
            .open(&path)?;
        file.write_all(self.config().as_bytes())?;
        // A file that existed already keeps the mode it was made with, so it is
        // said again rather than assumed.
        fs::set_permissions(&path, fs::Permissions::from_mode(FILE_MODE))?;
        Ok(path)
    }

    /// What goes in one.
    ///
    /// The order is the whole of the correctness here: every line this program
    /// insists on, and only then the user's own configuration.
    fn config(&self) -> String {
        let socket = self.socket();
        [
            PREAMBLE,
            &format!("ServerAliveInterval {ALIVE_INTERVAL}"),
            &format!("ServerAliveCountMax {ALIVE_COUNT}"),
            "BatchMode yes",
            "ControlMaster auto",
            &format!("ControlPath {socket}"),
            &format!("ControlPersist {PERSIST}"),
            &format!("Include {INCLUDE}"),
            "",
        ]
        .join("\n")
    }
}

/// Whether a socket built under `dir` would still fit in the field the kernel
/// keeps it in, once ssh has expanded the endpoint into the name.
fn fits(dir: &Path) -> bool {
    dir.as_os_str().len() + PREFIX.len() + EXPANSION < BUDGET
}

/// A short, stable name for a declaration.
///
/// The words are joined by a byte that cannot occur in one, so that two
/// declarations differing only in where a space fell are two names.
fn named(argv: &[String]) -> String {
    let mut hash = Sha256::new();
    for word in argv {
        hash.update(word.as_bytes());
        hash.update([0]);
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(NAMED_BY)
        .collect()
}
