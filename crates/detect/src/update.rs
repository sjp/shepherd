//! Taking newer manifests from a published catalog.
//!
//! Detection is data, and the point of data is that a UI change can be answered
//! by publishing a file rather than by cutting a release. This is the channel
//! that carries such a file to a machine: one catalog listing every manifest
//! that is published, one fetch per entry, and a copy committed into the tier
//! [`ManifestStore`] reads fetched manifests from.
//!
//! # What arrives is not trusted
//!
//! Everything here comes off the network, and what it decides is how a screen
//! is read — so every step is a refusal looking for a reason to fire. The
//! catalog must be the schema this build knows. A path in it may not be
//! absolute, may not climb out of the catalog's own directory and may not name
//! another host. A body is read to a cap and no further. A manifest must parse
//! and validate under its family's schema, must be about the agent it was
//! listed as, must declare a version and the engine it needs, and must be
//! strictly newer than every copy already held. Nothing that fails any of that
//! reaches the disk, and the copy already on the disk is left exactly as it
//! was.
//!
//! The one deliberate gap is provenance: there are no signatures here, and
//! whoever can publish the catalog can publish what it points at. What that
//! buys is a channel with no key distribution and no rotation story, and what
//! it costs is that the transport's own authentication is the whole of the
//! trust. It is worth revisiting if these files are ever hosted somewhere their
//! publisher does not control.
//!
//! # A version bump is what makes a change a change
//!
//! Two copies claiming the same version have to *be* the same copy. A publisher
//! who edits a file without bumping it gets a refusal rather than a silent
//! divergence between machines that fetched before the edit and machines that
//! fetched after it — and a machine that re-fetches the copy it already has
//! recognizes it and does nothing at all, which is what makes checking often
//! cheap.
//!
//! # One failure is one failure
//!
//! A catalog that cannot be read stops the run, because there is nothing to
//! act on. Past that, an entry that fails costs that entry: the rest are
//! fetched, the ones that are good are committed, and every outcome — updated,
//! already current, or refused and why — is in the [`UpdateOutcome`] and in the
//! status file beside the manifests. Checking is something a machine does on a
//! timer, unattended, so the record of what happened has to survive the process
//! that did it.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::identify::same;
use crate::store::{
    Family, Hooks, MAX_MANIFEST_BYTES, ManifestStore, Screen, StorePaths, covers,
    manifest_file_name, read_bounded,
};
use crate::version::ManifestVersion;

/// Where the catalog is published when nothing says otherwise.
///
/// It names the repository this program's releases come from, and like that
/// one it is expected to move: it is written down here and nowhere else, so
/// publishing the manifests somewhere else is this line plus a catalog at the
/// new location. Anyone who has not moved them can still point a single run
/// elsewhere with [`CATALOG_URL_VAR`], which is how a mirror is used without
/// rebuilding.
pub const DEFAULT_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/sjp/agentbus/main/manifests/index.toml";

/// The environment variable naming a catalog in place of [`DEFAULT_CATALOG_URL`].
pub const CATALOG_URL_VAR: &str = "AGENTBUS_MANIFEST_CATALOG_URL";

/// The catalog schema this understands.
///
/// A catalog that says anything else is refused whole rather than read
/// leniently. It was written by a publisher that knows something this build
/// does not, and the thing it might know is what a manifest entry means.
pub const CATALOG_SCHEMA_VERSION: u32 = 1;

/// The most of any one response this will read, in bytes.
///
/// Far above any manifest or catalog worth publishing and far below anything
/// worth streaming, so that a location answering with an endless body fails
/// instead of filling memory.
pub const MAX_FETCH_BYTES: u64 = 256 * 1024;

/// How long a connection may take to establish.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one whole request may take, connection included.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The file recording what the last check did, beside the manifests it checked.
pub const STATUS_FILE: &str = "status.toml";

/// The suffix a manifest carries while it is being written.
///
/// Not `.toml`, so that a half-written file is not something the store could
/// ever mistake for a manifest even in the moment before it is renamed away.
const PARTIAL: &str = "part";

/// The catalog a check reads, as its publisher wrote it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    /// The schema it is written in. Always [`CATALOG_SCHEMA_VERSION`] for one
    /// this build can read.
    schema_version: u32,
    /// Every manifest that is published, in whatever order the publisher chose.
    #[serde(default)]
    manifests: Vec<CatalogEntry>,
}

