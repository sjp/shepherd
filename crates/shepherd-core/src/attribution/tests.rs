use agentbus_protocol::SessionStatus::{Blocked, Done, Idle, Working};
use agentbus_protocol::{Agent, SessionStatus, Snapshot, Timestamp};

use crate::bus::{BusState, Update};
use crate::split::Direction;

use super::*;

/// The time these sessions are stamped with, which nothing here depends on.
fn ts() -> Timestamp {
    Timestamp::parse("2026-08-17T10:31:02.006Z").expect("a well-formed timestamp")
}

/// One session as the bus reports it: no correlation, no directory, and nothing
/// yet said about where it is running. The builders below add whichever of those
/// a test is about.
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

/// A session stamped with a correlation string, whoever wrote it.
fn correlated(mut session: SessionEntry, correlation: &str) -> SessionEntry {
    session.correlation = Some(correlation.to_owned());
    session
}

/// A session the bus knows the working directory of.
fn in_dir(mut session: SessionEntry, cwd: &str) -> SessionEntry {
    session.cwd = Some(cwd.to_owned());
    session
}

/// A session nothing but an observer ever knew about.
fn observed(mut session: SessionEntry) -> SessionEntry {
    session.source = Source::Observed;
    session
}

/// A hook-backed session whose *displayed* status is an observer's live claim
/// standing over its own quieter record.
fn upgraded(mut session: SessionEntry) -> SessionEntry {
    session.status_source = Some(Source::Observed);
    session
}

/// A workspace on `path` with `shells` shells side by side in one tab.
fn workspace(id: u32, path: &str, shells: usize) -> Workspace {
    let mut workspace = Workspace::new(WorkspaceId::from_raw(id), path);
    let tab = workspace.open_tab("one");
    let mut last = workspace.tab(tab).expect("the tab just opened").focused();
    for _ in 1..shells {
        last = workspace
            .split(tab, last, Direction::Right)
            .expect("the tab holds the shell being split");
    }
    workspace
}

/// The address of one of a workspace's shells, counting from the left of its
/// only tab.
fn shell(workspace: &Workspace, index: usize) -> ShellAddress {
    ShellAddress::new(workspace.id(), workspace.shells()[index])
}

/// The session ids attributed to one shell, in the order they were attributed.
fn ids_at(attribution: &Attribution, shell: ShellAddress) -> Vec<String> {
    attribution
        .sessions_at(shell)
        .iter()
        .map(|session| session.session.clone())
        .collect()
}

/// The session ids that could not be placed.
fn ids_elsewhere(attribution: &Attribution) -> Vec<String> {
    attribution
        .elsewhere()
        .iter()
        .map(|session| session.session.clone())
        .collect()
}

#[test]
fn a_correlation_naming_a_live_shell_places_the_session_in_it() {
    let open = workspace(9, "/src/thing", 2);
    let attribution = Attribution::derive(
        [&open],
        [correlated(
            session("a", Working),
            &open.correlation(shell(&open, 1).shell),
        )],
    );

    assert_eq!(ids_at(&attribution, shell(&open, 1)), ["a"]);
    assert_eq!(ids_at(&attribution, shell(&open, 0)), Vec::<String>::new());
    assert!(attribution.elsewhere().is_empty());
}

#[test]
fn an_attributed_session_shows_at_its_shell_its_tab_and_its_workspace() {
    let open = workspace(9, "/src/thing", 2);
    let busy = shell(&open, 1);
    let attribution = Attribution::derive(
        [&open],
        [correlated(
            session("a", Blocked),
            &open.correlation(busy.shell),
        )],
    );

    assert_eq!(attribution.status_at(busy).status, Blocked.into());
    assert_eq!(attribution.status_at(shell(&open, 0)), ShellStatus::NONE);

    let tab = open.tabs().first().expect("the workspace has one tab");
    assert_eq!(
        tab.status(attribution.shell_status(open.id())),
        RollupStatus::from(Blocked)
    );
    assert_eq!(
        open.status(attribution.shell_status(open.id())),
        RollupStatus::from(Blocked)
    );
}

#[test]
fn a_shell_running_nothing_rolls_up_to_none() {
    let open = workspace(9, "/src/thing", 2);
    let attribution = Attribution::derive([&open], []);

    assert_eq!(attribution.status_at(shell(&open, 0)), ShellStatus::NONE);
    assert_eq!(
        open.status(attribution.shell_status(open.id())),
        RollupStatus::None
    );
    assert!(attribution.shells().next().is_none());
}

