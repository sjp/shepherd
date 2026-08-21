//! Editing a TOML file somebody keeps by hand.
//!
//! Two of the agents this program installs for are configured through a
//! `config.toml` that belongs to the person whose machine it is on: their
//! settings, their comments, their ordering. The rule that governs
//! [`cst`](crate::cst) governs this too — change the bytes being changed and no
//! others — but there is no concrete-syntax-tree parser to lean on here, so the
//! job is done by line.
//!
//! That is only safe if the editor is honest about how little it understands.
//! It knows three things: where its own fenced block is, where a top-level
//! table header is, and where a key is assigned to on a single line. Everything
//! else in the file is text it copies through untouched. And where those three
//! things could mean more than one thing — a section written twice, a key whose
//! value carries on to the next line, one of its own marker lines sitting
//! inside a multi-line string — it refuses and writes nothing at all. A refusal
//! costs a plan; a wrong guess costs somebody their configuration.
//!
//! The one piece of TOML this does generate is a string literal, and
//! [`string`] escapes everything the format says it must, so that a path from
//! the machine this is running on cannot break out of the quotes it is being
//! put between.

use std::ops::Range;

use crate::lines::Lines;

/// The line that opens the block this program owns.
///
/// A TOML file has nowhere to hang the key that marks this program's entries in
/// a JSON one, so the marker lines do that job as well as bounding the block:
/// what is between them is this program's to replace and to remove, and
/// everything outside them is somebody else's.
pub const BLOCK_BEGIN: &str = "# agentbus hooks begin";

/// The line that closes it.
pub const BLOCK_END: &str = "# agentbus hooks end";

/// Why a file cannot be edited.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    /// The block was opened and never closed.
    #[error("`{BLOCK_BEGIN}` has no `{BLOCK_END}` after it")]
    Unclosed,
    /// The block was closed without being opened.
    #[error("`{BLOCK_END}` has no `{BLOCK_BEGIN}` before it")]
    Unopened,
    /// There is more than one block.
    #[error("`{BLOCK_BEGIN}` appears more than once, so there is no telling which block is which")]
    Repeated,
    /// Something this program owns is inside a multi-line string, where a
    /// line-by-line editor cannot tell what it means.
    #[error("`{line}` is inside a multi-line string, where there is no telling what it means")]
    InString { line: String },
    /// A string literal is opened on a line and never closed on it.
    #[error("the string opened on line {line} is never closed, so this is not TOML this can edit")]
    Malformed { line: usize },
    /// The section appears twice, so there is no saying which one a key belongs
    /// in.
    #[error("`[{section}]` appears more than once, so there is no telling which one to write into")]
    RepeatedSection { section: String },
    /// The key appears twice in the section.
    #[error("`{key}` appears more than once under `[{section}]`")]
    RepeatedKey { section: String, key: String },
    /// The key's value carries on past the end of its line.
    #[error("the value of `{key}` carries on past the end of its line, and this edits by line")]
    Spanning { key: String },
}

/// What setting a flag did, over and above the text it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    /// The file as it now reads.
    pub text: String,
    /// Whether the section had to be written as well as the key.
    ///
    /// The one thing about this edit that cannot be worked out again from what
    /// is on disk: a `[features]` holding nothing but this program's key looks
    /// the same whether this program wrote it or found it. The caller records
    /// the answer and hands it back when the key is taken out again.
    pub created_section: bool,
}

/// What is inside this program's own block, if there is one.
///
/// The lines come back joined with `\n` whatever the file is written with, so
/// that what is in the file can be compared against what would be put there.
pub fn block(text: &str) -> Result<Option<String>, Problem> {
    let lines = Lines::of(text);
    let shapes = classify(&lines)?;
    let Some(range) = fence(&lines, &shapes)? else {
        return Ok(None);
    };
    let inside: Vec<&str> = (range.start + 1..range.end - 1)
        .map(|index| lines.text(index))
        .collect();
    Ok(Some(inside.join("\n")))
}

