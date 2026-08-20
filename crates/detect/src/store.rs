//! Which copy of a manifest is the active one.
//!
//! The same agent can be described three times over on one machine: by the
//! copy compiled into this library, by a copy fetched from a catalog and
//! cached since, and by a copy the operator wrote themselves. This module is
//! the single place that decides which of them answers, keeps the compiled
//! result so that the decision is made once rather than per screen, and can be
//! told to forget everything it decided when the copies on disk change.
//!
//! # Precedence
//!
//! A local override always wins. It is the escape hatch: when an agent's UI
//! changes on a Tuesday, the person watching it should be able to fix their own
//! machine that afternoon, and nothing that arrives later may quietly undo
//! that. A cached remote copy comes next, but only when its version is not
//! older than the bundled one — a cache that predates the binary describes an
//! older UI than the binary already knows about. The bundled copy is the floor,
//! so there is always an answer for an agent this library ships knowledge of.
//!
//! # Fail-soft, and never quiet about it
//!
//! Every tier below the bundled one comes from outside this program, so every
//! way it can be wrong — missing, unreadable, enormous, not TOML, a manifest
//! for a different agent, older than what is already held — is handled by
//! moving to the next source and recording a sentence about it. A typo in an
//! override must cost its author one warning, not detection for that agent.
//! The bundled tier is held to the opposite standard: it is this library's own
//! data, so a bundled manifest that does not parse is a bug in the build and
//! panics rather than degrading in front of a user.
//!
//! # Two families, one store
//!
//! Screen manifests are not the only kind, and the question "which copy is
//! active" has exactly one right answer regardless of what the file describes.
//! So precedence, caching, bounded reads and warnings are written once against
//! the [`Family`] trait, and a family contributes only what is specific to it:
//! how to parse one, what its identity is, and what ships inside the binary.

use std::collections::{BTreeSet, HashMap};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use agentbus_protocol::{Agent, UnstampedEvent};
use serde::Serialize;

use crate::explain::{Explain, ManifestSource};
use crate::hooks::CompiledHookManifest;
use crate::hooks::bundled::bundled_hook_manifests;
use crate::hooks::schema::HookManifest;
use crate::identify::{ProcessInfo, same};
use crate::screen::bundled::bundled_screen_manifests;
use crate::screen::region::ScreenInput;
use crate::screen::rules::{CompiledManifest, Detection};
use crate::screen::schema::ScreenManifest;
use crate::version::ManifestVersion;

/// The most of one manifest file this store will read.
///
/// Comfortably above anything the bundled corpus needs and far below anything
/// that would be worth streaming. A file over the cap is refused rather than
/// truncated: half a manifest is a different manifest, and one that happens to
/// parse would be worse than one that does not.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// The variable naming the user's home directory.
const HOME_VAR: &str = "HOME";

/// The variable naming the base directory for a user's own configuration.
const CONFIG_HOME_VAR: &str = "XDG_CONFIG_HOME";

/// The variable naming the base directory for state that should survive a
/// reboot but is not configuration.
const STATE_HOME_VAR: &str = "XDG_STATE_HOME";

/// Where configuration sits when `XDG_CONFIG_HOME` does not say.
const DEFAULT_CONFIG_HOME: &str = ".config";

/// Where state sits when `XDG_STATE_HOME` does not say.
const DEFAULT_STATE_HOME: &str = ".local/state";

/// This program's own directory, inside whichever base directory it sits under.
const PROGRAM_DIR: &str = "agentbus";

/// The directory holding manifests, inside this program's own directory.
const MANIFESTS_DIR: &str = "manifests";

/// The directory holding copies fetched from a catalog, inside the state
/// directory. Separate from the rest of the state directory because everything
/// under it is replaceable: it can be deleted wholesale and the only cost is
/// the next fetch.
const REMOTE_DIR: &str = "remote";

/// The extension every manifest file carries.
const MANIFEST_EXTENSION: &str = "toml";

/// Where a machine keeps the manifests that outrank the bundled ones.
///
/// Both directories are roots, one per tier: `overrides` holds files a person
/// wrote, `state` holds what was fetched. Under `overrides` a manifest lives at
/// `<family>/<id>.toml`, and under `state` at `remote/<family>/<id>.toml`,
/// alongside whatever bookkeeping the fetching side keeps in the state root.
///
/// Neither directory needs to exist. Absent is the ordinary case — most
/// machines never override anything and many never fetch — so it is a normal
/// condition throughout this module, never an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePaths {
    /// The root of the tier a person edits.
    pub overrides: PathBuf,
    /// The root of the tier a fetch writes into.
    pub state: PathBuf,
}

impl StorePaths {
    /// The paths this process's environment describes.
    ///
    /// `XDG_CONFIG_HOME` and `XDG_STATE_HOME` are honoured when they hold an
    /// absolute path, and ignored otherwise as their specification requires;
    /// the fallbacks are the conventional `~/.config` and `~/.local/state`.
    ///
    /// A process with no home directory and no base directory variable gets a
    /// root that is not absolute, and this module consults no tier through such
    /// a root: a relative one would mean reading whichever files happen to sit
    /// beside the working directory, which is a surprising way to change what
    /// an agent's screen is taken to mean.
    pub fn from_env() -> Self {
        let home = env::var_os(HOME_VAR).filter(|home| !home.is_empty());
        Self {
            overrides: base(
                home.as_deref(),
                env::var_os(CONFIG_HOME_VAR),
                DEFAULT_CONFIG_HOME,
            ),
            state: base(
                home.as_deref(),
                env::var_os(STATE_HOME_VAR),
                DEFAULT_STATE_HOME,
            ),
        }
    }

    /// The paths a machine whose home directory is `home` would have, with the
    /// base directory variables ignored.
    pub fn rooted(home: impl AsRef<Path>) -> Self {
        let home = home.as_ref();
        Self {
            overrides: base(Some(home.as_os_str()), None, DEFAULT_CONFIG_HOME),
            state: base(Some(home.as_os_str()), None, DEFAULT_STATE_HOME),
        }
    }

    /// Where a person's copy of one family's manifests lives.
    pub fn override_dir(&self, family: &str) -> PathBuf {
        self.overrides.join(family)
    }

    /// Where fetched copies of one family's manifests live.
    pub fn remote_dir(&self, family: &str) -> PathBuf {
        self.state.join(REMOTE_DIR).join(family)
    }

