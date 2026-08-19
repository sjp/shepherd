//! What fetching a binary from a release does, against releases written into
//! temporary directories and served over a loopback http server.
//!
//! Nothing here reaches the network beyond the loopback interface, and nothing
//! reads or writes the cache of whoever is running the tests: every release is
//! given a cache directory of its own that goes away with it.

use std::fs;
use std::path::PathBuf;

use super::super::published::Published;
use super::{Error, Release, Transfer};

/// The version these releases are of. Deliberately not this build's, because
/// what a release says about itself and what a caller wants are two facts and
/// the code has to keep them apart.
const VERSION: &str = "1.2.3";

/// A machine some release was built for.
const TRIPLE: &str = "x86_64-unknown-linux-musl";

/// Another one.
const OTHER: &str = "aarch64-apple-darwin";

/// What the assets in these releases contain.
const CONTENTS: &str = "#!/bin/sh\necho 'agentbus 1.2.3'\n";

/// A cache directory that goes away when the test does.
struct Cache(tempfile::TempDir);

impl Cache {
    fn new() -> Self {
        Self(tempfile::tempdir().expect("cannot make a temporary directory"))
    }

    fn path(&self) -> &std::path::Path {
        self.0.path()
    }

    /// Everything in the cache, whatever depth it is at, as names.
    fn contents(&self) -> Vec<String> {
        fn walk(dir: &std::path::Path, into: &mut Vec<String>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                match entry.path().is_dir() {
                    true => walk(&entry.path(), into),
                    false => into.push(entry.file_name().to_string_lossy().into_owned()),
                }
            }
        }
        let mut found = Vec::new();
        walk(self.path(), &mut found);
        found.sort();
        found
    }
}

/// A release of [`VERSION`] holding both triples, and a cache to fetch it into.
fn release() -> (super::super::published::Site, Cache) {
    let site = Published::of(VERSION, &[TRIPLE, OTHER], CONTENTS).write();
    (site, Cache::new())
}

#[test]
fn a_release_in_a_directory_is_fetched_and_verified() {
    let (site, cache) = release();

    let path = Release::at(site.base(), VERSION)
        .caching_in(cache.path())
        .binary(TRIPLE)
        .expect("the binary was not fetched");

    assert_eq!(fs::read_to_string(&path).unwrap(), CONTENTS);
    assert!(
        path.starts_with(cache.path()),
        "{} is not in the cache",
        path.display()
    );
    assert_eq!(
        path.file_name().unwrap().to_string_lossy(),
        format!("agentbus-{VERSION}-{TRIPLE}"),
    );
}

#[test]
fn each_triple_gets_its_own_binary() {
    let (site, cache) = release();
    let release = Release::at(site.base(), VERSION).caching_in(cache.path());

    let one = release.binary(TRIPLE).expect("the binary was not fetched");
    let other = release.binary(OTHER).expect("the binary was not fetched");

    assert_ne!(one, other);
    assert_eq!(
        cache.contents(),
        vec![
            format!("agentbus-{VERSION}-{OTHER}"),
            format!("agentbus-{VERSION}-{TRIPLE}"),
            "manifest.json".to_owned(),
        ]
    );
}

#[test]
fn an_asset_that_is_not_what_the_manifest_describes_is_refused_and_left_nowhere() {
    let site = Published::of(VERSION, &[TRIPLE], CONTENTS)
        .tampered()
        .write();
    let cache = Cache::new();

    let error = Release::at(site.base(), VERSION)
        .caching_in(cache.path())
        .binary(TRIPLE)
        .expect_err("an asset that does not hash to what was promised was accepted");

    assert!(
        matches!(&error, Error::Corrupt { size, found_size, sha256, found, .. }
            if found_size != size && found != sha256),
        "{error:?}"
    );
    assert!(
        cache.contents().is_empty(),
        "something was left in the cache: {:?}",
        cache.contents()
    );
}

