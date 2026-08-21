//! Editing a YAML file somebody keeps by hand.
//!
//! One of the agents this program installs for is told which plugins to load by
//! a list in a YAML file of its user's own. Putting a name into that list, and
//! taking it out again, is the whole of what this does, and it does it the way
//! [`toml_text`](crate::toml_text) does its job: by line, touching the lines it
//! owns and copying every other byte through.
//!
//! YAML is a large format and this understands a corner of it — block
//! mappings, block sequences, and scalars that fit on one line. That is a
//! deliberate corner rather than an unfinished one. Everything outside it is
//! refused by name and nothing is written: a flow sequence would have to be
//! reformatted to add to it, an anchor or a tag means the value is somewhere
//! else, and a shape this cannot name is one it has no business rewriting. A
//! refusal tells somebody what to change; a wrong guess loses them a list.

use std::ops::Range;

use crate::lines::Lines;

/// The indentation a level is written with when the file has none to copy.
const DEFAULT_STEP: usize = 2;

/// Why a file cannot be edited.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    /// A line is indented with a tab, which YAML does not allow and this
    /// cannot measure against the spaces around it.
    #[error("line {line} is indented with a tab, which is not something this can edit")]
    Tabs { line: usize },
    /// The file holds more than one document.
    #[error("the file holds more than one document, and there is no telling which one to edit")]
    Documents,
    /// A key on the way to the list is written twice.
    #[error(
        "`{key}` appears more than once at the same level, so there is no telling which is which"
    )]
    Repeated { key: String },
    /// A key on the way to the list holds a flow collection.
    #[error(
        "`{key}` is written on one line as `[…]` or `{{…}}`, which this cannot add to without rewriting it"
    )]
    Flow { key: String },
    /// A key on the way to the list holds a plain value.
    #[error("`{key}` holds a value where this needs a list")]
    Scalar { key: String },
    /// A key on the way to the list carries an anchor, an alias or a tag.
    #[error("`{key}` carries an anchor, an alias or a tag, so what it holds is not written here")]
    Anchored { key: String },
    /// A key on the way to the list holds a block scalar.
    #[error("`{key}` holds a block of text where this needs a list")]
    Block { key: String },
    /// A key on the way to the list holds a list where this needs a mapping.
    #[error("`{key}` holds a list where this needs `{wanted}` under it")]
    Mapping { key: String, wanted: String },
    /// An entry of the list is not a scalar this can compare against.
    #[error("`{entry}` is an entry this cannot read, so the list is left as it is")]
    Entry { entry: String },
    /// A line in the list is a shape this editor has no name for.
    #[error("line {line} is not something this can read, so the list is left as it is")]
    Unreadable { line: usize },
    /// The value cannot be written into a list at all.
    #[error("`{value}` cannot be written as a one-line entry")]
    Unwritable { value: String },
}

/// What adding to a list did, over and above the text it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    /// The file as it now reads.
    pub text: String,
    /// How many of the path's keys had to be written, counted from the end.
    ///
    /// The one thing about this edit that cannot be worked out again from what
    /// is on disk: a list holding nothing but this program's entry looks the
    /// same whether this program wrote the keys above it or found them. The
    /// caller records the answer and hands it back when the entry comes out.
    pub created: usize,
}

/// Whether the block list at `path` holds `value`.
pub fn contains(text: &str, path: &[&str], value: &str) -> Result<bool, Problem> {
    let lines = Lines::of(text);
    let shapes = classify(&lines)?;
    let descent = descend(&shapes, path, step(&shapes))?;
    let Some(list) = descent.list else {
        return Ok(false);
    };
    Ok(entry(&shapes, &list, value)?.is_some())
}