/// One published manifest, as the catalog describes it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogEntry {
    /// The agent it describes.
    id: String,
    /// The family it belongs to.
    family: String,
    /// Where it is, relative to the catalog.
    path: String,
}

/// A catalog entry this build knows what to do with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Listed {
    id: String,
    family: Listing,
    path: String,
}

/// Which family a listed manifest belongs to.
///
/// A catalog naming a family this build has never heard of is describing
/// something a later engine grew, and the entry is skipped rather than
/// refused: forward compatibility here is what lets one catalog serve builds
/// on both sides of a new family being added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Listing {
    Screen,
    Hooks,
}

impl Listing {
    /// The family a catalog's `family` field names, if this build has it.
    fn named(family: &str) -> Option<Self> {
        match family {
            Screen::NAME => Some(Self::Screen),
            Hooks::NAME => Some(Self::Hooks),
            _ => None,
        }
    }

    /// How the family is written.
    fn name(self) -> &'static str {
        match self {
            Self::Screen => Screen::NAME,
            Self::Hooks => Hooks::NAME,
        }
    }
}

/// What one check did, in full.
///
/// Every entry the catalog listed is here whatever became of it, so a caller
/// showing a person what happened does not have to guess at the entries that
/// are missing from a list of successes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    /// The catalog that was read.
    pub catalog_url: String,
    /// When the check ran, in seconds since the Unix epoch.
    pub checked_at: u64,
    /// Whether the catalog itself could be read and acted on.
    pub result: CheckResult,
    /// One record per entry that was acted on, in the catalog's own order.
    pub manifests: Vec<ManifestOutcome>,
}

impl UpdateOutcome {
    /// Whether anything was written, which is whether the store is now stale.
    ///
    /// The check does not reload the store itself: a caller mid-way through
    /// reading screens may want to finish what it is doing first, and a caller
    /// that only wanted to know what is published may not want to reload at
    /// all.
    pub fn committed(&self) -> bool {
        self.manifests
            .iter()
            .any(|manifest| manifest.result == ItemResult::Updated)
    }

    /// The manifests this check replaced.
    pub fn updated(&self) -> impl Iterator<Item = &ManifestOutcome> {
        self.manifests
            .iter()
            .filter(|manifest| manifest.result == ItemResult::Updated)
    }
}

/// Whether a check got as far as acting on a catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    /// The catalog was read, and every entry in it was acted on.
    Checked,
    /// The catalog could not be read, or was not one this build accepts.
    Failed(String),
}

impl CheckResult {
    /// How the result is recorded, as one line.
    fn record(&self) -> String {
        match self {
            Self::Checked => "checked".to_owned(),
            Self::Failed(reason) => format!("failed: {reason}"),
        }
    }
}

/// What became of one listed manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestOutcome {
    /// The family it belongs to.
    pub family: &'static str,
    /// The agent it describes, as the catalog listed it.
    pub id: String,
    /// Where it was fetched from.
    pub url: String,
    /// The version already cached on this machine, if any was.
    pub cached_version: Option<ManifestVersion>,
    /// The version that was fetched, once the fetched copy said one. Present
    /// on a refusal too, because "it offered this and was turned down" is the
    /// sentence a person debugging a stuck manifest needs.
    pub attempted_version: Option<ManifestVersion>,
    /// What was decided.
    pub result: ItemResult,
}

/// What a check decided about one manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemResult {
    /// A newer copy was committed.
    Updated,
    /// What is published is what is already held.
    Current,
    /// It was not fetched, not accepted, or not written.
    Failed(String),
}

impl ItemResult {
    /// How the result is recorded.
    fn label(&self) -> &'static str {
        match self {
            Self::Updated => "updated",
            Self::Current => "current",
            Self::Failed(_) => "failed",
        }
    }

    /// The reason it failed, if it did.
    fn error(&self) -> Option<String> {
        match self {
            Self::Failed(reason) => Some(reason.clone()),
            _ => None,
        }
    }
}

