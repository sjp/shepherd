//! How one tab's arrangement of shells is written down.
//!
//! `h(0.5:s0, 0.5:v(0.25:s1, 0.75:s2))` is a row of two: shell zero on the
//! left, and on the right a column of shell one above shell two, the second
//! taking three quarters of that column. `h` and `v` are the axis, `s` and a
//! number are a shell, and each child of a split is its share of the space
//! followed by what is in it.
//!
//! # Why a grammar rather than more of the file's own structure
//!
//! The arrangement is the one recursive thing being saved, and the file's
//! format is not a recursive one to read: a tree three deep spelled out as
//! tables becomes `[[workspace.tab.layout.children.children]]`, which is a
//! shape nobody can hold in their head and which no amount of formatting
//! rescues. Written this way an arrangement is one line however deep it goes,
//! sits beside the tab's other fields rather than swallowing them, and can be
//! read and corrected by whoever opened the file to look.
//!
//! It buys one more thing, which is why the grammar is here rather than
//! expressed as a deserializer: what comes back when a hand-edited line is
//! wrong. This says which character it stopped at and what it expected there.
//! A general-purpose reader given a choice between two shapes can only say
//! that neither matched.
//!
//! Whitespace between anything is skipped, so a line somebody has spaced out
//! to read it still parses; what is written back is the one canonical spelling.

use std::fmt::Write as _;

use thiserror::Error;

use crate::ids::ShellId;
use crate::split::{Axis, Branch, MalformedSplit, SplitTree};

/// The letter introducing a shell.
const SHELL: char = 's';
/// The letter introducing a split whose children sit side by side.
const HORIZONTAL: char = 'h';
/// The letter introducing a split whose children are stacked.
const VERTICAL: char = 'v';
/// What separates a child's share from what is in it.
const SHARE: char = ':';
/// What separates one child of a split from the next.
const NEXT: char = ',';
/// What a split's children are wrapped in.
const OPEN: char = '(';
/// The other half of that.
const CLOSE: char = ')';

/// Why a line does not describe an arrangement of shells.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum TreeError {
    /// Something else was there — or nothing was, the line having ended.
    #[error("expected {wanted} at character {at}")]
    Expected {
        /// How far into the line the reader had got.
        at: usize,
        /// What would have been an arrangement.
        wanted: &'static str,
    },
    /// A number too large for what it names.
    #[error("the number at character {at} is too large to be {wanted}")]
    TooLarge {
        /// Where the number starts.
        at: usize,
        /// What it was being read as.
        wanted: &'static str,
    },
    /// A complete arrangement, and then more.
    #[error("the arrangement ends at character {at}, and what follows is not part of it")]
    Trailing {
        /// Where the leftovers start.
        at: usize,
    },
    /// A well-formed line describing an arrangement that could not exist.
    #[error("{source} (the split at character {at})")]
    Impossible {
        /// Where the split that cannot exist begins.
        at: usize,
        /// What is wrong with it.
        #[source]
        source: MalformedSplit,
    },
}

/// The line describing `tree`.
pub(super) fn write(tree: &SplitTree) -> String {
    let mut line = String::new();
    push(tree, &mut line);
    line
}

fn push(tree: &SplitTree, line: &mut String) {
    match tree {
        SplitTree::Leaf(shell) => {
            let _ = write!(line, "{SHELL}{}", shell.raw());
        }
        SplitTree::Split(split) => {
            let axis = match split.axis() {
                Axis::Horizontal => HORIZONTAL,
                Axis::Vertical => VERTICAL,
            };
            line.push(axis);
            line.push(OPEN);
            for (index, branch) in split.children().iter().enumerate() {
                if index > 0 {
                    line.push(NEXT);
                    line.push(' ');
                }
                // A share is written at whatever precision it takes to read
                // back as the same number, so that saving and loading a layout
                // nobody has touched changes nothing about it.
                let _ = write!(line, "{}{SHARE}", branch.size());
                push(branch.tree(), line);
            }
            line.push(CLOSE);
        }
    }
}

