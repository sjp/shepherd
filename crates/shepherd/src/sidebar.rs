//! Everything that is open, and everything that is running in it.
//!
//! Down the left of the window: the workspaces somebody has open, the tabs in
//! each of them and the shells in each tab, every row carrying a badge saying
//! what the bus knows about what is running there — and beneath that, one flat
//! list of every agent session, so that "which of these needs me" is a question
//! answered by reading down a column rather than by opening tabs.
//!
//! # Two halves, and the seam between them
//!
//! [`Sidebar::of`] works out *what* is shown: rows, names, badges, the order
//! they go in. It takes the model, what the bus said and what has been folded
//! away, and it draws nothing — so what the sidebar would say about any
//! arrangement of shells and any set of sessions can be asked, and asserted, on
//! a machine with no display. [`Sidebar::drawn`] is the other half, and it is
//! only how it looks.
//!
//! # Nothing is worked out twice
//!
//! Every status here comes from the fold the rest of the application uses: a
//! shell's badge is what was attributed to it, a tab's is the fold over its
//! shells' badges, a workspace's is the fold over its tabs'. There is no second
//! precedence order in this file, and no second opinion about which shell a
//! session is running in — those questions are answered before anything gets
//! here, and re-answering them in a renderer is how two parts of one window
//! start disagreeing about what is blocked.
//!
//! # On whose word
//!
//! A status an agent reported through its own hooks and a status something
//! worked out by watching from outside are not the same claim, and the sidebar
//! must not present them as though they were: the second is a floor, and a
//! floor shown as an authority is how trust in the whole thing goes. So a badge
//! carries where its status came from as well as what it says, and an observed
//! one is drawn differently — marked, outlined and italic, rather than merely a
//! different colour among equals.
//!
//! # Loudest for the one that matters
//!
//! `blocked` is the only status that needs a person *now*, and it is the only
//! one drawn as a filled badge. The rest are text in a colour. That difference
//! survives being glanced at from across a room, which is the whole of what the
//! badge is for; the row it sits on may also be several folds deep, which is
//! why a folded tab and a folded workspace keep their badges and lose only
//! their children.

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;

use agentbus_protocol::SessionStatus;
use gpui::{
    AnyElement, App, ClickEvent, Context, CursorStyle, Div, ElementId, FontWeight, Hsla,
    InteractiveElement, IntoElement, ParentElement, Pixels, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{ActiveTheme, Theme};
use shepherd_core::{
    Attribution, Branches, Layout, RollupStatus, ShellAddress, ShellId, ShellStatus, TabId,
    WorkspaceId,
};

use crate::terminal::TerminalView;

/// How wide the sidebar is.
///
/// Fixed rather than dragged: what is in it is a list of short names, and the
/// room a list of short names needs does not change as somebody works.
const WIDTH: Pixels = px(232.0);

/// How big the text in it is.
const FONT_SIZE: Pixels = px(12.0);

/// And the text on a badge, which is smaller because it is a word nobody reads
/// letter by letter — its colour and its weight say most of it.
const BADGE_SIZE: Pixels = px(10.0);

/// How far one level of the tree is indented from the one above it.
const INDENT: f32 = 10.0;

/// The marker on a row whose children are shown.
const SHOWN: &str = "\u{25be}";

/// And on one whose children are folded away.
const HIDDEN: &str = "\u{25b8}";

/// What separates the parts of an agent's path through the tree.
const STEP: &str = " \u{203a} ";

/// What marks a status nothing but an observer vouches for.
///
/// A character rather than only a colour or a slant, because this is the one
/// distinction that has to survive a screenshot, a colour-blind reader and a
/// glance: what follows is the best anybody could tell from outside, not what
/// the agent itself said.
const APPROXIMATE: &str = "~";

/// What a shell with no name of its own is called.
const UNNAMED: &str = "shell";

/// What the heading over the flat list of agents says.
const AGENTS: &str = "agents";

/// And over the ones that could not be placed.
const ELSEWHERE: &str = "elsewhere";

/// What is said where the bus reported no directory for such a session.
const NOWHERE: &str = "directory not reported";

/// What the sidebar says when the bus is reporting no agents at all.
const NO_AGENTS: &str = "no agents running";

/// What the window is currently showing, which the sidebar marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Showing {
    /// The workspace the window is open on.
    pub workspace: WorkspaceId,
    /// Which of that workspace's tabs is on screen.
    pub tab: TabId,
    /// And which of that tab's shells is being typed in.
    pub shell: ShellId,
}

