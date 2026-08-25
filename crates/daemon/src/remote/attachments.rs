//! What the daemon is actually doing about the endpoints it has been told
//! about.
//!
//! A declaration says what somebody wants; this says what came of it, and the
//! two are different often enough to be worth writing down separately. A host
//! that is switched off is declared and not attached. A container found by
//! looking rather than by being told about is attached and not declared. An
//! endpoint whose credentials are refused is both, and is going nowhere until a
//! person does something about it.
//!
//! It is written where the sockets are rather than where the declarations are,
//! because it belongs to one running daemon: it is made as that daemon starts,
//! rewritten whenever anything about an attachment changes, and taken away when
//! it stops. So a file here means a daemon here, and no file means nobody is
//! attached to anything — which is what lets something ask what is going on
//! without a daemon having to be running to answer.

use std::path::{Path, PathBuf};

use agentbus_protocol::Timestamp;
use serde::{Deserialize, Serialize};

use super::store::{self, Error};

/// The file the daemon writes its attachments to.
///
/// Named beside the sockets it sits with, because everything in that directory
/// has to be known to whoever clears it.
pub const FILE_NAME: &str = agentbus_paths::ATTACHMENTS_FILE;

/// The shape this build writes and is willing to read.
pub const SCHEMA: u32 = 1;

/// What is happening to one endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Reaching it, and getting a daemon running there.
    Connecting,
    /// Reading its stream.
    Attached,
    /// The stream broke and is being got back.
    Reconnecting,
    /// Something is wrong that trying again will not fix.
    NeedsAttention,
    /// On its way out: nobody wants it attached any more.
    Detaching,
}

impl State {
    /// The word this state is written as.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Attached => "attached",
            Self::Reconnecting => "reconnecting",
            Self::NeedsAttention => "needs attention",
            Self::Detaching => "detaching",
        }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether every set of words reaching one endpoint reaches it the same way.
///
/// Two things a person reads off this. Where they are shared, the endpoint is
/// costing one connection however many names it was declared under. Where they
/// are separate, the names were only found to be one endpoint by asking the
/// daemon over there, which is the answer that cost something and the one worth
/// knowing about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sharing {
    /// One connection, reached by every name.
    Shared,
    /// A connection per name.
    Separate,
}

impl Sharing {
    /// The word this is written as.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Separate => "separate",
        }
    }
}

impl std::fmt::Display for Sharing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One endpoint, as the daemon currently sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Which transport reaches it.
    pub transport: String,
    /// What that transport was given: the declared words, or whatever the
    /// transport's own discovery used to name it.
    pub args: Vec<String>,
    /// What it turned out to be, once anything has reached it: the far end's own
    /// account of itself, or whatever the transport knew it had reached.
    pub identity: Option<String>,
    /// The way in to it, as much as could be told before anything reached it.
    /// What stands in for an identity until there is one, and never as good as
    /// one: it says two endpoints are obviously the same and never that they are
    /// different.
    #[serde(default)]
    pub way_in: Option<String>,
    /// Whether every set of words below reaches it the same way, where there is
    /// more than one of them and the transport has a way in to compare.
    #[serde(default)]
    pub sharing: Option<Sharing>,
    /// Every set of words that turned out to reach this same endpoint, in the
    /// order they were declared in.
    pub aliases: Vec<Vec<String>>,
    /// What to call it when telling somebody about it.
    pub label: String,
    /// What is happening to it.
    pub state: State,
    /// How many attempts to reach it have failed since the last one worked.
    pub attempt: u32,
    /// What went wrong, where something did.
    pub last_error: Option<String>,
    /// When it entered this state.
    pub since: Timestamp,
    /// Whether it was found by looking rather than by being declared.
    pub auto: bool,
}

/// The file, and everything that can be done to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachments {
    path: PathBuf,
}

/// The document as it sits on disk.
#[derive(Debug, Serialize, Deserialize)]
struct Document {
    v: u32,
    attachments: Vec<Entry>,
}

impl Attachments {
    /// The file inside the directory a daemon serves.
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(FILE_NAME),
        }
    }

    /// Where it is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What the daemon serving this directory is doing, or nothing at all when
    /// there is no daemon serving it.
    ///
    /// The two are worth telling apart: an empty list means a daemon that is
    /// attached to nothing, and nothing means nobody to be attached to anything.
    pub fn read(&self) -> Result<Option<Vec<Entry>>, Error> {
        Ok(store::read::<Document>(&self.path, SCHEMA)?.map(|document| document.attachments))
    }

    /// Replaces the file with exactly `entries`.
    pub fn write(&self, entries: &[Entry]) -> Result<(), Error> {
        store::write(
            &self.path,
            &Document {
                v: SCHEMA,
                attachments: entries.to_vec(),
            },
        )
    }

    /// Takes the file away, which is what a daemon that is no longer running
    /// leaves behind.
    pub fn remove(&self) -> Result<(), Error> {
        store::remove(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn at(text: &str) -> Timestamp {
        Timestamp::parse(text).expect("not a timestamp")
    }

    fn entry(state: State) -> Entry {
        Entry {
            transport: "ssh".to_owned(),
            args: vec!["fileserver".to_owned()],
            identity: Some("9f3c:1000".to_owned()),
            way_in: Some("bob@fileserver:22".to_owned()),
            sharing: Some(Sharing::Shared),
            aliases: vec![vec!["fileserver".to_owned()]],
            label: "fileserver".to_owned(),
            state,
            attempt: 0,
            last_error: None,
            since: at("2026-08-17T10:00:00.000Z"),
            auto: false,
        }
    }

    #[test]
    fn no_file_means_no_daemon_and_an_empty_file_means_one_attached_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let attachments = Attachments::in_dir(dir.path());

        assert_eq!(attachments.read().unwrap(), None);

        attachments.write(&[]).unwrap();

        assert_eq!(attachments.read().unwrap(), Some(Vec::new()));
    }

    #[test]
    fn an_attachment_survives_being_written_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let attachments = Attachments::in_dir(dir.path());
        let written = entry(State::Reconnecting);

        attachments.write(std::slice::from_ref(&written)).unwrap();

        assert_eq!(attachments.read().unwrap(), Some(vec![written]));
        let mode = fs::metadata(attachments.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, store::FILE_MODE);
    }

    #[test]
    fn a_state_is_written_as_the_word_it_is_read_back_from() {
        let written = serde_json::to_string(&entry(State::NeedsAttention)).unwrap();

        assert!(
            written.contains(r#""state":"needs_attention""#),
            "{written}"
        );
    }

    #[test]
    fn removing_it_is_what_a_daemon_that_has_stopped_leaves_behind() {
        let dir = tempfile::tempdir().unwrap();
        let attachments = Attachments::in_dir(dir.path());
        attachments.write(&[entry(State::Attached)]).unwrap();

        attachments.remove().unwrap();
        // And doing it again is not a failure: a daemon that never wrote one
        // takes the same path out.
        attachments.remove().unwrap();

        assert_eq!(attachments.read().unwrap(), None);
    }
}
