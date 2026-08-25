//! What the application manages: folders, the tabs open in them, and the
//! shells arranged in those tabs.
//!
//! A workspace is a project folder somebody opened. It owns its tabs, the
//! settings that apply to work done inside it, and the runs of ids its tabs and
//! shells are named from — which is what makes a shell's identity a workspace
//! and a number rather than a number alone.
//!
//! Everything here is shape. A shell in this model is a slot in a tab with a
//! name; whether a process is running in it, what that process is doing and
//! what its screen says are all somebody else's to hold. Keeping the two apart
//! means the arrangement can be rearranged, saved and restored without any of
//! that being disturbed, and it means the questions asked of the arrangement
//! can be answered by tests that start no processes.

use std::path::{Path, PathBuf};

use crate::correlation::correlation_for;
use crate::ids::{ShellId, ShellIds, TabId, TabIds, WorkspaceId};
use crate::split::{Closed, Direction, SplitTree};

/// What a person has chosen about how work in one workspace is run.
///
/// This is the workspace-scoped half of the application's configuration. It is
/// kept with the workspace, and written wherever the application keeps its own
/// files — never into the project folder, which belongs to whoever's project it
/// is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceSettings {
    /// Whether shells here run inside the project's development container
    /// rather than on this machine. Off until something has established that
    /// there is a container to run in.
    pub devcontainer: bool,
}

/// One tab: a name, an arrangement of shells, and which of them has focus.
#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    id: TabId,
    name: String,
    tree: SplitTree,
    focused: ShellId,
}

impl Tab {
    /// This tab's id, unique within its workspace.
    pub fn id(&self) -> TabId {
        self.id
    }

    /// What the tab is called.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Renames the tab.
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    /// How this tab's shells are arranged.
    pub fn tree(&self) -> &SplitTree {
        &self.tree
    }

    /// The shell keystrokes go to.
    pub fn focused(&self) -> ShellId {
        self.focused
    }

    /// Focuses `shell`, if it is in this tab. Answers whether it was.
    pub fn focus(&mut self, shell: ShellId) -> bool {
        let here = self.tree.contains(shell);
        if here {
            self.focused = shell;
        }
        here
    }

    /// Every shell in this tab, in arrangement order.
    pub fn shells(&self) -> Vec<ShellId> {
        self.tree.shells()
    }

    /// The shell in `direction` from the focused one, or `None` at the edge of
    /// the arrangement.
    pub fn neighbour(&self, direction: Direction) -> Option<ShellId> {
        self.tree.neighbour(self.focused, direction)
    }

    /// Moves focus one shell in `direction`, answering whether there was
    /// anywhere to move to. Focus does not wrap around: an arrangement where
    /// pressing left twice comes back to where it started is one nobody can
    /// navigate by feel.
    pub fn move_focus(&mut self, direction: Direction) -> bool {
        match self.neighbour(direction) {
            Some(shell) => {
                self.focused = shell;
                true
            }
            None => false,
        }
    }
}

/// A project folder, and everything open in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    id: WorkspaceId,
    path: PathBuf,
    name: String,
    settings: WorkspaceSettings,
    tabs: Vec<Tab>,
    tab_ids: TabIds,
    shell_ids: ShellIds,
}

impl Workspace {
    /// A workspace on `path`, with no tabs open yet.
    ///
    /// It is named after the folder, because a workspace is added by picking a
    /// folder and a person who has just picked one already knows what they
    /// picked. A path with no final component — a filesystem root — keeps the
    /// path itself as its name, since it has no basename to be called after.
    pub fn new(id: WorkspaceId, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let name = default_name(&path);
        Self {
            id,
            path,
            name,
            settings: WorkspaceSettings::default(),
            tabs: Vec::new(),
            tab_ids: TabIds::new(),
            shell_ids: ShellIds::new(),
        }
    }

    /// This workspace's id.
    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    /// The folder this workspace is for.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What the workspace is called.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Renames the workspace, which changes nothing about the folder.
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    /// What has been chosen about how work here is run.
    pub fn settings(&self) -> &WorkspaceSettings {
        &self.settings
    }