/// The file with `value` in the block list at `path`.
///
/// Any key of the path that is not there is written, each one indented a level
/// further than the last; the entry itself goes on the end of the list, which
/// is where somebody reading the file would expect a new one. A value already
/// in the list is a no-op that hands back the bytes it was given.
pub fn add_to_list(text: &str, path: &[&str], value: &str) -> Result<Added, Problem> {
    let written = written(value)?;
    let mut lines = Lines::of(text);
    let shapes = classify(&lines)?;
    let step = step(&shapes);
    let descent = descend(&shapes, path, step)?;

    if let Some(list) = descent.list {
        if entry(&shapes, &list, value)?.is_some() {
            return Ok(Added {
                text: text.to_owned(),
                created: 0,
            });
        }
        let indent = " ".repeat(list.indent);
        lines.insert(list.after, format!("{indent}- {written}"));
        return Ok(Added {
            text: lines.render(),
            created: 0,
        });
    }

    let gap = descent
        .gap
        .expect("a descent that found no list found a gap");
    let created = path.len() - descent.found.len();
    let mut fresh = Vec::new();
    for (level, key) in path[descent.found.len()..].iter().enumerate() {
        let indent = " ".repeat(gap.indent + level * step);
        fresh.push(format!("{indent}{key}:"));
    }
    let indent = " ".repeat(gap.indent + created * step);
    fresh.push(format!("{indent}- {written}"));
    lines.insert_all(gap.at, fresh);
    Ok(Added {
        text: lines.render(),
        created,
    })
}

/// The file with `value` taken out of the block list at `path`.
///
/// `created` is what [`add_to_list`] reported when it put the value there: the
/// keys it had to write are the keys this takes away again, and only when
/// nothing is left under them.
pub fn remove_from_list(
    text: &str,
    path: &[&str],
    value: &str,
    created: usize,
) -> Result<String, Problem> {
    let mut lines = Lines::of(text);
    let shapes = classify(&lines)?;
    let descent = descend(&shapes, path, step(&shapes))?;
    let Some(list) = descent.list else {
        return Ok(text.to_owned());
    };
    let Some(at) = entry(&shapes, &list, value)? else {
        return Ok(text.to_owned());
    };
    lines.remove(at..at + 1);

    // Worked out again from the file as it now reads, because the line just
    // taken out has moved every line after it. Innermost first, because a key
    // only comes out once what was under it has.
    for depth in (path.len().saturating_sub(created)..path.len()).rev() {
        let shapes = classify(&lines)?;
        let found = trace(&shapes, &path[..depth + 1])?;
        if found.len() != depth + 1 {
            break;
        }
        let key = found[depth];
        let region = held(&shapes, key + 1, indent_of(&shapes, key));
        // A comment somebody wrote under the key is a reason to leave the key
        // where it is; blank lines are not, and go with it.
        if !region.clone().all(|line| shapes[line] == Shape::Blank) {
            break;
        }
        lines.remove(key..region.end);
    }
    Ok(lines.render())
}

/// What a line is, as far as this editor needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    /// A mapping key, with what was written after its colon.
    Key {
        indent: usize,
        name: String,
        value: String,
    },
    /// A sequence entry, with what was written after its dash.
    Item { indent: usize, value: String },
    /// A line holding nothing.
    Blank,
    /// A comment, a document marker, or the inside of a block of text.
    Skip,
    /// Something this editor has no name for.
    Other,
}

impl Shape {
    /// Whether this line is one the shape of the file is read from.
    fn structural(&self) -> bool {
        !matches!(self, Shape::Blank | Shape::Skip)
    }

    /// How far this line is indented, for the lines that are indented at all.
    fn indent(&self) -> Option<usize> {
        match self {
            Shape::Key { indent, .. } | Shape::Item { indent, .. } => Some(*indent),
            _ => None,
        }
    }
}

/// How far down `path` the file goes, and what was found at the end of it.
#[derive(Debug)]
struct Descent {
    /// The line each key of the path was found on, in order.
    found: Vec<usize>,
    /// The list under the last key, when every key of the path was found.
    list: Option<List>,
    /// Where the first key that is not there would go, when one is not.
    gap: Option<Gap>,
}

/// A block list, as the lines it is made of.
#[derive(Debug)]
struct List {
    /// The line each entry sits on.
    items: Vec<usize>,
    /// How far the entries are indented.
    indent: usize,
    /// Where another entry would go.
    after: usize,
}