/// What somebody has folded away.
///
/// Held by the window rather than worked out from the model, because it is a
/// fact about a person looking at the window and not about what is open in it:
/// a tab that was folded stays folded while a shell in it starts and finishes
/// three agents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Folded {
    workspaces: BTreeSet<WorkspaceId>,
    /// Tabs are numbered within their workspace, so a tab is remembered by
    /// which workspace it is in as well as which tab it is.
    tabs: BTreeSet<(WorkspaceId, TabId)>,
}

impl Folded {
    /// Whether this workspace's tabs are folded away.
    pub fn workspace(&self, workspace: WorkspaceId) -> bool {
        self.workspaces.contains(&workspace)
    }

    /// Whether this tab's shells are.
    pub fn tab(&self, workspace: WorkspaceId, tab: TabId) -> bool {
        self.tabs.contains(&(workspace, tab))
    }

    /// Folds a workspace away, or unfolds it.
    pub fn fold_workspace(&mut self, workspace: WorkspaceId) {
        if !self.workspaces.remove(&workspace) {
            self.workspaces.insert(workspace);
        }
    }

    /// Folds a tab away, or unfolds it.
    pub fn fold_tab(&mut self, workspace: WorkspaceId, tab: TabId) {
        if !self.tabs.remove(&(workspace, tab)) {
            self.tabs.insert((workspace, tab));
        }
    }
}

/// What pressing a row in the sidebar asks the window for.
///
/// Every row that answers a press answers it with one of these, and the window
/// has one place that acts on them — so a shell reached from the tree and the
/// same shell reached from the list of agents cannot end up doing two subtly
/// different things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picked {
    /// Show this shell: its tab, and focus in it.
    Shell(ShellAddress),
    /// Show this tab, and whichever of its shells was last typed in.
    Tab(WorkspaceId, TabId),
    /// Fold this workspace's tabs away, or unfold them.
    FoldWorkspace(WorkspaceId),
    /// Fold this tab's shells away, or unfold them.
    FoldTab(WorkspaceId, TabId),
}

/// Everything the sidebar shows, and nothing about how it looks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sidebar {
    /// The tree, in the order the model holds it.
    pub workspaces: Vec<WorkspaceRow>,
    /// Every session the bus reported that has a shell here, in the order the
    /// tree above walks: workspace by workspace, tab by tab, shell by shell.
    pub agents: Vec<AgentRow>,
    /// And every session it reported that has not.
    pub elsewhere: Vec<ElsewhereRow>,
}

/// One workspace, and what is open in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    pub workspace: WorkspaceId,
    pub name: SharedString,
    /// What the folder is checked out on, where it is a repository at all.
    pub branch: Option<SharedString>,
    pub status: ShellStatus,
    pub folded: bool,
    /// Its tabs, or nothing at all when it is folded away. The badge above
    /// stays either way: a workspace folded shut is exactly when its badge is
    /// the only thing saying something in there needs attending to.
    pub tabs: Vec<TabRow>,
}

/// One tab of one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabRow {
    pub tab: TabId,
    pub name: SharedString,
    pub status: ShellStatus,
    /// Whether this is the tab the window is showing.
    pub showing: bool,
    pub folded: bool,
    /// Its shells, in arrangement order, or nothing when it is folded away.
    pub shells: Vec<ShellRow>,
}

/// One shell of one tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRow {
    pub address: ShellAddress,
    /// What it is called, which is whatever it is currently running unless
    /// somebody has named it.
    pub name: SharedString,
    pub status: ShellStatus,
    /// Whether this is the shell being typed in.
    pub focused: bool,
}

/// One agent session, in the flat list under the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    /// The shell it is running in, which is where pressing the row goes.
    pub address: ShellAddress,
    pub agent: SharedString,
    /// Where that shell is, written out: workspace, tab and shell.
    pub path: SharedString,
    /// This one session's own status — not the shell's fold over several,
    /// because this row is about one of them.
    pub status: ShellStatus,
}

/// One agent session the bus reported that this window has no shell for.
///
/// There is nowhere to go from here, so the row says where the session said it
/// was instead and does not answer a press. An agent somebody started in a
/// terminal of their own is still an agent that may be waiting on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElsewhereRow {
    pub agent: SharedString,
    pub status: ShellStatus,
    /// The working directory the bus reported for it, where it reported one.
    pub directory: Option<SharedString>,
}

