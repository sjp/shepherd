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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::correlation::correlation_for;
use crate::ids::{ShellId, ShellIds, TabId, TabIds, WorkspaceId, WorkspaceIds};
use crate::rollup::{RollupStatus, rollup};
use crate::split::{Closed, Direction, Divider, SplitTree};

/// Why an arrangement restored from somewhere else is not one this model would
/// have produced.
///
/// Everything here is a uniqueness or a membership rule that the model
/// maintains as it goes — ids are handed out once, focus is only ever set to a
/// shell the tab holds — and that therefore has to be checked at the one place
/// something arrives already assembled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MalformedLayout {
    /// A tab focused on a shell that is somewhere else, or nowhere.
    #[error("the tab is focused on shell {0}, which is not in it")]
    FocusElsewhere(ShellId),
    /// Two tabs of one workspace with the same id.
    #[error("two tabs are both tab {0}")]
    DuplicateTab(TabId),
    /// One shell in two of a workspace's tabs.
    #[error("shell {0} is in more than one tab")]
    DuplicateShell(ShellId),
    /// Two workspaces with the same id.
    #[error("two workspaces are both workspace {0}")]
    DuplicateWorkspace(WorkspaceId),
}

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
    /// A tab put back together from a description of one.
    ///
    /// `focused` is where keystrokes should go; a description that does not say
    /// gets the first shell in the arrangement, which is the one somebody
    /// opening a tab would have been typing in anyway. One that names a shell
    /// this tab does not hold is refused rather than corrected: it says
    /// something about the arrangement that is not true, and quietly focusing
    /// something else would hide that.
    pub fn restore(
        id: TabId,
        name: impl Into<String>,
        tree: SplitTree,
        focused: Option<ShellId>,
    ) -> Result<Self, MalformedLayout> {
        let focused = match focused {
            Some(shell) if !tree.contains(shell) => {
                return Err(MalformedLayout::FocusElsewhere(shell));
            }
            Some(shell) => shell,
            None => tree.first_shell(),
        };
        Ok(Self {
            id,
            name: name.into(),
            tree,
            focused,
        })
    }

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

    /// Moves one of the arrangement's dividers, answering whether that changed
    /// anything. See [`SplitTree::resize`].
    ///
    /// The tree is handed out to be read and never to be changed, so that the
    /// invariants it maintains cannot be got round from outside; this is the
    /// one thing about the arrangement a window can move, and the shares it
    /// writes are the ones that get saved.
    pub fn resize(&mut self, divider: &Divider, position: f32) -> bool {
        self.tree.resize(divider, position)
    }

    /// What this tab rolls up to, given a way to ask what each of its shells is
    /// doing.
    ///
    /// The statuses are asked for rather than held, because this model is shape
    /// and what is running in it is somebody else's to keep. The fold is
    /// [`rollup`], the same one a shell folds its sessions with and a workspace
    /// folds its tabs with.
    pub fn status(&self, shell_status: impl FnMut(ShellId) -> RollupStatus) -> RollupStatus {
        rollup(self.shells().into_iter().map(shell_status))
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

    /// A workspace put back together from a description of one.
    ///
    /// Both runs of ids continue after the highest one still in use, so nothing
    /// handed out from here on collides with a tab or a shell that was
    /// restored. They do not continue after the highest one *ever* used: the
    /// numbers a previous run handed out and then closed are free again,
    /// because nothing outside this process remembers them across a restart —
    /// every shell in a restored workspace is a process that has yet to be
    /// started.
    pub fn restore(
        id: WorkspaceId,
        path: impl Into<PathBuf>,
        name: impl Into<String>,
        settings: WorkspaceSettings,
        tabs: Vec<Tab>,
    ) -> Result<Self, MalformedLayout> {
        let mut seen_tabs = BTreeSet::new();
        let mut seen_shells = BTreeSet::new();
        for tab in &tabs {
            if !seen_tabs.insert(tab.id) {
                return Err(MalformedLayout::DuplicateTab(tab.id));
            }
            for shell in tab.shells() {
                if !seen_shells.insert(shell) {
                    return Err(MalformedLayout::DuplicateShell(shell));
                }
            }
        }

        Ok(Self {
            id,
            path: path.into(),
            name: name.into(),
            settings,
            tabs,
            tab_ids: match seen_tabs.last() {
                Some(&last) => TabIds::resuming_after(last),
                None => TabIds::new(),
            },
            shell_ids: match seen_shells.last() {
                Some(&last) => ShellIds::resuming_after(last),
                None => ShellIds::new(),
            },
        })
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

    /// Moves a divider in one tab's arrangement, answering whether that changed
    /// anything. See [`SplitTree::resize`].
    pub fn resize(&mut self, tab: TabId, divider: &Divider, position: f32) -> bool {
        match self.tab_mut(tab) {
            Some(tab) => tab.resize(divider, position),
            None => false,
        }
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

    /// What this workspace rolls up to, given a way to ask what each of its
    /// shells is doing.
    ///
    /// The same shell lookup a tab takes, applied to every tab: a workspace's
    /// badge is the fold over its tabs' badges, and its tabs' badges are the
    /// fold over their shells'. A workspace with nothing open rolls up to
    /// [`RollupStatus::None`], which is what it should show.
    pub fn status(&self, mut shell_status: impl FnMut(ShellId) -> RollupStatus) -> RollupStatus {
        rollup(self.tabs.iter().map(|tab| tab.status(&mut shell_status)))
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

/// Everything open: the workspaces, and the run of ids the next one will be
/// named from.
///
/// This is the whole of the application's shape, and it is the whole of what is
/// saved between runs. It exists as a type rather than as a bare list because
/// the run of ids is part of the answer: a list restored from a description
/// says which workspaces there are, and something has to know which number the
/// next one somebody opens may safely have.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layout {
    workspaces: Vec<Workspace>,
    ids: WorkspaceIds,
}

impl Layout {
    /// Nothing open, which is what a first run has.
    pub fn new() -> Self {
        Self::default()
    }

    /// A layout put back together from a description of one.
    ///
    /// The run of ids continues after the highest workspace restored, for the
    /// same reason [`Workspace::restore`] gives.
    pub fn restore(workspaces: Vec<Workspace>) -> Result<Self, MalformedLayout> {
        let mut seen = BTreeSet::new();
        for workspace in &workspaces {
            if !seen.insert(workspace.id) {
                return Err(MalformedLayout::DuplicateWorkspace(workspace.id));
            }
        }
        Ok(Self {
            workspaces,
            ids: match seen.last() {
                Some(&last) => WorkspaceIds::resuming_after(last),
                None => WorkspaceIds::new(),
            },
        })
    }

    /// The open workspaces, in the order they were added.
    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    /// Whether nothing is open at all.
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    /// One workspace by id.
    pub fn workspace(&self, id: WorkspaceId) -> Option<&Workspace> {
        self.workspaces.iter().find(|workspace| workspace.id == id)
    }

    /// One workspace by id, to change.
    pub fn workspace_mut(&mut self, id: WorkspaceId) -> Option<&mut Workspace> {
        self.workspaces
            .iter_mut()
            .find(|workspace| workspace.id == id)
    }

    /// Adds a workspace for `path`, with no tabs open in it yet.
    ///
    /// The same folder may be opened twice: they are two workspaces with two
    /// sets of tabs, and nothing about one folder makes that a mistake.
    pub fn open(&mut self, path: impl Into<PathBuf>) -> WorkspaceId {
        let id = self.ids.allocate();
        self.workspaces.push(Workspace::new(id, path));
        id
    }

    /// Closes a workspace and everything in it. Answers whether there was such
    /// a workspace.
    pub fn close(&mut self, id: WorkspaceId) -> bool {
        let before = self.workspaces.len();
        self.workspaces.retain(|workspace| workspace.id != id);
        self.workspaces.len() != before
    }
}

/// What a workspace on `path` is called before anybody renames it.
pub(crate) fn default_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use agentbus_protocol::SessionStatus::{self, Blocked, Done, Idle, Working};

    use crate::rollup::shell_status;

    fn workspace() -> Workspace {
        Workspace::new(WorkspaceId::from_raw(9), "/home/someone/projects/thing")
    }

    /// A lookup over a list of `(shell, the sessions attributed to it)`. A
    /// shell that is not named hosts nothing, which is how most of them are.
    fn attributed<'a>(
        sessions: &'a [(ShellId, &'a [SessionStatus])],
    ) -> impl FnMut(ShellId) -> RollupStatus + 'a {
        move |shell| {
            sessions
                .iter()
                .find(|(id, _)| *id == shell)
                .map_or(RollupStatus::None, |(_, statuses)| {
                    shell_status(statuses.iter().copied())
                })
        }
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
    fn a_tab_shows_the_most_urgent_thing_happening_in_any_of_its_shells() {
        let mut workspace = workspace();
        let tab = workspace.open_tab("one");
        let first = workspace.tab(tab).unwrap().focused();
        let second = workspace.split(tab, first, Direction::Right).unwrap();
        let third = workspace.split(tab, second, Direction::Down).unwrap();
        let tab = workspace.tab(tab).unwrap();

        // The blocked session is not in the focused shell, is not first in the
        // arrangement, and shares its shell with a finished one.
        assert_eq!(
            tab.status(attributed(&[
                (first, &[Working]),
                (second, &[Done, Blocked]),
                (third, &[]),
            ])),
            RollupStatus::from(Blocked)
        );
    }

    #[test]
    fn a_tab_whose_shells_are_all_finished_is_finished_and_one_with_nothing_in_it_is_none() {
        let mut workspace = workspace();
        let tab = workspace.open_tab("one");
        let first = workspace.tab(tab).unwrap().focused();
        let second = workspace.split(tab, first, Direction::Right).unwrap();
        let tab = workspace.tab(tab).unwrap();

        assert_eq!(
            tab.status(attributed(&[(first, &[Done]), (second, &[Done])])),
            RollupStatus::from(Done)
        );
        assert_eq!(tab.status(attributed(&[])), RollupStatus::None);
    }

    #[test]
    fn a_workspace_rolls_up_through_its_tabs_and_their_shells() {
        let mut workspace = workspace();
        let quiet = workspace.open_tab("quiet");
        let waiting = workspace.open_tab("waiting");
        let busy = workspace.open_tab("busy");
        let editing = workspace.tab(quiet).unwrap().focused();
        let finished = workspace.tab(waiting).unwrap().focused();
        let first = workspace.tab(busy).unwrap().focused();
        let second = workspace.split(busy, first, Direction::Right).unwrap();

        let sessions: [(ShellId, &[SessionStatus]); 4] = [
            (editing, &[]),
            (finished, &[Idle]),
            (first, &[Working]),
            (second, &[Done, Blocked]),
        ];

        assert_eq!(
            workspace.tab(quiet).unwrap().status(attributed(&sessions)),
            RollupStatus::None
        );
        assert_eq!(
            workspace
                .tab(waiting)
                .unwrap()
                .status(attributed(&sessions)),
            RollupStatus::from(Idle)
        );
        assert_eq!(
            workspace.tab(busy).unwrap().status(attributed(&sessions)),
            RollupStatus::from(Blocked)
        );
        // One tab blocked, one idle and one hosting nothing at all: the
        // workspace is blocked, three levels up from the session that is.
        assert_eq!(
            workspace.status(attributed(&sessions)),
            RollupStatus::from(Blocked)
        );
    }

    #[test]
    fn a_workspace_with_nothing_open_is_none() {
        assert_eq!(workspace().status(attributed(&[])), RollupStatus::None);
    }

    /// The arrangement `shells` are in, side by side.
    fn row(shells: &[u32]) -> SplitTree {
        let mut tree = SplitTree::leaf(ShellId::from_raw(shells[0]));
        for pair in shells.windows(2) {
            tree.split(
                ShellId::from_raw(pair[0]),
                Direction::Right,
                ShellId::from_raw(pair[1]),
            );
        }
        tree
    }

    fn tab(id: u32, shells: &[u32]) -> Tab {
        Tab::restore(TabId::from_raw(id), "", row(shells), None).expect("nothing is focused")
    }

    #[test]
    fn a_restored_tab_is_focused_where_it_was_told_or_on_its_first_shell() {
        let shell = ShellId::from_raw(2);
        assert_eq!(
            Tab::restore(TabId::FIRST, "one", row(&[1, 2, 3]), Some(shell))
                .expect("the tab holds that shell")
                .focused(),
            shell
        );
        assert_eq!(
            tab(0, &[1, 2, 3]).focused(),
            ShellId::from_raw(1),
            "the arrangement's first shell takes focus"
        );
        assert_eq!(
            Tab::restore(
                TabId::FIRST,
                "one",
                row(&[1, 2]),
                Some(ShellId::from_raw(9))
            ),
            Err(MalformedLayout::FocusElsewhere(ShellId::from_raw(9)))
        );
    }

    #[test]
    fn a_restored_workspace_carries_on_after_the_ids_it_holds() {
        let mut restored = Workspace::restore(
            WorkspaceId::from_raw(1),
            "/home/someone/projects/thing",
            "thing",
            WorkspaceSettings::default(),
            vec![tab(0, &[0, 1]), tab(4, &[7])],
        )
        .expect("nothing is in two places");

        let opened = restored.open_tab("next");
        assert_eq!(opened, TabId::from_raw(5), "after the highest tab restored");
        let fresh = restored
            .split(
                opened,
                restored.tab(opened).unwrap().focused(),
                Direction::Down,
            )
            .expect("the tab was just opened");
        assert_eq!(
            fresh,
            ShellId::from_raw(9),
            "after the highest shell restored, and the one the new tab took"
        );
    }

    #[test]
    fn a_workspace_that_holds_one_thing_twice_is_refused() {
        let restore = |tabs| {
            Workspace::restore(
                WorkspaceId::FIRST,
                "/home/someone/projects/thing",
                "thing",
                WorkspaceSettings::default(),
                tabs,
            )
        };

        assert_eq!(
            restore(vec![tab(0, &[0, 1]), tab(0, &[2])]),
            Err(MalformedLayout::DuplicateTab(TabId::FIRST))
        );
        assert_eq!(
            restore(vec![tab(0, &[0, 1]), tab(1, &[1])]),
            Err(MalformedLayout::DuplicateShell(ShellId::from_raw(1)))
        );
    }

    #[test]
    fn a_layout_hands_out_a_workspace_id_per_folder_and_gives_them_back_on_closing() {
        let mut layout = Layout::new();
        assert!(layout.is_empty());

        let first = layout.open("/home/someone/projects/thing");
        let second = layout.open("/home/someone/projects/thing");
        assert_ne!(first, second, "one folder opened twice is two workspaces");
        assert_eq!(layout.workspaces().len(), 2);
        assert_eq!(layout.workspace(first).map(Workspace::name), Some("thing"));

        layout.workspace_mut(first).unwrap().set_name("renamed");
        assert_eq!(
            layout.workspace(first).map(Workspace::name),
            Some("renamed")
        );
        assert_eq!(
            layout.workspace(second).map(Workspace::name),
            Some("thing"),
            "the other workspace on the same folder is untouched"
        );

        assert!(layout.close(first));
        assert!(!layout.close(first));
        assert_eq!(layout.workspaces().len(), 1);
        assert!(layout.close(second));
        assert!(layout.is_empty());
    }

    #[test]
    fn a_restored_layout_carries_on_after_the_workspaces_it_holds() {
        let workspace = |id| Workspace::new(WorkspaceId::from_raw(id), "/home/someone/thing");

        let mut restored =
            Layout::restore(vec![workspace(0), workspace(3)]).expect("two workspaces");
        assert_eq!(
            restored.open("/home/someone/other"),
            WorkspaceId::from_raw(4)
        );

        assert_eq!(
            Layout::restore(vec![workspace(3), workspace(3)]),
            Err(MalformedLayout::DuplicateWorkspace(WorkspaceId::from_raw(
                3
            )))
        );
        assert_eq!(
            Layout::restore(Vec::new())
                .expect("nothing open")
                .open("/x"),
            WorkspaceId::FIRST
        );
    }

    #[test]
    fn a_workspaces_shell_knows_the_string_it_is_known_by_outside() {
        let mut workspace = workspace();
        let tab = workspace.open_tab("one");
        let shell = workspace.tab(tab).unwrap().focused();
        assert_eq!(workspace.correlation(shell), "w9:s0");
    }
}