/// What the last check did, as it is kept on disk.
///
/// Written after every check and read by anything reporting on the manifests,
/// so that "when was this last looked at, and what happened" survives the
/// process that asked. Records for manifests this check did not cover are kept
/// as they were: a catalog that stops listing an agent, or a check that never
/// got as far as the entries, does not erase what is known about the copy this
/// machine is still using.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    /// When the last check ran, in seconds since the Unix epoch.
    ///
    /// A count rather than a spelled-out date, because this library has no
    /// calendar and does not want one: the arithmetic that turns an instant
    /// into a date belongs to whatever displays it.
    pub last_checked_unix: Option<u64>,
    /// What became of the last check, as one line.
    pub last_result: Option<String>,
    /// One record per manifest, by family and then by agent.
    pub manifests: BTreeMap<String, BTreeMap<String, ManifestStatus>>,
}

impl Status {
    /// What the status file at `paths` says, or an empty record when there is
    /// no file or it cannot be read.
    ///
    /// Never an error: this is bookkeeping about checks, and a machine that
    /// cannot read it is a machine that has not checked as far as anything
    /// here is concerned. A file that exists and does not parse is worth a
    /// warning, because somebody has been editing it.
    pub fn read(paths: &StorePaths) -> Self {
        let path = paths.state.join(STATUS_FILE);
        let Ok(Some(content)) = read_bounded(&path) else {
            return Self::default();
        };
        toml::from_str(&content).unwrap_or_else(|error| {
            warn!(path = %path.display(), %error, "the manifest status file cannot be read");
            Self::default()
        })
    }

    /// What is recorded about one manifest, if anything is.
    pub fn record(&self, family: &str, id: &str) -> Option<&ManifestStatus> {
        self.manifests.get(family)?.get(id)
    }

    /// Folds one check's findings in, leaving everything it did not cover.
    fn absorb(&mut self, outcome: &UpdateOutcome) {
        self.last_checked_unix = Some(outcome.checked_at);
        self.last_result = Some(outcome.result.record());
        for manifest in &outcome.manifests {
            self.manifests
                .entry(manifest.family.to_owned())
                .or_default()
                .insert(
                    manifest.id.clone(),
                    ManifestStatus {
                        cached_version: manifest.cached_version.as_ref().map(ToString::to_string),
                        attempted_version: manifest
                            .attempted_version
                            .as_ref()
                            .map(ToString::to_string),
                        last_checked_unix: Some(outcome.checked_at),
                        last_result: manifest.result.label().to_owned(),
                        last_error: manifest.result.error(),
                    },
                );
        }
    }

    /// Writes the record out, by the same temp-and-rename this commits
    /// manifests with.
    fn write(&self, paths: &StorePaths) -> Result<(), String> {
        let path = paths.state.join(STATUS_FILE);
        let content = toml::to_string_pretty(self).map_err(|error| error.to_string())?;
        commit(&path, &content).map_err(|error| error.to_string())
    }
}

/// What the last check found out about one manifest.
///
/// The versions are strings rather than parsed ones because this is a report:
/// it is read to be shown, and a record written by a build whose grammar has
/// moved on should still be showable by this one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ManifestStatus {
    /// The version held on this machine when the check ran.
    pub cached_version: Option<String>,
    /// The version the catalog offered.
    pub attempted_version: Option<String>,
    /// When this manifest was last checked, in seconds since the Unix epoch.
    pub last_checked_unix: Option<u64>,
    /// `updated`, `current` or `failed`.
    pub last_result: String,
    /// Why it failed, when it did.
    pub last_error: Option<String>,
}

/// The catalog this process should read, as the environment says or as
/// [`DEFAULT_CATALOG_URL`] says.
pub fn catalog_url() -> String {
    std::env::var(CATALOG_URL_VAR)
        .ok()
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_CATALOG_URL.to_owned())
}

/// Checks the catalog this process's environment names, and takes what is newer.
pub fn update_from_env(store: &ManifestStore) -> UpdateOutcome {
    update(store, &catalog_url())
}

