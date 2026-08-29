//! What the sidebar would show, asked without a window to show it in.
//!
//! [`Sidebar::of`] takes the model, what the bus said and what has been folded
//! away, and answers with rows — so every question about what the sidebar says
//! for a given arrangement of shells and a given set of sessions is a question
//! about a value, answerable on a machine with no display. The few tests that
//! do need a window need it only for the palette a badge is painted from.

use std::fs;

use agentbus_protocol::SessionStatus::{Blocked, Done, Idle, Starting, Working};
use agentbus_protocol::{Agent, SessionEntry, SessionStatus, Snapshot, Source, Timestamp};
use gpui::TestAppContext;
use shepherd_core::{Attribution, BusState, Direction, Layout, Update, WorkspaceId};

use super::*;

/// The time these sessions are stamped with, which nothing here depends on.
fn ts() -> Timestamp {
    Timestamp::parse("2026-08-17T10:31:02.006Z").expect("a well-formed timestamp")
}

/// One session as the bus reports it, running nowhere in particular.
fn session(id: &str, status: SessionStatus) -> SessionEntry {
    SessionEntry {
        session: id.to_owned(),
        agent: Agent::new("claude").expect("a valid agent id"),
        status,
        source: Source::Hook,
        status_source: None,
        cwd: None,
        correlation: None,
        origin: Vec::new(),
        since: ts(),
    }
}

/// The same session, running in one of this application's shells.
fn running_in(layout: &Layout, shell: ShellAddress, mut session: SessionEntry) -> SessionEntry {
    let workspace = layout
        .workspace(shell.workspace)
        .expect("a workspace that is open");
    session.correlation = Some(workspace.correlation(shell.shell));
    session
}

/// A session whose status is an observer's reckoning rather than the agent's
/// own word.
fn observed(mut session: SessionEntry) -> SessionEntry {
    session.status_source = Some(Source::Observed);
    session
}

/// Two workspaces with something open in each: `alpha` holding a tab of three
/// shells arranged in an L and a tab of one, and `beta` a tab of two.
///
/// The L is the point of the first tab. An arrangement of three shells that is
/// not a row or a column is where a sidebar that quietly assumed one would come
/// apart, and the tree is meant to say what is open rather than what shape it
/// is in.
fn opened() -> Layout {
    let mut layout = Layout::new();
    let alpha = layout.open("/home/someone/alpha");
    {
        let workspace = layout.workspace_mut(alpha).expect("it just opened");
        let tab = workspace.open_tab("build");
        let first = workspace.tab(tab).expect("it just opened").focused();
        let second = workspace
            .split(tab, first, Direction::Right)
            .expect("the tab holds the shell being split");
        workspace
            .split(tab, second, Direction::Down)
            .expect("the tab holds the shell being split");
        workspace.open_tab("notes");
    }

    let beta = layout.open("/home/someone/beta");
    {
        let workspace = layout.workspace_mut(beta).expect("it just opened");
        let tab = workspace.open_tab("run");
        let only = workspace.tab(tab).expect("it just opened").focused();
        workspace
            .split(tab, only, Direction::Right)
            .expect("the tab holds the shell being split");
    }

    layout
}

/// The workspaces in the order the tree walks them.
fn workspaces(layout: &Layout) -> Vec<WorkspaceId> {
    layout
        .workspaces()
        .iter()
        .map(shepherd_core::Workspace::id)
        .collect()
}

/// The address of one of a workspace's shells, counting through its tabs.
fn shell_at(layout: &Layout, workspace: WorkspaceId, index: usize) -> ShellAddress {
    let open = layout
        .workspace(workspace)
        .expect("a workspace that is open");
    ShellAddress::new(open.id(), open.shells()[index])
}

/// What the window is showing while these tests look at it: the first shell of
/// the first tab of the first workspace.
fn showing(layout: &Layout) -> Showing {
    let workspace = layout.workspaces().first().expect("a workspace is open");
    let tab = workspace.tabs().first().expect("a tab is open");
    Showing {
        workspace: workspace.id(),
        tab: tab.id(),
        shell: tab.focused(),
    }
}

/// The sidebar for `layout` with `sessions` running, nothing folded away, no
/// folder looked at and no shell named.
fn sidebar(layout: &Layout, sessions: Vec<SessionEntry>) -> Sidebar {
    let attribution = Attribution::derive(layout.workspaces(), sessions);
    Sidebar::of(
        layout,
        &attribution,
        &Branches::new(),
        &Folded::default(),
        showing(layout),
        |_| None,
    )
}