#[test]
fn a_release_of_another_version_is_refused_naming_both() {
    let site = Published::of(VERSION, &[TRIPLE], CONTENTS)
        .claiming("9.9.9")
        .write();
    let cache = Cache::new();

    let error = Release::at(site.base(), VERSION)
        .caching_in(cache.path())
        .binary(TRIPLE)
        .expect_err("a release of another version was accepted");

    assert!(
        matches!(&error, Error::Version { wanted, found, .. }
            if wanted == VERSION && found == "9.9.9"),
        "{error:?}"
    );
    let said = error.to_string();
    assert!(said.contains(VERSION) && said.contains("9.9.9"), "{said}");
}

#[test]
fn a_release_without_the_wanted_triple_says_what_it_has() {
    let (site, cache) = release();

    let error = Release::at(site.base(), VERSION)
        .caching_in(cache.path())
        .binary("riscv64gc-unknown-linux-musl")
        .expect_err("a triple the release does not have was fetched anyway");

    let said = error.to_string();
    assert!(matches!(&error, Error::Missing { .. }), "{error:?}");
    assert!(
        said.contains(TRIPLE) && said.contains(OTHER),
        "the available triples are not named: {said}"
    );
}

#[test]
fn a_manifest_in_a_schema_this_does_not_know_is_refused() {
    let site = Published::of(VERSION, &[TRIPLE], CONTENTS)
        .in_schema(super::SCHEMA + 1)
        .write();
    let cache = Cache::new();

    let error = Release::at(site.base(), VERSION)
        .caching_in(cache.path())
        .binary(TRIPLE)
        .expect_err("a manifest in an unknown schema was read anyway");

    assert!(
        matches!(&error, Error::Schema { found, .. } if *found == super::SCHEMA + 1),
        "{error:?}"
    );
}

#[test]
fn a_manifest_that_is_not_json_says_where_it_came_from() {
    let (site, cache) = release();
    fs::write(site.path().join(super::MANIFEST), "not a manifest").unwrap();

    let error = Release::at(site.base(), VERSION)
        .caching_in(cache.path())
        .binary(TRIPLE)
        .expect_err("something that is not a manifest was read as one");

    assert!(
        matches!(&error, Error::Malformed { url, .. } if url.ends_with(super::MANIFEST)),
        "{error:?}"
    );
}

#[test]
fn a_release_that_is_not_there_says_so() {
    let cache = Cache::new();

    let error = Release::at("file:///no/such/release", VERSION)
        .caching_in(cache.path())
        .binary(TRIPLE)
        .expect_err("a release that does not exist was fetched from");

    assert!(
        matches!(
            &error,
            Error::Unreachable {
                source: Transfer::File(_),
                ..
            }
        ),
        "{error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("file:///no/such/release/manifest.json"),
        "{error}"
    );
}