/// Checks `catalog_url` and takes whatever it publishes that is newer than what
/// `store` already has.
///
/// Never fails: everything that can go wrong is something a caller wants
/// reported rather than propagated, because a check that could not reach the
/// network is not an error in the program that ran it. The status file is
/// written whatever happened, including when the catalog itself could not be
/// read — a check that failed is a thing worth knowing the time of.
///
/// The store is not reloaded here. Ask [`UpdateOutcome::committed`] whether
/// there would be any point.
pub fn update(store: &ManifestStore, catalog_url: &str) -> UpdateOutcome {
    let paths = store.paths().clone();
    let mut outcome = UpdateOutcome {
        catalog_url: catalog_url.to_owned(),
        checked_at: now_unix(),
        result: CheckResult::Checked,
        manifests: Vec::new(),
    };

    match read_catalog(catalog_url) {
        Ok((listed, base)) => {
            for entry in listed {
                let url = format!("{base}/{}", entry.path);
                debug!(family = entry.family.name(), id = %entry.id, %url, "checking a manifest");
                let item = match entry.family {
                    Listing::Screen => apply::<Screen>(&paths, &entry.id, &url),
                    Listing::Hooks => apply::<Hooks>(&paths, &entry.id, &url),
                };
                match &item.result {
                    ItemResult::Updated => info!(
                        family = item.family,
                        id = %item.id,
                        version = %describe(item.attempted_version.as_ref()),
                        "took a newer manifest",
                    ),
                    ItemResult::Current => {
                        debug!(family = item.family, id = %item.id, "already current")
                    }
                    ItemResult::Failed(reason) => warn!(
                        family = item.family,
                        id = %item.id,
                        %reason,
                        "a published manifest was not taken",
                    ),
                }
                outcome.manifests.push(item);
            }
        }
        Err(reason) => {
            warn!(url = %catalog_url, %reason, "the manifest catalog could not be read");
            outcome.result = CheckResult::Failed(reason);
        }
    }

    let mut status = Status::read(&paths);
    status.absorb(&outcome);
    if let Err(reason) = status.write(&paths) {
        warn!(%reason, "the manifest status could not be written");
        // Said in the outcome as well as in the log: a caller reporting on this
        // check would otherwise show a run that looks entirely successful while
        // the record of it went nowhere.
        outcome.result = CheckResult::Failed(format!("the status could not be written: {reason}"));
    }
    outcome
}

/// Reads the catalog and says what it lists, along with the location its paths
/// are relative to.
fn read_catalog(url: &str) -> Result<(Vec<Listed>, String), String> {
    let base = base_of(url)?;
    let content = fetch(url)?;
    Ok((parse_catalog(&content)?, base))
}

/// The location a catalog's own paths are relative to: everything up to its
/// last slash.
fn base_of(url: &str) -> Result<String, String> {
    match url.rsplit_once('/') {
        Some((base, _)) if !base.is_empty() => Ok(base.to_owned()),
        _ => Err(format!("the catalog url {url:?} names no directory")),
    }
}

/// What a catalog lists, once everything unusable has been refused or skipped.
///
/// The refusals here stop the whole run rather than one entry, because each of
/// them says the catalog is not what it claims to be: a schema this build
/// cannot read, a path that would fetch from somewhere else entirely, or two
/// entries for one manifest, which leaves no way to tell which was meant.
/// Skipping the bad entry and trusting the rest of the file would be trusting a
/// file that has just been shown to be untrustworthy.
fn parse_catalog(content: &str) -> Result<Vec<Listed>, String> {
    let catalog: Catalog =
        toml::from_str(content).map_err(|error| format!("it is not a catalog: {error}"))?;
    if catalog.schema_version != CATALOG_SCHEMA_VERSION {
        return Err(format!(
            "it is schema {}; this understands {CATALOG_SCHEMA_VERSION}",
            catalog.schema_version,
        ));
    }

    let mut listed: Vec<Listed> = Vec::new();
    for entry in catalog.manifests {
        let Some(family) = Listing::named(&entry.family) else {
            info!(
                family = %entry.family,
                id = %entry.id,
                "the catalog lists a manifest family this build has not got; skipping it",
            );
            continue;
        };
        if manifest_file_name(&entry.id).is_none() {
            return Err(format!(
                "the {} entry {:?} is not a name a manifest can be kept under",
                entry.family, entry.id,
            ));
        }
        let path = entry.path.trim();
        if path.is_empty() {
            return Err(format!(
                "the {} entry {:?} says nothing about where it is",
                entry.family, entry.id,
            ));
        }
        if !relative(path) {
            return Err(format!(
                "the {} entry {:?} is at {path:?}, which is not inside the catalog's own directory",
                entry.family, entry.id,
            ));
        }
        if listed
            .iter()
            .any(|other| other.family == family && same(&other.id, &entry.id))
        {
            return Err(format!(
                "it lists the {} manifest {:?} more than once",
                entry.family, entry.id,
            ));
        }
        listed.push(Listed {
            id: entry.id,
            family,
            path: path.to_owned(),
        });
    }
    Ok(listed)
}

