//! A daemon keeping this machine's detection manifests current.
//!
//! These drive the timer the way a machine drives it: a real http server
//! publishing a real catalog, a daemon that fetches from it on its own, and
//! events going through the emit socket throughout — because the property that
//! matters is not only that a newer manifest lands, but that nothing about
//! fetching one is ever felt by whatever is using the bus at the time.
//!
//! The intervals are shortened to milliseconds. Everything else, including what
//! is fetched and what is written where, is exactly what a daemon does at its
//! own half-hourly cadence.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agentbus_daemon::bus::Bus;
use agentbus_daemon::manifests::Updates;
use agentbus_daemon::{Daemon, Settings, SocketPaths};
use agentbus_detect::{Family, SCREEN_ENGINE_VERSION, Screen, Status, StorePaths};
use agentbus_protocol::StreamLine;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// How long a test waits for something that should happen within a few of the
/// intervals it asked for.
const PATIENCE: Duration = Duration::from_secs(10);

/// The agent the published manifests describe. One the bundled corpus already
/// covers, so that what arrives has to be newer than something to be taken.
const AGENT: &str = "claude";

/// A version beyond anything the bundled corpus will ever carry.
const NEWER: &str = "2999.01.01.1";

/// Where the catalog lists the screen manifest, relative to itself.
const PATH: &str = "screen/claude.toml";

/// Long enough for several checks at the cadence these tests ask for, so that a
/// daemon which was going to fetch has had every chance to.
const SEVERAL_CHECKS: Duration = Duration::from_millis(750);

/// A screen manifest that is newer than anything built in.
fn manifest() -> String {
    format!(
        "id = \"{AGENT}\"\nversion = \"{NEWER}\"\n\
         min_engine_version = {SCREEN_ENGINE_VERSION}\n\n\
         [identify]\nnames = [\"{AGENT}\"]\n\n\
         [[rules]]\nid = \"published_marker\"\nstate = \"blocked\"\npriority = 500\n\
         visible_blocker = true\ncontains = [\"the marker\"]\n"
    )
}

/// A catalog listing exactly that manifest.
fn catalog() -> String {
    format!(
        "schema_version = 1\n\n\
         [[manifests]]\nid = \"{AGENT}\"\nfamily = \"{}\"\npath = \"{PATH}\"\n",
        Screen::NAME,
    )
}

/// A directory of files behind an http server on a loopback port, counting what
/// is asked of it.
///
/// A real server rather than a seam: what is being tested is a daemon reaching
/// the network on its own, and the count is what turns "it did not fetch" from
/// an absence of evidence into an assertion.
struct Site {
    dir: tempfile::TempDir,
    server: Arc<tiny_http::Server>,
    requests: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl Site {
    /// An empty site, already listening.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("cannot make a temporary directory");
        let server = Arc::new(
            tiny_http::Server::http("127.0.0.1:0").expect("cannot listen on the loopback"),
        );
        let requests = Arc::new(AtomicUsize::new(0));
        let thread = std::thread::spawn({
            let server = Arc::clone(&server);
            let requests = Arc::clone(&requests);
            let root = dir.path().to_owned();
            move || serve(&server, &requests, &root)
        });
        Self {
            dir,
            server,
            requests,
            thread: Some(thread),
        }
    }

    /// Publishes `content` at `path`.
    fn put(&self, path: &str, content: &str) -> &Self {
        let file = self.dir.path().join(path);
        fs::create_dir_all(file.parent().expect("a path with a parent"))
            .expect("cannot make the directory");
        fs::write(file, content).expect("cannot publish the file");
        self
    }

    /// Publishes the catalog and the manifest it lists.
    fn publish(&self) -> &Self {
        self.put(PATH, &manifest()).put("index.toml", &catalog())
    }

    /// Where the catalog is, published or not.
    fn url(&self) -> String {
        let port = self
            .server
            .server_addr()
            .to_ip()
            .expect("the server is not on a port")
            .port();
        format!("http://127.0.0.1:{port}/index.toml")
    }

    /// How many requests have been answered.
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for Site {
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
        let climbs = name.split('/').any(|part| part == ".." || part.is_empty());
        let answered = match (climbs, fs::read(root.join(&name))) {
            (false, Ok(bytes)) => request.respond(tiny_http::Response::from_data(bytes)),
            _ => request.respond(tiny_http::Response::empty(404)),
        };
        let _ = answered;
    }
}

/// A daemon on its own directory, with a home of its own to keep manifests in.
struct Running {
    bus: Arc<Bus>,
    paths: SocketPaths,
    home: tempfile::TempDir,
    _dir: tempfile::TempDir,
}

impl Running {
    /// Where this daemon's manifests live.
    fn store(&self) -> StorePaths {
        StorePaths::rooted(self.home.path())
    }

    /// What the last check wrote down.
    fn status(&self) -> Status {
        Status::read(&self.store())
    }

    /// The fetched copy of the screen manifest, if one has been committed.
    fn fetched(&self) -> Option<String> {
        let file = self
            .store()
            .remote_file(Screen::NAME, AGENT)
            .expect("a machine with nowhere to keep a manifest");
        fs::read_to_string(file).ok()
    }
}