/// Every row of the tree, as `(indent, name)`, so that a shape can be asserted
/// in one piece.
fn tree(sidebar: &Sidebar) -> Vec<(usize, String)> {
    let mut rows = Vec::new();
    for workspace in &sidebar.workspaces {
        rows.push((0, workspace.name.to_string()));
        for tab in &workspace.tabs {
            rows.push((1, tab.name.to_string()));
            for shell in &tab.shells {
                rows.push((2, shell.name.to_string()));
            }
        }
    }
    rows
}

/// What one shell's badge says.
fn badge_at(sidebar: &Sidebar, address: ShellAddress) -> ShellStatus {
    sidebar
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .flat_map(|tab| &tab.shells)
        .find(|shell| shell.address == address)
        .map(|shell| shell.status)
        .expect("the tree holds every shell that is open")
}

#[test]
fn the_tree_is_the_model_workspace_by_workspace_tab_by_tab_shell_by_shell() {
    let layout = opened();
    let sidebar = sidebar(&layout, Vec::new());

    assert_eq!(
        tree(&sidebar),
        [
            (0, "alpha".to_owned()),
            (1, "build".to_owned()),
            (2, "shell".to_owned()),
            (2, "shell".to_owned()),
            (2, "shell".to_owned()),
            (1, "notes".to_owned()),
            (2, "shell".to_owned()),
            (0, "beta".to_owned()),
            (1, "run".to_owned()),
            (2, "shell".to_owned()),
            (2, "shell".to_owned()),
        ]
    );

    let [alpha, beta] = workspaces(&layout)[..] else {
        panic!("two workspaces are open");
    };
    let shells: Vec<ShellAddress> = sidebar
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .flat_map(|tab| &tab.shells)
        .map(|shell| shell.address)
        .collect();
    assert_eq!(
        shells,
        [
            shell_at(&layout, alpha, 0),
            shell_at(&layout, alpha, 1),
            shell_at(&layout, alpha, 2),
            shell_at(&layout, alpha, 3),
            shell_at(&layout, beta, 0),
            shell_at(&layout, beta, 1),
        ],
        "every shell of every tab, in the order the arrangement holds them"
    );
}

#[test]
fn a_shell_is_called_whatever_is_running_in_it() {
    let layout = opened();
    let first = shell_at(&layout, workspaces(&layout)[0], 0);
    let sidebar = Sidebar::of(
        &layout,
        &Attribution::default(),
        &Branches::new(),
        &Folded::default(),
        showing(&layout),
        |address| (address == first).then(|| "claude".to_owned()),
    );

    let named: Vec<String> = tree(&sidebar)
        .into_iter()
        .filter(|(indent, _)| *indent == 2)
        .map(|(_, name)| name)
        .collect();
    assert_eq!(named[0], "claude");
    assert!(
        named[1..].iter().all(|name| name == "shell"),
        "a shell this window is running no process for is called what a shell is called"
    );
}

#[test]
fn the_shell_being_typed_in_and_the_tab_on_screen_are_marked() {
    let layout = opened();
    let sidebar = sidebar(&layout, Vec::new());
    let showing = showing(&layout);

    let marked: Vec<ShellAddress> = sidebar
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .flat_map(|tab| &tab.shells)
        .filter(|shell| shell.focused)
        .map(|shell| shell.address)
        .collect();
    assert_eq!(
        marked,
        [ShellAddress::new(showing.workspace, showing.shell)],
        "one shell is being typed in"
    );

    let tabs: Vec<bool> = sidebar
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .map(|tab| tab.showing)
        .collect();
    assert_eq!(tabs, [true, false, false], "one tab is on screen");
}

#[test]
fn a_workspace_says_what_its_folder_is_checked_out_on() {
    let folder = tempfile::tempdir().expect("a temporary directory");
    fs::create_dir(folder.path().join(".git")).expect("somewhere to write the metadata");
    fs::write(
        folder.path().join(".git").join("HEAD"),
        "ref: refs/heads/topic\n",
    )
    .expect("a file to write");

    let mut layout = Layout::new();
    let workspace = layout.open(folder.path());
    layout
        .workspace_mut(workspace)
        .expect("it just opened")
        .open_tab("one");
    let mut branches = Branches::new();
    branches.focused(layout.workspace(workspace).expect("it just opened"));

    let sidebar = Sidebar::of(
        &layout,
        &Attribution::default(),
        &branches,
        &Folded::default(),
        showing(&layout),
        |_| None,
    );

    assert_eq!(
        sidebar.workspaces[0]
            .branch
            .as_ref()
            .map(SharedString::as_ref),
        Some("topic"),
        "the branch the folder is on, beside its name"
    );
    assert_eq!(
        self::sidebar(&opened(), Vec::new()).workspaces[0].branch,
        None,
        "a folder nobody has looked at says nothing about a branch"
    );
}