/// Whether a path stays underneath the place it is relative to.
///
/// Three ways it would not: a scheme, which names another location outright; a
/// leading slash, which names the root of the host; and a `..` segment, which
/// climbs. What is left can only reach files the catalog's own publisher put
/// beside it.
fn relative(path: &str) -> bool {
    !path.contains("://") && !path.starts_with('/') && !path.split('/').any(|part| part == "..")
}

/// Fetches one listed manifest and takes it if every rule allows.
fn apply<F: Family>(paths: &StorePaths, id: &str, url: &str) -> ManifestOutcome {
    let cached = paths
        .remote_file(F::NAME, id)
        .and_then(|path| read_bounded(&path).ok().flatten())
        .and_then(|content| {
            let version = F::parse(&content)
                .ok()
                .and_then(|m| F::version(&m).cloned())?;
            Some((content, version))
        });

    let mut outcome = ManifestOutcome {
        family: F::NAME,
        id: id.to_owned(),
        url: url.to_owned(),
        cached_version: cached.as_ref().map(|(_, version)| version.clone()),
        attempted_version: None,
        result: ItemResult::Current,
    };
    outcome.result = match accept::<F>(paths, id, url, cached, &mut outcome.attempted_version) {
        Ok(result) => result,
        Err(reason) => ItemResult::Failed(reason),
    };
    outcome
}

/// Everything one manifest has to satisfy, in the order a failure is cheapest
/// to find in.
///
/// `attempted` is filled in as soon as the fetched copy names a version, so
/// that a refusal further down still reports what was offered.
fn accept<F: Family>(
    paths: &StorePaths,
    id: &str,
    url: &str,
    cached: Option<(String, ManifestVersion)>,
    attempted: &mut Option<ManifestVersion>,
) -> Result<ItemResult, String> {
    let content = fetch(url)?;
    // The transfer cap is about what may be read off a network; this one is
    // about what the store will read back off the disk afterwards. Committing a
    // file between the two would be committing one that is never used again.
    if content.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(format!(
            "it is {} bytes, over the {MAX_MANIFEST_BYTES}-byte limit for a manifest",
            content.len(),
        ));
    }

    let manifest = F::parse(&content)?;
    if !covers::<F>(&manifest, id) {
        return Err(format!(
            "it describes {:?}, and the catalog lists it as {id:?}",
            F::id(&manifest),
        ));
    }
    let version = F::version(&manifest)
        .ok_or("it declares no version, so there is no telling whether it is newer")?
        .clone();
    attempted.replace(version.clone());
    // A published manifest says which engine it needs even when it needs the
    // first one. The field is how a later engine's features are kept away from
    // a build that would misread them, and a copy that leaves it out is a copy
    // whose author was not thinking about that at all.
    let required = F::min_engine_version(&manifest)
        .ok_or("it declares no min_engine_version, so there is no telling what it needs")?;
    if required > F::ENGINE_VERSION {
        return Err(format!(
            "it needs engine {required}; this understands {}",
            F::ENGINE_VERSION,
        ));
    }

    if let Some((held, held_version, what)) = held::<F>(id, cached)
        && let Some(result) = compare(&version, &content, &held_version, &held, what)?
    {
        return Ok(result);
    }

    let path = paths
        .remote_file(F::NAME, id)
        .ok_or("this machine has nowhere to keep a fetched manifest")?;
    commit(&path, &content)
        .map_err(|error| format!("it could not be written to {}: {error}", path.display(),))?;
    Ok(ItemResult::Updated)
}