/// Starts a daemon checking `catalog` every few milliseconds, or not checking
/// at all when `checking` is false.
fn start(catalog: &str, checking: bool) -> Running {
    let dir = tempfile::tempdir().expect("cannot make a temporary directory");
    let home = tempfile::tempdir().expect("cannot make a temporary directory");
    let daemon = Daemon::bind(
        SocketPaths::in_dir(dir.path().join("agentbus")),
        Settings {
            // No process table to read: these tests count what they sent, and a
            // daemon watching this machine's own would number its observations
            // in among them.
            proc_root: dir.path().join("no-process-table"),
            update_manifests: checking,
            ..Settings::default()
        },
    )
    .expect("cannot start the daemon")
    // Nothing on the machine this runs on is to be reached into: what is being
    // tested is one catalog, and a container found here would be a surprise
    // from somebody else's laptop.
    .discovering(Vec::new())
    .updating(Updates {
        store: StorePaths::rooted(home.path()),
        catalog: catalog.to_owned(),
        first_after: Duration::from_millis(50),
        every: Duration::from_millis(100),
    });
    let bus = Arc::clone(daemon.bus());
    let paths = daemon.paths().clone();
    tokio::spawn(daemon.run());
    Running {
        bus,
        paths,
        home,
        _dir: dir,
    }
}

/// A subscriber reading the stream, held open across a check so that the whole
/// of what a consumer sees while manifests are being fetched is one stream with
/// nothing missing from it.
struct Subscriber {
    lines: BufReader<UnixStream>,
}

impl Subscriber {
    /// Connects to `running` and reads the snapshot every stream begins with.
    async fn connect(running: &Running) -> Self {
        let mut subscriber = Self {
            lines: BufReader::new(
                UnixStream::connect(running.paths.sub())
                    .await
                    .expect("cannot connect to the stream"),
            ),
        };
        match subscriber.line().await {
            StreamLine::Snapshot(_) => subscriber,
            other => panic!("the stream began with {other:?}"),
        }
    }

    /// The next line, failing the test if none arrives.
    async fn line(&mut self) -> StreamLine {
        let mut line = String::new();
        let read = tokio::time::timeout(PATIENCE, self.lines.read_line(&mut line))
            .await
            .expect("nothing arrived on the stream")
            .expect("cannot read the stream");
        assert!(read > 0, "the daemon closed the stream");
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error} in {line:?}"))
    }

    /// The session named by the next event, skipping anything else.
    async fn session(&mut self) -> String {
        loop {
            if let StreamLine::Event(event) = self.line().await {
                return event.session.to_string();
            }
        }
    }
}

/// Emits one event and waits for the bus to have taken it, which is the whole
/// of "the bus is still serving" as an emitter experiences it.
async fn served(running: &Running, session: &str) {
    let before = running.bus.last_seq();
    let line = format!(r#"{{"v":1,"agent":"claude","session":"{session}","kind":"tool_start"}}"#);
    let mut stream = UnixStream::connect(running.paths.emit())
        .await
        .expect("cannot connect to the emit socket");
    stream
        .write_all(line.as_bytes())
        .await
        .expect("cannot write an event");
    stream.shutdown().await.expect("cannot close");

    let deadline = Instant::now() + PATIENCE;
    while running.bus.last_seq() <= before {
        assert!(
            Instant::now() < deadline,
            "the bus never took the event emitted as {session}",
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits for `wanted`, or fails the test saying what it was waiting for.
async fn until(what: &str, wanted: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !wanted() {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_manifest_is_taken_while_the_bus_goes_on_serving() {
    let site = Site::new();
    site.publish();
    let running = start(&site.url(), true);
    let mut subscriber = Subscriber::connect(&running).await;

    // Before, during and after: an emitter and a subscriber that were there
    // before the check see the same bus once it has been and gone.
    served(&running, "before").await;
    assert_eq!(subscriber.session().await, "before");
    until("no newer manifest was ever committed", || {
        running.fetched().is_some()
    })
    .await;
    served(&running, "after").await;
    assert_eq!(subscriber.session().await, "after");

    assert_eq!(running.fetched().as_deref(), Some(manifest().as_str()));
    let status = running.status();
    let record = status
        .record(Screen::NAME, AGENT)
        .expect("nothing was recorded about the manifest");
    assert_eq!(record.last_result, "updated");
    assert_eq!(record.attempted_version.as_deref(), Some(NEWER));
    assert_eq!(status.last_result.as_deref(), Some("checked"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_told_not_to_update_manifests_never_asks() {
    let site = Site::new();
    site.publish();
    let running = start(&site.url(), false);

    served(&running, "one").await;
    // Long enough that a daemon which was going to check has had several turns
    // at it.
    tokio::time::sleep(SEVERAL_CHECKS).await;
    served(&running, "two").await;

    assert_eq!(site.requests(), 0, "the catalog was read after all");
    assert_eq!(running.fetched(), None);
    assert_eq!(running.status(), Status::default());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_catalog_that_cannot_be_read_is_looked_at_again() {
    // Listening, and publishing nothing: every check answers 404 until the
    // catalog appears part-way through the test.
    let site = Site::new();
    let running = start(&site.url(), true);

    until("the failed check was never recorded", || {
        running
            .status()
            .last_result
            .as_deref()
            .is_some_and(|result| result.starts_with("failed"))
    })
    .await;
    // A failed check is not a failed daemon.
    served(&running, "while failing").await;

    site.publish();

    until("the catalog was never read again", || {
        running.fetched().is_some()
    })
    .await;
    assert_eq!(running.fetched().as_deref(), Some(manifest().as_str()));
    served(&running, "after recovering").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_catalog_nothing_is_listening_for_leaves_the_bus_untouched() {
    // A port this machine chose and then let go of, so that connecting to it
    // fails the way a published catalog whose host is down fails.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("cannot take a port");
    let port = closed.local_addr().expect("a port").port();
    drop(closed);
    let running = start(&format!("http://127.0.0.1:{port}/index.toml"), true);

    until("the failed check was never recorded", || {
        running
            .status()
            .last_result
            .as_deref()
            .is_some_and(|result| result.starts_with("failed"))
    })
    .await;
    tokio::time::sleep(SEVERAL_CHECKS).await;

    served(&running, "unreachable").await;
    assert_eq!(running.fetched(), None);
}