/// The file with `content` between this program's own marker lines.
///
/// A block already there is taken out wherever it sits and the new one is put
/// at the end, because the end is where a block this program wrote goes and
/// somewhere else is where somebody moved it to. The one exception is a block
/// that already says exactly this, which is left where it is: installing twice
/// is not an edit, and a file that did not need changing keeps the modification
/// time it had.
pub fn set_block(text: &str, content: &str) -> Result<String, Problem> {
    let wanted: Vec<&str> = content.lines().collect();
    if block(text)?.as_deref() == Some(wanted.join("\n").as_str()) {
        return Ok(text.to_owned());
    }
    let stripped = remove_block(text)?;
    let mut lines = Lines::of(&stripped);
    let mut fresh = Vec::new();
    if !lines.iter().all(|line| line.trim().is_empty()) {
        fresh.push(String::new());
    }
    fresh.push(BLOCK_BEGIN.to_owned());
    fresh.extend(wanted.into_iter().map(str::to_owned));
    fresh.push(BLOCK_END.to_owned());
    lines.insert_all(lines.count(), fresh);
    Ok(lines.render())
}

/// The file with this program's own block taken out.
///
/// A blank line in front of the block goes with it, but only when the block
/// runs to the end of the file — that blank line is the one put there to keep
/// the block off the end of somebody's last setting, and taking it back is what
/// makes an uninstall give back the bytes the install was handed.
pub fn remove_block(text: &str) -> Result<String, Problem> {
    let mut lines = Lines::of(text);
    let shapes = classify(&lines)?;
    let Some(range) = fence(&lines, &shapes)? else {
        return Ok(text.to_owned());
    };
    let start = match range.end == lines.count() && separator(&lines, range.start) {
        true => range.start - 1,
        false => range.start,
    };
    lines.remove(start..range.end);
    Ok(lines.render())
}

/// Whether `key` is set to `true` under the top-level `[section]`.
pub fn flag(text: &str, section: &str, key: &str) -> Result<bool, Problem> {
    let lines = Lines::of(text);
    let shapes = classify(&lines)?;
    refuse_quoted(&lines, &shapes, section, key)?;
    let Some(header) = section_at(&shapes, section)? else {
        return Ok(false);
    };
    let Some(index) = assignment(&lines, &shapes, header, section, key)? else {
        return Ok(false);
    };
    Ok(value(lines.text(index)) == Some("true"))
}

/// The file with `key` set to `true` under the top-level `[section]`.
///
/// The section is written only when there is none, and the key is written only
/// when the section does not already assign to it; a key that is already `true`
/// is left alone down to its spacing, and one that is set to something else has
/// its value replaced and nothing around it touched — the comment somebody put
/// on the end of that line is still theirs.
pub fn set_flag(text: &str, section: &str, key: &str) -> Result<Flag, Problem> {
    let mut lines = Lines::of(text);
    let shapes = classify(&lines)?;
    refuse_quoted(&lines, &shapes, section, key)?;

    let Some(header) = section_at(&shapes, section)? else {
        let mut fresh = Vec::new();
        if !lines.iter().all(|line| line.trim().is_empty()) {
            fresh.push(String::new());
        }
        fresh.push(format!("[{section}]"));
        fresh.push(format!("{key} = true"));
        lines.insert_all(lines.count(), fresh);
        return Ok(Flag {
            text: lines.render(),
            created_section: true,
        });
    };

    match assignment(&lines, &shapes, header, section, key)? {
        Some(index) => {
            let line = lines.text(index);
            let Some(span) = value_span(line) else {
                return Ok(Flag {
                    text: lines.render(),
                    created_section: false,
                });
            };
            if &line[span.clone()] != "true" {
                let replaced = format!("{}true{}", &line[..span.start], &line[span.end..]);
                lines.replace(index, replaced);
            }
        }
        None => {
            let indent = indentation(&lines, &shapes, header);
            lines.insert(header + 1, format!("{indent}{key} = true"));
        }
    }
    Ok(Flag {
        text: lines.render(),
        created_section: false,
    })
}

/// The file with `key` taken out from under the top-level `[section]`.
///
/// Only the key: the section stays, along with everything else in it, unless
/// `created_section` says this program wrote the section too and nothing but
/// blank lines is left in it.
pub fn clear_flag(
    text: &str,
    section: &str,
    key: &str,
    created_section: bool,
) -> Result<String, Problem> {
    let mut lines = Lines::of(text);
    let shapes = classify(&lines)?;
    refuse_quoted(&lines, &shapes, section, key)?;

    let Some(header) = section_at(&shapes, section)? else {
        return Ok(text.to_owned());
    };
    let Some(index) = assignment(&lines, &shapes, header, section, key)? else {
        return Ok(text.to_owned());
    };
    lines.remove(index..index + 1);

    if created_section {
        // Worked out from the file as it now reads rather than from the file as
        // it was read, because the line just taken out has moved every line
        // after it.
        let shapes = classify(&lines)?;
        if let Some(start) = section_at(&shapes, section)? {
            let end = shapes
                .iter()
                .enumerate()
                .skip(start + 1)
                .find(|(_, shape)| matches!(shape.shape, Shape::Header { .. }) && !shape.quoted)
                .map_or(lines.count(), |(index, _)| index);
            let vacant = (start + 1..end).all(|line| lines.text(line).trim().is_empty());
            if vacant {
                let from = match end == lines.count() && separator(&lines, start) {
                    true => start - 1,
                    false => start,
                };
                lines.remove(from..end);
            }
        }
    }
    Ok(lines.render())
}

