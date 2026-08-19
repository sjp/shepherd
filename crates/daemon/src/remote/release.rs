//! Getting the binary a machine needs when this one does not contain it.
//!
//! Provisioning normally sends the executable that is already running, which
//! costs nothing and works with no network at all. It only works when the far
//! end is the same kind of machine, though: an arm64 Mac does not have an
//! x86_64 Linux binary inside it, and no amount of asking will produce one. So
//! there is a second way to get one — the release this build came from, which
//! publishes a binary per target triple and a manifest describing them.
//!
//! # What is trusted
//!
//! The manifest says how long each asset is and what it hashes to, and nothing
//! is put anywhere until the bytes on disk match both. That is the whole of the
//! integrity story here: it catches a truncated download, a proxy that returned
//! an error page, and a mirror that is out of date, which are the ways this
//! actually goes wrong. It is not a signature and does not pretend to be one —
//! whoever can rewrite the manifest can rewrite the hashes in it — and the far
//! end still refuses to execute anything that does not answer to the expected
//! version.
//!
//! # Where it fetches from
//!
//! One base, holding the manifest and every asset beside it: `<base>/manifest.
//! json` and `<base>/<asset>`. The default base is where this version's own
//! release was published, and [`BASE_VAR`] replaces it with any other http(s)
//! location or a `file://` directory. That is what makes a mirror a copy of a
//! directory rather than a service: the manifest's own `url` fields still name
//! wherever its publisher put the assets, and what is actually read is the copy
//! beside the manifest that was just read.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::{debug, info};

/// The repository releases of this program are published from.
///
/// The one place this is written down. Everything else — the default base, the
/// manifest's location, an asset's location — is derived from it and the
/// version, so moving the releases somewhere else is this line and a release
/// that puts the assets there. Anyone who has not moved them can also point a
/// single run somewhere else with [`BASE_VAR`], which is the supported way to
/// use a mirror without rebuilding.
pub const REPOSITORY: &str = "sjp/agentbus";

/// The environment variable that says where releases are published, in place of
/// the location [`REPOSITORY`] implies.
///
/// Any http(s) base, or a `file://` directory, which is what an air-gapped
/// mirror is: the manifest and the assets copied into a directory.
pub const BASE_VAR: &str = "AGENTBUS_RELEASE_BASE";

/// The name of the file describing a release, relative to its base.
pub const MANIFEST: &str = "manifest.json";

/// The manifest schema this understands.
///
/// A manifest that says anything else is refused rather than read leniently: it
/// was written by a publisher that knows something this build does not, and
/// guessing at it would mean guessing at what a binary is.
pub const SCHEMA: u32 = 1;

/// The environment variable holding the user's cache directory.
const CACHE_VAR: &str = "XDG_CACHE_HOME";

/// The environment variable holding the user's home directory.
const HOME_VAR: &str = "HOME";

/// The cache directory used when there is no `XDG_CACHE_HOME`.
const CACHE_FALLBACK: &str = ".cache";

/// This program's own directory inside the cache.
const CACHE_DIR: &str = "agentbus";

/// The suffix a download carries until it has been verified.
const PARTIAL: &str = ".part";

/// The most manifest that will be read, in bytes.
const MANIFEST_LIMIT: u64 = 1 << 20;

/// The most asset that will be read, in bytes.
///
/// Generous next to a release binary of a few megabytes, and finite, so that a
/// base which answers every request with an endless stream fails instead of
/// filling the disk.
const ASSET_LIMIT: u64 = 1 << 28;

/// How much is moved at a time when copying and when hashing.
const CHUNK: usize = 64 * 1024;

/// What a release says about itself.
///
/// Deserialized permissively on purpose: a field this build has never heard of
/// is one a later publisher added, and ignoring it is what lets an old copy of
/// this program keep fetching from a newer release. What is *not* permissive is
/// [`Manifest::v`], which is checked exactly.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// The schema this manifest is written in. Always [`SCHEMA`] for one this
    /// build can read.
    pub v: u32,
    /// The program the assets are of.
    pub name: String,
    /// The version they are of.
    pub version: String,
    /// One entry per kind of machine a binary was built for.
    pub assets: Vec<Asset>,
}