#[test]
fn a_shells_badge_is_what_the_bus_says_is_running_in_it() {
    let layout = opened();
    let first = shell_at(&layout, workspaces(&layout)[0], 0);
    let second = shell_at(&layout, workspaces(&layout)[0], 1);

    let sidebar = sidebar(
        &layout,
        vec![running_in(&layout, first, session("s1", Working))],
    );

    assert_eq!(
        badge_at(&sidebar, first),
        ShellStatus {
            status: Working.into(),
            source: Some(Source::Hook),
        }
    );
    assert_eq!(
        badge_at(&sidebar, second),
        ShellStatus::NONE,
        "a shell with no agent in it is not a shell in an unknown state"
    );
}

#[test]
fn a_shell_hosting_several_sessions_shows_the_most_urgent_of_them() {
    let layout = opened();
    let first = shell_at(&layout, workspaces(&layout)[0], 0);

    let sidebar = sidebar(
        &layout,
        vec![
            running_in(&layout, first, session("s1", Idle)),
            running_in(&layout, first, session("s2", Blocked)),
            running_in(&layout, first, session("s3", Working)),
        ],
    );

    assert_eq!(badge_at(&sidebar, first).status, Blocked.into());
}

#[test]
fn a_badge_follows_the_bus_as_the_bus_changes_its_mind() {
    let layout = opened();
    let shell = shell_at(&layout, workspaces(&layout)[0], 2);
    let mut state = BusState::new();

    let mut said = |status| {
        let entry = running_in(&layout, shell, session("s1", status));
        state.apply(&Update::Reset(Snapshot::new(1, vec![entry])), &ts());
        let attribution = Attribution::derive(layout.workspaces(), state.sessions());
        let sidebar = Sidebar::of(
            &layout,
            &attribution,
            &Branches::new(),
            &Folded::default(),
            showing(&layout),
            |_| None,
        );
        (
            badge_at(&sidebar, shell).status,
            sidebar.workspaces[0].status.status,
        )
    };

    assert_eq!(said(Starting), (Starting.into(), Starting.into()));
    assert_eq!(said(Working), (Working.into(), Working.into()));
    assert_eq!(said(Blocked), (Blocked.into(), Blocked.into()));
    assert_eq!(said(Done), (Done.into(), Done.into()));
}

#[test]
fn one_blocked_shell_is_blocked_at_its_tab_and_at_its_workspace() {
    let layout = opened();
    let [alpha, beta] = workspaces(&layout)[..] else {
        panic!("two workspaces are open");
    };
    // The third shell of the first tab: neither the shell being typed in nor
    // the one a glance would land on anyway.
    let blocked = shell_at(&layout, alpha, 2);

    let sidebar = sidebar(
        &layout,
        vec![
            running_in(&layout, blocked, session("s1", Blocked)),
            running_in(&layout, shell_at(&layout, alpha, 0), session("s2", Idle)),
            running_in(&layout, shell_at(&layout, beta, 0), session("s3", Working)),
        ],
    );

    assert_eq!(sidebar.workspaces[0].status.status, Blocked.into());
    assert_eq!(sidebar.workspaces[0].tabs[0].status.status, Blocked.into());
    assert_eq!(
        sidebar.workspaces[0].tabs[1].status,
        ShellStatus::NONE,
        "the tab next to it has nothing running in it and says so"
    );
    assert_eq!(
        sidebar.workspaces[1].status.status,
        Working.into(),
        "the other workspace rolls up its own shells and nobody else's"
    );
}