/// `value` as a TOML basic string, quoted and escaped.
///
/// Every escape the format defines, and `\uXXXX` for the control characters it
/// has no shorthand for, because a literal control character in a basic string
/// is not TOML and the values going through here — paths, commands — come from
/// a machine rather than from this program.
pub fn string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\u{8}' => quoted.push_str("\\b"),
            '\t' => quoted.push_str("\\t"),
            '\n' => quoted.push_str("\\n"),
            '\u{c}' => quoted.push_str("\\f"),
            '\r' => quoted.push_str("\\r"),
            other if other <= '\u{1f}' || other == '\u{7f}' => {
                quoted.push_str(&format!("\\u{:04X}", other as u32));
            }
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// Whether the blank line in front of `start` is the one this program put
/// there.
///
/// What this program appends to a file that already holds something gets a
/// blank line in front of it, so that it does not run into somebody's last
/// setting. Taking that line back with the rest is what makes an uninstall give
/// back the bytes the install was handed — but only where it was ever added,
/// which a file holding nothing but blank lines never had.
fn separator(lines: &Lines, start: usize) -> bool {
    start > 0
        && lines.text(start - 1).trim().is_empty()
        && (0..start - 1).any(|line| !lines.text(line).trim().is_empty())
}

/// What a line is, as far as this editor needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    /// A table header, with the name between its brackets and whether it was
    /// written with two of them.
    Header { name: String, array: bool },
    /// An assignment, with the key it assigns to.
    Key(String),
    /// A blank line, a comment, or the continuation of something that started
    /// on an earlier line.
    Other,
}

/// A line's shape, and whether that shape is real.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Classified {
    shape: Shape,
    /// Whether the line is inside a multi-line string, in which case its shape
    /// is what it looks like rather than what it is.
    quoted: bool,
}

/// What carries over from one line to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Carry {
    /// The multi-line string this line ends inside, if any.
    string: Option<Multiline>,
    /// How many brackets and braces are open, so that a line in the middle of
    /// an array is not mistaken for a table header.
    depth: usize,
}

/// The two kinds of string that can span lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Multiline {
    /// `"""`, with escapes.
    Basic,
    /// `'''`, with none.
    Literal,
}

/// Works out what every line in the file is.
fn classify(lines: &Lines) -> Result<Vec<Classified>, Problem> {
    let mut carry = Carry::default();
    let mut shapes = Vec::with_capacity(lines.count());
    for index in 0..lines.count() {
        let line = lines.text(index);
        let quoted = carry.string.is_some();
        // A line in the middle of an array or an inline table is a
        // continuation, whatever it looks like on its own.
        let shape = match !quoted && carry.depth > 0 {
            true => Shape::Other,
            false => bare_shape(line),
        };
        shapes.push(Classified { shape, quoted });
        walk(line, index, &mut carry)?;
    }
    Ok(shapes)
}

/// What a line would be if it stood on its own.
fn bare_shape(line: &str) -> Shape {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Shape::Other;
    }
    if trimmed.starts_with('[') {
        return match table_header(trimmed) {
            Some(shape) => shape,
            None => Shape::Other,
        };
    }
    match key_of(trimmed) {
        Some(key) => Shape::Key(key),
        None => Shape::Other,
    }
}

/// The header a line holds, if it holds one.
fn table_header(trimmed: &str) -> Option<Shape> {
    let array = trimmed.starts_with("[[");
    let (open, close) = match array {
        true => ("[[", "]]"),
        false => ("[", "]"),
    };
    let inner = trimmed.strip_prefix(open)?;
    let at = inner.find(close)?;
    let rest = inner[at + close.len()..].trim_start();
    if !rest.is_empty() && !rest.starts_with('#') {
        return None;
    }
    Some(Shape::Header {
        name: inner[..at].trim().to_owned(),
        array,
    })
}

