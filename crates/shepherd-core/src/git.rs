//! What a workspace's folder is checked out on, read from the repository's own
//! metadata.
//!
//! A workspace is a project folder, and the one thing about a project folder
//! that changes underneath somebody without them doing anything in this
//! application is which branch it is on. That name is worth putting next to the
//! folder's, and it is the only thing about the repository read here: not what
//! is modified, not what is staged, not what is ahead of anything.
//!
//! # Read from the files rather than asked of a program
//!
//! [`current_branch`] opens two small files and reads a line out of them. It
//! does not run a program, and nothing here needs a version control system
//! installed on the machine — which matters because a workspace whose shells
//! run in a development container is a folder whose toolchain lives somewhere
//! else entirely, and the host it is opened on may be a machine with very
//! little on it.
//!
//! What that costs is generality. A repository is a large and old format, and
//! this reads the two shapes of it that a folder somebody opened actually has:
//! a working tree with its metadata beside it, and a working tree whose
//! metadata is somewhere else and says so. Anything it does not recognise is
//! nothing, silently — a folder nobody has run version control in is the
//! ordinary case of that, and it is not a failure worth telling anybody about.
//!
//! # Read when somebody looks, and remembered in between
//!
//! [`Branches`] is where that reading is done from. A branch name is drawn
//! wherever a workspace is drawn, which is every frame, and the filesystem is
//! not somewhere to go every frame — so a workspace is read when it becomes the
//! one somebody is looking at, and what was read is what is drawn until the
//! next time it does.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use tracing::debug;

use crate::ids::WorkspaceId;
use crate::workspace::Workspace;

#[cfg(test)]
mod tests;

/// What a repository's metadata is called inside a working tree.
///
/// A directory in a plain clone, and a file naming a directory elsewhere in the
/// arrangements where one working tree is not where the repository is — an
/// additional working tree of one repository, or a repository held inside
/// another.
pub const METADATA: &str = ".git";

/// The file inside that metadata saying what the tree is checked out on.
const HEAD: &str = "HEAD";

/// What that file says before the name of the branch it is on.
const POINTS_AT: &str = "ref:";

/// What the metadata file says before the directory the metadata is really in.
const ELSEWHERE: &str = "gitdir:";

/// The prefix under which a branch's full name is written.
const BRANCHES: &str = "refs/heads/";

/// How much of a commit's name is shown for a tree that is on no branch.
///
/// The length such a name is abbreviated to almost everywhere it is written
/// down, which is what makes it recognisable as one rather than as a word.
const ABBREVIATED: usize = 7;

/// The most of any one of these files that is read.
///
/// Every file read here holds a single short line. The cap is not a limit these
/// are expected to approach; it is what keeps a folder that happens to have
/// something enormous where the metadata should be from being read into memory
/// before it is rejected.
const MAX_BYTES: u64 = 64 * 1024;

/// What the working tree at `path` is checked out on, if anything says.
///
/// The branch's short name — `main`, not `refs/heads/main` — for the ordinary
/// case. For a tree checked out on a commit rather than a branch, the first
/// [`ABBREVIATED`] characters of that commit's name, which is what such a state
/// is called everywhere else it is displayed; a caller wanting to tell the two
/// apart cannot, and nothing here needs to.
///
/// [`None`] covers everything else, and covers it silently: a folder that is
/// not in a repository at all, a path that is not there, metadata that cannot
/// be read, and a repository saying something about its state that this does
/// not recognise. None of those is a condition to put in front of a person who
/// asked for a list of their projects — the branch is simply not shown.
///
/// This touches the filesystem. See [`Branches`] for where it is called from,
/// which is not from a place drawing a frame.
pub fn current_branch(path: &Path) -> Option<String> {
    let metadata = metadata(path)?;
    let head = read(&metadata.join(HEAD))?;
    let branch = checked_out(&head);
    if branch.is_none() {
        debug!(
            metadata = %metadata.display(),
            "the repository says something about `{HEAD}` that this does not read"
        );
    }
    branch
}

/// The branch names of the workspaces somebody has been looking at.
///
/// One name per workspace, read when [`focused`] says the workspace has become
/// the one on screen and held until it says so again. Nothing here goes to the
/// filesystem on its own account: a workspace nobody has looked at has no name
/// here, and a workspace whose branch changed while somebody was looking at
/// another one keeps the name it had until they come back to it. That staleness
/// is the whole bargain — a name that is right whenever somebody arrives at the
/// workspace it belongs to, for no cost at all while they are elsewhere.
///
/// [`focused`]: Branches::focused
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Branches {
    read: BTreeMap<WorkspaceId, Option<String>>,
}