    /// The file a fetched copy of one agent's manifest belongs at, or nothing
    /// when this machine has nowhere to keep one — an id that could not name a
    /// file on its own, or a state root that is not absolute.
    ///
    /// This is the path a fetch writes and the path the store then reads, so
    /// the two are the same expression rather than two spellings that have to
    /// be kept agreeing.
    pub fn remote_file(&self, family: &str, id: &str) -> Option<PathBuf> {
        self.file(Tier::Remote, family, id)
    }

    /// The file one agent's manifest would be read from, or nothing when this
    /// tier cannot be consulted for that id at all.
    fn file(&self, tier: Tier, family: &str, id: &str) -> Option<PathBuf> {
        let dir = match tier {
            Tier::Override => self.override_dir(family),
            Tier::Remote => self.remote_dir(family),
        };
        dir.is_absolute()
            .then(|| manifest_file_name(id))
            .flatten()
            .map(|name| dir.join(name))
    }
}

/// One of this program's own directories, under the base directory the
/// environment named if it named a usable one, and under the home directory
/// otherwise.
fn base(home: Option<&OsStr>, named: Option<OsString>, default: &str) -> PathBuf {
    let root = match named.map(PathBuf::from).filter(|base| base.is_absolute()) {
        Some(base) => base,
        None => match home {
            Some(home) => Path::new(home).join(default),
            None => PathBuf::new(),
        },
    };
    root.join(PROGRAM_DIR).join(MANIFESTS_DIR)
}

/// The file name an id maps to, for an id that can name a file at all.
///
/// An id reaches this module from a caller — a hint someone typed, a name read
/// off a process — and is about to become part of a path, so an id that is not
/// one plain file-name component is refused rather than resolved. Nothing else
/// in this library would be harmed by `../../elsewhere`, but a manifest names
/// the rules a screen is read with, and choosing which file that is by walking
/// out of the manifest directory is not a capability a caller should have.
pub(crate) fn manifest_file_name(id: &str) -> Option<String> {
    let mut components = Path::new(id).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(only)), None) if only == OsStr::new(id) => {
            Some(format!("{id}.{MANIFEST_EXTENSION}"))
        }
        _ => None,
    }
}

/// One kind of manifest, as far as the store is concerned.
///
/// Everything the store does — precedence, caching, bounded reads, warnings —
/// is the same for every family; this trait is the little that is not. A family
/// says how one of its files is parsed and validated, how to read the identity
/// out of the result, and what ships inside the binary.
pub trait Family: 'static {
    /// The family's name, which is also its directory under each tier's root.
    const NAME: &'static str;

    /// The highest manifest engine this build of the family implements.
    ///
    /// A manifest may say which engine it needs; one that needs a later engine
    /// than this describes behaviour that is not here yet, and is refused
    /// rather than half-understood.
    const ENGINE_VERSION: u32;

    /// One parsed and validated manifest.
    type Manifest;

    /// The form the family's consumers use, built once per active manifest.
    type Compiled: Send + Sync;

    /// Reads one manifest, rejecting anything the family does not accept.
    ///
    /// The failure is a sentence rather than a type: the store's only use for
    /// one is to repeat it in a warning, and a family that wants its error
    /// examined can be asked directly.
    fn parse(content: &str) -> Result<Self::Manifest, String>;

    /// The id the manifest declares.
    fn id(manifest: &Self::Manifest) -> &str;

    /// The other names the manifest answers to.
    fn aliases(manifest: &Self::Manifest) -> &[String];

    /// The manifest's own version, when it declares one.
    fn version(manifest: &Self::Manifest) -> Option<&ManifestVersion>;

    /// The lowest engine the manifest says it needs, when it says.
    fn min_engine_version(manifest: &Self::Manifest) -> Option<u32>;

    /// Prepares a manifest for use.
    fn compile(manifest: Self::Manifest) -> Self::Compiled;

    /// Every manifest of this family that ships inside the binary, as
    /// (declared id, source).
    fn bundled() -> &'static [(&'static str, &'static str)];
}

/// The screen-manifest family: what an agent's terminal looks like in each of
/// its states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen;

impl Family for Screen {
    const NAME: &'static str = "screen";
    const ENGINE_VERSION: u32 = crate::screen::schema::SCREEN_ENGINE_VERSION;

    type Manifest = ScreenManifest;
    type Compiled = CompiledManifest;

    fn parse(content: &str) -> Result<Self::Manifest, String> {
        ScreenManifest::parse(content).map_err(|error| error.to_string())
    }

    fn id(manifest: &Self::Manifest) -> &str {
        &manifest.id
    }

    fn aliases(manifest: &Self::Manifest) -> &[String] {
        &manifest.aliases
    }

    fn version(manifest: &Self::Manifest) -> Option<&ManifestVersion> {
        manifest.version.as_ref()
    }

    fn min_engine_version(manifest: &Self::Manifest) -> Option<u32> {
        manifest.min_engine_version
    }

    fn compile(manifest: Self::Manifest) -> Self::Compiled {
        CompiledManifest::compile(manifest)
    }

    fn bundled() -> &'static [(&'static str, &'static str)] {
        bundled_screen_manifests()
    }
}

/// The hook-mapping family: what an agent's hook payloads mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hooks;

impl Family for Hooks {
    const NAME: &'static str = "hooks";
    const ENGINE_VERSION: u32 = crate::hooks::schema::HOOKS_ENGINE_VERSION;

    type Manifest = HookManifest;
    type Compiled = CompiledHookManifest;

    fn parse(content: &str) -> Result<Self::Manifest, String> {
        HookManifest::parse(content).map_err(|error| error.to_string())
    }

    fn id(manifest: &Self::Manifest) -> &str {
        &manifest.id
    }

    fn aliases(manifest: &Self::Manifest) -> &[String] {
        &manifest.aliases
    }

    fn version(manifest: &Self::Manifest) -> Option<&ManifestVersion> {
        manifest.version.as_ref()
    }

    fn min_engine_version(manifest: &Self::Manifest) -> Option<u32> {
        manifest.min_engine_version
    }

    fn compile(manifest: Self::Manifest) -> Self::Compiled {
        CompiledHookManifest::compile(manifest)
    }

    fn bundled() -> &'static [(&'static str, &'static str)] {
        bundled_hook_manifests()
    }
}