/// The key a line assigns to, if it assigns to one.
fn key_of(trimmed: &str) -> Option<String> {
    let (name, rest) = match trimmed.starts_with(['"', '\'']) {
        true => {
            let quote = trimmed.as_bytes()[0];
            let at = trimmed[1..].find(quote as char)? + 1;
            (trimmed[1..at].to_owned(), &trimmed[at + 1..])
        }
        false => {
            let at = trimmed.find('=')?;
            (trimmed[..at].trim().to_owned(), &trimmed[at..])
        }
    };
    match rest.trim_start().starts_with('=') && !name.is_empty() {
        true => Some(name),
        false => None,
    }
}

/// What walking a line found on it, over and above what it leaves open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Scanned {
    /// Where the line's comment starts, if it has one outside every string.
    comment: Option<usize>,
    /// Where the line's first `=` sits, if it has one outside every string and
    /// outside every bracket.
    assign: Option<usize>,
}

/// Walks one line, working out what it leaves open for the next one and where
/// the two things an edit needs to find on it are.
fn walk(line: &str, index: usize, carry: &mut Carry) -> Result<Scanned, Problem> {
    let mut found = Scanned::default();
    let bytes = line.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if let Some(kind) = carry.string {
            match kind {
                Multiline::Basic => {
                    if bytes[at] == b'\\' {
                        at += 2;
                        continue;
                    }
                    if bytes[at..].starts_with(b"\"\"\"") {
                        carry.string = None;
                        at += 3;
                        continue;
                    }
                }
                Multiline::Literal => {
                    if bytes[at..].starts_with(b"'''") {
                        carry.string = None;
                        at += 3;
                        continue;
                    }
                }
            }
            at += 1;
            continue;
        }
        match bytes[at] {
            b'#' => {
                found.comment = Some(at);
                break;
            }
            b'=' => {
                if carry.depth == 0 && found.assign.is_none() {
                    found.assign = Some(at);
                }
                at += 1;
            }
            b'"' | b'\'' => {
                let quote = bytes[at];
                let three = [quote; 3];
                if bytes[at..].starts_with(&three) {
                    carry.string = Some(match quote {
                        b'"' => Multiline::Basic,
                        _ => Multiline::Literal,
                    });
                    at += 3;
                    continue;
                }
                at = closed(bytes, at, quote).ok_or(Problem::Malformed { line: index + 1 })?;
            }
            b'[' | b'{' => {
                carry.depth += 1;
                at += 1;
            }
            b']' | b'}' => {
                carry.depth = carry.depth.saturating_sub(1);
                at += 1;
            }
            _ => at += 1,
        }
    }
    Ok(found)
}

/// Where the single-line string opened at `at` ends, one past its closing
/// quote.
fn closed(bytes: &[u8], at: usize, quote: u8) -> Option<usize> {
    let escapes = quote == b'"';
    let mut at = at + 1;
    while at < bytes.len() {
        if escapes && bytes[at] == b'\\' {
            at += 2;
            continue;
        }
        if bytes[at] == quote {
            return Some(at + 1);
        }
        at += 1;
    }
    None
}

/// Where this program's own block is, as the range of lines from its opening
/// marker to its closing one.
fn fence(lines: &Lines, shapes: &[Classified]) -> Result<Option<Range<usize>>, Problem> {
    let mut open = None;
    let mut found: Option<Range<usize>> = None;
    for (index, classified) in shapes.iter().enumerate() {
        let trimmed = lines.text(index).trim();
        let marker = trimmed == BLOCK_BEGIN || trimmed == BLOCK_END;
        if marker && classified.quoted {
            return Err(Problem::InString {
                line: trimmed.to_owned(),
            });
        }
        if trimmed == BLOCK_BEGIN {
            if open.is_some() || found.is_some() {
                return Err(Problem::Repeated);
            }
            open = Some(index);
        } else if trimmed == BLOCK_END {
            match open.take() {
                Some(start) => found = Some(start..index + 1),
                None => return Err(Problem::Unopened),
            }
        }
    }
    match open.is_some() {
        true => Err(Problem::Unclosed),
        false => Ok(found),
    }
}