    /// Changes what has been chosen about how work here is run.
    pub fn settings_mut(&mut self) -> &mut WorkspaceSettings {
        &mut self.settings
    }

    /// The open tabs, left to right.
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// One tab by id.
    pub fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    /// One tab by id, to change.
    pub fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    /// Opens a tab at the right-hand end, holding one new shell which starts
    /// out focused.
    pub fn open_tab(&mut self, name: impl Into<String>) -> TabId {
        let id = self.tab_ids.allocate();
        let shell = self.shell_ids.allocate();
        self.tabs.push(Tab {
            id,
            name: name.into(),
            tree: SplitTree::leaf(shell),
            focused: shell,
        });
        id
    }

    /// Closes a tab and every shell in it. Answers whether there was such a
    /// tab.
    pub fn close_tab(&mut self, id: TabId) -> bool {
        let before = self.tabs.len();
        self.tabs.retain(|tab| tab.id != id);
        self.tabs.len() != before
    }

    /// Puts a new shell beside `target` on the given side and focuses it, the
    /// way splitting a pane in a terminal leaves you typing in the new one.
    ///
    /// Answers with the new shell, or `None` if that tab does not hold that
    /// shell.
    pub fn split(&mut self, tab: TabId, target: ShellId, direction: Direction) -> Option<ShellId> {
        // Found before an id is handed out, so that asking to split something
        // that is not there does not quietly consume one.
        let index = self
            .tabs
            .iter()
            .position(|open| open.id == tab && open.tree.contains(target))?;
        let fresh = self.shell_ids.allocate();
        let tab = &mut self.tabs[index];
        let placed = tab.tree.split(target, direction, fresh);
        debug_assert!(placed, "the tab was chosen for holding the shell");
        tab.focused = fresh;
        Some(fresh)
    }

    /// Closes one shell.
    ///
    /// Closing a tab's last shell closes the tab, which the answer reports as
    /// [`Closed::Emptied`]. Otherwise focus, if the closed shell had it, moves
    /// to the shell that took its place.
    pub fn close_shell(&mut self, tab: TabId, shell: ShellId) -> Closed {
        let Some(index) = self.tabs.iter().position(|open| open.id == tab) else {
            return Closed::NotFound;
        };
        let outcome = self.tabs[index].tree.close(shell);
        match outcome {
            Closed::Emptied => {
                self.tabs.remove(index);
            }
            Closed::Removed { successor } => {
                if self.tabs[index].focused == shell {
                    self.tabs[index].focused = successor;
                }
            }
            Closed::NotFound => {}
        }
        outcome
    }

    /// Every shell in the workspace, tab by tab.
    ///
    /// Whatever has to read from all of them reads from all of them: a shell in
    /// a tab nobody is looking at is still running, and going to look at it
    /// should show what it has been doing all along rather than what it started
    /// doing when it was looked at.
    pub fn shells(&self) -> Vec<ShellId> {
        self.tabs.iter().flat_map(Tab::shells).collect()
    }

    /// Which tab holds `shell`.
    pub fn tab_of(&self, shell: ShellId) -> Option<TabId> {
        self.tabs
            .iter()
            .find(|tab| tab.tree.contains(shell))
            .map(|tab| tab.id)
    }

    /// The string one of this workspace's shells is known by outside the
    /// application. See [`correlation_for`].
    pub fn correlation(&self, shell: ShellId) -> String {
        correlation_for(self.id, shell)
    }
}

