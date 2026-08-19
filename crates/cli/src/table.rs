//! Columns of values, laid out for somebody reading them in a terminal.
//!
//! Two commands print what the bus knows as a table, and what makes a table
//! readable is the same for both: a column as wide as the widest thing in it, no
//! trailing blanks on a row anybody might diff, an origin chain written as a
//! path, and a duration written the way a person says one. None of it knows what
//! the values mean — a cell is a string that arrived from somewhere else, and
//! deciding what belongs in one is the calling command's business.

use agentbus_protocol::{OriginHop, Timestamp};
use std::fmt::Write as _;

/// What is printed where a value is simply not there.
///
/// Only for the fields a thing may genuinely not have. An empty cell is the
/// right answer where the emptiness is the truth — an origin chain of length
/// zero says a local observation — and this is for where it is not.
pub const ABSENT: &str = "-";

/// What separates one column from the next.
const GAP: &str = "  ";

/// Turns the escape sequences that make a row stand out on and off.
const BOLD: &str = "\x1b[1m";
const PLAIN: &str = "\x1b[0m";

/// One row, and whether it is the row someone's eye should land on first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    cells: Vec<String>,
    emphasized: bool,
}

impl Row {
    /// A row of cells, in the order of the headings it will be printed under.
    pub fn new(cells: Vec<String>) -> Self {
        Self {
            cells,
            emphasized: false,
        }
    }

    /// The same row, standing out from the rest where `yes`.
    #[must_use]
    pub fn emphasized(mut self, yes: bool) -> Self {
        self.emphasized = yes;
        self
    }
}

/// The table for `rows` under `headings`, ending in a newline.
///
/// `styled` says whether the output is going somewhere escape sequences mean
/// something; where it is not, an emphasized row is printed like any other
/// rather than with its markup showing.
pub fn render(headings: &[&str], rows: &[Row], styled: bool) -> String {
    let widths = widths(headings, rows);

    let mut table = String::new();
    let headings: Vec<String> = headings
        .iter()
        .map(|heading| (*heading).to_owned())
        .collect();
    let _ = writeln!(table, "{}", line(&headings, &widths));
    for row in rows {
        let text = line(&row.cells, &widths);
        let _ = match styled && row.emphasized {
            true => writeln!(table, "{BOLD}{text}{PLAIN}"),
            false => writeln!(table, "{text}"),
        };
    }
    table
}

/// An origin chain as a path, outermost first, or empty for a local one.
///
/// A hop's name is display text and may be anything, including nothing; a hop
/// that has no name is shown by its identity rather than as a blank in the
/// middle of a path.
pub fn origin(hops: &[OriginHop]) -> String {
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
pub fn elapsed(now: &Timestamp, since: &Timestamp) -> String {
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
fn widths(headings: &[&str], rows: &[Row]) -> Vec<usize> {
    let mut widths: Vec<usize> = headings.iter().map(|heading| heading.len()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(&row.cells) {
            *width = (*width).max(cell.chars().count());
        }
    }
    widths
}

/// One row, padded to the column widths, with no trailing blanks.
fn line(cells: &[String], widths: &[usize]) -> String {
    let mut line = String::new();
    for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
        if index > 0 {
            line.push_str(GAP);
        }
        line.push_str(cell);
        let padding = width.saturating_sub(cell.chars().count());
        line.extend(std::iter::repeat_n(' ', padding));
    }
    // Whatever the last columns were padded to is not worth keeping: trailing
    // spaces are invisible until somebody diffs the output.
    line.truncate(line.trim_end().len());
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp(text: &str) -> Timestamp {
        Timestamp::parse(text).expect("not a timestamp")
    }

    fn row(cells: &[&str]) -> Row {
        Row::new(cells.iter().map(|cell| (*cell).to_owned()).collect())
    }

    #[test]
    fn columns_line_up_however_wide_the_values_are() {
        let table = render(
            &["ONE", "TWO"],
            &[row(&["a", "left"]), row(&["a-much-longer-cell", "right"])],
            false,
        );

        let starts: Vec<Option<usize>> = table
            .lines()
            .map(|line| {
                line.find("left")
                    .or_else(|| line.find("right"))
                    .or_else(|| line.find("TWO"))
            })
            .collect();
        assert_eq!(starts[0], starts[1], "{table}");
        assert_eq!(starts[1], starts[2], "{table}");
    }

    #[test]
    fn a_row_has_no_trailing_blanks() {
        let table = render(&["ONE", "TWO"], &[row(&["a-long-cell", ""])], false);

        for line in table.lines() {
            assert_eq!(line, line.trim_end(), "trailing spaces in {line:?}");
        }
    }

    #[test]
    fn a_row_stands_out_only_where_that_means_something() {
        let rows = [row(&["a"]).emphasized(true), row(&["b"])];

        let styled = render(&["ONE"], &rows, true);
        let plain = render(&["ONE"], &rows, false);

        let emphasized = styled.lines().nth(1).unwrap();
        assert!(
            emphasized.starts_with(BOLD) && emphasized.ends_with(PLAIN),
            "{emphasized}"
        );
        assert!(!styled.lines().nth(2).unwrap().contains(BOLD));
        assert!(!plain.contains('\x1b'), "{plain}");
    }

    #[test]
    fn an_origin_chain_is_printed_outermost_first_and_is_empty_when_local() {
        let hops = [
            OriginHop::new(OriginHop::SSH, "9f3c:1000", "fileserver"),
            OriginHop::new(OriginHop::CONTAINER, "e41a", "devcontainer"),
        ];

        assert_eq!(origin(&hops), "fileserver > devcontainer");
        assert_eq!(origin(&[]), "");
    }

    #[test]
    fn a_hop_with_no_name_falls_back_to_its_identity() {
        let hop = OriginHop::new(OriginHop::SSH, "9f3c:1000", "");

        assert_eq!(origin(&[hop]), "9f3c:1000");
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
    fn a_clock_that_is_ahead_reads_as_no_time_at_all() {
        assert_eq!(
            elapsed(
                &timestamp("2026-08-17T10:00:00.000Z"),
                &timestamp("2026-08-17T10:05:00.000Z")
            ),
            "0s"
        );
    }
}