/// Where a key that is not in the file would be written.
#[derive(Debug)]
struct Gap {
    /// The line it would become.
    at: usize,
    /// How far it would be indented.
    indent: usize,
}

/// Walks `path` down the file.
fn descend(shapes: &[Shape], path: &[&str], step: usize) -> Result<Descent, Problem> {
    let mut found = Vec::new();
    let mut region = 0..shapes.len();
    let mut indent = 0;
    for (level, key) in path.iter().enumerate() {
        let last = level + 1 == path.len();
        let Some(at) = key_in(shapes, region.clone(), indent, key)? else {
            return Ok(Descent {
                found,
                list: None,
                gap: Some(Gap {
                    at: opening(shapes, region),
                    indent,
                }),
            });
        };
        let Shape::Key { value, .. } = &shapes[at] else {
            unreachable!("a key was found on a line that is not one");
        };
        holdable(key, value)?;
        found.push(at);
        let held = held(shapes, at + 1, indent);
        let Some(child) = held.clone().find(|line| shapes[*line].structural()) else {
            // Nothing under the key at all, which is a key that holds nothing.
            // The next level down, or the list, is written from scratch.
            return Ok(Descent {
                found,
                list: match last {
                    true => Some(List {
                        items: Vec::new(),
                        indent: indent + step,
                        after: at + 1,
                    }),
                    false => None,
                },
                gap: match last {
                    true => None,
                    false => Some(Gap {
                        at: at + 1,
                        indent: indent + step,
                    }),
                },
            });
        };
        match (&shapes[child], last) {
            (
                Shape::Item {
                    indent: at_indent, ..
                },
                true,
            ) => {
                return Ok(Descent {
                    list: Some(list(shapes, held, *at_indent)?),
                    found,
                    gap: None,
                });
            }
            (Shape::Key { .. }, true) => {
                return Err(Problem::Mapping {
                    key: (*key).to_owned(),
                    wanted: String::from("a list"),
                });
            }
            (Shape::Item { .. }, false) => {
                return Err(Problem::Mapping {
                    key: (*key).to_owned(),
                    wanted: path[level + 1].to_owned(),
                });
            }
            (
                Shape::Key {
                    indent: at_indent, ..
                },
                false,
            ) => {
                indent = *at_indent;
                region = held;
            }
            (_, _) => return Err(Problem::Unreadable { line: child + 1 }),
        }
    }
    // Only an empty path gets here, and an empty path names the file itself.
    Ok(Descent {
        found,
        list: None,
        gap: Some(Gap { at: 0, indent: 0 }),
    })
}

/// The line each key of `path` sits on, for as far down as the file goes.
///
/// What [`descend`] does without the questions it asks about what it finds at
/// the end, because taking a key away again is not the moment to refuse a file
/// over the shape of what is left in it.
fn trace(shapes: &[Shape], path: &[&str]) -> Result<Vec<usize>, Problem> {
    let mut found = Vec::new();
    let mut region = 0..shapes.len();
    let mut indent = 0;
    for key in path {
        let Some(at) = key_in(shapes, region.clone(), indent, key)? else {
            break;
        };
        found.push(at);
        let held = held(shapes, at + 1, indent);
        let Some(child) = held.clone().find(|line| shapes[*line].structural()) else {
            break;
        };
        let Shape::Key { indent: at, .. } = &shapes[child] else {
            break;
        };
        indent = *at;
        region = held;
    }
    Ok(found)
}

/// Which line in `region` holds the key `name` at `indent`.
fn key_in(
    shapes: &[Shape],
    region: Range<usize>,
    indent: usize,
    name: &str,
) -> Result<Option<usize>, Problem> {
    let mut found = None;
    for line in region {
        let Shape::Key {
            indent: at,
            name: is,
            ..
        } = &shapes[line]
        else {
            continue;
        };
        if *at != indent || is != name {
            continue;
        }
        if found.is_some() {
            return Err(Problem::Repeated {
                key: name.to_owned(),
            });
        }
        found = Some(line);
    }
    Ok(found)
}

