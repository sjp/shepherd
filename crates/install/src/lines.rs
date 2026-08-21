//! A file as the lines it is made of, with the terminator each line was
//! followed by.
//!
//! The editors for the formats with no concrete syntax tree behind them work by
//! line: they find the handful of lines that are theirs, change those, and put
//! every other line back exactly as they found it. That only holds if "exactly"
//! includes the parts a naive split throws away — whether a line ended `\n` or
//! `\r\n`, and whether the last one ended at all. A file with Windows line
//! endings that came back with Unix ones would be a whole-file diff dressed up
//! as a two-line edit, which is the thing these editors exist to avoid.
//!
//! So each line carries its own terminator rather than the file carrying one
//! for all of them, and the one invariant maintained across every operation is
//! that only the last line may lack one. A file that arrived without a final
//! newline leaves without one, however many lines were added to or taken from
//! its end.

use std::ops::Range;

/// The terminator used for a line added to a file that has no line to copy one
/// from.
const DEFAULT_ENDING: &str = "\n";

/// One line, and what followed it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    /// The line itself, with no terminator in it.
    text: String,
    /// `"\n"` or `"\r\n"`, or empty for a last line that had neither.
    ending: &'static str,
}

/// A file split into lines that can be edited and put back together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lines {
    lines: Vec<Line>,
}

impl Lines {
    /// Splits `text` into the lines it is made of.
    pub fn of(text: &str) -> Self {
        let mut lines = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let Some(at) = rest.find('\n') else {
                lines.push(Line {
                    text: rest.to_owned(),
                    ending: "",
                });
                break;
            };
            let (line, tail) = rest.split_at(at + 1);
            let (text, ending) = match line.strip_suffix("\r\n") {
                Some(text) => (text, "\r\n"),
                None => (&line[..line.len() - 1], "\n"),
            };
            lines.push(Line {
                text: text.to_owned(),
                ending,
            });
            rest = tail;
        }
        Self { lines }
    }

    /// Puts the lines back together.
    pub fn render(&self) -> String {
        let mut text = String::new();
        for line in &self.lines {
            text.push_str(&line.text);
            text.push_str(line.ending);
        }
        text
    }

    /// How many lines there are.
    pub fn count(&self) -> usize {
        self.lines.len()
    }

    /// The line at `index`, without its terminator.
    pub fn text(&self, index: usize) -> &str {
        &self.lines[index].text
    }

    /// Every line in order, without their terminators.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(|line| line.text.as_str())
    }

    /// The terminator a line added to this file is given.
    ///
    /// Taken from the first line that has one, so that adding to a file written
    /// on a Windows machine writes what the rest of that file is written with.
    pub fn ending(&self) -> &'static str {
        self.lines
            .iter()
            .map(|line| line.ending)
            .find(|ending| !ending.is_empty())
            .unwrap_or(DEFAULT_ENDING)
    }

    /// Replaces the line at `index`, leaving its terminator as it was.
    pub fn replace(&mut self, index: usize, text: String) {
        self.lines[index].text = text;
    }

    /// Puts `text` in as the line at `index`, moving the rest down.
    ///
    /// The new line is given the terminator of the line it is being put in
    /// front of, or — when it is going on the end — the one the line that used
    /// to be last had to be given to make room for it.
    pub fn insert(&mut self, index: usize, text: String) {
        let ending = match self.lines.get(index) {
            Some(line) if !line.ending.is_empty() => line.ending,
            _ => self.ending(),
        };
        let unterminated = self.unterminated();
        self.lines.insert(index, Line { text, ending });
        self.settle(unterminated);
    }

    /// Puts `texts` in as the lines starting at `index`, in order.
    pub fn insert_all(&mut self, index: usize, texts: impl IntoIterator<Item = String>) {
        for (offset, text) in texts.into_iter().enumerate() {
            self.insert(index + offset, text);
        }
    }

    /// Takes the lines in `range` out.
    pub fn remove(&mut self, range: Range<usize>) {
        let unterminated = self.unterminated();
        self.lines.drain(range);
        self.settle(unterminated);
    }

    /// Whether the file ends without a final newline.
    fn unterminated(&self) -> bool {
        self.lines.last().is_some_and(|line| line.ending.is_empty())
    }

    /// Restores the invariant that only the last line may lack a terminator,
    /// and that it lacks one exactly when the file arrived without one.
    fn settle(&mut self, unterminated: bool) {
        let ending = self.ending();
        let last = self.lines.len().saturating_sub(1);
        for (index, line) in self.lines.iter_mut().enumerate() {
            if index == last {
                line.ending = match unterminated {
                    true => "",
                    false => match line.ending.is_empty() {
                        true => ending,
                        false => line.ending,
                    },
                };
            } else if line.ending.is_empty() {
                line.ending = ending;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case below has to hand back what it was given, so it is worth
    /// saying once.
    fn survives(text: &str) -> String {
        Lines::of(text).render()
    }

    #[test]
    fn a_file_comes_back_exactly_as_it_went_in() {
        for text in [
            "",
            "\n",
            "a",
            "a\n",
            "a\n\n",
            "a\r\nb\r\n",
            "a\r\nb",
            "\n\n\n",
            "a\nb\r\nc",
        ] {
            assert_eq!(survives(text), text, "{text:?}");
        }
    }

    #[test]
    fn the_terminator_a_new_line_gets_is_the_one_the_file_uses() {
        assert_eq!(Lines::of("a\r\nb\r\n").ending(), "\r\n");
        assert_eq!(Lines::of("a\nb\n").ending(), "\n");
        assert_eq!(Lines::of("a").ending(), "\n");
        assert_eq!(Lines::of("").ending(), "\n");
    }

    #[test]
    fn a_line_added_to_the_middle_leaves_the_others_alone() {
        let mut lines = Lines::of("a\r\nc\r\n");
        lines.insert(1, String::from("b"));
        assert_eq!(lines.render(), "a\r\nb\r\nc\r\n");
    }

    #[test]
    fn a_file_with_no_final_newline_still_has_none_after_a_line_is_added() {
        let mut lines = Lines::of("a");
        lines.insert(1, String::from("b"));
        assert_eq!(lines.render(), "a\nb");
    }

    #[test]
    fn a_file_with_no_final_newline_still_has_none_after_its_last_line_goes() {
        let mut lines = Lines::of("a\nb");
        lines.remove(1..2);
        assert_eq!(lines.render(), "a");
    }

    #[test]
    fn a_file_that_ended_in_a_newline_still_does_after_its_last_line_goes() {
        let mut lines = Lines::of("a\nb\n");
        lines.remove(1..2);
        assert_eq!(lines.render(), "a\n");
    }

    #[test]
    fn removing_everything_leaves_nothing() {
        let mut lines = Lines::of("a\nb");
        lines.remove(0..2);
        assert_eq!(lines.count(), 0);
        assert_eq!(lines.render(), "");
    }

    #[test]
    fn several_lines_go_in_in_the_order_they_were_given() {
        let mut lines = Lines::of("a\n");
        lines.insert_all(1, ["b".to_owned(), "c".to_owned()]);
        assert_eq!(lines.render(), "a\nb\nc\n");
    }

    #[test]
    fn replacing_a_line_keeps_the_terminator_it_had() {
        let mut lines = Lines::of("a\r\nb");
        lines.replace(0, String::from("z"));
        lines.replace(1, String::from("y"));
        assert_eq!(lines.render(), "z\r\ny");
    }
}