/// Which copy of a manifest is active for one agent, and what was passed over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestSummary {
    /// The agent the manifest describes.
    pub id: String,
    /// The family it belongs to.
    pub family: &'static str,
    /// Which copy answers, or nothing when no copy of it could be used.
    pub source: Option<ManifestSource>,
    /// The version of the copy inside the binary.
    pub bundled_version: Option<ManifestVersion>,
    /// The version of the fetched copy, whether or not it is the active one.
    pub remote_version: Option<ManifestVersion>,
    /// The version of the operator's copy, which need not declare one.
    pub override_version: Option<ManifestVersion>,
    /// What was skipped on the way to the answer, in the order it was found.
    pub warnings: Vec<String>,
}

/// Which tier a file was read from, for the warnings that name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    Override,
    Remote,
}

impl Tier {
    /// How the tier is named in a warning.
    fn label(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Remote => "cached remote",
        }
    }
}

/// What the store concluded about one agent, held until the next reload.
struct Entry<F: Family> {
    /// The copy that answers, when one does.
    active: Option<Active<F>>,
    bundled_version: Option<ManifestVersion>,
    remote_version: Option<ManifestVersion>,
    override_version: Option<ManifestVersion>,
    warnings: Vec<String>,
}

/// The winning copy, ready to be used.
struct Active<F: Family> {
    compiled: F::Compiled,
    source: ManifestSource,
}

/// The store for one family.
struct FamilyStore<F: Family> {
    paths: StorePaths,
    /// One entry per id asked for, built on first use. Held behind a lock
    /// because the natural consumer is long-lived and reads screens from more
    /// than one thread.
    cache: RwLock<HashMap<String, Arc<Entry<F>>>>,
}

impl<F: Family> FamilyStore<F> {
    fn new(paths: StorePaths) -> Self {
        Self {
            paths,
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// The cache, with a lock a panicking reader poisoned taken anyway.
    ///
    /// Nothing under this lock is a mutation half-done: an entry is built
    /// whole and then inserted. A panic somewhere else in the process is
    /// therefore no reason to stop reading screens, and propagating it as one
    /// would turn an unrelated bug into a total outage of detection.
    fn read(&self) -> RwLockReadGuard<'_, HashMap<String, Arc<Entry<F>>>> {
        self.cache.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, HashMap<String, Arc<Entry<F>>>> {
        self.cache.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets everything decided so far, so that the next question is answered
    /// from what is on disk now.
    fn reload(&self) {
        self.write().clear();
    }

    /// What the store concludes about `id`, deciding it now if it has not
    /// already.
    fn entry(&self, id: &str) -> Arc<Entry<F>> {
        if let Some(entry) = self.read().get(id) {
            return Arc::clone(entry);
        }
        let entry = Arc::new(self.load(id));
        // A racing thread may have inserted its own; either is a correct answer
        // to the same question, and keeping the one already published means a
        // caller holding it never sees the store change under it.
        Arc::clone(
            self.write()
                .entry(id.to_owned())
                .or_insert_with(|| Arc::clone(&entry)),
        )
    }

    /// Applies the precedence rules for one id.
    fn load(&self, id: &str) -> Entry<F> {
        let mut warnings = Vec::new();

        let bundled = self.bundled(id);
        let bundled_version = bundled.as_ref().and_then(F::version).cloned();

        let override_path = self.paths.file(Tier::Override, F::NAME, id);
        let overridden = override_path
            .as_ref()
            .and_then(|path| read_source::<F>(Tier::Override, path, id, &mut warnings));
        let override_version = overridden.as_ref().and_then(F::version).cloned();

        let remote_path = self.paths.file(Tier::Remote, F::NAME, id);
        let remote = remote_path
            .as_ref()
            .and_then(|path| read_source::<F>(Tier::Remote, path, id, &mut warnings));
        let remote_version = remote.as_ref().and_then(F::version).cloned();

        // Comparing the versions as options is the rule the tiers want: a
        // remote copy that declares nothing cannot show it is at least as new
        // as a bundled copy that does, and when neither declares one there is
        // nothing to prefer the older file for.
        let remote_is_current = remote_version >= bundled_version;

        let remote_present = remote.is_some();
        let active = if let Some(manifest) = overridden {
            if remote_present && let Some(path) = &remote_path {
                warnings.push(format!(
                    "{} {} is shadowed by the override",
                    Tier::Remote.label(),
                    path.display(),
                ));
            }
            let path = override_path.unwrap_or_default();
            Some((manifest, ManifestSource::Override { path }))
        } else if let Some(manifest) = remote.filter(|_| remote_is_current) {
            // A fetched copy always declares a version; one that does not is
            // only ever reached when the bundled tier declares none either, and
            // so has nothing to report.
            let version = remote_version
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            Some((manifest, ManifestSource::Remote { version }))
        } else {
            if remote_present && let Some(path) = &remote_path {
                warnings.push(format!(
                    "{} {} declares {}, older than the bundled {}; ignored",
                    Tier::Remote.label(),
                    path.display(),
                    describe(remote_version.as_ref()),
                    describe(bundled_version.as_ref()),
                ));
            }
            bundled.map(|manifest| (manifest, ManifestSource::Bundled))
        };

        Entry {
            active: active.map(|(manifest, source)| Active {
                compiled: F::compile(manifest),
                source,
            }),
            bundled_version,
            remote_version,
            override_version,
            warnings,
        }
    }

    /// The copy of `id` that ships inside the binary.
    ///
    /// A bundled manifest that does not load is this library's own data being
    /// wrong, which no user can fix and no fallback can hide: it panics, and
    /// the corpus's own tests reach it long before a release does.
    fn bundled(&self, id: &str) -> Option<F::Manifest> {
        F::bundled()
            .iter()
            .find(|(key, _)| same(key, id))
            .map(|(key, content)| {
                F::parse(content).unwrap_or_else(|message| {
                    panic!(
                        "bundled {} manifest {key:?} is not loadable: {message}",
                        F::NAME,
                    )
                })
            })
    }

    /// Every id this store could answer for: the bundled ones, plus whatever
    /// the two disk tiers hold copies of.
    fn ids(&self) -> Vec<String> {
        let mut ids: BTreeSet<String> = F::bundled()
            .iter()
            .map(|(id, _)| (*id).to_owned())
            .collect();
        for dir in [
            self.paths.override_dir(F::NAME),
            self.paths.remote_dir(F::NAME),
        ] {
            for id in manifest_ids(&dir) {
                if !ids.iter().any(|known| same(known, &id)) {
                    ids.insert(id);
                }
            }
        }
        ids.into_iter().collect()
    }

    /// One line per id about which copy answers for it.
    fn summaries(&self) -> Vec<ManifestSummary> {
        self.ids()
            .into_iter()
            .map(|id| {
                let entry = self.entry(&id);
                ManifestSummary {
                    id,
                    family: F::NAME,
                    source: entry.active.as_ref().map(|active| active.source.clone()),
                    bundled_version: entry.bundled_version.clone(),
                    remote_version: entry.remote_version.clone(),
                    override_version: entry.override_version.clone(),
                    warnings: entry.warnings.clone(),
                }
            })
            .collect()
    }
}

/// A version as a warning should name it.
fn describe(version: Option<&ManifestVersion>) -> String {
    match version {
        Some(version) => format!("version {version}"),
        None => "no version".to_owned(),
    }
}

/// The ids one directory holds manifests for.
///
/// A directory that is not there, or cannot be listed, holds none: both are the
/// same absence as far as a caller asking what is available is concerned.
fn manifest_ids(dir: &Path) -> Vec<String> {
    let Ok(entries) = dir.read_dir() else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let named = path.extension() == Some(OsStr::new(MANIFEST_EXTENSION));
            let stem = path.file_stem()?.to_str()?;
            // The name has to survive the trip back to a path, because that is
            // the trip a lookup makes.
            (named && manifest_file_name(stem).is_some()).then(|| stem.to_owned())
        })
        .collect()
}

/// Reads one tier's copy of a manifest, or says in a warning why it did not.
fn read_source<F: Family>(
    tier: Tier,
    path: &Path,
    requested: &str,
    warnings: &mut Vec<String>,
) -> Option<F::Manifest> {
    let mut refuse = |reason: String| {
        warnings.push(format!("{} {}: {reason}", tier.label(), path.display()));
        None
    };
    let content = match read_bounded(path) {
        Ok(Some(content)) => content,
        Ok(None) => return None,
        Err(reason) => return refuse(reason),
    };
    let manifest = match F::parse(&content) {
        Ok(manifest) => manifest,
        Err(message) => return refuse(message),
    };
    if !covers::<F>(&manifest, requested) {
        return refuse(format!(
            "describes {:?}, not {requested:?}; ignored",
            F::id(&manifest),
        ));
    }
    Some(manifest)
}

/// Whether a manifest answers to the id it was looked up under.
///
/// A file named after one agent holding another agent's manifest is a mistake
/// worth refusing rather than honouring: whichever of the two names is wrong,
/// reading a screen with it would attribute one agent's UI to another.
pub(crate) fn covers<F: Family>(manifest: &F::Manifest, requested: &str) -> bool {
    same(F::id(manifest), requested)
        || F::aliases(manifest)
            .iter()
            .any(|alias| same(alias, requested))
}

/// Reads a file that must be small, reporting an absent one as no content
/// rather than as a failure.
pub(crate) fn read_bounded(path: &Path) -> Result<Option<String>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot be opened: {error}")),
    };
    if let Ok(metadata) = file.metadata()
        && metadata.len() > MAX_MANIFEST_BYTES
    {
        return Err(format!(
            "is {} bytes, over the {MAX_MANIFEST_BYTES}-byte limit; ignored",
            metadata.len(),
        ));
    }
    let mut content = String::new();
    // Bounded by the same cap a second time: what the metadata said and what
    // the file turns out to hold are two different questions when something
    // else is writing it.
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_string(&mut content)
        .map_err(|error| format!("cannot be read: {error}"))?;
    if content.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(format!(
            "is over the {MAX_MANIFEST_BYTES}-byte limit; ignored"
        ));
    }
    Ok(Some(content))
}