/// The lines held by a key on line `from - 1` that is indented `indent`.
///
/// A block ends at the first line that belongs to something else: a key at the
/// same indentation or less, an entry indented less than the key — an entry at
/// the same indentation as its key is that key's own list — or a line this
/// cannot read at all.
fn held(shapes: &[Shape], from: usize, indent: usize) -> Range<usize> {
    for (line, shape) in shapes.iter().enumerate().skip(from) {
        let ends = match shape {
            Shape::Key { indent: at, .. } => *at <= indent,
            Shape::Item { indent: at, .. } => *at < indent,
            Shape::Other => true,
            Shape::Blank | Shape::Skip => false,
        };
        if ends {
            return from..line;
        }
    }
    from..shapes.len()
}

/// The list made of the entries in `region` indented `indent`.
fn list(shapes: &[Shape], region: Range<usize>, indent: usize) -> Result<List, Problem> {
    let mut items = Vec::new();
    let mut after = region.start;
    for line in region {
        match &shapes[line] {
            Shape::Item { indent: at, .. } if *at == indent => {
                items.push(line);
                after = line + 1;
            }
            Shape::Blank | Shape::Skip => continue,
            _ => return Err(Problem::Unreadable { line: line + 1 }),
        }
    }
    Ok(List {
        items,
        indent,
        after,
    })
}

/// Which line of the list holds `value`.
fn entry(shapes: &[Shape], list: &List, value: &str) -> Result<Option<usize>, Problem> {
    for &line in &list.items {
        let Shape::Item { value: written, .. } = &shapes[line] else {
            continue;
        };
        if scalar(written)? == value {
            return Ok(Some(line));
        }
    }
    Ok(None)
}

/// Where a key written into `region` would go.
///
/// On the end of what is there, but in front of the blank lines and comments
/// that trail it — unless the region runs to the end of the file, where the end
/// is where a person would put it.
fn opening(shapes: &[Shape], region: Range<usize>) -> usize {
    if region.end == shapes.len() {
        return region.end;
    }
    region
        .clone()
        .rev()
        .find(|line| shapes[*line].structural())
        .map_or(region.start, |line| line + 1)
}

/// How far the key on `line` is indented.
fn indent_of(shapes: &[Shape], line: usize) -> usize {
    shapes[line].indent().unwrap_or(0)
}

/// Refuses a key whose value says that what this is looking for is not under
/// it.
fn holdable(key: &str, value: &str) -> Result<(), Problem> {
    let key = key.to_owned();
    match value.chars().next() {
        None => Ok(()),
        Some('[' | '{') => Err(Problem::Flow { key }),
        Some('&' | '*' | '!') => Err(Problem::Anchored { key }),
        Some('|' | '>') => Err(Problem::Block { key }),
        Some(_) => Err(Problem::Scalar { key }),
    }
}

/// The indentation one level is written with in this file.
///
/// The shallowest indentation anything in the file is written at, which in a
/// file written by one hand is one level. It only decides what a key this
/// program writes looks like, so being wrong about an unusual file costs a diff
/// and nothing more.
fn step(shapes: &[Shape]) -> usize {
    shapes
        .iter()
        .filter_map(Shape::indent)
        .filter(|indent| *indent > 0)
        .min()
        .unwrap_or(DEFAULT_STEP)
}

