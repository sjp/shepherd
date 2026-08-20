//! The hook mappings that ship inside this library.
//!
//! The same floor the screen family has: a machine with nothing installed on it
//! still knows how to read the payloads of the agents this library was built
//! knowing about, and a copy found on disk or fetched later shadows the bundled
//! one for that agent.
//!
//! Each entry is keyed by the `id` its manifest declares, never by the file it
//! came from, because the id is what a caller asks for.

/// Every bundled hook mapping, as (declared id, TOML source).
///
/// Sorted by id so that the list reads the way a listing of it prints. The
/// contents are not parsed here: a mapping is validated when it is loaded, and
/// a test in this module proves the bundled ones survive that. An agent no
/// mapping describes normalizes to nothing, which is the same answer an
/// unmapped event gets.
pub(crate) const BUNDLED_HOOKS: &[(&str, &str)] = &[
    ("claude", include_str!("../../manifests/hooks/claude.toml")),
    ("codex", include_str!("../../manifests/hooks/codex.toml")),
    (
        "opencode",
        include_str!("../../manifests/hooks/opencode.toml"),
    ),
];

/// The bundled mappings.
///
/// A function rather than the constant itself so that callers outside this
/// module never grow a habit of indexing into a fixed list.
pub(crate) fn bundled_hook_manifests() -> &'static [(&'static str, &'static str)] {
    BUNDLED_HOOKS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::schema::{HOOKS_ENGINE_VERSION, HookManifest};
    use crate::store::MAX_MANIFEST_BYTES;
    use std::collections::HashSet;

    /// How many agents the bundled mappings cover. Asserted rather than derived
    /// so that a mapping dropped from the list has to be an explicit decision.
    const BUNDLED_COUNT: usize = 3;

    #[test]
    fn every_bundled_manifest_loads_cleanly_under_the_key_it_is_filed_by() {
        assert_eq!(bundled_hook_manifests().len(), BUNDLED_COUNT);

        let mut seen = HashSet::new();
        for (id, content) in bundled_hook_manifests() {
            let manifest = HookManifest::parse(content)
                .unwrap_or_else(|error| panic!("bundled mapping {id:?} is not loadable: {error}"));
            assert_eq!(
                &manifest.id, id,
                "registry key {id:?} does not match the id the manifest declares",
            );
            assert!(seen.insert(*id), "duplicate registry key {id:?}");
            let required = manifest.min_engine_version.unwrap_or(1);
            assert!(
                required <= HOOKS_ENGINE_VERSION,
                "bundled mapping {id:?} asks for engine {required}, this engine speaks \
                 {HOOKS_ENGINE_VERSION}",
            );
        }
    }

    #[test]
    fn every_bundled_manifest_is_readable_where_a_copy_of_it_would_be() {
        // A bundled mapping big enough that the same file on disk would be
        // refused is a mapping nobody could override or update, since every
        // tier above the bundled one is read through that cap.
        for (id, content) in bundled_hook_manifests() {
            assert!(
                content.len() as u64 <= MAX_MANIFEST_BYTES,
                "bundled mapping {id:?} is {} bytes, over the {MAX_MANIFEST_BYTES}-byte limit \
                 every other tier is read under",
                content.len(),
            );
        }
    }

    #[test]
    fn every_bundled_manifest_maps_something() {
        // A mapping with no events parses and answers nothing, which is a
        // slower way of not shipping it.
        for (id, content) in bundled_hook_manifests() {
            let manifest = HookManifest::parse(content).expect("loadable");
            assert!(
                !manifest.events.is_empty(),
                "bundled mapping {id:?} maps no events",
            );
        }
    }
}