/// The arrangement `line` describes.
pub(super) fn parse(line: &str) -> Result<SplitTree, TreeError> {
    let mut reader = Reader { line, at: 0 };
    let tree = reader.tree()?;
    reader.skip_spaces();
    if reader.peek().is_some() {
        return Err(TreeError::Trailing { at: reader.at });
    }
    Ok(tree)
}

/// Where in a line the reader has got to.
struct Reader<'a> {
    line: &'a str,
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<char> {
        self.line[self.at..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let found = self.peek()?;
        self.at += found.len_utf8();
        Some(found)
    }

    fn skip_spaces(&mut self) {
        while let Some(found) = self.peek().filter(|found| found.is_whitespace()) {
            self.at += found.len_utf8();
        }
    }

    /// Whichever of the two an arrangement begins with.
    fn tree(&mut self) -> Result<SplitTree, TreeError> {
        self.skip_spaces();
        match self.peek() {
            Some(SHELL) => self.shell(),
            Some(HORIZONTAL) => self.split(Axis::Horizontal),
            Some(VERTICAL) => self.split(Axis::Vertical),
            _ => Err(self.expected(self.at, "a shell or a split")),
        }
    }

    fn shell(&mut self) -> Result<SplitTree, TreeError> {
        self.bump();
        let at = self.at;
        let digits = self.take(|found| found.is_ascii_digit());
        if digits.is_empty() {
            return Err(self.expected(at, "a shell's number"));
        }
        // Nothing but digits got this far, so the only way a number can be
        // refused is by being larger than an id.
        let raw = digits.parse().map_err(|_| TreeError::TooLarge {
            at,
            wanted: "a shell's number",
        })?;
        Ok(SplitTree::leaf(ShellId::from_raw(raw)))
    }

    fn split(&mut self, axis: Axis) -> Result<SplitTree, TreeError> {
        let at = self.at;
        self.bump();
        self.skip_spaces();
        let opens = self.at;
        if self.bump() != Some(OPEN) {
            return Err(self.expected(opens, "`(`"));
        }

        let mut children = Vec::new();
        loop {
            children.push(self.branch()?);
            self.skip_spaces();
            let separates = self.at;
            match self.bump() {
                Some(NEXT) => continue,
                Some(CLOSE) => break,
                _ => return Err(self.expected(separates, "`,` or `)`")),
            }
        }

        SplitTree::split_of(axis, children).map_err(|source| TreeError::Impossible { at, source })
    }

    fn branch(&mut self) -> Result<Branch, TreeError> {
        self.skip_spaces();
        let at = self.at;
        let wanted = "a share, as a decimal fraction";
        let size = self
            .take(|found| found.is_ascii_digit() || found == '.')
            .parse()
            .map_err(|_| self.expected(at, wanted))?;
        self.skip_spaces();
        let separates = self.at;
        if self.bump() != Some(SHARE) {
            return Err(self.expected(separates, "`:`"));
        }
        Ok(Branch::new(size, self.tree()?))
    }

    /// As much of the line as `wanted` accepts, which may be none of it.
    fn take(&mut self, wanted: impl Fn(char) -> bool) -> &str {
        let from = self.at;
        while let Some(found) = self.peek().filter(|found| wanted(*found)) {
            self.at += found.len_utf8();
        }
        &self.line[from..self.at]
    }

    fn expected(&self, at: usize, wanted: &'static str) -> TreeError {
        TreeError::Expected { at, wanted }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::split::Direction;

    fn shell(raw: u32) -> ShellId {
        ShellId::from_raw(raw)
    }

    #[test]
    fn one_shell_is_the_whole_line() {
        assert_eq!(write(&SplitTree::leaf(shell(3))), "s3");
        assert_eq!(parse("s3"), Ok(SplitTree::leaf(shell(3))));
    }

    #[test]
    fn an_arrangement_survives_being_written_down_and_read_back() {
        let mut tree = SplitTree::leaf(shell(0));
        tree.split(shell(0), Direction::Right, shell(1));
        tree.split(shell(1), Direction::Down, shell(2));
        tree.split(shell(2), Direction::Down, shell(3));
        tree.split(shell(0), Direction::Right, shell(4));

        let line = write(&tree);
        assert_eq!(parse(&line), Ok(tree), "{line} did not survive");
    }

    #[test]
    fn the_encoding_is_the_documented_one() {
        let tree = parse("h(0.5:s0, 0.5:v(0.25:s1, 0.75:s2))").expect("the documented example");
        assert_eq!(write(&tree), "h(0.5:s0, 0.5:v(0.25:s1, 0.75:s2))");
    }

    #[test]
    fn a_line_somebody_has_spaced_out_reads_the_same() {
        assert_eq!(
            parse(" h ( 0.5 : s0 , 0.5 : s1 ) "),
            parse("h(0.5:s0, 0.5:s1)")
        );
    }

    #[test]
    fn shares_that_do_not_sum_to_one_are_rescaled_and_ones_that_do_are_not() {
        assert_eq!(write(&parse("h(1:s0, 1:s1)").unwrap()), "h(0.5:s0, 0.5:s1)");
        assert_eq!(
            write(&parse("h(0.3:s0, 0.3:s1, 0.4:s2)").unwrap()),
            "h(0.3:s0, 0.3:s1, 0.4:s2)"
        );
    }

    #[test]
    fn a_line_that_stops_early_says_where_and_what_was_wanted() {
        assert_eq!(
            parse("h(0.5:s0, 0.5:"),
            Err(TreeError::Expected {
                at: 14,
                wanted: "a shell or a split"
            })
        );
        assert_eq!(
            parse(""),
            Err(TreeError::Expected {
                at: 0,
                wanted: "a shell or a split"
            })
        );
    }

    #[test]
    fn every_way_of_being_wrong_is_reported_rather_than_guessed_at() {
        assert!(matches!(
            parse("x(0.5:s0, 0.5:s1)"),
            Err(TreeError::Expected { at: 0, .. })
        ));
        assert!(matches!(
            parse("h[0.5:s0]"),
            Err(TreeError::Expected { at: 1, .. })
        ));
        assert!(matches!(parse("s"), Err(TreeError::Expected { at: 1, .. })));
        assert!(matches!(
            parse("h(0.5:s0; 0.5:s1)"),
            Err(TreeError::Expected { at: 8, .. })
        ));
        assert!(matches!(
            parse("h(x:s0, 0.5:s1)"),
            Err(TreeError::Expected { at: 2, .. })
        ));
        assert!(matches!(
            parse("h(0.5.5:s0, 0.5:s1)"),
            Err(TreeError::Expected { at: 2, .. })
        ));
        assert_eq!(
            parse("s4294967296"),
            Err(TreeError::TooLarge {
                at: 1,
                wanted: "a shell's number"
            })
        );
        assert_eq!(parse("s0 s1"), Err(TreeError::Trailing { at: 3 }));
    }

    #[test]
    fn an_arrangement_that_could_not_exist_is_refused_by_the_tree_itself() {
        assert_eq!(
            parse("h(1:s0)"),
            Err(TreeError::Impossible {
                at: 0,
                source: MalformedSplit::TooFewChildren(1)
            })
        );
        assert_eq!(
            parse("h(0.5:s0, 0.5:h(0.5:s1, 0.5:s2))"),
            Err(TreeError::Impossible {
                at: 0,
                source: MalformedSplit::NestedAxis(Axis::Horizontal)
            })
        );
        assert_eq!(
            parse("h(0.5:s0, 0.5:s0)"),
            Err(TreeError::Impossible {
                at: 0,
                source: MalformedSplit::DuplicateShell(shell(0))
            })
        );
        assert_eq!(
            parse("h(0:s0, 1:s1)"),
            Err(TreeError::Impossible {
                at: 0,
                source: MalformedSplit::Share(0.0)
            })
        );
    }

    #[test]
    fn what_is_wrong_with_a_line_reads_as_a_sentence() {
        assert_eq!(
            parse("h(0.5:s0, 0.5:").unwrap_err().to_string(),
            "expected a shell or a split at character 14"
        );
        assert_eq!(
            parse("h(1:s0)").unwrap_err().to_string(),
            "a split divides its space between at least two children, and this one has 1 \
             (the split at character 0)"
        );
    }
}