/// Refuses a file that holds something of this program's inside a multi-line
/// string.
///
/// Nothing there is this program's to change — but the point of the check is
/// that this editor cannot say so with any confidence, and a file it cannot
/// read confidently is one it declines to write.
fn refuse_quoted(
    lines: &Lines,
    shapes: &[Classified],
    section: &str,
    key: &str,
) -> Result<(), Problem> {
    for (index, classified) in shapes.iter().enumerate() {
        if !classified.quoted {
            continue;
        }
        let ours = match &classified.shape {
            Shape::Header { name, array } => name == section && !array,
            Shape::Key(name) => name == key,
            Shape::Other => false,
        };
        if ours {
            return Err(Problem::InString {
                line: lines.text(index).trim().to_owned(),
            });
        }
    }
    Ok(())
}

/// Which line holds the top-level `[section]` header.
fn section_at(shapes: &[Classified], section: &str) -> Result<Option<usize>, Problem> {
    let mut found = None;
    for (index, classified) in shapes.iter().enumerate() {
        if classified.quoted {
            continue;
        }
        let Shape::Header { name, array } = &classified.shape else {
            continue;
        };
        if name != section || *array {
            continue;
        }
        if found.is_some() {
            return Err(Problem::RepeatedSection {
                section: section.to_owned(),
            });
        }
        found = Some(index);
    }
    Ok(found)
}

/// Which line assigns to `key` in the section that starts at `header`.
fn assignment(
    lines: &Lines,
    shapes: &[Classified],
    header: usize,
    section: &str,
    key: &str,
) -> Result<Option<usize>, Problem> {
    let mut found = None;
    for (index, classified) in shapes.iter().enumerate().skip(header + 1) {
        if classified.quoted {
            continue;
        }
        if matches!(classified.shape, Shape::Header { .. }) {
            break;
        }
        let Shape::Key(name) = &classified.shape else {
            continue;
        };
        if name != key {
            continue;
        }
        if found.is_some() {
            return Err(Problem::RepeatedKey {
                section: section.to_owned(),
                key: key.to_owned(),
            });
        }
        if value_span(lines.text(index)).is_none() {
            return Err(Problem::Spanning {
                key: key.to_owned(),
            });
        }
        found = Some(index);
    }
    Ok(found)
}

/// The indentation the keys in the section starting at `header` are written
/// with.
fn indentation(lines: &Lines, shapes: &[Classified], header: usize) -> String {
    for (index, classified) in shapes.iter().enumerate().skip(header + 1) {
        if matches!(classified.shape, Shape::Header { .. }) && !classified.quoted {
            break;
        }
        if !matches!(classified.shape, Shape::Key(_)) || classified.quoted {
            continue;
        }
        let line = lines.text(index);
        return line[..line.len() - line.trim_start().len()].to_owned();
    }
    String::new()
}

/// Where the value assigned on a line sits, or `None` if the value does not
/// end on that line.
///
/// The `=` and the `#` are both found by walking the line rather than by
/// searching it, because either one can appear inside the value's own string
/// without meaning anything.
fn value_span(line: &str) -> Option<Range<usize>> {
    let mut carry = Carry::default();
    let found = walk(line, 0, &mut carry).ok()?;
    if carry.string.is_some() || carry.depth > 0 {
        return None;
    }
    let at = found.assign?;
    let rest = &line[at + 1..];
    let start = at + 1 + (rest.len() - rest.trim_start().len());
    let end = found.comment.unwrap_or(line.len()).max(start);
    let end = start + line[start..end].trim_end().len();
    Some(start..end)
}