#[test]
fn several_sessions_in_one_shell_fold_to_the_most_urgent_of_them() {
    let open = workspace(9, "/src/thing", 2);
    let busy = shell(&open, 0);
    let correlation = open.correlation(busy.shell);
    let attribution = Attribution::derive(
        [&open],
        [
            correlated(session("a", Done), &correlation),
            correlated(session("b", Blocked), &correlation),
            correlated(session("c", Idle), &correlation),
        ],
    );

    assert_eq!(ids_at(&attribution, busy), ["a", "b", "c"]);
    assert_eq!(attribution.status_at(busy).status, Blocked.into());
    assert_eq!(
        open.status(attribution.shell_status(open.id())),
        RollupStatus::from(Blocked)
    );
}

#[test]
fn a_correlation_naming_a_shell_that_is_no_longer_open_is_not_attributed_to_anything() {
    let mut open = workspace(9, "/src/thing", 2);
    let closed = shell(&open, 1);
    let correlation = open.correlation(closed.shell);
    let tab = open.tabs()[0].id();
    open.close_shell(tab, closed.shell);

    let attribution = Attribution::derive(
        [&open],
        [
            correlated(session("gone", Working), &correlation),
            // A workspace that was never open here at all.
            correlated(session("stranger", Working), "w4:s0"),
        ],
    );

    assert_eq!(ids_elsewhere(&attribution), ["gone", "stranger"]);
    assert!(attribution.shells().next().is_none());
    assert_eq!(
        open.status(attribution.shell_status(open.id())),
        RollupStatus::None
    );
}

#[test]
fn a_session_whose_shell_has_closed_is_not_moved_onto_a_shell_that_is_still_open() {
    let mut open = workspace(9, "/src/thing", 2);
    let closed = shell(&open, 1);
    let correlation = open.correlation(closed.shell);
    let tab = open.tabs()[0].id();
    open.close_shell(tab, closed.shell);

    // Everything the directory rule asks for is true — the one remaining shell
    // has nothing attributed to it — and it is still not offered the session,
    // because the session already said where it belonged.
    let attribution = Attribution::derive(
        [&open],
        [in_dir(
            correlated(session("gone", Blocked), &correlation),
            "/src/thing",
        )],
    );

    assert_eq!(ids_elsewhere(&attribution), ["gone"]);
    assert_eq!(attribution.status_at(shell(&open, 0)), ShellStatus::NONE);
}

#[test]
fn a_session_with_no_correlation_lands_in_the_one_shell_it_could_be_running_in() {
    let open = workspace(9, "/src/thing", 1);
    let attribution = Attribution::derive(
        [&open],
        [
            in_dir(session("here", Working), "/src/thing"),
            // The same rule, a directory below.
            in_dir(session("deeper", Working), "/src/thing/crates/inner"),
        ],
    );

    // The first takes the only free shell; the second has nowhere left to go.
    assert_eq!(ids_at(&attribution, shell(&open, 0)), ["here"]);
    assert_eq!(ids_elsewhere(&attribution), ["deeper"]);
}

#[test]
fn a_directory_below_an_open_workspace_is_inside_it() {
    let open = workspace(9, "/src/thing", 1);
    let attribution = Attribution::derive(
        [&open],
        [in_dir(
            session("deeper", Working),
            "/src/thing/crates/inner",
        )],
    );

    assert_eq!(ids_at(&attribution, shell(&open, 0)), ["deeper"]);
}

#[test]
fn a_directory_that_could_be_in_either_of_two_workspaces_is_in_neither() {
    let outer = workspace(1, "/src", 1);
    let inner = workspace(2, "/src/thing", 1);
    let attribution = Attribution::derive(
        [&outer, &inner],
        [in_dir(session("ambiguous", Working), "/src/thing/sub")],
    );

    assert_eq!(ids_elsewhere(&attribution), ["ambiguous"]);
    assert!(attribution.shells().next().is_none());
}

#[test]
fn a_workspace_with_more_than_one_free_shell_is_not_guessed_between() {
    let open = workspace(9, "/src/thing", 2);
    let attribution =
        Attribution::derive([&open], [in_dir(session("which", Working), "/src/thing")]);

    assert_eq!(ids_elsewhere(&attribution), ["which"]);
    assert!(attribution.shells().next().is_none());
}

