//! Who this daemon is, as everything reading its stream is told.
//!
//! A stream says what is happening; it does not, by itself, say whose account it
//! is. That matters as soon as one daemon reads another's, because two ways of
//! reaching a machine are not two machines and nothing about an address says so:
//! `fileserver`, `192.168.0.42` and `fs.example.net` may be one host, and the
//! only party in a position to settle it is the daemon at the other end. So
//! every daemon names itself on every snapshot, and whoever is aggregating
//! compares the names.
//!
//! # What the name is made of
//!
//! The machine, and the user the daemon runs as. Both halves are load-bearing.
//! The machine alone would merge two daemons that share a host and nothing else:
//! this program's sockets are per-user ([`crate::paths`]), so logging in as
//! somebody else reaches a different daemon holding a different set of sessions.
//! The user alone would merge every machine somebody has an account on.
//!
//! The machine half is whatever this host already calls itself — `/etc/machine-id`
//! where systemd or D-Bus put one — because an id somebody else maintains is
//! stable across reboots, reinstalls of this program and changes of address. A
//! host with neither file gets one made up and kept, which is weaker in exactly
//! one way: it lasts as long as the directory it is written in, so on a machine
//! whose runtime directory is cleared at boot it is stable for the life of that
//! boot. That is enough for what it is for — an attachment does not outlive the
//! machine at the far end.
//!
//! # It is a string
//!
//! Everything above is how the string is arrived at, and nothing that reads it
//! is entitled to any of it. An identity is compared with another identity for
//! equality and used for nothing else, here or at any other end of a connection:
//! the format is this module's business, and a reader that took it apart would
//! be relying on something no daemon promises.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use agentbus_protocol::DaemonIdentity;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

/// Where a host says what machine it is, in the order the files are tried.
///
/// The first is systemd's and the second is D-Bus's, which is what a host
/// without systemd is likely to have; several distributions have one as a link
/// to the other. Neither is written by this program.
pub const MACHINE_ID_FILES: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];

/// What the id this program makes up for itself is kept in, when the host names
/// no machine of its own.
pub const FILE_NAME: &str = "daemon-id";

/// How many bytes of randomness go into one, which is what a machine id is
/// conventionally made of: sixteen bytes written as thirty-two hexadecimal
/// characters.
const RANDOM_BYTES: usize = 16;

/// Where randomness comes from on the machines this runs on.
const RANDOM_SOURCE: &str = "/dev/urandom";

/// What this daemon calls itself, from the machine it is on and the user it runs
/// as.
///
/// The directory is where an id this program had to make up is kept. It is
/// deliberately not the directory the sockets are in: which directory those go
/// in is something a caller may choose per run — a test, a second bus for one
/// user — and an identity that moved with it would make one machine look like
/// several.
pub fn resolve() -> DaemonIdentity {
    of(
        &MACHINE_ID_FILES.map(PathBuf::from),
        &crate::paths::runtime_dir(),
        current_uid(),
    )
}

/// The same, from files and a user a caller chose.
pub fn of(files: &[PathBuf], dir: &Path, uid: u32) -> DaemonIdentity {
    let machine = machine_id(files).unwrap_or_else(|| remembered(dir));
    DaemonIdentity::new(format!("{machine}:{uid}"))
}

/// What the host says it is, or nothing where it says nothing.
///
/// Emptiness counts as absence. A file that is there and blank is a machine that
/// has not been given an id — a container image built with one truncated, most
/// often — and taking the empty string for an answer would make every such
/// machine identical to every other.
fn machine_id(files: &[PathBuf]) -> Option<String> {
    for file in files {
        match fs::read_to_string(file) {
            Ok(read) => match read.trim() {
                "" => debug!(path = %file.display(), "this machine's id is empty"),
                id => return Some(id.to_owned()),
            },
            Err(error) => debug!(path = %file.display(), %error, "this machine has no id here"),
        }
    }
    None
}

/// The id this program made up for itself, made now if this is the first time
/// and read back on every run after that.
///
/// Said out loud, once, because it is a weaker identity than the machine's own
/// and somebody looking at why two hosts will not tell themselves apart should
/// find out here rather than by reading this source.
fn remembered(dir: &Path) -> String {
    let path = dir.join(FILE_NAME);
    if let Some(kept) = machine_id(std::slice::from_ref(&path)) {
        warn!(path = %path.display(), "this machine names no id of its own; using the one kept here");
        return kept;
    }
    let made = random();
    match fs::create_dir_all(dir).and_then(|()| fs::write(&path, format!("{made}\n"))) {
        Ok(()) => {
            warn!(path = %path.display(), "this machine names no id of its own; making one up and keeping it here")
        }
        // Not fatal, and not worth failing to start over: an id that is not kept
        // is still an identity for as long as this daemon runs, which is as long
        // as anything is attached to it. What is lost is only that it will be a
        // different one after a restart.
        Err(error) => {
            warn!(path = %path.display(), %error, "this machine names no id of its own, and one made up for it cannot be kept")
        }
    }
    made
}