/// Works out what every line in the file is.
fn classify(lines: &Lines) -> Result<Vec<Shape>, Problem> {
    let mut shapes = Vec::with_capacity(lines.count());
    let mut block: Option<usize> = None;
    let mut documents = 0;
    let mut started = false;
    for index in 0..lines.count() {
        let line = lines.text(index);
        let trimmed = line.trim();
        if let Some(owner) = block {
            let inside = trimmed.is_empty() || leading(line).len() > owner;
            if inside {
                shapes.push(Shape::Skip);
                continue;
            }
            block = None;
        }
        if trimmed.is_empty() {
            shapes.push(Shape::Blank);
            continue;
        }
        if trimmed.starts_with('#') {
            shapes.push(Shape::Skip);
            continue;
        }
        if trimmed == "---" || trimmed.starts_with("--- ") || trimmed == "..." {
            documents += 1;
            if documents > 1 || started {
                return Err(Problem::Documents);
            }
            shapes.push(Shape::Skip);
            continue;
        }
        let lead = leading(line);
        if lead.contains('\t') {
            return Err(Problem::Tabs { line: index + 1 });
        }
        started = true;
        let indent = lead.len();
        let shape = match entry_value(trimmed) {
            Some(value) => Shape::Item {
                indent,
                value: value.to_owned(),
            },
            None => match key_value(trimmed) {
                Some((name, value)) => Shape::Key {
                    indent,
                    name,
                    value: value.to_owned(),
                },
                None => Shape::Other,
            },
        };
        if let Shape::Key { value, .. } | Shape::Item { value, .. } = &shape
            && value.starts_with(['|', '>'])
        {
            block = Some(indent);
        }
        shapes.push(shape);
    }
    Ok(shapes)
}

/// The whitespace a line starts with.
fn leading(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// What a sequence entry holds, if the line is one.
fn entry_value(trimmed: &str) -> Option<&str> {
    if trimmed == "-" {
        return Some("");
    }
    let rest = trimmed.strip_prefix("- ")?;
    Some(uncommented(rest).trim())
}

/// The key a line names and what it holds, if the line is a mapping key.
///
/// A colon only opens a mapping when a space or the end of the line follows it,
/// which is what keeps `a:b` — a plain value with a colon in it — from reading
/// as a key.
fn key_value(trimmed: &str) -> Option<(String, &str)> {
    let (name, rest) = match trimmed.starts_with(['"', '\'']) {
        true => {
            let quote = trimmed.as_bytes()[0] as char;
            let at = trimmed[1..].find(quote)? + 1;
            (trimmed[1..at].to_owned(), &trimmed[at + 1..])
        }
        false => {
            let mut at = None;
            let mut cursor = 0;
            while let Some(offset) = trimmed[cursor..].find(':') {
                let colon = cursor + offset;
                let after = &trimmed[colon + 1..];
                if after.is_empty() || after.starts_with(' ') {
                    at = Some(colon);
                    break;
                }
                cursor = colon + 1;
            }
            let at = at?;
            (trimmed[..at].trim().to_owned(), &trimmed[at..])
        }
    };
    let rest = rest.trim_start().strip_prefix(':')?;
    match name.is_empty() {
        true => None,
        false => Some((name, uncommented(rest).trim())),
    }
}

/// What is left of a value once the comment at the end of its line is taken
/// off.
///
/// A `#` only starts a comment when it stands on its own, so one inside a
/// quoted value, or in the middle of a word, stays where it is.
fn uncommented(value: &str) -> &str {
    let mut quote = None;
    for (at, character) in value.char_indices() {
        match (quote, character) {
            (Some(open), _) if character == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(character),
            (None, '#') if at == 0 || value[..at].ends_with(char::is_whitespace) => {
                return &value[..at];
            }
            (None, _) => {}
        }
    }
    value
}

/// What a list entry says, with the quotes taken off.
fn scalar(written: &str) -> Result<String, Problem> {
    let refuse = || Problem::Entry {
        entry: written.to_owned(),
    };
    if written.starts_with(['[', '{', '&', '*', '!', '|', '>', '?']) {
        return Err(refuse());
    }
    if let Some(inner) = written
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return Ok(inner.replace("''", "'"));
    }
    if let Some(inner) = written
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        let mut out = String::with_capacity(inner.len());
        let mut characters = inner.chars();
        while let Some(character) = characters.next() {
            if character != '\\' {
                out.push(character);
                continue;
            }
            match characters.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('/') => out.push('/'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                // Every other escape is one this does not spell out, and a
                // guess at it would be a wrong answer to "is this already
                // here?".
                _ => return Err(refuse()),
            }
        }
        return Ok(out);
    }
    if written.contains(": ") || written.ends_with(':') || written.contains('"') {
        return Err(refuse());
    }
    Ok(written.to_owned())
}