/// What a workspace on `path` is called before anybody renames it.
fn default_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> Workspace {
        Workspace::new(WorkspaceId::from_raw(9), "/home/someone/projects/thing")
    }

    #[test]
    fn a_workspace_is_named_after_its_folder() {
        assert_eq!(workspace().name(), "thing");
        assert_eq!(Workspace::new(WorkspaceId::FIRST, "/").name(), "/");
    }

    #[test]
    fn a_new_tab_holds_one_focused_shell() {
        let mut workspace = workspace();
        let tab = workspace.open_tab("build");
        let tab = workspace.tab(tab).expect("the tab just opened");
        assert_eq!(tab.name(), "build");
        assert_eq!(tab.shells(), vec![tab.focused()]);
    }

    #[test]
    fn shell_ids_are_unique_across_the_workspaces_tabs() {
        let mut workspace = workspace();
        let first = workspace.open_tab("one");
        let second = workspace.open_tab("two");
        workspace.split(
            first,
            workspace.tab(first).unwrap().focused(),
            Direction::Right,
        );
        workspace.split(
            second,
            workspace.tab(second).unwrap().focused(),
            Direction::Down,
        );

        let mut shells = workspace.shells();
        let count = shells.len();
        shells.sort();
        shells.dedup();
        assert_eq!(count, 4);
        assert_eq!(shells.len(), count, "no shell id was handed out twice");
    }

    #[test]
    fn splitting_focuses_the_new_shell() {
        let mut workspace = workspace();
        let tab = workspace.open_tab("one");
        let first = workspace.tab(tab).unwrap().focused();
        let second = workspace
            .split(tab, first, Direction::Right)
            .expect("the tab holds the shell being split");
        assert_eq!(workspace.tab(tab).unwrap().focused(), second);
        assert_ne!(second, first);
    }

    #[test]
    fn splitting_a_shell_that_is_somewhere_else_does_nothing() {
        let mut workspace = workspace();
        let one = workspace.open_tab("one");
        let two = workspace.open_tab("two");
        let elsewhere = workspace.tab(two).unwrap().focused();
        assert_eq!(workspace.split(one, elsewhere, Direction::Right), None);
        assert_eq!(workspace.tab(one).unwrap().shells().len(), 1);
    }

    #[test]
    fn closing_the_focused_shell_moves_focus_to_what_took_its_place() {
        let mut workspace = workspace();
        let tab = workspace.open_tab("one");
        let first = workspace.tab(tab).unwrap().focused();
        let second = workspace.split(tab, first, Direction::Right).unwrap();

        assert_eq!(
            workspace.close_shell(tab, second),
            Closed::Removed { successor: first }
        );
        assert_eq!(workspace.tab(tab).unwrap().focused(), first);
    }

    #[test]
    fn closing_a_tabs_last_shell_closes_the_tab() {
        let mut workspace = workspace();
        let tab = workspace.open_tab("one");
        let only = workspace.tab(tab).unwrap().focused();

        assert_eq!(workspace.close_shell(tab, only), Closed::Emptied);
        assert!(workspace.tab(tab).is_none());
        assert!(workspace.tabs().is_empty());
    }

    #[test]
    fn a_shell_can_be_traced_back_to_the_tab_holding_it() {
        let mut workspace = workspace();
        let one = workspace.open_tab("one");
        let two = workspace.open_tab("two");
        let nested = workspace
            .split(two, workspace.tab(two).unwrap().focused(), Direction::Down)
            .unwrap();

        assert_eq!(workspace.tab_of(nested), Some(two));
        assert_eq!(
            workspace.tab_of(workspace.tab(one).unwrap().focused()),
            Some(one)
        );
    }

    #[test]
    fn focus_only_moves_to_a_shell_this_tab_holds() {
        let mut workspace = workspace();
        let one = workspace.open_tab("one");
        let two = workspace.open_tab("two");
        let elsewhere = workspace.tab(two).unwrap().focused();
        let held = workspace.tab(one).unwrap().focused();

        let tab = workspace.tab_mut(one).unwrap();
        assert!(!tab.focus(elsewhere));
        assert_eq!(tab.focused(), held);
    }

    #[test]
    fn settings_start_off_and_can_be_changed() {
        let mut workspace = workspace();
        assert!(!workspace.settings().devcontainer);
        workspace.settings_mut().devcontainer = true;
        assert!(workspace.settings().devcontainer);
    }

    #[test]
    fn a_workspaces_shell_knows_the_string_it_is_known_by_outside() {
        let mut workspace = workspace();
        let tab = workspace.open_tab("one");
        let shell = workspace.tab(tab).unwrap().focused();
        assert_eq!(workspace.correlation(shell), "w9:s0");
    }
}
