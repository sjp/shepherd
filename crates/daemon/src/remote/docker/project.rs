//! Which project a directory belongs to.
//!
//! A devcontainer records the host path of the project it was built for, so
//! associating one with the work somebody is actually doing means turning a
//! working directory into that same path. The rule is the one the tooling that
//! builds these containers uses: the project is the nearest directory at or
//! above the working directory that holds a devcontainer definition.
//!
//! This decides nothing about what exists. Which containers there are comes
//! from Docker, and it comes from Docker whether or not anybody has ever
//! reported a working directory — otherwise a bus would find fewer containers
//! the less its subscribers happened to say, which is exactly the dependency
//! that must not exist. What this is for is putting a project's own container
//! first, and knowing which project a container belongs to when saying so.

use std::path::{Path, PathBuf};

/// The directory a devcontainer definition may sit in.
const DIR: &str = ".devcontainer";

/// The file that is one.
const FILE: &str = "devcontainer.json";

/// The same file, as it is written when there is no directory.
const LOOSE: &str = ".devcontainer.json";

/// The project directory `cwd` belongs to, or nothing when it belongs to none.
///
/// The walk stops at the parent of `home`: above that are directories shared
/// with everybody on the machine, where a definition would belong to no
/// particular person and matching against one would be a coincidence rather
/// than a fact. A machine that says nothing about where home is gets a walk
/// that ends at the root, which is the same rule with the boundary at the only
/// other place it could be.
pub fn root(cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let stop = home.and_then(Path::parent);
    let mut dir = cwd;
    loop {
        if defined(dir) {
            return Some(dir.to_owned());
        }
        if Some(dir) == stop {
            return None;
        }
        dir = dir.parent()?;
    }
}

/// Whether `dir` holds a devcontainer definition, in any of the three places
/// one is written.
fn defined(dir: &Path) -> bool {
    if dir.join(LOOSE).is_file() || dir.join(DIR).join(FILE).is_file() {
        return true;
    }
    // A project with several configurations puts each in a subdirectory of its
    // own, and any of them being there makes this the project's root.
    let Ok(entries) = std::fs::read_dir(dir.join(DIR)) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.path().join(FILE).is_file())
}