impl Sidebar {
    /// Works out every row, from the model, what the bus said, what the folders
    /// are checked out on, and what has been folded away.
    ///
    /// `name` answers what one shell is currently called; a shell it has no
    /// answer for is one this window is not running a process for, which a
    /// workspace somebody has open in another window is full of.
    pub fn of(
        layout: &Layout,
        attribution: &Attribution,
        branches: &Branches,
        folded: &Folded,
        showing: Showing,
        name: impl Fn(ShellAddress) -> Option<String>,
    ) -> Self {
        let mut workspaces = Vec::new();
        let mut agents = Vec::new();

        for workspace in layout.workspaces() {
            let id = workspace.id();
            let mut tabs = Vec::new();
            for tab in workspace.tabs() {
                let mut shells = Vec::new();
                for shell in tab.shells() {
                    let address = ShellAddress::new(id, shell);
                    let called = name(address)
                        .map(SharedString::from)
                        .unwrap_or_else(|| SharedString::from(UNNAMED));
                    // The flat list is this same walk, one level further in, so
                    // that its order is the tree's own by construction rather
                    // than by two pieces of code agreeing to sort the same way.
                    agents.extend(attribution.sessions_at(address).iter().map(|session| {
                        AgentRow {
                            address,
                            agent: SharedString::from(session.agent.to_string()),
                            path: SharedString::from(format!(
                                "{}{STEP}{}{STEP}{called}",
                                workspace.name(),
                                tab.name()
                            )),
                            status: ShellStatus::of_session(session),
                        }
                    }));
                    shells.push(ShellRow {
                        address,
                        name: called,
                        status: attribution.status_at(address),
                        focused: showing.workspace == id && showing.shell == shell,
                    });
                }

                let status = ShellStatus::fold(shells.iter().map(|shell| shell.status));
                let folded_tab = folded.tab(id, tab.id());
                tabs.push(TabRow {
                    tab: tab.id(),
                    name: SharedString::from(tab.name().to_owned()),
                    status,
                    showing: showing.workspace == id && showing.tab == tab.id(),
                    folded: folded_tab,
                    shells: if folded_tab { Vec::new() } else { shells },
                });
            }

            let status = ShellStatus::fold(tabs.iter().map(|tab| tab.status));
            let folded_workspace = folded.workspace(id);
            workspaces.push(WorkspaceRow {
                workspace: id,
                name: SharedString::from(workspace.name().to_owned()),
                branch: branches
                    .of(id)
                    .map(|branch| SharedString::from(branch.to_owned())),
                status,
                folded: folded_workspace,
                tabs: if folded_workspace { Vec::new() } else { tabs },
            });
        }

        let elsewhere = attribution
            .elsewhere()
            .iter()
            .map(|session| ElsewhereRow {
                agent: SharedString::from(session.agent.to_string()),
                status: ShellStatus::of_session(session),
                directory: session
                    .cwd
                    .as_deref()
                    .map(|cwd| SharedString::from(cwd.to_owned())),
            })
            .collect();

        Self {
            workspaces,
            agents,
            elsewhere,
        }
    }

    /// The sidebar, on screen.
    pub fn drawn(&self, cx: &mut Context<TerminalView>) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        for workspace in &self.workspaces {
            rows.push(workspace_row(workspace, cx));
            for tab in &workspace.tabs {
                rows.push(tab_row(workspace.workspace, tab, cx));
                for shell in &tab.shells {
                    rows.push(shell_row(shell, cx));
                }
            }
        }

        div()
            .id("sidebar")
            .flex()
            .flex_col()
            .w(WIDTH)
            .flex_shrink_0()
            .h_full()
            .py_1()
            .gap_1()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .bg(cx.theme().sidebar)
            .text_size(FONT_SIZE)
            .text_color(cx.theme().sidebar_foreground)
            .children(rows)
            .child(heading(AGENTS, cx))
            .when(
                self.agents.is_empty() && self.elsewhere.is_empty(),
                |list| list.child(note(NO_AGENTS, cx)),
            )
            .children(self.agents.iter().map(|agent| agent_row(agent, cx)))
            .when(!self.elsewhere.is_empty(), |list| {
                list.child(heading(ELSEWHERE, cx)).children(
                    self.elsewhere
                        .iter()
                        .map(|session| elsewhere_row(session, cx)),
                )
            })
            .into_any_element()
    }
}

