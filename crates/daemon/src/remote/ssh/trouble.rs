//! Telling a host that is not there yet from one that will never let this
//! daemon in.
//!
//! Almost every way of failing to reach a machine is temporary. It is being
//! rebooted, the wireless has dropped, the VPN has not come up yet — and the
//! answer to all of those is to wait and try again. A few are not: a key that is
//! refused stays refused, and a host key that does not match what is on record
//! is a question nobody but a person can answer. Retrying those forever is worse
//! than useless, because a connection attempt every minute is exactly how a
//! problem that needed somebody's attention gets none.
//!
//! # Why this reads sentences
//!
//! ssh has no machine-readable vocabulary for why it failed. Every one of these
//! ends in exit status 255, and the difference between them is the English
//! sentence it printed. So the sentence is what is read. That is a thing to do
//! carefully rather than a thing to avoid: the phrases matched here have been in
//! OpenSSH for decades, they are matched loosely rather than exactly, and
//! anything not recognized is treated as temporary — which is the safe way to be
//! wrong, since it means an unfamiliar failure keeps being retried instead of
//! being written off.

/// What kind of trouble a host is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// The host is not who it was last time, or is not known at all.
    HostKey,
    /// The credentials were refused.
    Credentials,
    /// ssh wanted to ask a person something and was not allowed to.
    ///
    /// Rarer than it sounds, and deliberately so. Refusing to ask turns most of
    /// these into one of the two above — a host it may not ask about is a host
    /// key failure, a key it may not unlock is a refused credential — which is
    /// why this catches only what says outright that somebody was going to be
    /// asked. All three are answered the same way in any case.
    Asking,
    /// Nothing answered.
    Unreachable,
    /// Something else went wrong.
    Unrecognized,
}

/// The phrases that name each kind, lowercased, in the order they are looked
/// for.
///
/// Only ssh's own words go in here, and none of them may be a word this program
/// puts on its own command line: what is being read is often a sentence with
/// that command line inside it, and a phrase common to both would classify every
/// failure as the same thing.
///
/// Order matters where a message could match two: a host key that has changed
/// says a great deal, some of which is about permission.
const PHRASES: [(Trouble, &[&str]); 4] = [
    (
        Trouble::HostKey,
        &[
            "host key verification failed",
            "remote host identification has changed",
            "no matching host key",
        ],
    ),
    (
        Trouble::Asking,
        &[
            "enter passphrase",
            "passphrase for key",
            "keyboard-interactive",
        ],
    ),
    (Trouble::Credentials, &["permission denied"]),
    (
        Trouble::Unreachable,
        &[
            "connection refused",
            "could not resolve",
            "connection timed out",
            "operation timed out",
            "no route to host",
            "network is unreachable",
            "name or service not known",
        ],
    ),
];

impl Trouble {
    /// What `said` says the trouble is.
    ///
    /// `said` is whatever ssh complained, which in practice arrives wrapped in
    /// a sentence of this program's own; looking for the phrase anywhere in it
    /// rather than anchoring is what lets the two be the same call.
    pub fn of(said: &str) -> Self {
        let said = said.to_ascii_lowercase();
        PHRASES
            .iter()
            .find(|(_, phrases)| phrases.iter().any(|phrase| said.contains(phrase)))
            .map_or(Self::Unrecognized, |(trouble, _)| *trouble)
    }

    /// Whether trying again could get a different answer.
    ///
    /// The three that cannot are the three a person has to act on: change a key,
    /// accept a host, unlock something. There is nothing in between, because
    /// `BatchMode` means ssh never sits waiting on an answer — it fails, and
    /// this is what is done with the failure.
    pub fn retries(self) -> bool {
        match self {
            Self::HostKey | Self::Credentials | Self::Asking => false,
            Self::Unreachable | Self::Unrecognized => true,
        }
    }
}
