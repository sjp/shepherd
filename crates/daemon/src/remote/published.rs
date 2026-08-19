//! A release, published where a test can point something at it.
//!
//! The two ways a release is ever read are a directory and a web server, and
//! both are here so that the code that reads them is exercised as itself rather
//! than through a seam opened up for testing. [`Published`] writes the manifest
//! and the assets into a temporary directory and hands back a `file://` base;
//! [`Served`] puts that same directory behind a real http server on a loopback
//! port and counts what is asked of it, which is how "the second run fetched
//! nothing" is asserted rather than assumed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;

use sha2::{Digest, Sha256};

/// A release written into a directory: a manifest and one file per triple.
///
/// Every departure from a well-formed release is asked for explicitly, so that
/// a test naming one is a test about that one thing.
pub struct Published {
    dir: tempfile::TempDir,
    version: String,
    schema: u32,
    claims: Option<String>,
    assets: Vec<(String, String)>,
    tampered: bool,
    url: Option<String>,
}

impl Published {
    /// A release of `version` with one asset per triple, each holding
    /// `contents`.
    pub fn of(version: &str, triples: &[&str], contents: &str) -> Self {
        Self {
            dir: tempfile::tempdir().expect("cannot make a temporary directory"),
            version: version.to_owned(),
            schema: super::release::SCHEMA,
            claims: None,
            assets: triples
                .iter()
                .map(|triple| ((*triple).to_owned(), contents.to_owned()))
                .collect(),
            tampered: false,
            url: None,
        }
    }

    /// The same release, whose manifest says it is of another version.
    pub fn claiming(mut self, version: &str) -> Self {
        self.claims = Some(version.to_owned());
        self
    }

    /// The same release, whose manifest is written in another schema.
    pub fn in_schema(mut self, schema: u32) -> Self {
        self.schema = schema;
        self
    }

    /// The same release, whose assets are not the bytes their hashes describe.
    pub fn tampered(mut self) -> Self {
        self.tampered = true;
        self
    }

    /// The same release, whose manifest gives every asset the url `url`.
    pub fn naming_assets(mut self, url: &str) -> Self {
        self.url = Some(url.to_owned());
        self
    }

    /// Writes it out and says where it is.
    pub fn write(self) -> Site {
        let mut entries = Vec::new();
        for (triple, contents) in &self.assets {
            let name = format!("agentbus-{}-{triple}", self.version);
            let hash = hex(&Sha256::digest(contents.as_bytes()));
            let size = contents.len();
            // The hash and the length go into the manifest before the bytes are
            // spoilt, so a tampered release is one whose manifest is right and
            // whose asset is not, which is what a truncated download looks like.
            let written = match self.tampered {
                true => format!("{contents}and something else"),
                false => contents.clone(),
            };
            fs::write(self.dir.path().join(&name), written).expect("cannot write an asset");
            let url = self
                .url
                .clone()
                .unwrap_or_else(|| format!("https://example.invalid/v{}/{name}", self.version));
            entries.push(serde_json::json!({
                "triple": triple,
                "url": url,
                "sha256": hash,
                "size": size,
                // A field this build has never heard of, present in every
                // fixture, because ignoring it is a property worth keeping.
                "notes": "written by a later publisher",
            }));
        }
        let manifest = serde_json::json!({
            "v": self.schema,
            "name": "agentbus",
            "version": self.claims.as_deref().unwrap_or(&self.version),
            "assets": entries,
        });
        fs::write(
            self.dir.path().join(super::release::MANIFEST),
            serde_json::to_string_pretty(&manifest).expect("cannot write the manifest"),
        )
        .expect("cannot write the manifest");
        Site { dir: self.dir }
    }
}

/// A written release and the directory it is in.
pub struct Site {
    dir: tempfile::TempDir,
}

impl Site {
    /// The directory holding it.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Where it is, as a base something can fetch from.
    pub fn base(&self) -> String {
        format!("file://{}", self.dir.path().display())
    }

    /// Takes the whole release away, so that anything which fetches from it
    /// afterwards fails rather than quietly succeeding.
    pub fn remove(&self) {
        for entry in fs::read_dir(self.dir.path()).expect("cannot list the release") {
            let path = entry.expect("cannot list the release").path();
            fs::remove_file(path).expect("cannot remove part of the release");
        }
    }

    /// Serves it over http on a loopback port.
    pub fn serve(self) -> Served {
        Served::of(self)
    }
}

/// A release behind an http server, and a count of what has been asked of it.
pub struct Served {
    /// Held so that the directory being served outlives the server.
    _site: Site,
    server: Arc<tiny_http::Server>,
    requests: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl Served {
    /// Starts serving `site` on a port the operating system chooses.
    fn of(site: Site) -> Self {
        let server = Arc::new(
            tiny_http::Server::http("127.0.0.1:0").expect("cannot listen on the loopback"),
        );
        let requests = Arc::new(AtomicUsize::new(0));
        let thread = std::thread::spawn({
            let server = Arc::clone(&server);
            let requests = Arc::clone(&requests);
            let root = site.path().to_owned();
            move || serve(&server, &requests, &root)
        });
        Self {
            _site: site,
            server,
            requests,
            thread: Some(thread),
        }
    }

    /// Where it is, as a base something can fetch from.
    pub fn base(&self) -> String {
        let address = self
            .server
            .server_addr()
            .to_ip()
            .expect("the server is not on a port");
        format!("http://127.0.0.1:{}", address.port())
    }

    /// How many requests have been answered.
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Answers requests with the file each one names, until the server is unblocked.
fn serve(server: &tiny_http::Server, requests: &AtomicUsize, root: &Path) {
    for request in server.incoming_requests() {
        requests.fetch_add(1, Ordering::SeqCst);
        let name = request
            .url()
            .trim_start_matches('/')
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_owned();
        let path: PathBuf = root.join(&name);
        let answered = match (name.contains('/'), fs::read(&path)) {
            (false, Ok(bytes)) => request.respond(tiny_http::Response::from_data(bytes)),
            _ => request.respond(tiny_http::Response::empty(404)),
        };
        let _ = answered;
    }
}

/// Bytes as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