#[test]
fn a_base_that_is_not_a_location_this_can_read_says_so() {
    let cache = Cache::new();

    let error = Release::at("ftp://example.invalid/releases", VERSION)
        .caching_in(cache.path())
        .binary(TRIPLE)
        .expect_err("a scheme this cannot read was fetched from");

    assert!(
        matches!(
            &error,
            Error::Unreachable {
                source: Transfer::Scheme { .. },
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn an_asset_named_something_that_is_not_a_file_name_is_refused() {
    let site = Published::of(VERSION, &[TRIPLE], CONTENTS)
        .naming_assets("https://example.invalid/releases/")
        .write();
    let cache = Cache::new();

    let error = Release::at(site.base(), VERSION)
        .caching_in(cache.path())
        .binary(TRIPLE)
        .expect_err("an asset with no name was fetched anyway");

    assert!(matches!(&error, Error::Unnamed { .. }), "{error:?}");
}

#[test]
fn a_second_fetch_reads_nothing() {
    let (site, cache) = release();
    let release = Release::at(site.base(), VERSION).caching_in(cache.path());
    let first = release.binary(TRIPLE).expect("the binary was not fetched");

    site.remove();
    let second = release
        .binary(TRIPLE)
        .expect("the cached binary was not used");

    assert_eq!(first, second);
    assert_eq!(fs::read_to_string(&second).unwrap(), CONTENTS);
}

#[test]
fn a_release_that_is_gone_and_was_never_fetched_is_not_invented() {
    let (site, cache) = release();
    let release = Release::at(site.base(), VERSION).caching_in(cache.path());
    release.binary(TRIPLE).expect("the binary was not fetched");

    site.remove();

    release
        .binary(OTHER)
        .expect_err("a triple that was never fetched was produced from an empty cache");
}

#[test]
fn a_cached_binary_that_has_been_meddled_with_is_fetched_again() {
    let (site, cache) = release();
    let release = Release::at(site.base(), VERSION).caching_in(cache.path());
    let path = release.binary(TRIPLE).expect("the binary was not fetched");
    fs::write(&path, "something else entirely").unwrap();

    let again = release
        .binary(TRIPLE)
        .expect("the binary was not fetched again");

    assert_eq!(again, path);
    assert_eq!(fs::read_to_string(&again).unwrap(), CONTENTS);
}

#[test]
fn a_cache_holding_no_manifest_fetches_rather_than_trusting_what_is_there() {
    let (site, cache) = release();
    let release = Release::at(site.base(), VERSION).caching_in(cache.path());
    let path = release.binary(TRIPLE).expect("the binary was not fetched");
    fs::remove_file(path.parent().unwrap().join(super::MANIFEST)).unwrap();
    fs::write(&path, "something else entirely").unwrap();

    let again = release
        .binary(TRIPLE)
        .expect("the binary was not fetched again");

    assert_eq!(fs::read_to_string(&again).unwrap(), CONTENTS);
}

#[test]
fn a_release_over_http_is_read_once_and_then_not_at_all() {
    let (site, cache) = release();
    let served = site.serve();
    let release = Release::at(served.base(), VERSION).caching_in(cache.path());

    let path = release.binary(TRIPLE).expect("the binary was not fetched");
    let requests = served.requests();
    let again = release
        .binary(TRIPLE)
        .expect("the cached binary was not used");

    assert_eq!(fs::read_to_string(&path).unwrap(), CONTENTS);
    assert_eq!(path, again);
    assert_eq!(
        requests, 2,
        "the manifest and the asset are one request each"
    );
    assert_eq!(
        served.requests(),
        requests,
        "the second fetch went to the server"
    );
}

#[test]
fn an_asset_missing_from_an_http_release_is_reported_against_its_own_url() {
    let (site, cache) = release();
    fs::remove_file(site.path().join(format!("agentbus-{VERSION}-{TRIPLE}"))).unwrap();
    let served = site.serve();

    let error = Release::at(served.base(), VERSION)
        .caching_in(cache.path())
        .binary(TRIPLE)
        .expect_err("an asset that is not published was fetched anyway");

    assert!(
        matches!(&error, Error::Unfetchable { url, source: Transfer::Http(_) }
            if url.ends_with(&format!("agentbus-{VERSION}-{TRIPLE}"))),
        "{error:?}"
    );
}

#[test]
fn the_default_base_names_the_repository_and_the_version() {
    let base = super::default_base("4.5.6");

    assert_eq!(
        base,
        format!(
            "https://github.com/{}/releases/download/v4.5.6",
            super::REPOSITORY
        )
    );
}

#[test]
fn the_manifest_sits_beside_the_assets_however_the_base_was_written() {
    let wanted = "https://example.invalid/v1.2.3/manifest.json";

    assert_eq!(
        Release::at("https://example.invalid/v1.2.3", VERSION).manifest_url(),
        wanted
    );
    assert_eq!(
        Release::at("https://example.invalid/v1.2.3/", VERSION).manifest_url(),
        wanted
    );
}

#[test]
fn the_cache_is_under_the_users_cache_directory_and_names_the_version() {
    let root = PathBuf::from("/somewhere");

    let cache = Release::at("file:///anywhere", VERSION)
        .caching_in(&root)
        .cache()
        .to_owned();

    assert_eq!(cache, root.join("agentbus").join(VERSION));
}
