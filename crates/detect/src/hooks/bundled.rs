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
/// Sorted by id so that the list reads the way a listing of it prints. Empty
/// while the engine is younger than the data it will carry: the engine answers
/// for whatever the two disk tiers hold in the meantime, and an agent nothing
/// describes normalizes to nothing, which is the same answer an unmapped event
/// gets.
pub(crate) const BUNDLED_HOOKS: &[(&str, &str)] = &[];

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
    use std::collections::HashSet;

    #[test]
    fn every_bundled_manifest_loads_cleanly_under_the_key_it_is_filed_by() {
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
}
