//! Rendering a snapshot as a table someone can read.
//!
//! The columns are the whole of what the bus knows about a session, in the order
//! someone scanning the output cares about it: which agent, which session, what
//! it is doing, and only then where it is and where it came from. Nothing is
//! summarized away — `correlation` in particular is printed exactly as it
//! arrived, because it is an opaque string whose meaning belongs to whoever set
//! it and this command has no business interpreting it.

use std::fmt::Write as _;

use agentbus_protocol::{OriginHop, SessionEntry, SessionStatus, Snapshot, Timestamp};

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

/// What separates one column from the next.
const GAP: &str = "  ";

/// What is printed where a session has no value for a column.
///
/// Only for the fields a session may simply not have. An origin chain is never
/// absent — a local session has one of length zero — so an empty `ORIGIN` is the
/// truth about it rather than a gap in what is known.
const ABSENT: &str = "-";

/// Turns the escape sequences that make a row stand out on and off.
const BOLD: &str = "\x1b[1m";
const PLAIN: &str = "\x1b[0m";

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

    let rows: Vec<[String; 8]> = snapshot
        .sessions
        .iter()
        .map(|session| row(session, now))
        .collect();
    let widths = widths(&rows);

    let mut table = String::new();
    let headings = HEADINGS.map(str::to_owned);
    let _ = writeln!(table, "{}", line(&headings, &widths));
    for (session, row) in snapshot.sessions.iter().zip(&rows) {
        let text = line(row, &widths);
        let emphasized = styled && session.status == SessionStatus::Blocked;
        let _ = match emphasized {
            true => writeln!(table, "{BOLD}{text}{PLAIN}"),
            false => writeln!(table, "{text}"),
        };
    }
    table
}

/// One session's cells, in the order of [`HEADINGS`].
fn row(session: &SessionEntry, now: &Timestamp) -> [String; 8] {
    [
        session.agent.to_string(),
        session.session.clone(),
        session.status.to_string(),
        session.source.to_string(),
        origin(&session.origin),
        session.cwd.clone().unwrap_or_else(|| ABSENT.to_owned()),
        session
            .correlation
            .clone()
            .unwrap_or_else(|| ABSENT.to_owned()),
        elapsed(now, &session.since),
    ]
}

/// The origin chain as a path, outermost first, or empty for a local session.
///
/// A hop's name is display text and may be anything, including nothing; a hop
/// that has no name is shown by its identity rather than as a blank in the
/// middle of a path.
fn origin(hops: &[OriginHop]) -> String {
    hops.iter()
        .map(|hop| match hop.name.is_empty() {
            true => hop.id.as_str(),
            false => hop.name.as_str(),
        })
        .collect::<Vec<&str>>()
        .join(" > ")
}

/// How long ago `since` was, as a person would say it.
///
/// Units that are zero at the front are left out, so the value stays short
/// enough to scan down a column. A timestamp in the future — two machines
/// disagreeing about the clock — reads as no time at all rather than as a
/// negative duration nobody could act on.
fn elapsed(now: &Timestamp, since: &Timestamp) -> String {
    let seconds = now.millis_since(since).max(0) / 1_000;
    let (hours, minutes, seconds) = (seconds / 3_600, (seconds % 3_600) / 60, seconds % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m{seconds}s"),
        _ => format!("{hours}h{minutes}m{seconds}s"),
    }
}

/// How wide each column has to be to fit its heading and every cell under it.
///
/// Width is counted in characters rather than bytes, so that a path or a
/// correlation with non-ASCII in it lines up. It is not the whole truth — a
/// double-width glyph still occupies two columns — but it is the part that can
/// be got right without teaching this command about Unicode.
fn widths(rows: &[[String; 8]]) -> [usize; 8] {
    let mut widths = HEADINGS.map(str::len);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    widths
}

