//! The names the model calls its three kinds of thing by.
//!
//! An id is a number and nothing else. Nothing about a workspace, a tab or a
//! shell is recoverable from one, and none of them is a handle: holding an id
//! for something that has since been closed is expected, and is answered by a
//! lookup that finds nothing rather than by a dangling reference.
//!
//! Each kind of id has a companion that hands them out, in order and without
//! reuse. A closed shell's id is never given to a later one — events about a
//! shell arrive from outside this process and can arrive after it has gone, and
//! reusing its id would attribute what it did to whatever took its place.
//!
//! Tab and shell numbering restarts per workspace, so a shell id on its own
//! names a shell only once it is known which workspace's it is.
//! [`ShellAddress`] is the two of them together, which is what anything holding
//! shells from more than one workspace at a time has to key them by.
//!
//! # Why a counter rather than a random identifier
//!
//! These ids are handed out and consumed inside one running process, so the
//! properties a uuid buys — being unguessable, and being unique across machines
//! that never talk to each other — are properties nothing here needs. What is
//! needed is the opposite: an id short enough to spell out in an environment
//! variable that every process a shell starts inherits, and cheap enough to
//! compare that a redraw can compare thousands of them without thinking about
//! it. A counter gives both, and the longest one it can produce is ten digits.

use std::fmt;

/// Defines one id and the run it comes from. The three of them differ only in
/// what they name, and writing that difference out three times invites the
/// three to drift apart.
macro_rules! id_type {
    (
        $(#[$id_doc:meta])* $id:ident,
        $(#[$run_doc:meta])* $run:ident
    ) => {
        $(#[$id_doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $id(u32);

        impl $id {
            /// The first id of this kind to be handed out.
            pub const FIRST: Self = Self(0);

            /// The last id the representation holds. Reaching it means
            /// [`Self::next`] has nothing left to give.
            pub const LAST: Self = Self(u32::MAX);

            /// The id for a number that came from somewhere else — a parsed
            /// correlation string, or a layout restored from disk.
            pub const fn from_raw(raw: u32) -> Self {
                Self(raw)
            }

            /// The number behind the id, for writing it somewhere that takes
            /// only numbers.
            pub const fn raw(self) -> u32 {
                self.0
            }

            /// The id after this one, or `None` at [`Self::LAST`].
            ///
            /// Exhausting this would take four billion of whatever it names in
            /// a single run, so the `None` arm is a formality — but it is a
            /// formality that costs one `Option` and rules out silently
            /// handing out an id something else is already using.
            pub fn next(self) -> Option<Self> {
                self.0.checked_add(1).map(Self)
            }
        }

        impl fmt::Display for $id {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        $(#[$run_doc])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $run {
            next: Option<$id>,
        }

        impl $run {
            /// A run starting at the first id.
            pub const fn new() -> Self {
                Self {
                    next: Some($id::FIRST),
                }
            }

            /// A run continuing after `last`, for a model that was restored
            /// rather than started empty.
            pub fn resuming_after(last: $id) -> Self {
                Self { next: last.next() }
            }

            /// The next id.
            ///
            /// # Panics
            ///
            /// If every id of this kind has already been handed out — four
            /// billion of them, which is not a case worth threading a `Result`
            /// through the whole model for.
            pub fn allocate(&mut self) -> $id {
                let id = self.next.expect("ran out of ids");
                self.next = id.next();
                id
            }
        }

        impl Default for $run {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

id_type! {
    /// Names one workspace: a project folder somebody opened, with its own
    /// tabs, its own settings and its own runs of tab and shell ids.
    WorkspaceId,
    /// The workspace ids handed out so far.
    WorkspaceIds
}

id_type! {
    /// Names one tab within a workspace. Unique within its workspace and
    /// meaningless outside it.
    TabId,
    /// One workspace's tab ids.
    TabIds
}

id_type! {
    /// Names one shell — one terminal running one process — within a
    /// workspace. Unique within its workspace, which is what lets a workspace
    /// and a shell together identify a terminal anywhere in the application.
    ShellId,
    /// One workspace's shell ids.
    ShellIds
}

/// Which shell, anywhere in the application: the workspace, and the shell's
/// number within it.
///
/// Shell numbers restart per workspace, so `s3` names a shell only once the
/// workspace is known. Anything holding shells from several workspaces at once
/// — what is attributed to each of them, what is running in each of them —
/// keys them by this rather than by a number that two workspaces both use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShellAddress {
    /// The workspace the shell is open in.
    pub workspace: WorkspaceId,
    /// Which of that workspace's shells.
    pub shell: ShellId,
}

impl ShellAddress {
    /// The address of `shell` in `workspace`.
    pub const fn new(workspace: WorkspaceId, shell: ShellId) -> Self {
        Self { workspace, shell }
    }
}

impl From<(WorkspaceId, ShellId)> for ShellAddress {
    fn from((workspace, shell): (WorkspaceId, ShellId)) -> Self {
        Self::new(workspace, shell)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_the_workspace_and_the_shell_within_it() {
        let address = ShellAddress::new(WorkspaceId::from_raw(9), ShellId::from_raw(3));
        assert_eq!(
            address,
            (WorkspaceId::from_raw(9), ShellId::from_raw(3)).into()
        );
        assert_eq!(address.workspace, WorkspaceId::from_raw(9));
        assert_eq!(address.shell, ShellId::from_raw(3));
        // The same number in two workspaces is two shells.
        assert_ne!(
            address,
            ShellAddress::new(WorkspaceId::FIRST, ShellId::from_raw(3))
        );
    }

    #[test]
    fn ids_are_handed_out_in_order_and_never_repeat() {
        let mut ids = ShellIds::new();
        let handed: Vec<ShellId> = (0..5).map(|_| ids.allocate()).collect();
        assert_eq!(
            handed,
            (0..5).map(ShellId::from_raw).collect::<Vec<ShellId>>()
        );
    }

    #[test]
    fn a_resumed_run_continues_after_the_id_it_was_given() {
        let mut ids = TabIds::resuming_after(TabId::from_raw(41));
        assert_eq!(ids.allocate(), TabId::from_raw(42));
    }

    #[test]
    fn the_last_id_has_no_successor() {
        assert_eq!(WorkspaceId::LAST.raw(), u32::MAX);
        assert_eq!(WorkspaceId::LAST.next(), None);
        assert_eq!(
            WorkspaceId::FIRST.next(),
            Some(WorkspaceId::from_raw(1)),
            "the first id is followed by the second"
        );
    }

    #[test]
    fn an_id_spells_itself_as_a_decimal_number() {
        assert_eq!(ShellId::FIRST.to_string(), "0");
        assert_eq!(ShellId::LAST.to_string(), "4294967295");
    }
}