/// One workspace's row: what it is called, what it is checked out on, and what
/// everything in it rolls up to.
fn workspace_row(row: &WorkspaceRow, cx: &mut Context<TerminalView>) -> AnyElement {
    let workspace = row.workspace;
    line(("sidebar-workspace", workspace.raw()).into(), 0, cx)
        .child(marker(row.folded, cx))
        .child(name(row.name.clone()).font_weight(FontWeight::SEMIBOLD))
        .when_some(row.branch.clone(), |row, branch| {
            row.child(
                div()
                    .flex_shrink_0()
                    .max_w(px(80.0))
                    .truncate()
                    .text_color(cx.theme().muted_foreground)
                    .child(branch),
            )
        })
        .children(badge(row.status, cx))
        .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
            view.picked(Picked::FoldWorkspace(workspace), window, cx);
        }))
        .into_any_element()
}

/// One tab's row. Pressing the marker folds its shells away; pressing the rest
/// of it shows the tab.
fn tab_row(workspace: WorkspaceId, row: &TabRow, cx: &mut Context<TerminalView>) -> AnyElement {
    let tab = row.tab;
    line(("sidebar-tab", pair(workspace, tab.raw())).into(), 1, cx)
        .when(row.showing, |row| row.bg(cx.theme().sidebar_accent))
        .child(
            div()
                .id(("sidebar-fold-tab", pair(workspace, tab.raw())))
                .child(marker(row.folded, cx))
                .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                    // Otherwise this would go on to the row it is drawn in, and
                    // folding a tab away is not a way of asking to look at it.
                    cx.stop_propagation();
                    view.picked(Picked::FoldTab(workspace, tab), window, cx);
                })),
        )
        .child(name(row.name.clone()))
        .children(badge(row.status, cx))
        .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
            view.picked(Picked::Tab(workspace, tab), window, cx);
        }))
        .into_any_element()
}

/// One shell's row: what is running in it, and what the bus says that is doing.
fn shell_row(row: &ShellRow, cx: &mut Context<TerminalView>) -> AnyElement {
    let address = row.address;
    line(
        (
            "sidebar-shell",
            pair(address.workspace, address.shell.raw()),
        )
            .into(),
        2,
        cx,
    )
    .when(row.focused, |row| {
        row.bg(cx.theme().sidebar_accent)
            .text_color(cx.theme().sidebar_accent_foreground)
    })
    .child(name(row.name.clone()))
    .children(badge(row.status, cx))
    .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
        view.picked(Picked::Shell(address), window, cx);
    }))
    .into_any_element()
}

/// One agent's row in the flat list: which agent, where it is, what it is
/// doing.
fn agent_row(row: &AgentRow, cx: &mut Context<TerminalView>) -> AnyElement {
    let address = row.address;
    div()
        .id((
            "sidebar-agent",
            pair(address.workspace, address.shell.raw()),
        ))
        .flex()
        .flex_col()
        .px_2()
        .py_0p5()
        .cursor(CursorStyle::PointingHand)
        .hover(|row| row.bg(cx.theme().sidebar_accent))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(name(row.agent.clone()))
                .children(badge(row.status, cx)),
        )
        .child(
            div()
                .truncate()
                .text_color(cx.theme().muted_foreground)
                .child(row.path.clone()),
        )
        .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
            view.picked(Picked::Shell(address), window, cx);
        }))
        .into_any_element()
}

/// One agent the bus can see and this window cannot claim.
fn elsewhere_row(row: &ElsewhereRow, cx: &mut Context<TerminalView>) -> AnyElement {
    let directory = row
        .directory
        .clone()
        .unwrap_or_else(|| SharedString::from(NOWHERE));
    div()
        .flex()
        .flex_col()
        .px_2()
        .py_0p5()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(name(row.agent.clone()))
                .children(badge(row.status, cx)),
        )
        .child(
            div()
                .truncate()
                .text_color(cx.theme().muted_foreground)
                .child(directory),
        )
        .into_any_element()
}

/// The shape every row of the tree has: indented by its level, one line high,
/// and lit up as the pointer passes over it.
fn line(id: ElementId, level: usize, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .pr_2()
        .py_0p5()
        .pl(px(4.0 + INDENT * level as f32))
        .cursor(CursorStyle::PointingHand)
        .hover(|row| row.bg(cx.theme().sidebar_accent))
}