/// `value` as it goes into the file.
///
/// Plainly where it can be read back as itself, and in single quotes where it
/// could not — a name that YAML would read as a number, a date or a boolean,
/// or one carrying a character that means something in the place it is going.
fn written(value: &str) -> Result<String, Problem> {
    let unwritable = || Problem::Unwritable {
        value: value.to_owned(),
    };
    if value.is_empty() || value.chars().any(|character| character.is_control()) {
        return Err(unwritable());
    }
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_.-/@".contains(character))
        && value
            .starts_with(|character: char| character.is_ascii_alphanumeric() || character == '_')
        && value.parse::<f64>().is_err()
        && !RESERVED.iter().any(|word| word.eq_ignore_ascii_case(value));
    match plain {
        true => Ok(value.to_owned()),
        false => Ok(format!("'{}'", value.replace('\'', "''"))),
    }
}

/// The words a YAML reader would take for something other than text.
const RESERVED: &[&str] = &[
    "true", "false", "yes", "no", "on", "off", "null", "y", "n", "~",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Where the one list this exists to edit lives.
    const PATH: &[&str] = &["plugins", "enabled"];

    /// What goes in it.
    const NAME: &str = "agentbus-hooks";

    /// Every file the list edit has to survive.
    fn fixtures() -> Vec<&'static str> {
        vec![
            "",
            "\n",
            "model: a\n",
            "model: a",
            "model: a\r\nlogging:\r\n  level: debug\r\n",
            "# a note somebody wrote\n\nmodel: a\n",
            "plugins:\n",
            "plugins:\n  enabled:\n",
            "plugins:\n  enabled:\n    - theirs\n",
            "plugins:\n  enabled:\n    - 'theirs'   # kept\n    - \"quoted\"\n",
            "plugins:\n  other: 1\n",
            "---\nmodel: a\n",
            "notes: |\n  plugins:\n    enabled:\n      - not this one\n",
        ]
    }

    fn add(text: &str) -> Added {
        add_to_list(text, PATH, NAME).unwrap()
    }

    #[test]
    fn a_file_without_the_value_says_so() {
        for text in fixtures() {
            assert!(!contains(text, PATH, NAME).unwrap(), "{text:?}");
        }
    }

    #[test]
    fn the_value_goes_in_and_reads_back_as_being_there() {
        for text in fixtures() {
            let added = add(text);
            assert!(contains(&added.text, PATH, NAME).unwrap(), "{text:?}");
        }
    }

    #[test]
    fn putting_the_value_in_twice_hands_back_the_very_same_bytes() {
        for text in fixtures() {
            let once = add(text).text;
            let twice = add_to_list(&once, PATH, NAME).unwrap();
            assert_eq!(twice.text, once, "{text:?}");
            assert_eq!(twice.created, 0, "{text:?}");
        }
    }

    #[test]
    fn taking_the_value_out_gives_back_the_file_it_was_put_into() {
        for text in fixtures() {
            let added = add(text);
            assert_eq!(
                remove_from_list(&added.text, PATH, NAME, added.created).unwrap(),
                text,
                "{text:?}"
            );
        }
    }

    #[test]
    fn every_key_that_was_missing_is_written_and_counted() {
        let added = add("model: a\n");
        assert_eq!(
            added.text,
            "model: a\nplugins:\n  enabled:\n    - agentbus-hooks\n"
        );
        assert_eq!(added.created, 2);

        let added = add("plugins:\n  other: 1\n");
        assert_eq!(
            added.text,
            "plugins:\n  other: 1\n  enabled:\n    - agentbus-hooks\n"
        );
        assert_eq!(added.created, 1);

        let added = add("plugins:\n  enabled:\n    - theirs\n");
        assert_eq!(
            added.text,
            "plugins:\n  enabled:\n    - theirs\n    - agentbus-hooks\n"
        );
        assert_eq!(added.created, 0);
    }

    #[test]
    fn the_entries_that_were_there_come_back_exactly_as_they_went_in() {
        let text = "plugins:\n  enabled:\n    - 'theirs'   # kept\n    - \"quoted\"\n";
        let added = add(text);
        assert!(added.text.starts_with(text), "{:?}", added.text);
    }

    #[test]
    fn a_value_already_in_the_list_however_it_is_written_is_left_alone() {
        for entry in ["agentbus-hooks", "'agentbus-hooks'", "\"agentbus-hooks\""] {
            let text = format!("plugins:\n  enabled:\n    - {entry}\n");
            assert!(contains(&text, PATH, NAME).unwrap(), "{entry}");
            assert_eq!(add(&text).text, text, "{entry}");
        }
    }

    #[test]
    fn the_indentation_the_file_already_uses_is_the_one_a_new_key_gets() {
        let added = add("logging:\n    level: debug\n");
        assert_eq!(
            added.text,
            "logging:\n    level: debug\nplugins:\n    enabled:\n        - agentbus-hooks\n"
        );
    }

    #[test]
    fn a_new_entry_lines_up_with_the_entries_already_there() {
        let added = add("plugins:\n  enabled:\n  - theirs\n");
        assert_eq!(
            added.text,
            "plugins:\n  enabled:\n  - theirs\n  - agentbus-hooks\n"
        );
    }

    #[test]
    fn a_file_written_with_windows_line_endings_keeps_them() {
        let added = add("model: a\r\n");
        assert_eq!(
            added.text.matches("\r\n").count(),
            added.text.matches('\n').count()
        );
    }

    #[test]
    fn a_file_that_ended_without_a_newline_still_does() {
        let added = add("model: a");
        assert!(added.text.ends_with("- agentbus-hooks"), "{:?}", added.text);
    }

    #[test]
    fn a_comment_somebody_left_under_the_key_keeps_the_key_where_it_is() {
        let text = "plugins:\n  enabled:\n    # one day\n";
        let added = add(text);
        assert_eq!(
            remove_from_list(&added.text, PATH, NAME, added.created).unwrap(),
            text
        );
    }

    #[test]
    fn what_is_inside_a_block_of_text_is_not_read_as_the_list() {
        let text = "notes: |\n  plugins:\n    enabled:\n      - not this one\n";
        assert!(!contains(text, PATH, NAME).unwrap());
        let added = add(text);
        assert_eq!(added.created, 2);
        assert!(added.text.starts_with(text), "{:?}", added.text);
    }

    #[test]
    fn a_list_written_on_one_line_is_refused_rather_than_rewritten() {
        assert_eq!(
            add_to_list("plugins:\n  enabled: [a, b]\n", PATH, NAME),
            Err(Problem::Flow {
                key: String::from("enabled")
            })
        );
        assert_eq!(
            add_to_list("plugins: {enabled: [a]}\n", PATH, NAME),
            Err(Problem::Flow {
                key: String::from("plugins")
            })
        );
    }

    #[test]
    fn an_anchor_an_alias_or_a_tag_means_the_list_is_somewhere_else() {
        for text in [
            "plugins: &all\n  enabled:\n    - a\n",
            "plugins: *all\n",
            "plugins: !!map\n  enabled:\n    - a\n",
        ] {
            let problem = add_to_list(text, PATH, NAME).unwrap_err();
            assert!(matches!(problem, Problem::Anchored { .. }), "{text:?}");
        }
    }

    #[test]
    fn a_key_holding_a_value_where_a_list_belongs_is_refused() {
        assert_eq!(
            add_to_list("plugins:\n  enabled: none\n", PATH, NAME),
            Err(Problem::Scalar {
                key: String::from("enabled")
            })
        );
    }

    #[test]
    fn a_key_holding_a_block_of_text_where_a_list_belongs_is_refused() {
        assert_eq!(
            add_to_list("plugins:\n  enabled: |\n    a\n", PATH, NAME),
            Err(Problem::Block {
                key: String::from("enabled")
            })
        );
    }

    #[test]
    fn a_list_where_a_mapping_belongs_and_the_other_way_round_are_both_refused() {
        assert_eq!(
            add_to_list("plugins:\n  - a\n", PATH, NAME),
            Err(Problem::Mapping {
                key: String::from("plugins"),
                wanted: String::from("enabled"),
            })
        );
        assert_eq!(
            add_to_list("plugins:\n  enabled:\n    a: 1\n", PATH, NAME),
            Err(Problem::Mapping {
                key: String::from("enabled"),
                wanted: String::from("a list"),
            })
        );
    }

    #[test]
    fn a_key_written_twice_at_the_same_level_is_refused() {
        assert_eq!(
            add_to_list("plugins:\n  enabled:\n    - a\nplugins:\n", PATH, NAME),
            Err(Problem::Repeated {
                key: String::from("plugins")
            })
        );
    }

    #[test]
    fn an_entry_this_cannot_read_leaves_the_list_as_it_is() {
        for entry in ["&a theirs", "*theirs", "!!str theirs", "{a: 1}", "name: 1"] {
            let text = format!("plugins:\n  enabled:\n    - {entry}\n");
            let problem = add_to_list(&text, PATH, NAME).unwrap_err();
            assert!(
                matches!(problem, Problem::Entry { .. }),
                "{entry}: {problem:?}"
            );
        }
    }

    #[test]
    fn a_tab_where_the_indentation_belongs_is_refused() {
        assert_eq!(
            add_to_list("plugins:\n\tenabled:\n", PATH, NAME),
            Err(Problem::Tabs { line: 2 })
        );
    }

    #[test]
    fn a_file_of_more_than_one_document_is_refused() {
        assert_eq!(
            add_to_list("model: a\n---\nmodel: b\n", PATH, NAME),
            Err(Problem::Documents)
        );
    }

    #[test]
    fn taking_out_a_value_that_is_not_there_changes_nothing() {
        for text in fixtures() {
            assert_eq!(
                remove_from_list(text, PATH, NAME, 0).unwrap(),
                text,
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_key_this_program_wrote_that_somebody_has_added_to_stays() {
        let added = add("model: a\n");
        let theirs = added.text.replace(
            "    - agentbus-hooks\n",
            "    - agentbus-hooks\n    - theirs\n",
        );
        assert_eq!(
            remove_from_list(&theirs, PATH, NAME, added.created).unwrap(),
            "model: a\nplugins:\n  enabled:\n    - theirs\n"
        );
    }

    #[test]
    fn a_value_that_yaml_would_read_as_something_else_is_written_in_quotes() {
        assert_eq!(written("plain-name").unwrap(), "plain-name");
        assert_eq!(written("a/b@1.0").unwrap(), "a/b@1.0");
        assert_eq!(written("yes").unwrap(), "'yes'");
        assert_eq!(written("Null").unwrap(), "'Null'");
        assert_eq!(written("12").unwrap(), "'12'");
        assert_eq!(written("a b").unwrap(), "'a b'");
        assert_eq!(written("it's").unwrap(), "'it''s'");
        assert_eq!(written("#a").unwrap(), "'#a'");
        assert_eq!(
            written(""),
            Err(Problem::Unwritable {
                value: String::new()
            })
        );
        assert!(written("a\nb").is_err());
    }

    #[test]
    fn a_quoted_value_is_read_back_as_what_it_says() {
        assert_eq!(scalar("'it''s'").unwrap(), "it's");
        assert_eq!(scalar("\"a\\\"b\"").unwrap(), "a\"b");
        assert_eq!(scalar("plain").unwrap(), "plain");
        assert!(scalar("\"a\\u0041\"").is_err());
    }

    #[test]
    fn a_hash_only_starts_a_comment_when_it_stands_on_its_own() {
        assert_eq!(uncommented("a # b"), "a ");
        assert_eq!(uncommented("a#b"), "a#b");
        assert_eq!(uncommented("'a # b'"), "'a # b'");
        assert_eq!(uncommented("# all of it"), "");
    }
}
