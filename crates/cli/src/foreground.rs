//! Rendering the foreground observations a snapshot carries.
//!
//! What is printed answers "what is running in that terminal", which is a
//! different question from the one `status` answers and has different columns:
//! the correlation first, because that is what somebody is looking a row up by,
//! then the connection the shell arrived over where there is one, then the
//! process and how it stands to its terminal, then where the observation was
//! made and how long it has held.
//!
//! The connection has a column of its own because a shell that carries no
//! correlation is identified by nothing else. Printing it in the correlation's
//! column would say the shell was labelled when it was not, and leaving it out
//! would leave a row nobody could tell from any other.
//!
//! Every value here arrived from a daemon and is printed as it arrived. A
//! correlation in particular is an opaque string belonging to whoever exported
//! it, and this command neither splits it nor tidies it.

use agentbus_protocol::{ForegroundEntry, ForegroundState, Timestamp};

use crate::table::{self, ABSENT, Row};

/// The column headings, in order.
const HEADINGS: [&str; 8] = [
    "CORRELATION",
    "CONNECTION",
    "PID",
    "STATE",
    "PROCESS",
    "CMDLINE",
    "ORIGIN",
    "SINCE",
];

/// What is printed when a daemon that is watching has seen nothing.
const NOTHING: &str = "no foreground processes";

/// The table for `entries`, as of `now`, ending in a newline.
pub fn render(entries: &[&ForegroundEntry], now: &Timestamp) -> String {
    if entries.is_empty() {
        return format!("{NOTHING}\n");
    }

    let rows: Vec<Row> = entries.iter().map(|entry| row(entry, now)).collect();
    table::render(&HEADINGS, &rows, false)
}

/// The entries as newline-delimited JSON, one object per line.
///
/// The shape is the protocol's own, so anything that can read the stream can
/// read this without being taught a second format for the same thing.
pub fn json(entries: &[&ForegroundEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        // An observation the daemon serialized to send here cannot fail to
        // serialize again; a value that somehow did is left out rather than
        // written half-formed into a stream somebody is piping into a parser.
        if let Ok(line) = serde_json::to_string(entry) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// One observation's row, in the order of [`HEADINGS`].
fn row(entry: &ForegroundEntry, now: &Timestamp) -> Row {
    Row::new(vec![
        entry
            .correlation
            .clone()
            .unwrap_or_else(|| ABSENT.to_owned()),
        entry
            .ssh_connection
            .clone()
            .unwrap_or_else(|| ABSENT.to_owned()),
        entry.pid.to_string(),
        entry
            .state
            .map_or_else(|| ABSENT.to_owned(), |state| state_name(state).to_owned()),
        entry.process.clone(),
        entry.cmdline.clone(),
        table::origin(&entry.origin),
        table::elapsed(now, &entry.since),
    ])
}

/// What a state is called in the column.
///
/// Spelled here rather than taken from the wire, so that the word someone reads
/// is this command's to choose even though the value it stands for is not.
fn state_name(state: ForegroundState) -> &'static str {
    match state {
        ForegroundState::Foreground => "foreground",
        ForegroundState::Suspended => "suspended",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use agentbus_protocol::OriginHop;

    fn timestamp(text: &str) -> Timestamp {
        Timestamp::parse(text).expect("not a timestamp")
    }

    fn now() -> Timestamp {
        timestamp("2026-08-17T10:35:00.000Z")
    }

    fn entry(correlation: &str) -> ForegroundEntry {
        let mut entry = ForegroundEntry::new(
            4471,
            "claude",
            "claude --resume",
            timestamp("2026-08-17T10:31:48.000Z"),
        )
        .with_correlation(correlation);
        entry.state = Some(ForegroundState::Foreground);
        entry
    }

    fn rendered(entries: &[ForegroundEntry]) -> String {
        let borrowed: Vec<&ForegroundEntry> = entries.iter().collect();
        render(&borrowed, &now())
    }

    #[test]
    fn nothing_observed_says_so() {
        assert_eq!(rendered(&[]), "no foreground processes\n");
    }

    #[test]
    fn every_column_is_headed_and_filled() {
        let mut entry = entry("w9:p3");
        entry.origin = vec![OriginHop::new(OriginHop::CONTAINER, "e41a", "devcontainer")];

        let table = rendered(&[entry]);

        assert_eq!(
            table
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<&str>>(),
            HEADINGS
        );
        let row = table.lines().nth(1).unwrap();
        for value in [
            "w9:p3",
            "4471",
            "foreground",
            "claude",
            "claude --resume",
            "devcontainer",
            "3m12s",
        ] {
            assert!(row.contains(value), "{value} is missing from {row}");
        }
        assert_eq!(table.lines().count(), 2);
    }

    #[test]
    fn a_correlation_is_printed_exactly_as_it_arrived() {
        let table = rendered(&[entry("anything at all: 1/2/3")]);

        assert!(table.contains("anything at all: 1/2/3"), "{table}");
    }

    #[test]
    fn an_observation_with_no_state_shows_it_has_none() {
        let mut entry = entry("w9:p3");
        entry.state = None;

        let table = rendered(&[entry]);

        assert!(table.lines().nth(1).unwrap().contains(ABSENT), "{table}");
    }

    #[test]
    fn a_suspended_process_is_named_as_one() {
        let mut entry = entry("w9:p3");
        entry.state = Some(ForegroundState::Suspended);

        assert!(rendered(&[entry]).contains("suspended"));
    }

    #[test]
    fn the_json_is_one_entry_per_line_in_the_shape_it_arrived_in() {
        let entries = [entry("w9:p3"), entry("w9:p4")];
        let borrowed: Vec<&ForegroundEntry> = entries.iter().collect();

        let text = json(&borrowed);

        let read: Vec<ForegroundEntry> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("not an entry"))
            .collect();
        assert_eq!(read, entries);
    }

    #[test]
    fn nothing_observed_is_no_json_at_all() {
        assert_eq!(json(&[]), "");
    }
}