/// A name, which is what gives way when a row is too narrow for everything on
/// it.
fn name(text: SharedString) -> Div {
    div().flex_1().min_w_0().truncate().child(text)
}

/// The mark on a row saying whether what is under it is shown.
fn marker(folded: bool, cx: &App) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .w(px(10.0))
        .text_color(cx.theme().muted_foreground)
        .child(if folded { HIDDEN } else { SHOWN })
}

/// A heading over one of the lists under the tree.
fn heading(text: &'static str, cx: &App) -> AnyElement {
    div()
        .px_2()
        .pt_1()
        .mt_1()
        .border_t_1()
        .border_color(cx.theme().sidebar_border)
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

/// A line of explanation where a list would otherwise be empty.
fn note(text: &'static str, cx: &App) -> AnyElement {
    div()
        .px_2()
        .py_0p5()
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

/// One badge, or nothing at all where nothing is running.
fn badge(status: ShellStatus, cx: &App) -> Option<AnyElement> {
    let paint = Paint::of(status, cx.theme())?;
    Some(
        div()
            .flex_shrink_0()
            .px_1()
            .rounded_sm()
            .text_size(BADGE_SIZE)
            .text_color(paint.text)
            .when_some(paint.fill, |badge, fill| badge.bg(fill))
            .when_some(paint.outline, |badge, outline| {
                badge.border_1().border_color(outline)
            })
            .when(paint.bold, |badge| badge.font_weight(FontWeight::BOLD))
            .when(paint.italic, Styled::italic)
            .child(paint.label)
            .into_any_element(),
    )
}

/// How one badge is drawn.
///
/// Worked out apart from the element it becomes, because what makes a badge
/// legible — that `blocked` is heavier than everything else, and that a status
/// vouched for by an observer does not look like one an agent reported itself —
/// is a claim about these values, and one worth being able to assert.
#[derive(Debug, Clone, PartialEq)]
pub struct Paint {
    /// What it says, marked where the status is an outsider's reckoning.
    pub label: SharedString,
    pub text: Hsla,
    /// What is behind it. Filled is the loud one, and only `blocked` reported
    /// by the agent itself gets it.
    pub fill: Option<Hsla>,
    /// The line around it, which is what an observed status has instead of a
    /// fill.
    pub outline: Option<Hsla>,
    pub bold: bool,
    pub italic: bool,
}

impl Paint {
    /// How to draw `status`, or `None` where there is nothing to draw: a shell
    /// running an editor, a tab of plain shells, a workspace nobody has started
    /// an agent in. Nothing is an ordinary answer and it is drawn as nothing.
    pub fn of(status: ShellStatus, theme: &Theme) -> Option<Self> {
        let session = status.status.session()?;
        let observed = status.source.is_some_and(|source| !source.is_hook());
        let colour = colour(status.status, theme);
        let blocked = session == SessionStatus::Blocked;
        let label = if observed {
            SharedString::from(format!("{APPROXIMATE}{session}"))
        } else {
            SharedString::from(session.as_str())
        };

        Some(Self {
            label,
            // Filled, the word is read against the fill rather than the
            // sidebar, so it takes the colour meant to be read on it.
            text: if blocked && !observed {
                theme.danger_foreground
            } else {
                colour
            },
            fill: (blocked && !observed).then_some(theme.danger),
            outline: observed.then_some(colour),
            bold: blocked,
            italic: observed,
        })
    }
}

/// The colour one status is said in.
///
/// Every one of them differs from every other, because a badge is read by its
/// colour before it is read by its word — and they run warm to cool the way the
/// precedence runs, from the one that needs somebody now to the one that is
/// over.
fn colour(status: RollupStatus, theme: &Theme) -> Hsla {
    use SessionStatus::{Blocked, Done, Idle, Stale, Starting, Working};

    match status.session() {
        Some(Blocked) => theme.danger,
        Some(Working) => theme.blue,
        Some(Starting) => theme.cyan,
        Some(Stale) => theme.yellow,
        Some(Idle) => theme.muted_foreground,
        Some(Done) => theme.green,
        None => theme.muted_foreground,
    }
}

/// Two of the model's numbers as one, for naming an element that belongs to a
/// tab or a shell of a particular workspace. Tabs and shells are numbered
/// within their own workspace, so neither number is a name on its own.
fn pair(workspace: WorkspaceId, within: u32) -> u64 {
    (u64::from(workspace.raw()) << 32) | u64::from(within)
}