/// Thirty-two hexadecimal characters that are not any other machine's.
///
/// From the kernel where it can be had, because that is the only source on these
/// machines that is random rather than merely varied. Where it cannot — a
/// process with no `/dev`, most plausibly — what is to hand is hashed instead:
/// the identity is worth less then, and still worth more than every such daemon
/// agreeing on one value.
fn random() -> String {
    if let Some(bytes) = urandom() {
        return hex(&bytes);
    }
    let mut hash = Sha256::new();
    hash.update(std::process::id().to_le_bytes());
    hash.update(current_uid().to_le_bytes());
    if let Ok(since) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        hash.update(since.as_nanos().to_le_bytes());
    }
    hex(&hash.finalize()[..RANDOM_BYTES])
}

/// Bytes from the kernel, or nothing where they cannot be read.
fn urandom() -> Option<[u8; RANDOM_BYTES]> {
    let mut bytes = [0; RANDOM_BYTES];
    let mut file = fs::File::open(RANDOM_SOURCE).ok()?;
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

/// Bytes as the characters a machine id is written in.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The effective user id of this process.
fn current_uid() -> u32 {
    // Safe by construction: `geteuid` reads one field of the calling process and
    // cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file with `contents` in it, under `dir`.
    fn written(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).expect("cannot write");
        path
    }

    #[test]
    fn a_daemon_is_the_machine_it_is_on_and_the_user_it_runs_as() {
        let dir = tempfile::tempdir().unwrap();
        // As the file is actually written: an id and a newline.
        let file = written(dir.path(), "machine-id", "9f3c1000deadbeef\n");

        let identity = of(&[file], dir.path(), 1000);

        assert_eq!(identity.id, "9f3c1000deadbeef:1000");
    }

    #[test]
    fn one_machine_and_two_users_are_two_daemons() {
        let dir = tempfile::tempdir().unwrap();
        let file = written(dir.path(), "machine-id", "9f3c1000deadbeef");

        let root = of(std::slice::from_ref(&file), dir.path(), 0);
        let theirs = of(&[file], dir.path(), 1000);

        assert_ne!(root.id, theirs.id);
    }

    #[test]
    fn the_first_file_that_names_a_machine_is_the_one_that_answers() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-there");
        let blank = written(dir.path(), "blank", "  \n");
        let named = written(dir.path(), "machine-id", "9f3c1000deadbeef");
        let other = written(dir.path(), "other", "somethingelse");

        let identity = of(&[missing, blank, named, other], dir.path(), 1000);

        assert_eq!(identity.id, "9f3c1000deadbeef:1000");
    }

    #[test]
    fn a_machine_that_names_none_gets_one_that_is_kept_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let nowhere = dir.path().join("not-there");

        let first = of(std::slice::from_ref(&nowhere), dir.path(), 1000);
        let again = of(&[nowhere], dir.path(), 1000);

        assert_eq!(first, again);
        let (machine, uid) = first.id.split_once(':').expect("no user in the identity");
        assert_eq!(uid, "1000");
        assert_eq!(machine.len(), RANDOM_BYTES * 2, "{machine}");
        assert!(machine.chars().all(|char| char.is_ascii_hexdigit()));
        // And it is kept where somebody could find it.
        let kept = fs::read_to_string(dir.path().join(FILE_NAME)).expect("nothing was kept");
        assert_eq!(kept.trim(), machine);
    }

    #[test]
    fn where_a_daemon_keeps_its_sockets_is_no_part_of_what_it_is() {
        let one = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let file = written(one.path(), "machine-id", "9f3c1000deadbeef");

        // Two daemons for one user on one machine, told to put their files in
        // different places, are the same machine and the same user.
        assert_eq!(
            of(std::slice::from_ref(&file), one.path(), 1000),
            of(&[file], other.path(), 1000)
        );
    }

    #[test]
    fn two_machines_that_both_had_to_make_one_up_do_not_agree() {
        let mine = tempfile::tempdir().unwrap();
        let theirs = tempfile::tempdir().unwrap();
        let nowhere = PathBuf::from("/nonexistent/machine-id");

        let one = of(std::slice::from_ref(&nowhere), mine.path(), 1000);
        let other = of(&[nowhere], theirs.path(), 1000);

        assert_ne!(one.id, other.id);
    }

    #[test]
    fn a_directory_that_cannot_be_written_still_yields_an_identity() {
        let nowhere = PathBuf::from("/nonexistent/machine-id");

        let identity = of(&[nowhere], Path::new("/proc/nowhere/at/all"), 1000);

        assert!(identity.id.ends_with(":1000"), "{}", identity.id);
    }

    #[test]
    fn what_this_machine_says_it_is_is_read_the_same_way() {
        let identity = resolve();

        let (machine, uid) = identity
            .id
            .split_once(':')
            .expect("no user in the identity");
        assert!(!machine.is_empty());
        assert_eq!(uid, current_uid().to_string());
    }
}