impl Manifest {
    /// The asset built for `triple`, if the release has one.
    pub fn asset(&self, triple: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.triple == triple)
    }

    /// Every triple this release was built for, as a person would want them
    /// listed.
    pub fn triples(&self) -> String {
        match self.assets.is_empty() {
            true => "nothing".to_owned(),
            false => self
                .assets
                .iter()
                .map(|asset| asset.triple.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

/// One built binary in a release.
#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    /// The target triple it was built for.
    pub triple: String,
    /// Where its publisher put it.
    pub url: String,
    /// What its bytes hash to, as 64 lowercase hex digits.
    pub sha256: String,
    /// How many bytes it is.
    pub size: u64,
}

impl Asset {
    /// What the asset is called, which is the last segment of the place its
    /// publisher put it.
    ///
    /// Nothing for a `url` whose last segment could not be a file name, which
    /// includes the empty string and the two names that mean a directory: this
    /// is joined onto a base and onto a cache directory, and neither may be
    /// talked into meaning somewhere else.
    pub fn name(&self) -> Option<&str> {
        let name = self.url.rsplit('/').next().unwrap_or_default();
        let name = name.split(['?', '#']).next().unwrap_or_default();
        match name {
            "" | "." | ".." => None,
            name => Some(name),
        }
    }
}

/// Why a binary could not be got from a release.
#[derive(Debug, Error)]
pub enum Error {
    /// The manifest could not be read from where the release is published.
    #[error("could not fetch the manifest from {url}")]
    Unreachable {
        /// Where it was looked for.
        url: String,
        /// What went wrong.
        #[source]
        source: Transfer,
    },
    /// The manifest was fetched and is not a manifest.
    #[error("the manifest at {url} cannot be read")]
    Malformed {
        /// Where it came from.
        url: String,
        /// What was wrong with it.
        #[source]
        source: serde_json::Error,
    },
    /// The manifest is written in a schema this build does not know.
    #[error("the manifest at {url} is schema {found}; this understands {SCHEMA}")]
    Schema {
        /// Where it came from.
        url: String,
        /// The schema it declared.
        found: u32,
    },
    /// The release is of a different version than this build.
    #[error("the release at {url} is agentbus {found}; this is agentbus {wanted}")]
    Version {
        /// Where it came from.
        url: String,
        /// The version that was wanted.
        wanted: String,
        /// The version that is published there.
        found: String,
    },
    /// The release has no binary for the machine that needs one.
    #[error("the release at {url} has no binary for {triple}; it has {available}")]
    Missing {
        /// Where it came from.
        url: String,
        /// The triple that was wanted.
        triple: String,
        /// The triples it does have.
        available: String,
    },
    /// The manifest names an asset in a way that cannot be turned into a file.
    #[error("the release at {url} names its {triple} binary {named:?}, which is not a file name")]
    Unnamed {
        /// Where it came from.
        url: String,
        /// The triple whose entry is unusable.
        triple: String,
        /// What the entry said.
        named: String,
    },
    /// The asset itself could not be read.
    #[error("could not fetch {url}")]
    Unfetchable {
        /// Where it was looked for.
        url: String,
        /// What went wrong.
        #[source]
        source: Transfer,
    },
    /// What arrived is not what the manifest described.
    #[error(
        "{url} is {found_size} bytes hashing to {found}, and the manifest says \
         {size} bytes hashing to {sha256}"
    )]
    Corrupt {
        /// Where it came from.
        url: String,
        /// How long it should have been.
        size: u64,
        /// What it should have hashed to.
        sha256: String,
        /// How long it was.
        found_size: u64,
        /// What it hashed to.
        found: String,
    },
    /// The cache could not be written, read or cleaned.
    #[error("cannot use the download cache at {}", path.display())]
    Cache {
        /// The file or directory that could not be used.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
}

/// Why something could not be got from where it was said to be.
///
/// One error for the two ways of reading a location, so that everything above
/// this is written once and works the same against a release on the web and a
/// release in a directory.
#[derive(Debug, Error)]
pub enum Transfer {
    /// The request failed, or the server refused it.
    #[error(transparent)]
    Http(#[from] Box<ureq::Error>),
    /// The file could not be read.
    #[error(transparent)]
    File(#[from] io::Error),
    /// The location is not one this knows how to read.
    #[error("{url} is not an http, https or file location")]
    Scheme {
        /// The location that was asked for.
        url: String,
    },
}

impl From<ureq::Error> for Transfer {
    fn from(error: ureq::Error) -> Self {
        Self::Http(Box::new(error))
    }
}

/// A published release of this program, and the cache of what has been taken
/// from it.
///
/// Holding one is holding a place to fetch from and a version to insist on; it
/// makes no request until something asks it for a binary.
#[derive(Debug, Clone)]
pub struct Release {
    version: String,
    base: String,
    cache: PathBuf,
}

impl Release {
    /// The release `version` was published in, as the environment says or as
    /// [`REPOSITORY`] implies.
    pub fn published(version: impl Into<String>) -> Self {
        let version = version.into();
        let base = std::env::var(BASE_VAR)
            .ok()
            .map(|base| base.trim().to_owned())
            .filter(|base| !base.is_empty())
            .unwrap_or_else(|| default_base(&version));
        Self::at(base, version)
    }

    /// A release of `version` published at `base`.
    pub fn at(base: impl Into<String>, version: impl Into<String>) -> Self {
        let version = version.into();
        Self {
            cache: cache_root().join(CACHE_DIR).join(&version),
            base: base.into().trim_end_matches('/').to_owned(),
            version,
        }
    }

    /// The same release, cached under `root` instead of under the user's cache
    /// directory.
    pub fn caching_in(mut self, root: impl AsRef<Path>) -> Self {
        self.cache = root.as_ref().join(CACHE_DIR).join(&self.version);
        self
    }

    /// Where this release is published.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The version it is a release of.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Where the manifest is.
    pub fn manifest_url(&self) -> String {
        format!("{}/{MANIFEST}", self.base)
    }

    /// The directory this release's downloads are kept in.
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// A verified binary for `triple`, fetching it if the cache has not got one.
    ///
    /// What comes back is a path to a file whose length and hash are the ones
    /// the manifest gives, which is all the caller needs to know before sending
    /// it somewhere.
    pub fn binary(&self, triple: &str) -> Result<PathBuf, Error> {
        if let Some(path) = self.cached(triple) {
            debug!(triple, path = %path.display(), "using an already fetched binary");
            return Ok(path);
        }
        let (manifest, text) = self.fetch_manifest()?;
        let asset = manifest.asset(triple).ok_or_else(|| Error::Missing {
            url: self.manifest_url(),
            triple: triple.to_owned(),
            available: manifest.triples(),
        })?;
        let path = self.download(asset)?;
        // Only now, with an asset from it verified on disk, is the manifest
        // worth keeping: it is what the next run checks that asset against, and
        // one that described something that never arrived would send every run
        // back to the network for nothing.
        self.remember(&text)?;
        Ok(path)
    }

    /// A binary for `triple` that has already been fetched and still hashes to
    /// what the release said, or nothing.
    ///
    /// The manifest is cached beside the binaries, which is what lets a second
    /// run make no request at all: a release's description of itself does not
    /// change, so the copy taken when the binary was fetched is the copy to
    /// verify it against. Anything here that does not verify is removed, so the
    /// next step is an ordinary fetch rather than a special case.
    fn cached(&self, triple: &str) -> Option<PathBuf> {
        let manifest = self.remembered()?;
        let asset = manifest.asset(triple)?;
        let path = self.cache.join(asset.name()?);
        if !path.exists() {
            return None;
        }
        match verified(&path, asset) {
            Ok(true) => Some(path),
            Ok(false) => {
                info!(path = %path.display(), "a cached binary is not what the release describes; fetching it again");
                discard(&path);
                None
            }
            Err(error) => {
                debug!(path = %path.display(), %error, "cannot check a cached binary");
                discard(&path);
                None
            }
        }
    }

    /// The manifest kept beside the binaries, if there is one and it still
    /// describes this release.
    fn remembered(&self) -> Option<Manifest> {
        let text = fs::read_to_string(self.cache.join(MANIFEST)).ok()?;
        let manifest: Manifest = serde_json::from_str(&text).ok()?;
        (manifest.v == SCHEMA && manifest.version == self.version).then_some(manifest)
    }

    /// Keeps the manifest beside the binaries it describes, exactly as the
    /// release wrote it.
    ///
    /// Kept verbatim rather than re-serialized from what was parsed, so that
    /// what a later run checks against is what the publisher actually said —
    /// including the fields this build did not understand, which a later build
    /// reading the same cache will.
    fn remember(&self, text: &str) -> Result<(), Error> {
        let path = self.cache.join(MANIFEST);
        fs::write(&path, text).map_err(|source| Error::Cache { path, source })
    }

    /// Reads the manifest from where the release is published, and insists that
    /// it describes this release.
    fn fetch_manifest(&self) -> Result<(Manifest, String), Error> {
        let url = self.manifest_url();
        debug!(%url, "reading the release manifest");
        let text = text(&url, MANIFEST_LIMIT).map_err(|source| Error::Unreachable {
            url: url.clone(),
            source,
        })?;
        let manifest: Manifest =
            serde_json::from_str(&text).map_err(|source| Error::Malformed {
                url: url.clone(),
                source,
            })?;
        if manifest.v != SCHEMA {
            return Err(Error::Schema {
                url,
                found: manifest.v,
            });
        }
        // The far end has to run the same version this one does, because the
        // only check that licenses executing anything over there is that it
        // answers to this version. A release of anything else is not a
        // near-miss; it is a binary that would be fetched, sent and refused.
        if manifest.version != self.version {
            return Err(Error::Version {
                url,
                wanted: self.version.clone(),
                found: manifest.version,
            });
        }
        Ok((manifest, text))
    }

    /// Fetches one asset into the cache and verifies it before it is nameable.
    ///
    /// The download goes to a name ending in [`PARTIAL`] and is renamed only
    /// once its length and hash are the manifest's, so a run that is killed
    /// halfway leaves nothing another run would mistake for a binary, and the
    /// file appearing under its real name is one atomic step.
    fn download(&self, asset: &Asset) -> Result<PathBuf, Error> {
        let name = asset.name().ok_or_else(|| Error::Unnamed {
            url: self.manifest_url(),
            triple: asset.triple.clone(),
            named: asset.url.clone(),
        })?;
        fs::create_dir_all(&self.cache).map_err(|source| Error::Cache {
            path: self.cache.clone(),
            source,
        })?;

        let url = format!("{}/{name}", self.base);
        let path = self.cache.join(name);
        let partial = self.cache.join(format!("{name}{PARTIAL}"));
        info!(%url, size = asset.size, "fetching a binary for {}", asset.triple);
        if let Err(error) = save(&url, &partial) {
            discard(&partial);
            return Err(match error {
                Saving::Fetching(source) => Error::Unfetchable { url, source },
                Saving::Writing(source) => Error::Cache {
                    path: partial,
                    source,
                },
            });
        }

        let (found, found_size) = digest(&partial).map_err(|source| Error::Cache {
            path: partial.clone(),
            source,
        })?;
        if found != asset.sha256 || found_size != asset.size {
            discard(&partial);
            return Err(Error::Corrupt {
                url,
                size: asset.size,
                sha256: asset.sha256.clone(),
                found_size,
                found,
            });
        }

        fs::rename(&partial, &path).map_err(|source| Error::Cache {
            path: partial,
            source,
        })?;
        Ok(path)
    }
}

/// Why saving a location to a file did not work: reading it or writing it.
///
/// Worth telling apart, because one of them is somebody else's outage and the
/// other is this machine's disk.
enum Saving {
    /// The location could not be read.
    Fetching(Transfer),
    /// The file could not be written.
    Writing(io::Error),
}

/// Where a release of `version` is published when nothing says otherwise.
fn default_base(version: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/download/v{version}")
}

/// Whether the file at `path` is the asset the manifest describes.
fn verified(path: &Path, asset: &Asset) -> io::Result<bool> {
    let (found, size) = digest(path)?;
    Ok(found == asset.sha256 && size == asset.size)
}

/// What a file hashes to, as 64 lowercase hex digits, and how long it is.
fn digest(path: &Path) -> io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok((hex(&hasher.finalize()), size));
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
}

/// Bytes as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Removes a file, saying so at most in the log.
///
/// Every call site is already returning an error or about to fetch again, and
/// neither has anything better to do about a file that will not go away: the
/// next run checks what it finds rather than trusting it.
fn discard(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => debug!(path = %path.display(), "removed"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => debug!(path = %path.display(), %error, "cannot remove"),
    }
}

/// Where the user's cache lives.
///
/// The XDG variable, then the conventional directory under a home, then a
/// temporary directory — the last so that a process with no home at all still
/// fetches rather than failing, at the cost of fetching again next time.
fn cache_root() -> PathBuf {
    let named = |variable| {
        std::env::var_os(variable)
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
    };
    named(CACHE_VAR)
        .or_else(|| named(HOME_VAR).map(|home| home.join(CACHE_FALLBACK)))
        .unwrap_or_else(std::env::temp_dir)
}

/// Reads a location as text, up to `limit` bytes of it.
fn text(url: &str, limit: u64) -> Result<String, Transfer> {
    let mut read = String::new();
    open(url, limit)?.read_to_string(&mut read)?;
    Ok(read)
}

/// Reads a location into a file at `path`, creating or truncating it.
fn save(url: &str, path: &Path) -> Result<(), Saving> {
    let mut source = open(url, ASSET_LIMIT).map_err(Saving::Fetching)?;
    let file = File::create(path).map_err(Saving::Writing)?;
    let mut file = BufWriter::new(file);
    let mut buffer = vec![0u8; CHUNK];
    loop {
        // Both kinds of location report a read that failed part way through as
        // an io error — a file that vanished, a connection that dropped — and
        // both mean the same thing here: what was being fetched did not all
        // arrive.
        let read = source
            .read(&mut buffer)
            .map_err(|error| Saving::Fetching(Transfer::File(error)))?;
        if read == 0 {
            return file.flush().map_err(Saving::Writing);
        }
        file.write_all(&buffer[..read]).map_err(Saving::Writing)?;
    }
}

/// Starts reading a location, whatever kind of location it is.
///
/// The two schemes are not alternatives to each other in the way a fallback
/// would be: a base is one or the other, and which one is a property of what
/// the caller was told to read.
fn open(url: &str, limit: u64) -> Result<Box<dyn Read + Send>, Transfer> {
    if let Some(path) = url.strip_prefix("file://") {
        // A `file://` location is a path and nothing else here: no host, and no
        // percent-decoding, because what these name is a directory somebody
        // typed into a variable rather than a URL any web server produced.
        return Ok(Box::new(File::open(path)?));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(Transfer::Scheme {
            url: url.to_owned(),
        });
    }
    tls();
    let response = ureq::get(url).call()?;
    let (_, body) = response.into_parts();
    Ok(Box::new(body.into_with_config().limit(limit).reader()))
}

/// Puts the cryptography behind https in place, once.
///
/// The TLS library takes its algorithms from a provider installed for the whole
/// process, and this build ships one that needs no C compiler to build for any
/// machine a release is made for — which is what keeps producing those releases
/// a matter of adding a target rather than of installing a toolchain per
/// architecture. An error means something else installed one first, which is
/// just as good an answer as installing this one.
fn tls() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls_graviola::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests;