impl Branches {
    /// Nothing read yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads `workspace`'s branch, because it has become the one on screen, and
    /// answers what was read.
    ///
    /// The one place here that touches the filesystem, and the reason the rest
    /// of this does not: a caller that has a workspace becoming current has
    /// somewhere to put this call, and a caller drawing a row does not.
    pub fn focused(&mut self, workspace: &Workspace) -> Option<&str> {
        let branch = current_branch(workspace.path());
        self.read.insert(workspace.id(), branch);
        self.of(workspace.id())
    }

    /// What was read for `workspace` when it was last looked at, without
    /// looking again.
    pub fn of(&self, workspace: WorkspaceId) -> Option<&str> {
        self.read.get(&workspace)?.as_deref()
    }

    /// Forgets what was read for a workspace that has been closed.
    ///
    /// Answers whether there was anything to forget, so that a caller closing a
    /// workspace it never showed is not a caller doing something wrong.
    pub fn forget(&mut self, workspace: WorkspaceId) -> bool {
        self.read.remove(&workspace).is_some()
    }
}

/// Where the repository holding `path` keeps its metadata.
///
/// Every folder from `path` up to the root is asked, because somebody may open
/// a folder inside a project rather than the project itself, and the branch a
/// subdirectory is on is the branch its repository is on.
fn metadata(path: &Path) -> Option<PathBuf> {
    // Resolved first, so that walking upwards from a path written relatively —
    // or through a link — walks the folders it actually names rather than
    // running out at the first component. A path that is not there resolves to
    // nothing, which is one of the several ordinary ways to have no branch.
    let path = path.canonicalize().ok()?;
    path.ancestors().find_map(held_in)
}

/// Where the metadata beside `folder` is, if there is any beside it.
///
/// A directory is the metadata. A file is a repository saying its metadata is
/// somewhere else and naming where, which is how an additional working tree of
/// one repository and a repository inside another are both arranged.
fn held_in(folder: &Path) -> Option<PathBuf> {
    let at = folder.join(METADATA);
    let found = at.metadata().ok()?;
    if found.is_dir() {
        return Some(at);
    }
    if !found.is_file() {
        return None;
    }

    let named = read(&at)?;
    let named = Path::new(named.strip_prefix(ELSEWHERE)?.trim());
    Some(match named.is_absolute() {
        true => named.to_owned(),
        // Written relative to the folder the file is in, not to wherever this
        // application happens to have been started.
        false => folder.join(named),
    })
}

/// What a repository's `HEAD` says the tree is on.
///
/// A name for a tree on a branch, whether or not that branch has any commits on
/// it yet — a folder somebody has only just started a repository in is on a
/// branch that exists in name alone, and its name is the right thing to show.
/// A commit's name, abbreviated, for a tree on no branch. Nothing for anything
/// else, including a branch whose name is under something other than the
/// ordinary prefix, which is a repository arranged in a way this does not claim
/// to read.
fn checked_out(head: &str) -> Option<String> {
    let Some(reference) = head.strip_prefix(POINTS_AT) else {
        return abbreviated(head);
    };
    let branch = reference.trim().strip_prefix(BRANCHES)?;
    match branch.is_empty() {
        true => None,
        false => Some(branch.to_owned()),
    }
}

/// `name` shortened to how a commit is written down, if it is one.
///
/// The length is not checked against any particular one: a repository may name
/// its objects with either of two hashes, and a name long enough to abbreviate
/// and made of nothing but hexadecimal is a commit in both.
fn abbreviated(name: &str) -> Option<String> {
    let named = name.len() >= ABBREVIATED && name.chars().all(|c| c.is_ascii_hexdigit());
    // The whole of it has to be a name, not just as much of it as is shown:
    // shortening the first seven characters of something else would produce a
    // plausible-looking answer out of a file this does not understand, which is
    // worse than the nothing it should produce.
    named.then(|| name[..ABBREVIATED].to_owned())
}

/// One of these files as the single line it holds, or nothing if it cannot be
/// read.
fn read(path: &Path) -> Option<String> {
    let mut text = String::new();
    File::open(path)
        .ok()?
        .take(MAX_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    Some(text.trim().to_owned())
}