/// Every family's manifests on one machine.
///
/// Cheap to hold and safe to share: opening one touches no disk at all, and a
/// manifest is read the first time something asks about the agent it describes.
/// A long-lived consumer keeps one for its lifetime and calls
/// [`reload`](Self::reload) when the files on disk have changed; a one-shot
/// process opens one, asks its question and exits.
pub struct ManifestStore {
    screen: FamilyStore<Screen>,
    hooks: FamilyStore<Hooks>,
}

impl ManifestStore {
    /// A store over the manifests at `paths`.
    pub fn open(paths: StorePaths) -> Self {
        Self {
            screen: FamilyStore::new(paths.clone()),
            hooks: FamilyStore::new(paths),
        }
    }

    /// A store over the manifests this process's environment points at.
    pub fn from_env() -> Self {
        Self::open(StorePaths::from_env())
    }

    /// Where this store looks.
    pub fn paths(&self) -> &StorePaths {
        &self.screen.paths
    }

    /// Forgets every manifest read so far.
    ///
    /// The compiled form is kept for as long as it is useful, which is until
    /// the files behind it change — after an update, or after someone edits
    /// their own copy. Nothing is read here: the next question rereads what it
    /// needs and nothing else.
    pub fn reload(&self) {
        self.screen.reload();
        self.hooks.reload();
    }

    /// Which copy of each manifest answers, and what was passed over.
    ///
    /// This reports the manifests in force rather than the files on disk this
    /// instant: an agent already asked about is reported as it was decided,
    /// which is what a caller checking why detection behaves as it does needs
    /// to see. An agent nothing has asked about yet is decided here and now,
    /// there being no earlier decision to contradict.
    ///
    /// Every family is reported, one family after another, because "which copy
    /// is active" is asked of a machine rather than of a family: an agent whose
    /// screen rules come from an override and whose hook mapping comes from the
    /// bundled tier is two lines, and both of them are the answer.
    pub fn summaries(&self) -> Vec<ManifestSummary> {
        let mut summaries = self.screen.summaries();
        summaries.append(&mut self.hooks.summaries());
        summaries
    }

    /// What one screen says `agent` is doing.
    ///
    /// An agent no copy of any manifest describes is
    /// [`Detection::unknown_agent`]: there were no rules, so there is no
    /// verdict to give.
    pub fn detect(&self, agent: &str, input: ScreenInput<'_>) -> Detection {
        match &self.screen.entry(agent).active {
            Some(active) => active.compiled.detect(input),
            None => Detection::unknown_agent(),
        }
    }