/// The newest copy of a manifest this machine already holds, and what to call
/// it in a sentence.
///
/// The bundled copy counts: a fetched copy no newer than the one compiled in
/// describes an older UI than this build already knows about, and taking it
/// would be a downgrade that the store would then have to notice and ignore.
/// A tie goes to the cached copy, because that is the file a commit would
/// overwrite and so the one an identical-content check should be against.
fn held<F: Family>(
    id: &str,
    cached: Option<(String, ManifestVersion)>,
) -> Option<(String, ManifestVersion, &'static str)> {
    let bundled = F::bundled()
        .iter()
        .find(|(key, _)| same(key, id))
        .and_then(|(_, source)| {
            let version = F::parse(source)
                .ok()
                .and_then(|m| F::version(&m).cloned())?;
            Some(((*source).to_owned(), version, "the bundled copy"))
        });
    match (cached, bundled) {
        (Some((content, version)), Some((_, bundled_version, _))) if version >= bundled_version => {
            Some((content, version, "the cached copy"))
        }
        (_, Some(bundled)) => Some(bundled),
        (Some((content, version)), None) => Some((content, version, "the cached copy")),
        (None, None) => None,
    }
}

/// What a fetched version means next to the one already held: nothing to do,
/// a refusal, or `None` for "take it".
fn compare(
    version: &ManifestVersion,
    content: &str,
    held_version: &ManifestVersion,
    held: &str,
    what: &str,
) -> Result<Option<ItemResult>, String> {
    match version.cmp(held_version) {
        std::cmp::Ordering::Greater => Ok(None),
        std::cmp::Ordering::Less => Err(format!(
            "it is version {version}, older than {what}'s {held_version}",
        )),
        // The same version has to be the same file. Anything else is one
        // version number meaning two different things, which is a fleet where
        // no two machines can be shown to agree.
        std::cmp::Ordering::Equal if content == held => Ok(Some(ItemResult::Current)),
        std::cmp::Ordering::Equal => Err(format!(
            "it is version {version}, which {what} already is, with different content: \
             changed content without a version bump",
        )),
    }
}

/// Writes `content` to `path` so that nothing ever reads a half-written one.
///
/// The bytes go to a name in the same directory, are flushed and synced there,
/// and only then take the real name in a single rename. A run killed at any
/// point leaves either the old file or the new one, never a mixture — which
/// matters because the file being written is the one that decides what a screen
/// means, and a truncated manifest that happened to parse would be a different
/// manifest.
fn commit(path: &Path, content: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other(format!("{} has no directory", path.display())))?;
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} names no file", path.display())))?;
    // Named after the process writing it, so that two checks running at once
    // cannot write each other's temporary file.
    let partial = dir.join(format!(
        "{}.{}.{PARTIAL}",
        name.to_string_lossy(),
        std::process::id(),
    ));

    let written = (|| {
        let mut file = File::create(&partial)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    if let Err(error) = fs::rename(&partial, path) {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    sync(dir);
    Ok(())
}

/// Asks the filesystem to make a directory's own contents durable.
///
/// The rename is what makes the file appear, and on a crash it is the
/// directory's record of that rename which decides whether it stayed. Nothing
/// is done about a failure: the file is already in place for every reader, and
/// there are filesystems where a directory cannot be opened for this at all.
fn sync(dir: &Path) {
    match File::open(dir).and_then(|dir| dir.sync_all()) {
        Ok(()) => {}
        Err(error) => {
            debug!(dir = %dir.display(), %error, "a committed manifest's directory was not synced")
        }
    }
}

/// Reads a location as text, up to [`MAX_FETCH_BYTES`] of it.
fn fetch(url: &str) -> Result<String, String> {
    tls();
    let http = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build(),
    );
    let response = http
        .get(url)
        .call()
        .map_err(|error| format!("{url} could not be fetched: {error}"))?;
    let (_, body) = response.into_parts();
    body.into_with_config()
        .limit(MAX_FETCH_BYTES)
        .read_to_string()
        .map_err(|error| format!("{url} could not be read: {error}"))
}

/// Puts the cryptography behind https in place, once.
///
/// The TLS library takes its algorithms from a provider installed for the whole
/// process. An error means something else installed one first, which answers
/// the question just as well as installing this one would have.
fn tls() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls_graviola::default_provider().install_default();
    });
}

/// Now, in seconds since the Unix epoch.
///
/// A clock set before 1970 reads as the epoch rather than as an error: this
/// number is a note about when something was last looked at, and no part of
/// checking manifests should stop because a machine's clock is wrong.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// A version as a log line should name it.
fn describe(version: Option<&ManifestVersion>) -> String {
    match version {
        Some(version) => version.to_string(),
        None => "an unnamed version".to_owned(),
    }
}

#[cfg(test)]
mod tests;