#[test]
fn folding_takes_away_the_rows_and_leaves_the_badge() {
    let layout = opened();
    let [alpha, _] = workspaces(&layout)[..] else {
        panic!("two workspaces are open");
    };
    let blocked = shell_at(&layout, alpha, 2);
    let sessions = vec![running_in(&layout, blocked, session("s1", Blocked))];
    let attribution = Attribution::derive(layout.workspaces(), sessions);

    let mut folded = Folded::default();
    let tab = layout
        .workspace(alpha)
        .expect("it is open")
        .tabs()
        .first()
        .expect("a tab is open")
        .id();
    folded.fold_tab(alpha, tab);
    let shut_tab = Sidebar::of(
        &layout,
        &attribution,
        &Branches::new(),
        &Folded::default(),
        showing(&layout),
        |_| None,
    );
    assert_eq!(shut_tab.workspaces[0].tabs[0].shells.len(), 3);

    let shut_tab = Sidebar::of(
        &layout,
        &attribution,
        &Branches::new(),
        &folded,
        showing(&layout),
        |_| None,
    );
    assert!(shut_tab.workspaces[0].tabs[0].folded);
    assert!(shut_tab.workspaces[0].tabs[0].shells.is_empty());
    assert_eq!(
        shut_tab.workspaces[0].tabs[0].status.status,
        Blocked.into(),
        "a tab folded shut is when its badge is the only thing saying this"
    );

    folded.fold_workspace(alpha);
    let shut_workspace = Sidebar::of(
        &layout,
        &attribution,
        &Branches::new(),
        &folded,
        showing(&layout),
        |_| None,
    );
    assert!(shut_workspace.workspaces[0].folded);
    assert!(shut_workspace.workspaces[0].tabs.is_empty());
    assert_eq!(
        shut_workspace.workspaces[0].status.status,
        Blocked.into(),
        "and so is a workspace folded shut"
    );
    assert_eq!(
        shut_workspace.agents.len(),
        1,
        "folding hides rows of the tree and never an agent"
    );
}

#[test]
fn the_list_of_agents_is_the_tree_in_one_column() {
    let layout = opened();
    let [alpha, beta] = workspaces(&layout)[..] else {
        panic!("two workspaces are open");
    };
    // Given to the bus in an order that is nobody's, to be sure the list is put
    // in the tree's rather than kept in the one it arrived in.
    let sidebar = sidebar(
        &layout,
        vec![
            running_in(&layout, shell_at(&layout, beta, 1), session("s1", Idle)),
            running_in(&layout, shell_at(&layout, alpha, 3), session("s2", Working)),
            running_in(&layout, shell_at(&layout, alpha, 0), session("s3", Blocked)),
        ],
    );

    assert_eq!(
        sidebar
            .agents
            .iter()
            .map(|agent| agent.address)
            .collect::<Vec<ShellAddress>>(),
        [
            shell_at(&layout, alpha, 0),
            shell_at(&layout, alpha, 3),
            shell_at(&layout, beta, 1),
        ]
    );
    assert_eq!(
        sidebar.agents[0].path, "alpha \u{203a} build \u{203a} shell",
        "where the shell it is running in sits, written out"
    );
    assert_eq!(
        sidebar.agents[1].path,
        "alpha \u{203a} notes \u{203a} shell"
    );
    assert_eq!(sidebar.agents[2].path, "beta \u{203a} run \u{203a} shell");
    assert_eq!(sidebar.agents[0].agent, "claude");
    assert_eq!(
        sidebar.agents[0].status.status,
        Blocked.into(),
        "a row in this list says what that one session is doing"
    );
    assert!(sidebar.elsewhere.is_empty());
}

#[test]
fn a_shell_running_two_agents_has_a_row_for_each_of_them() {
    let layout = opened();
    let shell = shell_at(&layout, workspaces(&layout)[0], 0);
    let sidebar = sidebar(
        &layout,
        vec![
            running_in(&layout, shell, session("s1", Working)),
            running_in(&layout, shell, session("s2", Blocked)),
        ],
    );

    assert_eq!(sidebar.agents.len(), 2);
    assert_eq!(sidebar.agents[0].status.status, Working.into());
    assert_eq!(sidebar.agents[1].status.status, Blocked.into());
    assert_eq!(
        badge_at(&sidebar, shell).status,
        Blocked.into(),
        "the shell they are both in shows the more urgent of them"
    );
}

#[test]
fn an_agent_that_cannot_be_placed_is_kept_apart_with_where_it_said_it_was() {
    let layout = opened();
    let mut somewhere_else = session("s1", Blocked);
    somewhere_else.cwd = Some("/home/someone/gamma".to_owned());
    let mut nowhere = session("s2", Idle);
    nowhere.correlation = Some("w9000:s9000".to_owned());

    let sidebar = sidebar(&layout, vec![somewhere_else, nowhere]);

    assert!(
        sidebar.agents.is_empty(),
        "neither of them is running in a shell this window has"
    );
    let said: Vec<(RollupStatus, Option<&str>)> = sidebar
        .elsewhere
        .iter()
        .map(|row| {
            (
                row.status.status,
                row.directory.as_ref().map(SharedString::as_ref),
            )
        })
        .collect();
    assert_eq!(
        said,
        [
            // The one carrying a correlation this application wrote, for a
            // shell that is not open, was placed — as unplaceable — before the
            // one that had to be guessed at from a directory.
            (Idle.into(), None),
            (Blocked.into(), Some("/home/someone/gamma")),
        ],
        "each says where it said it was, and nothing is invented for the one that said nothing"
    );
    assert!(
        sidebar
            .workspaces
            .iter()
            .all(|workspace| workspace.status == ShellStatus::NONE),
        "an agent nothing here is running does not badge anything here"
    );
}