    /// Why that screen reads the way it does, rule by rule.
    ///
    /// This is where the store's own findings meet the manifest's: the
    /// explanation names the copy that answered, and carries what was skipped
    /// to reach it ahead of whatever the manifest itself warned about.
    pub fn explain(&self, agent: &str, input: ScreenInput<'_>) -> Explain {
        let entry = self.screen.entry(agent);
        let mut explanation = match &entry.active {
            Some(active) => {
                crate::explain::explain(&active.compiled, input).with_source(active.source.clone())
            }
            None => Explain::unknown_agent(),
        };
        let mut warnings = entry.warnings.clone();
        warnings.append(&mut explanation.warnings);
        explanation.warnings = warnings;
        explanation
    }

    /// What one hook payload from `agent` means.
    ///
    /// `None` is every way a payload can produce nothing, and a caller treats
    /// them alike — send nothing, say nothing: no copy of any mapping describes
    /// this agent, the payload names an event the mapping does not map, the
    /// entry's condition does not hold, or the payload carries no session.
    ///
    /// What comes back is the event as the *mapping* sees it. The things only
    /// the caller knows — where the hook was run, what it should be correlated
    /// with — are the caller's to add.
    pub fn normalize_hook(
        &self,
        agent: &str,
        payload: &serde_json::Value,
    ) -> Option<UnstampedEvent> {
        let entry = self.hooks.entry(agent);
        let active = entry.active.as_ref()?;
        active.compiled.normalize(payload)
    }

    /// What was passed over on the way to the mapping that answers for
    /// `agent`.
    ///
    /// A screen verdict carries these inside its explanation; a normalized
    /// event has nowhere to put them, and they are worth just as much — the
    /// file a warning names is nearly always the one whose author is wondering
    /// why nothing changed. Empty is the ordinary case, and a caller with
    /// nowhere to show a sentence is free never to ask.
    pub fn hook_warnings(&self, agent: &str) -> Vec<String> {
        self.hooks.entry(agent).warnings.clone()
    }

    /// Whose manifest a screen should be read with, over the active copies.
    ///
    /// Identification is a question about the manifests in force on this
    /// machine, not about the ones that shipped: an override that teaches an
    /// agent a new executable name is exactly the sort of fix this tier exists
    /// for, and it would be no fix at all if the name it added were invisible
    /// here.
    pub fn identify(&self, hint: Option<&str>, process: Option<ProcessInfo<'_>>) -> Option<Agent> {
        let entries: Vec<Arc<Entry<Screen>>> = self
            .screen
            .ids()
            .iter()
            .map(|id| self.screen.entry(id))
            .collect();
        let manifests: Vec<&ScreenManifest> = entries
            .iter()
            .filter_map(|entry| entry.active.as_ref())
            .map(|active| active.compiled.manifest())
            .collect();
        crate::identify::identify(manifests, hint, process)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use tempfile::TempDir;

    use crate::screen::rules::KNOWN_AGENT_IDLE_FALLBACK;

    /// An agent the bundled corpus describes, and the version it describes it
    /// at. Pinned here rather than read back out of the corpus: these tests are
    /// about what happens around a bundled manifest, and a test that derived
    /// the version from the same place the code does could not tell whether the
    /// comparison ran at all.
    const BUNDLED_AGENT: &str = "claude";
    const BUNDLED_VERSION: &str = "2026.08.19.1";

    /// A screen that no bundled manifest recognizes, so that a verdict on it
    /// can only have come from a manifest one of these tests wrote.
    const SCREEN: &str = "a screen with the marker on it\n";

    /// How many agents the bundled corpus covers, screen manifests and hook
    /// mappings counted separately: the two families describe overlapping sets
    /// of agents and a summary is per family, not per agent.
    const BUNDLED_SCREEN_COUNT: usize = 20;
    const BUNDLED_HOOK_COUNT: usize = 3;
    const BUNDLED_COUNT: usize = BUNDLED_SCREEN_COUNT + BUNDLED_HOOK_COUNT;

    /// An agent the bundled corpus describes the screen of and says nothing
    /// about the hooks of, for the tests about one family answering for the
    /// other.
    const SCREEN_ONLY_AGENT: &str = "gemini";

    /// A manifest whose one rule fires on [`SCREEN`] and names itself.
    fn marker_manifest(id: &str, version: Option<&str>, rule: &str) -> String {
        let version = match version {
            Some(version) => format!("version = \"{version}\"\n"),
            None => String::new(),
        };
        format!(
            r#"
id = "{id}"
{version}
[identify]
names = ["{id}"]

[[rules]]
id = "{rule}"
state = "blocked"
priority = 500
visible_blocker = true
contains = ["the marker"]
"#
        )
    }

    /// A machine with nothing on it.
    fn machine() -> (TempDir, ManifestStore) {
        let home = TempDir::new().expect("a temporary directory");
        let store = ManifestStore::open(StorePaths::rooted(home.path()));
        (home, store)
    }

    /// Puts one file in one tier.
    fn place(store: &ManifestStore, tier: Tier, id: &str, content: &str) {
        place_in(store, tier, Screen::NAME, id, content);
    }

    /// Puts one file in one tier of one family.
    fn place_in(store: &ManifestStore, tier: Tier, family: &str, id: &str, content: &str) {
        let path = store
            .paths()
            .file(tier, family, id)
            .expect("a path for the id");
        fs::create_dir_all(path.parent().expect("a parent")).expect("the directory is created");
        fs::write(&path, content).expect("the manifest is written");
    }

    /// The rule that answered for `id`, or nothing when none did.
    fn answered_by(store: &ManifestStore, id: &str) -> Option<String> {
        store
            .detect(id, ScreenInput::from_screen(SCREEN))
            .matched_rule
    }

    /// What the store says about one agent.
    fn summary(store: &ManifestStore, id: &str) -> ManifestSummary {
        store
            .summaries()
            .into_iter()
            .find(|summary| summary.id == id)
            .unwrap_or_else(|| panic!("no summary for {id:?}"))
    }

    /// Whether any warning mentions each of `needles`.
    fn warned_about(warnings: &[String], needles: &[&str]) -> bool {
        warnings
            .iter()
            .any(|warning| needles.iter().all(|needle| warning.contains(needle)))
    }

    #[test]
    fn a_store_can_be_shared_between_threads() {
        // The lock exists for consumers that read screens from more than one
        // thread; that this compiles is the whole assertion.
        fn assert_shareable<T: Send + Sync>() {}
        assert_shareable::<ManifestStore>();
    }

    #[test]
    fn a_machine_with_no_files_reads_the_bundled_copy() {
        let (_home, store) = machine();
        let summary = summary(&store, BUNDLED_AGENT);
        assert_eq!(summary.source, Some(ManifestSource::Bundled));
        assert_eq!(
            summary.bundled_version.as_ref().map(ToString::to_string),
            Some(BUNDLED_VERSION.to_owned()),
        );
        assert_eq!(summary.remote_version, None);
        assert_eq!(summary.override_version, None);
        assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);
        assert_eq!(answered_by(&store, BUNDLED_AGENT), None);
    }

