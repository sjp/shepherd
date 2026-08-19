//! The endpoints somebody has said they want attached.
//!
//! A declaration is a fact about what a person wants, not about what is
//! happening, so it lives in a configuration file and outlives every daemon that
//! reads it: a machine that is turned off is still one whose events are wanted
//! back when it returns. The daemon watches the file and keeps its attachments
//! in step with it, which is the whole of the channel between the two — there is
//! no request protocol on the bus's sockets and no need of one, because a
//! declaration is a thing to remember rather than a thing to do.
//!
//! Nothing here interprets what it stores. A target is a transport's name and
//! the words that transport was given, kept exactly as they were typed, and two
//! declarations are the same one when both are equal element for element.
//! Whether those words name a host that exists, whether two of them reach the
//! same machine, and what to do about it are all questions for the transport
//! that will be handed them; asking any of them here would mean this file's
//! meaning changed with the version of `ssh` on the machine.
//!
//! It also does not matter who wrote the file. A person running `agentbus
//! attach`, a configuration management system laying one down, and a program
//! that noticed one of its terminals go somewhere else all leave the same three
//! fields behind, and the daemon treats them identically.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agentbus_protocol::Timestamp;
use serde::{Deserialize, Serialize};

use super::store::{self, Error};

/// The environment variable that names the configuration directory outright.
pub const CONFIG_DIR_VAR: &str = "AGENTBUS_CONFIG_DIR";

/// The environment variable holding the base directory for configuration.
pub const CONFIG_HOME_VAR: &str = "XDG_CONFIG_HOME";

/// The environment variable naming the user's home directory.
pub const HOME_VAR: &str = "HOME";

/// The name of this program's own directory inside a base directory.
const DIR_NAME: &str = "agentbus";

/// Where the configuration directory sits when nothing else says.
const DEFAULT_CONFIG_HOME: &str = ".config";

/// The file declared targets are kept in.
pub const FILE_NAME: &str = "targets.json";

/// The shape this build writes and is willing to read.
pub const SCHEMA: u32 = 1;

/// The name a declaration gives the transport that reaches a host over ssh.
pub const SSH: &str = "ssh";

/// The name a declaration gives the transport that reaches a container.
pub const DOCKER: &str = "docker";

/// One endpoint somebody wants attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// Which transport reaches it.
    pub transport: String,
    /// What that transport was given, verbatim.
    pub args: Vec<String>,
    /// When it was declared.
    pub added: Timestamp,
}

impl Target {
    /// A target declared now.
    pub fn new(transport: impl Into<String>, args: &[String], added: Timestamp) -> Self {
        Self {
            transport: transport.into(),
            args: args.to_vec(),
            added,
        }
    }

    /// Whether this is the same target as one reached by `transport` with
    /// `args`.
    pub fn is(&self, transport: &str, args: &[String]) -> bool {
        self.transport == transport && self.args == args
    }
}

/// The file, and everything that can be done to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Targets {
    path: PathBuf,
}

/// The document as it sits on disk.
#[derive(Debug, Serialize, Deserialize)]
struct Document {
    v: u32,
    targets: Vec<Target>,
}

impl Targets {
    /// The file this machine's configuration says to use.
    pub fn resolve() -> Self {
        Self::in_dir(config_dir(
            std::env::var_os(CONFIG_DIR_VAR),
            std::env::var_os(CONFIG_HOME_VAR),
            std::env::var_os(HOME_VAR),
        ))
    }