/// One row, padded to the column widths, with no trailing blanks.
fn line(cells: &[String; 8], widths: &[usize; 8]) -> String {
    let mut line = String::new();
    for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
        if index > 0 {
            line.push_str(GAP);
        }
        line.push_str(cell);
        // The last column is never padded, and neither is any column whose
        // remaining ones are all empty: trailing spaces are invisible until
        // somebody diffs the output.
        if cells[index + 1..].iter().any(|cell| !cell.is_empty()) {
            let padding = width.saturating_sub(cell.chars().count());
            line.extend(std::iter::repeat_n(' ', padding));
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    use agentbus_protocol::{Agent, Source};

    fn timestamp(text: &str) -> Timestamp {
        Timestamp::parse(text).expect("not a timestamp")
    }

    fn now() -> Timestamp {
        timestamp("2026-08-17T10:35:00.000Z")
    }

    fn session(session: &str, status: SessionStatus) -> SessionEntry {
        SessionEntry {
            session: session.to_owned(),
            agent: Agent::Claude,
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
    fn an_origin_chain_is_printed_outermost_first_and_is_empty_when_local() {
        let mut remote = session("abc123", SessionStatus::Working);
        remote.origin = vec![
            OriginHop::new(OriginHop::SSH, "9f3c:1000", "fileserver"),
            OriginHop::new(OriginHop::CONTAINER, "e41a", "devcontainer"),
        ];
        let local = session("def456", SessionStatus::Working);

        let table = rendered(vec![remote, local]);

        assert!(
            table.contains("fileserver > devcontainer"),
            "the chain is missing from {table}"
        );
        assert_eq!(
            cells(&table, 2)[4],
            "",
            "a local session was given an origin"
        );
    }

    #[test]
    fn a_hop_with_no_name_falls_back_to_its_identity() {
        let mut session = session("abc123", SessionStatus::Working);
        session.origin = vec![OriginHop::new(OriginHop::SSH, "9f3c:1000", "")];

        assert!(rendered(vec![session]).contains("9f3c:1000"));
    }

    #[test]
    fn columns_line_up_however_wide_the_values_are() {
        let table = rendered(vec![
            session("a", SessionStatus::Working),
            session("a-much-longer-session-id", SessionStatus::Idle),
        ]);

        let starts: Vec<Option<usize>> = table
            .lines()
            .map(|line| {
                line.find("working")
                    .or_else(|| line.find("idle"))
                    .or_else(|| line.find("STATUS"))
            })
            .collect();
        assert_eq!(starts[0], starts[1], "{table}");
        assert_eq!(starts[1], starts[2], "{table}");
    }

    #[test]
    fn a_row_has_no_trailing_blanks() {
        let table = rendered(vec![session("abc123", SessionStatus::Working)]);

        for line in table.lines() {
            assert_eq!(line, line.trim_end(), "trailing spaces in {line:?}");
        }
    }

    #[test]
    fn a_blocked_session_stands_out_only_where_that_means_something() {
        let sessions = vec![
            session("abc123", SessionStatus::Blocked),
            session("def456", SessionStatus::Working),
        ];

        let styled = render(&Snapshot::new(7, sessions.clone()), &now(), true);
        let plain = render(&Snapshot::new(7, sessions), &now(), false);

        let blocked = styled.lines().nth(1).unwrap();
        assert!(
            blocked.starts_with(BOLD) && blocked.ends_with(PLAIN),
            "{blocked}"
        );
        assert!(!styled.lines().nth(2).unwrap().contains(BOLD));
        assert!(!plain.contains('\x1b'), "{plain}");
    }

    #[test]
    fn a_duration_drops_the_units_it_does_not_need() {
        let since = timestamp("2026-08-17T10:00:00.000Z");
        let elapsed = |now: &str| elapsed(&timestamp(now), &since);

        assert_eq!(elapsed("2026-08-17T10:00:00.000Z"), "0s");
        assert_eq!(elapsed("2026-08-17T10:00:12.500Z"), "12s");
        assert_eq!(elapsed("2026-08-17T10:03:12.000Z"), "3m12s");
        assert_eq!(elapsed("2026-08-17T11:03:12.000Z"), "1h3m12s");
        assert_eq!(elapsed("2026-08-18T10:00:00.000Z"), "24h0m0s");
    }

    #[test]
    fn a_session_whose_clock_is_ahead_reads_as_no_time_at_all() {
        assert_eq!(
            elapsed(
                &timestamp("2026-08-17T10:00:00.000Z"),
                &timestamp("2026-08-17T10:05:00.000Z")
            ),
            "0s"
        );
    }
}