    #[test]
    fn an_override_beats_a_newer_remote_copy() {
        let (_home, store) = machine();
        place(
            &store,
            Tier::Remote,
            BUNDLED_AGENT,
            &marker_manifest(BUNDLED_AGENT, Some("2999.01.01.1"), "from-the-remote"),
        );
        place(
            &store,
            Tier::Override,
            BUNDLED_AGENT,
            &marker_manifest(BUNDLED_AGENT, None, "from-the-override"),
        );

        assert_eq!(
            answered_by(&store, BUNDLED_AGENT).as_deref(),
            Some("from-the-override"),
        );
        let summary = summary(&store, BUNDLED_AGENT);
        assert!(
            matches!(summary.source, Some(ManifestSource::Override { .. })),
            "{:?}",
            summary.source,
        );
        assert!(
            warned_about(&summary.warnings, &["shadowed by the override"]),
            "{:?}",
            summary.warnings,
        );
    }

    #[test]
    fn a_corrupt_override_falls_through_to_the_remote_copy() {
        let (_home, store) = machine();
        place(
            &store,
            Tier::Remote,
            BUNDLED_AGENT,
            &marker_manifest(BUNDLED_AGENT, Some("2999.01.01.1"), "from-the-remote"),
        );
        place(
            &store,
            Tier::Override,
            BUNDLED_AGENT,
            "id = \"claude\"\nrules = 3\n",
        );

        assert_eq!(
            answered_by(&store, BUNDLED_AGENT).as_deref(),
            Some("from-the-remote"),
        );
        let summary = summary(&store, BUNDLED_AGENT);
        assert!(
            matches!(summary.source, Some(ManifestSource::Remote { .. })),
            "{:?}",
            summary.source,
        );
        assert!(
            warned_about(&summary.warnings, &["override", "claude.toml"]),
            "{:?}",
            summary.warnings,
        );
        assert_eq!(summary.override_version, None);
    }

    #[test]
    fn an_override_describing_another_agent_is_refused() {
        let (_home, store) = machine();
        place(
            &store,
            Tier::Override,
            BUNDLED_AGENT,
            &marker_manifest("codex", None, "from-the-wrong-agent"),
        );

        assert_eq!(answered_by(&store, BUNDLED_AGENT), None);
        let summary = summary(&store, BUNDLED_AGENT);
        assert_eq!(summary.source, Some(ManifestSource::Bundled));
        assert!(
            warned_about(&summary.warnings, &["describes", "codex"]),
            "{:?}",
            summary.warnings,
        );
    }

    #[test]
    fn an_override_may_be_filed_under_an_alias_the_manifest_declares() {
        let (_home, store) = machine();
        let manifest = format!(
            "aliases = [\"claude-code\"]\n{}",
            marker_manifest(BUNDLED_AGENT, None, "from-the-override"),
        );
        place(&store, Tier::Override, "claude-code", &manifest);

        assert_eq!(
            answered_by(&store, "claude-code").as_deref(),
            Some("from-the-override"),
        );
    }

    #[test]
    fn a_remote_copy_older_than_the_bundled_one_loses_to_it() {
        let (_home, store) = machine();
        place(
            &store,
            Tier::Remote,
            BUNDLED_AGENT,
            &marker_manifest(BUNDLED_AGENT, Some("2020.01.01.1"), "from-the-remote"),
        );

        assert_eq!(answered_by(&store, BUNDLED_AGENT), None);
        let summary = summary(&store, BUNDLED_AGENT);
        assert_eq!(summary.source, Some(ManifestSource::Bundled));
        assert_eq!(
            summary.remote_version.as_ref().map(ToString::to_string),
            Some("2020.01.01.1".to_owned()),
        );
        assert!(
            warned_about(&summary.warnings, &["older than the bundled"]),
            "{:?}",
            summary.warnings,
        );
    }