    /// The file inside a directory chosen by the caller.
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(FILE_NAME),
        }
    }

    /// Where the declarations are kept.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Everything that has been declared, oldest declaration first.
    ///
    /// A file that is not there yet is an empty list: nothing has been declared,
    /// which is what every machine starts out as.
    pub fn list(&self) -> Result<Vec<Target>, Error> {
        Ok(store::read::<Document>(&self.path, SCHEMA)?
            .map(|document| document.targets)
            .unwrap_or_default())
    }

    /// Declares an endpoint, and says whether that changed anything.
    ///
    /// Declaring one that is already declared is not an error and does not
    /// touch the file: what somebody asked for is already what the file says,
    /// and a second entry would only mean a second attachment to one endpoint.
    pub fn declare(
        &self,
        transport: &str,
        args: &[String],
        added: &Timestamp,
    ) -> Result<bool, Error> {
        let mut targets = self.list()?;
        if targets.iter().any(|target| target.is(transport, args)) {
            return Ok(false);
        }
        targets.push(Target::new(transport, args, added.clone()));
        self.put(targets)?;
        Ok(true)
    }

    /// Takes a declaration back, and says whether there was one to take.
    pub fn undeclare(&self, transport: &str, args: &[String]) -> Result<bool, Error> {
        let targets = self.list()?;
        let left: Vec<Target> = targets
            .iter()
            .filter(|target| !target.is(transport, args))
            .cloned()
            .collect();
        if left.len() == targets.len() {
            return Ok(false);
        }
        self.put(left)?;
        Ok(true)
    }

    /// When the declarations last changed, or nothing when there are none.
    ///
    /// This is what makes watching the file cost nothing in the ordinary case,
    /// where it has not been touched since the last look.
    pub fn changed_at(&self) -> Option<SystemTime> {
        store::changed_at(&self.path)
    }

    /// Replaces the file with exactly `targets`.
    fn put(&self, targets: Vec<Target>) -> Result<(), Error> {
        store::write(&self.path, &Document { v: SCHEMA, targets })
    }
}