#[test]
fn a_directory_no_open_workspace_contains_places_nothing() {
    let open = workspace(9, "/src/thing", 1);
    let attribution = Attribution::derive(
        [&open],
        [
            in_dir(session("outside", Working), "/src/other"),
            // A folder whose *name* starts with the workspace's, which is not
            // the same as being inside it.
            in_dir(session("lookalike", Working), "/src/thing-other/sub"),
            // Nothing said about where it is running at all.
            session("nowhere", Working),
        ],
    );

    assert_eq!(
        ids_elsewhere(&attribution),
        ["outside", "lookalike", "nowhere"]
    );
    assert_eq!(attribution.status_at(shell(&open, 0)), ShellStatus::NONE);
}

#[test]
fn a_correlation_this_application_did_not_write_is_treated_as_no_correlation() {
    let open = workspace(9, "/src/thing", 1);
    let attribution = Attribution::derive(
        [&open],
        [in_dir(
            correlated(session("elsewhere-tooling", Working), "pane-%7"),
            "/src/thing",
        )],
    );

    assert_eq!(ids_at(&attribution, shell(&open, 0)), ["elsewhere-tooling"]);
}

#[test]
fn a_shell_a_correlated_session_has_claimed_is_not_free_for_a_guess() {
    let open = workspace(9, "/src/thing", 2);
    let claimed = shell(&open, 0);
    // The guess comes first in the bus's order, and is still made only after
    // every correlation has been honoured.
    let attribution = Attribution::derive(
        [&open],
        [
            in_dir(session("guessed", Working), "/src/thing"),
            correlated(
                session("claimed", Working),
                &open.correlation(claimed.shell),
            ),
        ],
    );

    assert_eq!(ids_at(&attribution, claimed), ["claimed"]);
    assert_eq!(ids_at(&attribution, shell(&open, 1)), ["guessed"]);
    assert!(attribution.elsewhere().is_empty());
}

#[test]
fn two_sessions_that_could_both_be_the_only_one_are_not_both_placed() {
    let open = workspace(9, "/src/thing", 1);
    let attribution = Attribution::derive(
        [&open],
        [
            in_dir(session("first", Blocked), "/src/thing"),
            in_dir(session("second", Working), "/src/thing"),
        ],
    );

    assert_eq!(ids_at(&attribution, shell(&open, 0)), ["first"]);
    assert_eq!(ids_elsewhere(&attribution), ["second"]);
}

#[test]
fn a_status_says_whether_the_agent_or_an_observer_is_the_one_saying_it() {
    let open = workspace(9, "/src/thing", 3);
    let correlations: Vec<String> = (0..3)
        .map(|index| open.correlation(shell(&open, index).shell))
        .collect();

    let attribution = Attribution::derive(
        [&open],
        [
            correlated(session("hooked", Blocked), &correlations[0]),
            correlated(observed(session("watched", Blocked)), &correlations[1]),
            correlated(upgraded(session("upgraded", Blocked)), &correlations[2]),
        ],
    );

    // The agent's own word.
    assert_eq!(
        attribution.status_at(shell(&open, 0)),
        ShellStatus {
            status: Blocked.into(),
            source: Some(Source::Hook),
        }
    );
    // A session only an observer ever knew about.
    assert_eq!(
        attribution.status_at(shell(&open, 1)).source,
        Some(Source::Observed)
    );
    // A hook-backed session whose displayed status is an observer's live claim:
    // it is the claim that is on screen, so it is the claim's provenance that
    // has to be reported.
    assert_eq!(
        attribution.status_at(shell(&open, 2)).source,
        Some(Source::Observed)
    );
}

#[test]
fn the_provenance_reported_is_that_of_the_session_whose_status_is_shown() {
    let open = workspace(9, "/src/thing", 2);
    let quiet = shell(&open, 0);
    let correlation = open.correlation(quiet.shell);

    // The observed session is the one that is blocked, so the badge is on its
    // word however authoritative the idle session beside it is.
    let attribution = Attribution::derive(
        [&open],
        [
            correlated(session("idle", Idle), &correlation),
            correlated(observed(session("blocked", Blocked)), &correlation),
        ],
    );
    assert_eq!(
        attribution.status_at(quiet),
        ShellStatus {
            status: Blocked.into(),
            source: Some(Source::Observed),
        }
    );

    // Where two sessions agree on the status being shown, the stronger evidence
    // for it is what gets reported.
    let attribution = Attribution::derive(
        [&open],
        [
            correlated(observed(session("watched", Blocked)), &correlation),
            correlated(session("hooked", Blocked), &correlation),
        ],
    );
    assert_eq!(
        attribution.status_at(quiet).source,
        Some(Source::Hook),
        "a hook and an observer both saying blocked is a hook saying blocked"
    );
}