    #[test]
    fn a_remote_copy_no_older_than_the_bundled_one_wins() {
        for version in [BUNDLED_VERSION, "2999.01.01.1"] {
            let (_home, store) = machine();
            place(
                &store,
                Tier::Remote,
                BUNDLED_AGENT,
                &marker_manifest(BUNDLED_AGENT, Some(version), "from-the-remote"),
            );

            assert_eq!(
                answered_by(&store, BUNDLED_AGENT).as_deref(),
                Some("from-the-remote"),
                "a remote copy at {version} should have answered",
            );
            let summary = summary(&store, BUNDLED_AGENT);
            assert_eq!(
                summary.source,
                Some(ManifestSource::Remote {
                    version: version.to_owned(),
                }),
            );
            assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);
        }
    }

    #[test]
    fn a_remote_copy_that_declares_no_version_cannot_displace_a_bundled_one() {
        let (_home, store) = machine();
        place(
            &store,
            Tier::Remote,
            BUNDLED_AGENT,
            &marker_manifest(BUNDLED_AGENT, None, "from-the-remote"),
        );

        assert_eq!(answered_by(&store, BUNDLED_AGENT), None);
        assert!(
            warned_about(
                &summary(&store, BUNDLED_AGENT).warnings,
                &["no version", "older than the bundled"],
            ),
            "{:?}",
            summary(&store, BUNDLED_AGENT).warnings,
        );
    }

    #[test]
    fn a_file_over_the_read_limit_is_skipped() {
        let (_home, store) = machine();
        let padding = "# ".to_owned() + &"x".repeat(MAX_MANIFEST_BYTES as usize) + "\n";
        place(
            &store,
            Tier::Override,
            BUNDLED_AGENT,
            &(padding + &marker_manifest(BUNDLED_AGENT, None, "from-the-override")),
        );

        assert_eq!(answered_by(&store, BUNDLED_AGENT), None);
        let summary = summary(&store, BUNDLED_AGENT);
        assert_eq!(summary.source, Some(ManifestSource::Bundled));
        assert!(
            warned_about(&summary.warnings, &["over the", "limit"]),
            "{:?}",
            summary.warnings,
        );
    }

    #[test]
    fn an_edited_override_is_read_again_only_after_a_reload() {
        let (_home, store) = machine();
        place(
            &store,
            Tier::Override,
            BUNDLED_AGENT,
            &marker_manifest(BUNDLED_AGENT, Some("1"), "as-first-written"),
        );
        assert_eq!(
            answered_by(&store, BUNDLED_AGENT).as_deref(),
            Some("as-first-written"),
        );

        place(
            &store,
            Tier::Override,
            BUNDLED_AGENT,
            &marker_manifest(BUNDLED_AGENT, Some("2"), "as-edited"),
        );
        assert_eq!(
            answered_by(&store, BUNDLED_AGENT).as_deref(),
            Some("as-first-written"),
            "the compiled copy already read should have been served",
        );
        assert_eq!(
            summary(&store, BUNDLED_AGENT)
                .override_version
                .map(|version| version.to_string()),
            Some("1".to_owned()),
        );

        store.reload();
        assert_eq!(
            answered_by(&store, BUNDLED_AGENT).as_deref(),
            Some("as-edited"),
        );
        assert_eq!(
            summary(&store, BUNDLED_AGENT)
                .override_version
                .map(|version| version.to_string()),
            Some("2".to_owned()),
        );
    }

    #[test]
    fn every_bundled_agent_is_summarized() {
        let (_home, store) = machine();
        let summaries = store.summaries();
        assert_eq!(summaries.len(), BUNDLED_COUNT);
        assert!(
            summaries
                .iter()
                .all(|summary| summary.source == Some(ManifestSource::Bundled)),
        );

        for (family, count) in [
            (Screen::NAME, BUNDLED_SCREEN_COUNT),
            (Hooks::NAME, BUNDLED_HOOK_COUNT),
        ] {
            let of_family: Vec<&ManifestSummary> = summaries
                .iter()
                .filter(|summary| summary.family == family)
                .collect();
            assert_eq!(of_family.len(), count, "{family}");
            assert!(
                of_family.windows(2).all(|pair| pair[0].id < pair[1].id),
                "{family} summaries should read in id order",
            );
        }
    }

    #[test]
    fn an_agent_only_the_disk_knows_about_is_summarized_too() {
        let (_home, store) = machine();
        place(
            &store,
            Tier::Override,
            "an-agent-of-ones-own",
            &marker_manifest("an-agent-of-ones-own", None, "from-the-override"),
        );

        let summaries = store.summaries();
        assert_eq!(summaries.len(), BUNDLED_COUNT + 1);
        let summary = summary(&store, "an-agent-of-ones-own");
        assert!(matches!(
            summary.source,
            Some(ManifestSource::Override { .. }),
        ));
        assert_eq!(summary.bundled_version, None);
    }

    #[test]
    fn an_agent_nothing_describes_has_no_verdict() {
        let (_home, store) = machine();
        let detection = store.detect("nobody", ScreenInput::from_screen(SCREEN));
        assert_eq!(detection, Detection::unknown_agent());

        let explanation = store.explain("nobody", ScreenInput::from_screen(SCREEN));
        assert_eq!(explanation.agent, None);
        assert_eq!(explanation.source, None);
    }

    #[test]
    fn an_id_is_looked_up_however_it_is_capitalized() {
        let (_home, store) = machine();
        let explanation = store.explain("CLAUDE", ScreenInput::from_screen(SCREEN));
        assert_eq!(explanation.agent.as_deref(), Some(BUNDLED_AGENT));
        assert_eq!(explanation.source, Some(ManifestSource::Bundled));
    }

    #[test]
    fn an_id_that_is_not_a_file_name_reaches_no_file() {
        let (home, store) = machine();
        let elsewhere = home.path().join("elsewhere.toml");
        fs::write(
            &elsewhere,
            marker_manifest("elsewhere", None, "from-outside-the-directory"),
        )
        .expect("the manifest is written");

        for id in ["../../elsewhere", "/etc/passwd", ".", ".."] {
            assert_eq!(store.paths().file(Tier::Override, Screen::NAME, id), None);
            assert_eq!(
                store.detect(id, ScreenInput::from_screen(SCREEN)),
                Detection::unknown_agent(),
                "{id:?} should have reached no manifest at all",
            );
        }
    }

    #[test]
    fn a_root_that_is_not_absolute_is_never_read_from() {
        let paths = StorePaths {
            overrides: PathBuf::from("relative/config"),
            state: PathBuf::from("relative/state"),
        };
        for tier in [Tier::Override, Tier::Remote] {
            assert_eq!(paths.file(tier, Screen::NAME, BUNDLED_AGENT), None);
        }
    }

    #[test]
    fn a_base_directory_is_honoured_only_when_it_is_absolute() {
        let home = Path::new("/home/someone");
        assert_eq!(
            base(Some(home.as_os_str()), Some("/elsewhere".into()), ".config"),
            Path::new("/elsewhere/agentbus/manifests"),
        );
        assert_eq!(
            base(Some(home.as_os_str()), Some("relative".into()), ".config"),
            Path::new("/home/someone/.config/agentbus/manifests"),
        );
        assert_eq!(
            base(Some(home.as_os_str()), None, ".local/state"),
            Path::new("/home/someone/.local/state/agentbus/manifests"),
        );
        assert!(!base(None, None, ".config").is_absolute());
    }

    #[test]
    fn an_explanation_names_the_copy_that_answered_and_what_was_skipped() {
        let (_home, store) = machine();
        place(
            &store,
            Tier::Override,
            BUNDLED_AGENT,
            "not a manifest at all",
        );
        place(
            &store,
            Tier::Remote,
            BUNDLED_AGENT,
            &marker_manifest(BUNDLED_AGENT, Some("2999.01.01.1"), "from-the-remote"),
        );

        let explanation = store.explain(BUNDLED_AGENT, ScreenInput::from_screen(SCREEN));
        assert_eq!(
            explanation.source,
            Some(ManifestSource::Remote {
                version: "2999.01.01.1".to_owned(),
            }),
        );
        assert_eq!(
            explanation.matched_rule.map(|rule| rule.id),
            Some("from-the-remote".to_owned()),
        );
        assert!(
            warned_about(&explanation.warnings, &["override"]),
            "{:?}",
            explanation.warnings,
        );
    }

    #[test]
    fn an_unrecognized_screen_still_reads_as_calm() {
        let (_home, store) = machine();
        let detection = store.detect(BUNDLED_AGENT, ScreenInput::from_screen(SCREEN));
        assert_eq!(detection.fallback, Some(KNOWN_AGENT_IDLE_FALLBACK));
    }

    #[test]
    fn identification_uses_the_copy_that_is_in_force() {
        let (_home, store) = machine();
        let process = ProcessInfo {
            comm: "claude-next",
            cmdline: "claude-next --resume",
        };
        assert_eq!(store.identify(None, Some(process)), None);

        let manifest = marker_manifest(BUNDLED_AGENT, None, "from-the-override")
            .replace("names = [\"claude\"]", "names = [\"claude-next\"]");
        place(&store, Tier::Override, BUNDLED_AGENT, &manifest);
        store.reload();

        assert_eq!(
            store
                .identify(None, Some(process))
                .map(|agent| agent.as_str().to_owned()),
            Some(BUNDLED_AGENT.to_owned()),
        );
    }

    /// A hook mapping whose one event is enough to tell which copy answered.
    fn hook_manifest(id: &str, aliases: &[&str], kind: &str) -> String {
        let aliases = aliases
            .iter()
            .map(|alias| format!("{alias:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"
id = "{id}"
aliases = [{aliases}]

[payload]
event = "hook_event_name"
session = ["session_id"]
cwd = ["cwd"]

[[events]]
name = "Stop"
kind = "{kind}"
"#
        )
    }

    /// The payload the mappings above answer to.
    fn stop_payload() -> serde_json::Value {
        serde_json::json!({"hook_event_name": "Stop", "session_id": "s1", "cwd": "/work"})
    }

    #[test]
    fn a_payload_is_normalized_by_the_mapping_in_force() {
        let (_home, store) = machine();
        place_in(
            &store,
            Tier::Override,
            Hooks::NAME,
            "agent",
            &hook_manifest("agent", &[], "turn_end"),
        );

        let event = store
            .normalize_hook("agent", &stop_payload())
            .expect("an event");
        assert_eq!(event.agent.as_str(), "agent");
        assert_eq!(event.session, "s1");
        assert_eq!(event.kind, agentbus_protocol::Kind::TurnEnd);
        assert_eq!(event.cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn a_mapping_answers_to_an_alias_it_declares() {
        let (_home, store) = machine();
        // Filed under the alias, declaring the id: the caller knows the agent
        // by the name it typed, and the events carry the name the mapping owns.
        place_in(
            &store,
            Tier::Override,
            Hooks::NAME,
            "nickname",
            &hook_manifest("agent", &["nickname"], "turn_end"),
        );

        let event = store
            .normalize_hook("nickname", &stop_payload())
            .expect("an event");
        assert_eq!(event.agent.as_str(), "agent");
    }

    #[test]
    fn an_agent_no_mapping_describes_normalizes_nothing() {
        let (_home, store) = machine();
        assert!(store.normalize_hook("nobody", &stop_payload()).is_none());
        // The bundled corpus describes this agent's screen and says nothing
        // about its hooks; one family is never an answer for the other.
        assert!(
            store
                .normalize_hook(SCREEN_ONLY_AGENT, &stop_payload())
                .is_none()
        );
    }

    #[test]
    fn a_mapping_that_could_not_be_used_is_reported_to_whoever_asks() {
        let (_home, store) = machine();
        place_in(
            &store,
            Tier::Override,
            Hooks::NAME,
            BUNDLED_AGENT,
            "this is not a mapping",
        );

        // The mapping inside the binary answers, so nothing about the payload
        // changes; the sentence is the only trace the skipped file leaves.
        assert!(
            store
                .normalize_hook(BUNDLED_AGENT, &stop_payload())
                .is_some()
        );
        let warnings = store.hook_warnings(BUNDLED_AGENT);
        assert!(
            warned_about(&warnings, &["override", "hooks"]),
            "{warnings:?}",
        );
    }

    #[test]
    fn a_mapping_nothing_was_wrong_with_is_reported_as_nothing() {
        let (_home, store) = machine();
        assert!(store.hook_warnings(BUNDLED_AGENT).is_empty());
    }

    #[test]
    fn each_family_is_read_from_its_own_directory() {
        let (_home, store) = machine();
        // A mapping filed under the screen family is not a screen manifest, and
        // is no use to the family that would look for it there either.
        place_in(
            &store,
            Tier::Override,
            Screen::NAME,
            "agent",
            &hook_manifest("agent", &[], "turn_end"),
        );

        assert!(store.normalize_hook("agent", &stop_payload()).is_none());
        assert!(warned_about(
            &summary(&store, "agent").warnings,
            &["override", "not readable"],
        ));
    }

    #[test]
    fn a_mapping_on_disk_is_summarized_under_its_own_family() {
        let (_home, store) = machine();
        place_in(
            &store,
            Tier::Override,
            Hooks::NAME,
            "agent",
            &hook_manifest("agent", &[], "turn_end"),
        );

        let summaries = store.summaries();
        assert_eq!(summaries.len(), BUNDLED_COUNT + 1);
        let mapping = summaries
            .iter()
            .find(|summary| summary.family == Hooks::NAME && summary.id == "agent")
            .expect("a summary for the mapping");
        assert!(matches!(
            mapping.source,
            Some(ManifestSource::Override { .. })
        ));
    }

    #[test]
    fn an_edited_mapping_is_read_again_only_after_a_reload() {
        let (_home, store) = machine();
        place_in(
            &store,
            Tier::Override,
            Hooks::NAME,
            "agent",
            &hook_manifest("agent", &[], "turn_end"),
        );
        let kind_of = |store: &ManifestStore| {
            store
                .normalize_hook("agent", &stop_payload())
                .expect("an event")
                .kind
        };
        assert_eq!(kind_of(&store), agentbus_protocol::Kind::TurnEnd);

        place_in(
            &store,
            Tier::Override,
            Hooks::NAME,
            "agent",
            &hook_manifest("agent", &[], "session_end"),
        );
        assert_eq!(
            kind_of(&store),
            agentbus_protocol::Kind::TurnEnd,
            "the decision already made should stand until it is discarded",
        );

        store.reload();
        assert_eq!(kind_of(&store), agentbus_protocol::Kind::SessionEnd);
    }
}