/// Applies the precedence rules to values already read from the environment.
///
/// An empty value counts as unset, for the reason every variable in this program
/// treats one that way: a variable that is present and says nothing names no
/// directory, and taking it at its word would put the file at a relative path
/// under whatever the working directory happened to be.
///
/// A machine with no home directory at all — a bare container, something running
/// as a system account — falls back to the same per-user directory under `/tmp`
/// the sockets do. Declarations made there do not survive the machine, which is
/// the honest answer for a machine that has nowhere to keep them.
fn config_dir(
    explicit: Option<std::ffi::OsString>,
    config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(dir) = explicit.filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(base) = config_home.filter(|value| !value.is_empty()) {
        return PathBuf::from(base).join(DIR_NAME);
    }
    match home.filter(|value| !value.is_empty()) {
        Some(home) => PathBuf::from(home).join(DEFAULT_CONFIG_HOME).join(DIR_NAME),
        None => crate::paths::per_user_dir(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn at(text: &str) -> Timestamp {
        Timestamp::parse(text).expect("not a timestamp")
    }

    fn now() -> Timestamp {
        at("2026-08-17T10:00:00.000Z")
    }

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    fn resolved(explicit: Option<&str>, config_home: Option<&str>, home: Option<&str>) -> PathBuf {
        config_dir(
            explicit.map(OsString::from),
            config_home.map(OsString::from),
            home.map(OsString::from),
        )
    }

    #[test]
    fn an_explicit_directory_is_used_verbatim() {
        assert_eq!(
            resolved(
                Some("/somewhere/else"),
                Some("/home/u/.config"),
                Some("/home/u")
            ),
            PathBuf::from("/somewhere/else")
        );
    }

    #[test]
    fn the_configuration_base_is_used_when_there_is_no_explicit_directory() {
        assert_eq!(
            resolved(None, Some("/home/u/.config"), Some("/home/u")),
            PathBuf::from("/home/u/.config/agentbus")
        );
    }

    #[test]
    fn without_a_configuration_base_it_sits_under_the_home_directory() {
        assert_eq!(
            resolved(None, None, Some("/home/u")),
            PathBuf::from("/home/u/.config/agentbus")
        );
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        assert_eq!(
            resolved(Some(""), Some(""), Some("/home/u")),
            PathBuf::from("/home/u/.config/agentbus")
        );
    }

    #[test]
    fn a_machine_with_no_home_directory_still_has_somewhere() {
        assert_eq!(resolved(None, None, None), crate::paths::per_user_dir());
    }

    #[test]
    fn nothing_is_declared_before_anything_has_been_declared() {
        let dir = tempfile::tempdir().unwrap();
        let targets = Targets::in_dir(dir.path());

        assert_eq!(targets.list().unwrap(), Vec::new());
        assert_eq!(targets.changed_at(), None);
    }

    #[test]
    fn a_declaration_survives_being_written_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let targets = Targets::in_dir(dir.path().join("nested"));

        assert!(
            targets
                .declare(SSH, &words(&["-p", "2222", "bob@fs.example.net"]), &now())
                .unwrap()
        );

        let listed = targets.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].transport, SSH);
        // Verbatim: the words are what somebody typed and nothing here has any
        // business tidying them.
        assert_eq!(listed[0].args, words(&["-p", "2222", "bob@fs.example.net"]));
        assert_eq!(listed[0].added, now());
    }

    #[test]
    fn the_file_and_its_directory_are_nobody_elses_business() {
        let dir = tempfile::tempdir().unwrap();
        let targets = Targets::in_dir(dir.path().join("agentbus"));
        targets
            .declare(SSH, &words(&["fileserver"]), &now())
            .unwrap();

        let file = fs::metadata(targets.path()).unwrap().permissions().mode();
        let holding = fs::metadata(targets.path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode();

        assert_eq!(file & 0o777, store::FILE_MODE);
        assert_eq!(holding & 0o777, store::DIR_MODE);
    }

    #[test]
    fn declaring_the_same_target_twice_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let targets = Targets::in_dir(dir.path());
        let args = words(&["fileserver"]);
        targets.declare(SSH, &args, &now()).unwrap();
        let written = fs::read_to_string(targets.path()).unwrap();

        assert!(
            !targets
                .declare(SSH, &args, &at("2026-08-18T11:00:00.000Z"))
                .unwrap()
        );

        assert_eq!(targets.list().unwrap().len(), 1);
        // Not even the timestamp: the declaration that stands is the first one.
        assert_eq!(fs::read_to_string(targets.path()).unwrap(), written);
    }

    #[test]
    fn targets_differing_in_one_word_are_two_targets() {
        let dir = tempfile::tempdir().unwrap();
        let targets = Targets::in_dir(dir.path());

        targets
            .declare(SSH, &words(&["fileserver"]), &now())
            .unwrap();
        targets
            .declare(SSH, &words(&["-p", "2222", "fileserver"]), &now())
            .unwrap();
        targets
            .declare(DOCKER, &words(&["fileserver"]), &now())
            .unwrap();

        assert_eq!(targets.list().unwrap().len(), 3);
    }

    #[test]
    fn undeclaring_removes_exactly_the_one_named() {
        let dir = tempfile::tempdir().unwrap();
        let targets = Targets::in_dir(dir.path());
        targets
            .declare(SSH, &words(&["fileserver"]), &now())
            .unwrap();
        targets
            .declare(DOCKER, &words(&["eager_mclean"]), &now())
            .unwrap();

        assert!(targets.undeclare(SSH, &words(&["fileserver"])).unwrap());

        let left = targets.list().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].transport, DOCKER);
    }

    #[test]
    fn undeclaring_something_nobody_declared_says_so_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let targets = Targets::in_dir(dir.path());

        assert!(!targets.undeclare(SSH, &words(&["fileserver"])).unwrap());

        assert!(!targets.path().exists());
    }

    #[test]
    fn a_file_from_a_later_build_is_reported_rather_than_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let targets = Targets::in_dir(dir.path());
        let written = r#"{"v":2,"targets":[],"whatever":true}"#;
        fs::write(targets.path(), written).unwrap();

        let error = targets.list().unwrap_err();

        assert!(
            matches!(error, Error::Version { found: 2, .. }),
            "{error:?}"
        );
        assert!(
            targets
                .declare(SSH, &words(&["fileserver"]), &now())
                .is_err()
        );
        assert_eq!(fs::read_to_string(targets.path()).unwrap(), written);
    }

    #[test]
    fn a_file_that_is_not_a_list_of_targets_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let targets = Targets::in_dir(dir.path());
        fs::write(targets.path(), r#"{"v":1,"targets":"all of them"}"#).unwrap();

        assert!(matches!(
            targets.list().unwrap_err(),
            Error::Malformed { .. }
        ));
    }

    #[test]
    fn a_write_leaves_no_file_that_is_half_of_one() {
        let dir = tempfile::tempdir().unwrap();
        let targets = Targets::in_dir(dir.path());
        for index in 0..20 {
            targets
                .declare(SSH, &words(&[&format!("host{index}")]), &now())
                .unwrap();
            // Whatever is at the path is a whole document at every moment,
            // because what is written is a rename over it rather than an edit
            // of it.
            assert_eq!(targets.list().unwrap().len(), index + 1);
        }
        // And nothing is left beside it.
        let beside: Vec<PathBuf> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path != targets.path())
            .collect();
        assert!(beside.is_empty(), "{beside:?}");
    }
}
