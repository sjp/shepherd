//! Rendering a snapshot as a table someone can read.
//!
//! The columns are the whole of what the bus knows about a session, in the order
//! someone scanning the output cares about it: which agent, which session, what
//! it is doing, and only then where it is and where it came from. Nothing is
//! summarized away — `correlation` in particular is printed exactly as it
//! arrived, because it is an opaque string whose meaning belongs to whoever set
//! it and this command has no business interpreting it.

use agentbus_protocol::{SessionEntry, SessionStatus, Snapshot, Timestamp};

use crate::table::{self, ABSENT, Row};

/// The column headings, in order.
const HEADINGS: [&str; 8] = [
    "AGENT",
    "SESSION",
    "STATUS",
    "SOURCE",
    "ORIGIN",
    "CWD",
    "CORRELATION",
    "SINCE",
];

/// What is printed when the bus knows of no sessions at all.
const NO_SESSIONS: &str = "no sessions";

/// The table for `snapshot`, as of `now`, ending in a newline.
///
/// `styled` says whether the output is going somewhere escape sequences mean
/// something. They are used for one thing only: a session that is waiting on a
/// human is the reason this bus exists, and it should be the row someone's eye
/// lands on first.
pub fn render(snapshot: &Snapshot, now: &Timestamp, styled: bool) -> String {
    if snapshot.sessions.is_empty() {
        return format!("{NO_SESSIONS}\n");
    }

    let rows: Vec<Row> = snapshot
        .sessions
        .iter()
        .map(|session| row(session, now))
        .collect();
    table::render(&HEADINGS, &rows, styled)
}

/// One session's row, in the order of [`HEADINGS`].
///
/// A session that is waiting on a human is what somebody runs this command to
/// find out about, so it is the row that stands out.
fn row(session: &SessionEntry, now: &Timestamp) -> Row {
    Row::new(vec![
        session.agent.to_string(),
        session.session.clone(),
        session.status.to_string(),
        session.source.to_string(),
        table::origin(&session.origin),
        session.cwd.clone().unwrap_or_else(|| ABSENT.to_owned()),
        session
            .correlation
            .clone()
            .unwrap_or_else(|| ABSENT.to_owned()),
        table::elapsed(now, &session.since),
    ])
    .emphasized(session.status == SessionStatus::Blocked)
}

#[cfg(test)]
mod tests {
    use super::*;

    use agentbus_protocol::{Agent, OriginHop, Source};

    /// Builds an agent id from a literal, which is what every one of these is.
    fn agent(name: &str) -> Agent {
        Agent::new(name).expect("a test's own agent id is a valid one")
    }

    fn timestamp(text: &str) -> Timestamp {
        Timestamp::parse(text).expect("not a timestamp")
    }

    fn now() -> Timestamp {
        timestamp("2026-08-17T10:35:00.000Z")
    }

    fn session(session: &str, status: SessionStatus) -> SessionEntry {
        SessionEntry {
            session: session.to_owned(),
            agent: agent("claude"),
            status,
            source: Source::Hook,
            cwd: Some("/workspaces/foo".to_owned()),
            correlation: Some("w9:p3".to_owned()),
            origin: Vec::new(),
            since: timestamp("2026-08-17T10:31:48.000Z"),
        }
    }

    fn rendered(sessions: Vec<SessionEntry>) -> String {
        render(&Snapshot::new(7, sessions), &now(), false)
    }

    /// The cells of one row, split where the headings say the columns are, so
    /// that a column a session left empty is still a cell rather than something
    /// that vanishes between two others.
    fn cells(table: &str, row: usize) -> Vec<String> {
        let headings = table.lines().next().expect("an empty table");
        let starts: Vec<usize> = HEADINGS
            .iter()
            .map(|heading| headings.find(heading).expect("a missing heading"))
            .collect();
        let line = table.lines().nth(row).expect("a missing row");
        starts
            .iter()
            .enumerate()
            .map(|(column, start)| {
                let end = starts.get(column + 1).copied().unwrap_or(line.len());
                line[*start..end.min(line.len())].trim_end().to_owned()
            })
            .collect()
    }

    #[test]
    fn a_snapshot_with_no_sessions_says_so() {
        assert_eq!(rendered(Vec::new()), "no sessions\n");
    }

    #[test]
    fn every_column_is_headed_and_filled() {
        let mut session = session("abc123", SessionStatus::Working);
        session.origin = vec![OriginHop::new(OriginHop::SSH, "9f3c:1000", "fileserver")];

        let table = rendered(vec![session]);

        assert_eq!(
            table
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<&str>>(),
            HEADINGS
        );
        assert_eq!(
            cells(&table, 1),
            [
                "claude",
                "abc123",
                "working",
                "hook",
                "fileserver",
                "/workspaces/foo",
                "w9:p3",
                "3m12s"
            ]
        );
        assert_eq!(table.lines().count(), 2);
    }

    #[test]
    fn an_observed_session_is_marked_as_one() {
        let mut observed = session("observed:w9:p3", SessionStatus::Working);
        observed.source = Source::Observed;

        let table = rendered(vec![observed]);

        let row = table.lines().nth(1).unwrap();
        assert!(row.contains("observed"), "{row}");
    }

    #[test]
    fn a_correlation_is_printed_exactly_as_it_arrived() {
        let mut session = session("abc123", SessionStatus::Working);
        session.correlation = Some("anything at all: 1/2/3".to_owned());

        let table = rendered(vec![session]);

        assert!(table.contains("anything at all: 1/2/3"), "{table}");
    }

    #[test]
    fn a_session_without_a_directory_or_a_correlation_shows_it_has_none() {
        let mut session = session("abc123", SessionStatus::Working);
        session.cwd = None;
        session.correlation = None;

        let table = rendered(vec![session]);

        assert_eq!(
            cells(&table, 1),
            ["claude", "abc123", "working", "hook", "", "-", "-", "3m12s"]
        );
    }

    #[test]
    fn a_remote_session_shows_where_it_came_from_and_a_local_one_shows_nothing() {
        let mut remote = session("abc123", SessionStatus::Working);
        remote.origin = vec![
            OriginHop::new(OriginHop::SSH, "9f3c:1000", "fileserver"),
            OriginHop::new(OriginHop::CONTAINER, "e41a", "devcontainer"),
        ];
        let local = session("def456", SessionStatus::Working);

        let table = rendered(vec![remote, local]);

        assert_eq!(cells(&table, 1)[4], "fileserver > devcontainer");
        assert_eq!(
            cells(&table, 2)[4],
            "",
            "a local session was given an origin"
        );
    }

    #[test]
    fn a_blocked_session_is_the_row_that_stands_out() {
        let sessions = vec![
            session("abc123", SessionStatus::Blocked),
            session("def456", SessionStatus::Working),
        ];

        let styled = render(&Snapshot::new(7, sessions), &now(), true);

        let blocked = styled.lines().nth(1).unwrap();
        assert!(blocked.contains('\x1b'), "{blocked}");
        assert!(!styled.lines().nth(2).unwrap().contains('\x1b'));
    }
}
