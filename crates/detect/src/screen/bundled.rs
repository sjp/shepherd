//! The screen manifests that ship inside this library.
//!
//! Detection is data-driven, but a library that starts out knowing nothing is
//! useless on a fresh machine: someone would have to find, fetch and install a
//! manifest before the first screen could be read. So a corpus of them is
//! compiled in, and it is the floor rather than the ceiling — a manifest found
//! on disk or fetched later shadows the bundled copy for the same agent, which
//! is how a UI change gets answered without waiting for a release.
//!
//! The corpus covers far more agents than anything else here has been taught
//! about. That is deliberate: reading the screen of an agent this software has
//! never heard of is the whole reason the knowledge lives in data.
//!
//! Each entry is keyed by the `id` its manifest declares, never by the file it
//! came from — two of the files are named after the product and declare a
//! shorter id, and the id is what a caller asks for.

// The corpus is stocked before the shop opens: the loader that will choose
// between a bundled manifest and a newer one found on disk is not written yet,
// so for now only this module's own tests read the list.
#![allow(dead_code)]

/// Every bundled screen manifest, as (declared id, TOML source).
///
/// Sorted by id so that the list reads the way a listing of it prints. The
/// contents are not parsed here: a manifest is validated when it is loaded, and
/// a test in this module proves the bundled ones survive that.
pub(crate) const BUNDLED_SCREEN: &[(&str, &str)] = &[
    (
        "agy",
        include_str!("../../manifests/screen/antigravity.toml"),
    ),
    ("amp", include_str!("../../manifests/screen/amp.toml")),
    ("claude", include_str!("../../manifests/screen/claude.toml")),
    ("cline", include_str!("../../manifests/screen/cline.toml")),
    ("codex", include_str!("../../manifests/screen/codex.toml")),
    (
        "copilot",
        include_str!("../../manifests/screen/github-copilot.toml"),
    ),
    ("cursor", include_str!("../../manifests/screen/cursor.toml")),
    ("devin", include_str!("../../manifests/screen/devin.toml")),
    ("droid", include_str!("../../manifests/screen/droid.toml")),
    ("gemini", include_str!("../../manifests/screen/gemini.toml")),
    ("grok", include_str!("../../manifests/screen/grok.toml")),
    ("hermes", include_str!("../../manifests/screen/hermes.toml")),
    ("kilo", include_str!("../../manifests/screen/kilo.toml")),
    ("kimi", include_str!("../../manifests/screen/kimi.toml")),
    ("kiro", include_str!("../../manifests/screen/kiro.toml")),
    ("maki", include_str!("../../manifests/screen/maki.toml")),
    (
        "opencode",
        include_str!("../../manifests/screen/opencode.toml"),
    ),
    ("pi", include_str!("../../manifests/screen/pi.toml")),
    (
        "qodercli",
        include_str!("../../manifests/screen/qodercli.toml"),
    ),
    ("qwen", include_str!("../../manifests/screen/qwen.toml")),
];

/// The bundled corpus.
///
/// A function rather than the constant itself so that callers outside this
/// module never grow a habit of indexing into a fixed list.
pub(crate) fn bundled_screen_manifests() -> &'static [(&'static str, &'static str)] {
    BUNDLED_SCREEN
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::rules::CompiledManifest;
    use crate::screen::schema::{SCREEN_ENGINE_VERSION, ScreenManifest};
    use std::collections::HashSet;

    /// How many agents the bundled corpus covers. Asserted rather than derived
    /// so that a manifest dropped from the list has to be an explicit decision.
    const BUNDLED_COUNT: usize = 20;

    #[test]
    fn every_bundled_manifest_loads_cleanly() {
        for (id, content) in bundled_screen_manifests() {
            let manifest = ScreenManifest::parse(content)
                .unwrap_or_else(|error| panic!("bundled manifest {id:?} is not loadable: {error}"));
            let compiled = CompiledManifest::compile(manifest);
            assert!(
                compiled.warnings().is_empty(),
                "bundled manifest {id:?} compiled with warnings: {:?}",
                compiled.warnings(),
            );
        }
    }

    #[test]
    fn every_bundled_manifest_declares_a_supported_engine() {
        for (id, content) in bundled_screen_manifests() {
            let manifest = ScreenManifest::parse(content).expect("loadable");
            let required = manifest.min_engine_version.unwrap_or(1);
            assert!(
                required <= SCREEN_ENGINE_VERSION,
                "bundled manifest {id:?} asks for engine {required}, this engine speaks \
                 {SCREEN_ENGINE_VERSION}",
            );
        }
    }

    #[test]
    fn the_registry_is_keyed_by_the_declared_id() {
        let corpus = bundled_screen_manifests();
        assert_eq!(corpus.len(), BUNDLED_COUNT);

        let mut seen = HashSet::new();
        for (id, content) in corpus {
            let manifest = ScreenManifest::parse(content).expect("loadable");
            assert_eq!(
                &manifest.id, id,
                "registry key {id:?} does not match the id the manifest declares",
            );
            assert!(seen.insert(*id), "duplicate registry key {id:?}");
        }
    }

    #[test]
    fn agents_whose_file_is_named_after_the_product_are_keyed_by_their_id() {
        let keys: HashSet<&str> = bundled_screen_manifests()
            .iter()
            .map(|(id, _)| *id)
            .collect();
        for id in ["agy", "copilot"] {
            assert!(keys.contains(id), "the corpus has no entry for {id:?}");
        }
    }

    #[test]
    fn no_bundled_manifest_carries_a_namespaced_alias() {
        // Upstream corpora sometimes namespace an alias with the name of the
        // project that published it. Such an alias means nothing here, so none
        // may survive the vendoring.
        for (id, content) in bundled_screen_manifests() {
            let manifest = ScreenManifest::parse(content).expect("loadable");
            for alias in &manifest.aliases {
                assert!(
                    !alias.contains(':'),
                    "bundled manifest {id:?} carries a namespaced alias {alias:?}",
                );
            }
        }
    }
}