#[test]
fn a_session_that_could_not_be_placed_keeps_everything_that_was_said_about_it() {
    let open = workspace(9, "/src/thing", 2);
    let attribution = Attribution::derive(
        [&open],
        [in_dir(
            observed(session("stray", Blocked)),
            "/somewhere/else",
        )],
    );

    let stray = attribution.elsewhere().first().expect("one stray session");
    assert_eq!(stray.cwd.as_deref(), Some("/somewhere/else"));
    assert_eq!(stray.status, Blocked);
    assert_eq!(status_source(stray), Source::Observed);
}

#[test]
fn every_shell_running_something_can_be_walked_in_one_pass() {
    let open = workspace(9, "/src/thing", 3);
    let attribution = Attribution::derive(
        [&open],
        [
            correlated(
                session("a", Working),
                &open.correlation(shell(&open, 2).shell),
            ),
            correlated(session("b", Idle), &open.correlation(shell(&open, 0).shell)),
        ],
    );

    let walked: Vec<(ShellAddress, Vec<String>)> = attribution
        .shells()
        .map(|(shell, sessions)| {
            (
                shell,
                sessions
                    .iter()
                    .map(|s| s.session.clone())
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(
        walked,
        vec![
            (shell(&open, 0), vec!["b".to_owned()]),
            (shell(&open, 2), vec!["a".to_owned()]),
        ],
        "shells with nothing attributed do not appear, and the rest are in order"
    );
}

#[test]
fn a_workspaces_shells_are_told_apart_from_another_workspaces_by_the_same_number() {
    let first = workspace(1, "/src/one", 1);
    let second = workspace(2, "/src/two", 1);
    assert_eq!(
        shell(&first, 0).shell,
        shell(&second, 0).shell,
        "both workspaces number their first shell the same"
    );

    let attribution = Attribution::derive(
        [&first, &second],
        [correlated(
            session("a", Blocked),
            &second.correlation(shell(&second, 0).shell),
        )],
    );

    assert_eq!(
        attribution.status_at(shell(&second, 0)).status,
        Blocked.into()
    );
    assert_eq!(attribution.status_at(shell(&first, 0)), ShellStatus::NONE);
    assert_eq!(
        first.status(attribution.shell_status(first.id())),
        RollupStatus::None
    );
}

#[test]
fn a_fresh_snapshot_is_attributed_from_scratch_rather_than_patched() {
    let open = workspace(9, "/src/thing", 2);
    let first = shell(&open, 0);
    let second = shell(&open, 1);
    let mut state = BusState::new();

    let reset = |sessions: Vec<SessionEntry>| Update::Reset(Snapshot::new(1, sessions));
    state.apply(
        &reset(vec![correlated(
            session("a", Blocked),
            &open.correlation(first.shell),
        )]),
        &ts(),
    );
    let attribution = Attribution::derive([&open], state.sessions());
    assert_eq!(attribution.status_at(first).status, Blocked.into());

    // A reconnection: the bus's new account has the first session gone and
    // another running somewhere else. Nothing of the old attribution survives,
    // because the new one is worked out from the new account alone.
    state.apply(
        &reset(vec![correlated(
            session("b", Idle),
            &open.correlation(second.shell),
        )]),
        &ts(),
    );
    let attribution = Attribution::derive([&open], state.sessions());
    assert_eq!(attribution.status_at(first), ShellStatus::NONE);
    assert_eq!(ids_at(&attribution, second), ["b"]);
    assert_eq!(
        open.status(attribution.shell_status(open.id())),
        RollupStatus::from(Idle)
    );
}

#[test]
fn closing_a_shell_moves_what_was_running_in_it_out_of_the_model() {
    let mut open = workspace(9, "/src/thing", 2);
    let closing = shell(&open, 1);
    let sessions = [correlated(
        session("a", Working),
        &open.correlation(closing.shell),
    )];

    let attribution = Attribution::derive([&open], sessions.clone());
    assert_eq!(ids_at(&attribution, closing), ["a"]);

    let tab = open.tabs()[0].id();
    open.close_shell(tab, closing.shell);
    let attribution = Attribution::derive([&open], sessions);
    assert_eq!(ids_elsewhere(&attribution), ["a"]);
    assert_eq!(
        open.status(attribution.shell_status(open.id())),
        RollupStatus::None
    );
}