/// The value assigned on a line, if it ends on that line.
fn value(line: &str) -> Option<&str> {
    value_span(line).map(|span| &line[span])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The block this program would write, in the shape the one agent that
    /// wants one writes it.
    const HOOKS: &str = "[[hooks]]\nevent = \"Stop\"\ncommand = \"agentbus emit\"\n";

    /// A second, different block, for the case where a build writes over what
    /// an earlier one left.
    const OTHER: &str = "[[hooks]]\nevent = \"Start\"\n";

    /// Every file a fenced-block edit has to survive.
    fn fixtures() -> Vec<&'static str> {
        vec![
            "",
            "\n",
            "model = \"a\"\n",
            "model = \"a\"",
            "model = \"a\"\r\n[features]\r\nhooks = true\r\n",
            "# a note somebody wrote\n\n[profiles.work]\nmodel = \"b\"  # and another\n",
            "text = \"\"\"\nnot a header [features]\n\"\"\"\n",
        ]
    }

    /// The one thing the tests are allowed to use a TOML library for.
    fn parses(text: &str) {
        toml::from_str::<toml::Value>(text).unwrap_or_else(|error| panic!("{error}\n{text:?}"));
    }

    #[test]
    fn a_file_with_no_block_of_ours_says_so_and_is_left_alone() {
        for text in fixtures() {
            assert_eq!(block(text).unwrap(), None, "{text:?}");
            assert_eq!(remove_block(text).unwrap(), text, "{text:?}");
        }
    }

    #[test]
    fn the_block_goes_on_the_end_and_reads_back_as_what_was_put_there() {
        for text in fixtures() {
            let written = set_block(text, HOOKS).unwrap();
            assert_eq!(block(&written).unwrap().unwrap(), HOOKS.trim_end());
            assert!(written.contains(BLOCK_BEGIN), "{written:?}");
            parses(&written);
        }
    }

    #[test]
    fn everything_that_was_in_the_file_is_still_in_it_word_for_word() {
        let text = "# a note\n\n[profiles.work]\nmodel = \"b\"  # and another\n";
        let written = set_block(text, HOOKS).unwrap();
        assert!(written.starts_with(text), "{written:?}");
    }

    #[test]
    fn writing_the_same_block_twice_hands_back_the_very_same_bytes() {
        for text in fixtures() {
            let once = set_block(text, HOOKS).unwrap();
            assert_eq!(set_block(&once, HOOKS).unwrap(), once, "{text:?}");
        }
    }

    #[test]
    fn taking_the_block_out_gives_back_the_file_it_was_put_into() {
        for text in fixtures() {
            let written = set_block(text, HOOKS).unwrap();
            assert_eq!(remove_block(&written).unwrap(), text, "{text:?}");
        }
    }

    #[test]
    fn a_block_saying_something_else_is_replaced_and_not_added_to() {
        for text in fixtures() {
            let old = set_block(text, OTHER).unwrap();
            let new = set_block(&old, HOOKS).unwrap();
            assert_eq!(new.matches(BLOCK_BEGIN).count(), 1, "{new:?}");
            assert_eq!(block(&new).unwrap().unwrap(), HOOKS.trim_end());
            assert_eq!(remove_block(&new).unwrap(), text, "{text:?}");
        }
    }

    #[test]
    fn a_block_somebody_moved_is_taken_from_where_it_sits_and_put_back_on_the_end() {
        let text =
            format!("{BLOCK_BEGIN}\n[[hooks]]\nevent = \"Start\"\n{BLOCK_END}\nmodel = \"a\"\n");
        let written = set_block(&text, HOOKS).unwrap();
        assert_eq!(
            written,
            format!("model = \"a\"\n\n{BLOCK_BEGIN}\n{HOOKS}{BLOCK_END}\n")
        );
    }

    #[test]
    fn a_file_written_with_windows_line_endings_keeps_them() {
        let written = set_block("model = \"a\"\r\n", HOOKS).unwrap();
        assert!(!written.contains("\n\n"), "{written:?}");
        assert_eq!(
            written.matches("\r\n").count(),
            written.matches('\n').count()
        );
    }

    #[test]
    fn a_file_that_ended_without_a_newline_still_does() {
        let written = set_block("model = \"a\"", HOOKS).unwrap();
        assert!(written.ends_with(BLOCK_END), "{written:?}");
        assert_eq!(remove_block(&written).unwrap(), "model = \"a\"");
    }

    #[test]
    fn a_block_that_is_never_closed_is_refused() {
        let text = format!("{BLOCK_BEGIN}\n[[hooks]]\n");
        assert_eq!(block(&text), Err(Problem::Unclosed));
        assert_eq!(remove_block(&text), Err(Problem::Unclosed));
        assert_eq!(set_block(&text, HOOKS), Err(Problem::Unclosed));
    }

    #[test]
    fn a_block_that_is_closed_without_being_opened_is_refused() {
        let text = format!("model = \"a\"\n{BLOCK_END}\n");
        assert_eq!(remove_block(&text), Err(Problem::Unopened));
    }

    #[test]
    fn two_blocks_are_refused_because_there_is_no_saying_which_is_ours() {
        let text = format!("{BLOCK_BEGIN}\n{BLOCK_END}\n{BLOCK_BEGIN}\n{BLOCK_END}\n");
        assert_eq!(remove_block(&text), Err(Problem::Repeated));
    }

    #[test]
    fn a_marker_inside_a_multi_line_string_is_refused_rather_than_obeyed() {
        let text = format!("note = \"\"\"\n{BLOCK_BEGIN}\n\"\"\"\n");
        let problem = remove_block(&text).unwrap_err();
        assert!(matches!(problem, Problem::InString { .. }), "{problem:?}");
    }

    #[test]
    fn a_string_that_is_never_closed_means_this_is_not_a_file_to_edit() {
        assert_eq!(
            remove_block("model = \"a\n"),
            Err(Problem::Malformed { line: 1 })
        );
    }

    #[test]
    fn a_marker_after_a_multi_line_string_that_did_close_is_still_ours() {
        let text =
            format!("note = \"\"\"\nanything\n\"\"\"\n\n{BLOCK_BEGIN}\n[[hooks]]\n{BLOCK_END}\n");
        assert_eq!(block(&text).unwrap().unwrap(), "[[hooks]]");
    }

    #[test]
    fn the_section_and_the_key_are_both_written_when_neither_is_there() {
        let set = set_flag("model = \"a\"\n", "features", "hooks").unwrap();
        assert_eq!(set.text, "model = \"a\"\n\n[features]\nhooks = true\n");
        assert!(set.created_section);
        parses(&set.text);
        assert!(flag(&set.text, "features", "hooks").unwrap());
    }

    #[test]
    fn an_empty_file_gets_the_section_with_nothing_in_front_of_it() {
        let set = set_flag("", "features", "hooks").unwrap();
        assert_eq!(set.text, "[features]\nhooks = true\n");
    }

    #[test]
    fn the_key_goes_into_a_section_that_is_already_there() {
        let text = "[features]\nother = 1\n\n[profiles.work]\nmodel = \"b\"\n";
        let set = set_flag(text, "features", "hooks").unwrap();
        assert_eq!(
            set.text,
            "[features]\nhooks = true\nother = 1\n\n[profiles.work]\nmodel = \"b\"\n"
        );
        assert!(!set.created_section);
        parses(&set.text);
    }

    #[test]
    fn the_key_is_written_with_the_indentation_the_section_already_uses() {
        let text = "[features]\n    other = 1\n";
        let set = set_flag(text, "features", "hooks").unwrap();
        assert_eq!(set.text, "[features]\n    hooks = true\n    other = 1\n");
    }

    #[test]
    fn a_key_that_already_says_true_is_left_alone_down_to_its_spacing() {
        for text in [
            "[features]\nhooks   =   true\n",
            "[features]\nhooks = true # on purpose\n",
        ] {
            let set = set_flag(text, "features", "hooks").unwrap();
            assert_eq!(set.text, text);
            assert!(!set.created_section);
            assert!(flag(text, "features", "hooks").unwrap());
        }
    }

    #[test]
    fn a_key_set_to_something_else_has_only_its_value_replaced() {
        let text = "[features]\nhooks = false  # turned off once\nother = 1\n";
        let set = set_flag(text, "features", "hooks").unwrap();
        assert_eq!(
            set.text,
            "[features]\nhooks = true  # turned off once\nother = 1\n"
        );
        parses(&set.text);
    }

    #[test]
    fn a_hash_inside_the_value_is_not_a_comment() {
        let text = "[features]\nhooks = \"# not a comment\"\n";
        let set = set_flag(text, "features", "hooks").unwrap();
        assert_eq!(set.text, "[features]\nhooks = true\n");
    }

    #[test]
    fn a_section_of_the_same_name_under_another_one_is_never_touched() {
        let text = "[profiles.features]\nhooks = false\n";
        let set = set_flag(text, "features", "hooks").unwrap();
        assert_eq!(
            set.text,
            "[profiles.features]\nhooks = false\n\n[features]\nhooks = true\n"
        );
        assert!(set.created_section);
        parses(&set.text);
    }

    #[test]
    fn an_array_of_tables_of_the_same_name_is_not_the_section() {
        let text = "[[features]]\nhooks = false\n";
        let set = set_flag(text, "features", "hooks").unwrap();
        assert!(set.created_section, "{:?}", set.text);
        assert!(set.text.starts_with(text), "{:?}", set.text);
    }

    #[test]
    fn a_line_that_looks_like_a_header_inside_an_array_is_not_one() {
        let text = "keys = [\n  [\"features\"],\n]\n";
        let set = set_flag(text, "features", "hooks").unwrap();
        assert!(set.created_section);
        assert_eq!(set.text, format!("{text}\n[features]\nhooks = true\n"));
        parses(&set.text);
    }

    #[test]
    fn a_section_written_twice_is_refused() {
        let text = "[features]\nother = 1\n\n[features]\nmore = 2\n";
        assert_eq!(
            set_flag(text, "features", "hooks"),
            Err(Problem::RepeatedSection {
                section: String::from("features")
            })
        );
    }

    #[test]
    fn a_key_written_twice_in_the_section_is_refused() {
        let text = "[features]\nhooks = false\nhooks = true\n";
        assert_eq!(
            set_flag(text, "features", "hooks"),
            Err(Problem::RepeatedKey {
                section: String::from("features"),
                key: String::from("hooks"),
            })
        );
    }

    #[test]
    fn a_value_that_carries_on_to_the_next_line_is_refused() {
        let text = "[features]\nhooks = [\n  1,\n]\n";
        assert_eq!(
            set_flag(text, "features", "hooks"),
            Err(Problem::Spanning {
                key: String::from("hooks")
            })
        );
    }

    #[test]
    fn the_key_inside_a_multi_line_string_is_refused_rather_than_edited() {
        let text = "note = \"\"\"\n[features]\nhooks = true\n\"\"\"\n";
        let problem = set_flag(text, "features", "hooks").unwrap_err();
        assert!(matches!(problem, Problem::InString { .. }), "{problem:?}");
    }

    #[test]
    fn taking_the_key_out_leaves_a_section_somebody_else_wrote() {
        let text = "[features]\nother = 1\n";
        let set = set_flag(text, "features", "hooks").unwrap();
        assert_eq!(
            clear_flag(&set.text, "features", "hooks", set.created_section).unwrap(),
            text
        );
    }

    #[test]
    fn taking_the_key_out_takes_the_section_this_program_wrote_with_it() {
        for text in [
            "",
            "\n",
            "model = \"a\"\n",
            "model = \"a\"",
            "model = \"a\"\r\n",
            "# a note\n\n[profiles.work]\nmodel = \"b\"\n",
        ] {
            let set = set_flag(text, "features", "hooks").unwrap();
            assert_eq!(
                clear_flag(&set.text, "features", "hooks", set.created_section).unwrap(),
                text,
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_section_this_program_wrote_that_somebody_has_added_to_stays() {
        let set = set_flag("model = \"a\"\n", "features", "hooks").unwrap();
        let theirs = format!("{}other = 1\n", set.text);
        assert_eq!(
            clear_flag(&theirs, "features", "hooks", set.created_section).unwrap(),
            "model = \"a\"\n\n[features]\nother = 1\n"
        );
    }

    #[test]
    fn taking_out_a_key_that_is_not_there_changes_nothing() {
        for text in ["", "model = \"a\"\n", "[features]\nother = 1\n"] {
            assert_eq!(clear_flag(text, "features", "hooks", false).unwrap(), text);
            assert!(!flag(text, "features", "hooks").unwrap());
        }
    }

    #[test]
    fn setting_the_flag_twice_hands_back_the_very_same_bytes() {
        for text in ["", "model = \"a\"\n", "[features]\nother = 1\n"] {
            let once = set_flag(text, "features", "hooks").unwrap().text;
            assert_eq!(set_flag(&once, "features", "hooks").unwrap().text, once);
        }
    }

    #[test]
    fn a_string_comes_out_quoted_with_every_escape_the_format_asks_for() {
        assert_eq!(string("plain"), "\"plain\"");
        assert_eq!(string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(string("a\tb\nc\rd"), "\"a\\tb\\nc\\rd\"");
        assert_eq!(string("\u{8}\u{c}"), "\"\\b\\f\"");
        assert_eq!(string("\u{1}\u{7f}"), "\"\\u0001\\u007F\"");
        assert_eq!(string("héllo"), "\"héllo\"");
    }

    #[test]
    fn every_string_this_writes_reads_back_as_what_went_in() {
        for value in [
            "plain",
            "a\"b\\c",
            "a\tb",
            "\u{1}",
            "C:\\Users\\a b\\agentbus.exe",
        ] {
            let text = format!("value = {}\n", string(value));
            let parsed: toml::Value = toml::from_str(&text).unwrap();
            assert_eq!(parsed["value"].as_str(), Some(value));
        }
    }
}