#[test]
fn a_status_only_an_observer_vouches_for_is_still_only_that_at_every_level() {
    let layout = opened();
    let [alpha, _] = workspaces(&layout)[..] else {
        panic!("two workspaces are open");
    };
    let watched = shell_at(&layout, alpha, 2);

    let sidebar = sidebar(
        &layout,
        vec![
            running_in(&layout, watched, observed(session("s1", Blocked))),
            running_in(&layout, shell_at(&layout, alpha, 0), session("s2", Idle)),
        ],
    );

    let outsiders = ShellStatus {
        status: Blocked.into(),
        source: Some(Source::Observed),
    };
    assert_eq!(badge_at(&sidebar, watched), outsiders);
    assert_eq!(
        sidebar.workspaces[0].tabs[0].status, outsiders,
        "a tab standing for it says it is standing for a reckoning, not a report"
    );
    assert_eq!(
        sidebar.workspaces[0].status, outsiders,
        "and so does the workspace above it"
    );
    assert_eq!(sidebar.agents[1].status, outsiders);
    assert_eq!(
        sidebar.agents[0].status.source,
        Some(Source::Hook),
        "the agent beside it that did report for itself is unaffected"
    );
}

/// What the badge for `status` looks like, in the palette a window would paint
/// it from.
fn paint(cx: &mut TestAppContext, status: ShellStatus) -> Option<Paint> {
    cx.update(|cx| {
        gpui_component::init(cx);
        Paint::of(status, cx.theme())
    })
}

/// A badge on an agent's own word.
fn hook(status: SessionStatus) -> ShellStatus {
    ShellStatus {
        status: status.into(),
        source: Some(Source::Hook),
    }
}

/// The same status, on an observer's.
fn watched(status: SessionStatus) -> ShellStatus {
    ShellStatus {
        status: status.into(),
        source: Some(Source::Observed),
    }
}

#[gpui::test]
fn nothing_running_is_drawn_as_nothing(cx: &mut TestAppContext) {
    assert_eq!(
        paint(cx, ShellStatus::NONE),
        None,
        "a shell running an editor is not a shell in an error state"
    );
}

#[gpui::test]
fn blocked_is_the_only_badge_with_weight_behind_it(cx: &mut TestAppContext) {
    let blocked = paint(cx, hook(Blocked)).expect("something to draw");
    assert!(blocked.fill.is_some(), "it is filled, not merely coloured");
    assert!(blocked.bold);
    assert_eq!(blocked.label, "blocked");

    for status in SessionStatus::ALL {
        if status == Blocked {
            continue;
        }
        let quieter = paint(cx, hook(status)).expect("something to draw");
        assert_eq!(
            quieter.fill, None,
            "{status} is text in a colour, so that blocked is the one that shouts"
        );
        assert!(!quieter.bold, "{status} is not drawn in bold");
        assert_ne!(
            quieter.text, blocked.text,
            "{status} does not look like blocked"
        );
    }
}

#[gpui::test]
fn every_status_is_a_colour_of_its_own(cx: &mut TestAppContext) {
    let mut painted = Vec::new();
    for status in SessionStatus::ALL {
        let paint = paint(cx, hook(status)).expect("something to draw");
        assert_eq!(paint.label, status.as_str());
        assert!(
            !painted.contains(&paint.text),
            "{status} is drawn in a colour another status already uses"
        );
        painted.push(paint.text);
    }
}

#[gpui::test]
fn an_observed_status_is_not_drawn_like_one_the_agent_reported(cx: &mut TestAppContext) {
    for status in SessionStatus::ALL {
        let reported = paint(cx, hook(status)).expect("something to draw");
        let watched = paint(cx, watched(status)).expect("something to draw");

        assert_ne!(watched, reported, "{status} looks the same either way");
        assert!(
            watched.label.starts_with(APPROXIMATE),
            "{status} on an observer's word is marked as the reckoning it is"
        );
        assert!(watched.italic && !reported.italic);
        assert!(
            watched.outline.is_some() && reported.outline.is_none(),
            "{status} is outlined where it is an outsider's word and not otherwise"
        );
    }

    let watched = paint(cx, watched(Blocked)).expect("something to draw");
    assert_eq!(
        watched.fill, None,
        "a blocked nobody has heard from the agent about is not given the loud badge"
    );
    assert!(
        watched.bold,
        "it is still the status that needs somebody now"
    );
}
