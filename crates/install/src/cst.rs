//! Changing a few lines of a JSON document and leaving the rest of the bytes
//! alone.
//!
//! [`merge`](crate::merge) reads a document into a value, adds to it and writes
//! the whole thing back out. That is the right thing to do to a file an agent
//! generates and owns, where the only reader is the agent and the only writer is
//! this program. It is the wrong thing to do to a file a person keeps: a rewrite
//! loses their comments, straightens their one-liners, turns their tabs into
//! spaces and their `\r\n` into `\n`, and hands them a diff of the whole file
//! where they asked for two lines.
//!
//! So the documents people keep are edited through a concrete syntax tree — a
//! parse that keeps the whitespace, the comments and the punctuation as well as
//! the values — and only the region being changed is written. Everything else
//! comes back out byte for byte, including the things a value-level rewrite
//! cannot even see: which quotes an escape was spelled with, whether a number
//! was written `1e+02` or `100`, and the blank lines at the end.
//!
//! Two dialects, because two kinds of file. [`CstDocument::parse_strict`] reads
//! real JSON and nothing else, for a file whose own program would reject a
//! comment. [`CstDocument::parse_jsonc`] reads and preserves comments and
//! trailing commas, for a file whose own program allows them.
//!
//! # Being sure
//!
//! Editing text with the tree's help is guesswork in a way that rewriting a
//! value is not, and the guesses here are deliberate ones — where to put a comma
//! in a one-line object, which side of a trailing comma an element belongs on.
//! So no edit is trusted. Each one works out what the document is to *mean*
//! first, as a plain value; makes the change to the text; then reads the text
//! back and compares. If the two disagree by so much as a key, the edit is
//! refused, the tree is put back to the last text that was agreed to be right,
//! and the caller gets an [`Error`] naming the file. Nothing here writes to
//! disk, so a refusal costs a plan and never a file.
//!
//! That check is also what makes the one shortcut safe: an edit whose desired
//! value is the value the document already has changes no text at all, and
//! [`CstDocument::render`] hands back the original bytes. Installing twice, or
//! taking out exactly what was just put in, therefore leaves a file with the
//! same modification time it had before anybody looked at it.
//!
//! The document a caller edits here is an object at the top level. Both of the
//! shapes this exists for are, every operation below is addressed by a path of
//! object keys, and a file that turns out to hold an array or a number instead
//! is a file this program does not understand well enough to add to.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use jsonc_parser::ast::{Array as AstArray, Object as AstObject, Value as AstValue};
use jsonc_parser::common::{Range, Ranged};
use jsonc_parser::cst::{CstInputValue, CstNode, CstObject, CstRootNode};
use jsonc_parser::{CollectOptions, ParseOptions, parse_to_ast};
use serde_json::{Map, Value};

use crate::Error;
use crate::json::{self, Problem};

/// What a document is allowed to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// JSON, as the standard has it and nothing more.
    Plain,
    /// JSON with comments and trailing commas.
    Commented,
}

impl Dialect {
    /// How the parser is to read this dialect.
    ///
    /// Everything not part of the dialect is off rather than left at its
    /// default, so that a later version of the parser cannot quietly start
    /// accepting something in a file this program is about to write back.
    fn options(self) -> ParseOptions {
        let commented = self == Dialect::Commented;
        ParseOptions {
            allow_comments: commented,
            allow_trailing_commas: commented,
            allow_loose_object_property_names: false,
            allow_missing_commas: false,
            allow_single_quoted_strings: false,
            allow_hexadecimal_numbers: false,
            allow_unary_plus_numbers: false,
        }
    }
}

/// A document open for editing, and the text it was read from.
#[derive(Debug)]
pub struct CstDocument {
    /// The file this came from, for the message when something is refused.
    path: PathBuf,
    dialect: Dialect,
    /// The bytes as they were read.
    original: String,
    /// What those bytes meant.
    was: Value,
    /// The bytes as every edit so far has left them.
    text: String,
    /// What those bytes mean.
    is: Value,
    /// The tree the next edit is made on, always a parse of `text`.
    root: CstRootNode,
}

impl CstDocument {
    /// Reads a document that is JSON and nothing else.
    ///
    /// A comment, a trailing comma or a single-quoted string is a parse error,
    /// and a key repeated anywhere in the document is refused before any edit is
    /// attempted — a repeat reads perfectly well, with the last one winning, but
    /// there is no way to write the document back out that keeps the ones that
    /// lost.
    pub fn parse_strict(path: &Path, text: &str) -> Result<Self, Error> {
        Self::parse(path, text, Dialect::Plain)
    }

    /// Reads a document that may hold comments and trailing commas, and keeps
    /// them.
    pub fn parse_jsonc(path: &Path, text: &str) -> Result<Self, Error> {
        Self::parse(path, text, Dialect::Commented)
    }

    fn parse(path: &Path, text: &str, dialect: Dialect) -> Result<Self, Error> {
        let value = read(path, text, dialect)?;
        let root = tree(path, text, dialect)?;
        Ok(Self {
            path: path.to_owned(),
            dialect,
            original: text.to_owned(),
            was: value.clone(),
            text: text.to_owned(),
            is: value,
            root,
        })
    }

    /// What the document holds at `at`, as every edit so far has left it.
    ///
    /// An empty path is the document itself.
    pub fn get(&self, at: &[&str]) -> Option<&Value> {
        let mut value = &self.is;
        for key in at {
            value = value.as_object()?.get(*key)?;
        }
        Some(value)
    }

    /// Appends `entry` to the array at `at`, making any object or array on the
    /// way to it that is not there yet.
    ///
    /// Where the container being added to is written on one line, so is the
    /// addition: a one-line object stays a one-line object, spaced and comma'd
    /// the way the rest of it is. Anything else is left to the tree, which
    /// indents a new entry to match the ones around it.
    pub fn append(&mut self, at: &[&str], entry: Value) -> Result<(), Error> {
        let desired = self.appended(at, &entry)?;
        self.edit(desired, |document| document.graft(at, &entry))
    }

    /// Takes out every element of the array at `at` that `keep` says no to, and
    /// with them every container the removal left empty.
    ///
    /// Only containers this call emptied go: one that was already empty when the
    /// file was read is part of what the file's owner wrote, and this has no
    /// business tidying it away. Across two runs the distinction cannot be kept
    /// — a container an install made on the way to its entry and one that was
    /// already there are the same bytes by then, and the later uninstall takes
    /// both. A path that does not lead anywhere is not an error: there was
    /// nothing of ours there, which is the answer an uninstall is asking for.
    pub fn retain(&mut self, at: &[&str], keep: impl Fn(&Value) -> bool) -> Result<(), Error> {
        let mut desired = self.is.clone();
        strip(&mut desired, at, 0, &keep, &self.path)?;
        self.edit(desired, |document| {
            document.strip_tree(at, &keep)?;
            Ok(None)
        })
    }

    /// Puts `value` at `key` at the top level of the document, replacing
    /// whatever was there.
    pub fn set(&mut self, key: &str, value: Value) -> Result<(), Error> {
        let mut desired = self.is.clone();
        desired
            .as_object_mut()
            .ok_or_else(|| conflict(&self.path, &[], "an object"))?
            .insert(key.to_owned(), value.clone());
        self.edit(desired, |document| {
            let object = document.root_object()?;
            match object.get(key) {
                Some(property) => {
                    property.set_value(input(&value));
                    Ok(None)
                }
                None if compact(&object.children()) => {
                    document.splice_property(&[], key, &value).map(Some)
                }
                None => {
                    object.append(key, input(&value));
                    Ok(None)
                }
            }
        })
    }

    /// Takes `key` out of the top level of the document, if it is there.
    pub fn remove(&mut self, key: &str) -> Result<(), Error> {
        let mut desired = self.is.clone();
        desired
            .as_object_mut()
            .ok_or_else(|| conflict(&self.path, &[], "an object"))?
            .remove(key);
        self.edit(desired, |document| {
            if let Some(property) = document.root_object()?.get(key) {
                property.remove();
            }
            Ok(None)
        })
    }

    /// The document as it now stands.
    ///
    /// A document nothing has changed the meaning of gives back the bytes it was
    /// read from, unaltered.
    pub fn render(&self) -> String {
        match self.is == self.was {
            true => self.original.clone(),
            false => self.text.clone(),
        }
    }

    /// Makes one change, and keeps it only if the text still says what it is
    /// supposed to.
    ///
    /// `change` edits the tree in place and answers with the text to take
    /// instead, where it could not use the tree and spliced the text itself.
    fn edit(
        &mut self,
        desired: Value,
        change: impl FnOnce(&Self) -> Result<Option<String>, Error>,
    ) -> Result<(), Error> {
        if desired == self.is {
            return Ok(());
        }
        let settled = change(self).and_then(|spliced| {
            let text = spliced.unwrap_or_else(|| self.root.to_string());
            match read(&self.path, &text, self.dialect)? == desired {
                true => Ok(text),
                false => Err(lossy(&self.path)),
            }
        });
        match settled {
            Ok(text) => {
                self.text = text;
                self.is = desired;
                self.resync();
                Ok(())
            }
            // A refused edit leaves the tree half-changed, and the document is
            // still the caller's to use — it may have several files to plan and
            // only one of them was hopeless.
            Err(error) => {
                self.resync();
                Err(error)
            }
        }
    }

    /// Puts the tree back to the last text that was agreed to be right.
    fn resync(&mut self) {
        if let Ok(root) = CstRootNode::parse(&self.text, &self.dialect.options()) {
            self.root = root;
        }
    }

    /// What the document would mean with `entry` appended to the array at `at`.
    fn appended(&self, at: &[&str], entry: &Value) -> Result<Value, Error> {
        let mut desired = self.is.clone();
        let mut value = &mut desired;
        for (depth, key) in at.iter().enumerate() {
            let entries = value
                .as_object_mut()
                .ok_or_else(|| conflict(&self.path, &at[..depth], "an object"))?;
            let leaf = depth + 1 == at.len();
            value = entries
                .entry((*key).to_owned())
                .or_insert_with(|| match leaf {
                    true => Value::Array(Vec::new()),
                    false => Value::Object(Map::new()),
                });
        }
        value
            .as_array_mut()
            .ok_or_else(|| conflict(&self.path, at, "an array"))?
            .push(entry.clone());
        Ok(desired)
    }

    /// Appends `entry` to the array at `at` in the tree, or says what text to
    /// take instead where the tree would have reformatted somebody's one-liner.
    fn graft(&self, at: &[&str], entry: &Value) -> Result<Option<String>, Error> {
        let mut object = self.root_object()?;
        for (depth, key) in at.iter().enumerate() {
            let Some(property) = object.get(key) else {
                // Nothing from here down exists, so what goes in is one nested
                // value and the whole of it can be written at once.
                if compact(&object.children()) {
                    let value = nested(&at[depth + 1..], entry);
                    return self.splice_property(&at[..depth], key, &value).map(Some);
                }
                let mut at_depth = object;
                for (offset, missing) in at[depth..].iter().enumerate() {
                    if depth + offset + 1 == at.len() {
                        let array = at_depth
                            .append(missing, CstInputValue::Array(Vec::new()))
                            .array_value()
                            .ok_or_else(|| lossy(&self.path))?;
                        array.append(input(entry));
                    } else {
                        at_depth = at_depth
                            .append(missing, CstInputValue::Object(Vec::new()))
                            .object_value()
                            .ok_or_else(|| lossy(&self.path))?;
                    }
                }
                return Ok(None);
            };
            if depth + 1 == at.len() {
                let array = property
                    .array_value()
                    .ok_or_else(|| conflict(&self.path, at, "an array"))?;
                if compact(&array.children()) {
                    return self.splice_element(at, entry).map(Some);
                }
                array.append(input(entry));
                return Ok(None);
            }
            object = property
                .object_value()
                .ok_or_else(|| conflict(&self.path, &at[..depth + 1], "an object"))?;
        }
        Ok(None)
    }

    /// Takes the unwanted elements of the array at `at` out of the tree, along
    /// with every container that leaves empty.
    fn strip_tree(&self, at: &[&str], keep: &impl Fn(&Value) -> bool) -> Result<(), Error> {
        let mut object = self.root_object()?;
        let mut holders = Vec::with_capacity(at.len());
        let mut properties = Vec::with_capacity(at.len());
        for (depth, key) in at.iter().enumerate() {
            let Some(property) = object.get(key) else {
                return Ok(());
            };
            holders.push(object.clone());
            properties.push(property.clone());
            if depth + 1 == at.len() {
                let array = property
                    .array_value()
                    .ok_or_else(|| conflict(&self.path, at, "an array"))?;
                let mut emptied = false;
                for element in array.elements() {
                    if element.to_serde_value().is_some_and(|value| !keep(&value)) {
                        element.remove();
                        emptied = true;
                    }
                }
                if emptied && array.elements().is_empty() {
                    let mut level = depth;
                    properties[level].clone().remove();
                    while level > 0 && holders[level].properties().is_empty() {
                        level -= 1;
                        properties[level].clone().remove();
                    }
                }
                return Ok(());
            }
            object = property
                .object_value()
                .ok_or_else(|| conflict(&self.path, &at[..depth + 1], "an object"))?;
        }
        Ok(())
    }

    /// The object at the top of the tree, made if the document was empty.
    fn root_object(&self) -> Result<CstObject, Error> {
        self.root
            .object_value_or_create()
            .ok_or_else(|| conflict(&self.path, &[], "an object"))
    }

    /// The text with `key` written into the one-line object at `prefix`.
    fn splice_property(&self, prefix: &[&str], key: &str, value: &Value) -> Result<String, Error> {
        let text = self.root.to_string();
        let parsed = self.outline(&text)?;
        let object = descend(&parsed, prefix).ok_or_else(|| lossy(&self.path))?;
        let entry = format!(
            "\"{}\"{}{value}",
            json::escaped(key),
            key_separator(&text, object)
        );
        Ok(splice(
            &text,
            object.range,
            object.properties.last().map(Ranged::end),
            object_delimiter(&text, object),
            &entry,
        ))
    }

    /// The text with `entry` written into the one-line array at `at`.
    fn splice_element(&self, at: &[&str], entry: &Value) -> Result<String, Error> {
        let text = self.root.to_string();
        let parsed = self.outline(&text)?;
        let (key, prefix) = at.split_last().ok_or_else(|| lossy(&self.path))?;
        let array = descend(&parsed, prefix)
            .and_then(|object| object.get_array(key))
            .ok_or_else(|| lossy(&self.path))?;
        Ok(splice(
            &text,
            array.range,
            array.elements.last().map(Ranged::end),
            array_delimiter(&text, array),
            &entry.to_string(),
        ))
    }

    /// Where each of the document's values begins and ends in `text`.
    ///
    /// The tree deliberately does not carry positions — it is a description of
    /// what the document is made of, not of where in the file each part of it
    /// was found — so the one thing that needs them, splicing text into a
    /// one-line container, reads them from a second parse of the same text.
    fn outline<'a>(&self, text: &'a str) -> Result<AstObject<'a>, Error> {
        let parsed = parse_to_ast(text, &CollectOptions::default(), &self.dialect.options())
            .map_err(|error| unreadable(&self.path, &error.to_string()))?;
        match parsed.value {
            Some(AstValue::Object(object)) => Ok(object),
            _ => Err(conflict(&self.path, &[], "an object")),
        }
    }
}

/// Reads what a document means, refusing what cannot be written back.
///
/// The same function reads the file and reads back what an edit produced, so
/// that the two answers are comparable by construction: if the reader is wrong
/// about something, it is wrong about it the same way twice and the comparison
/// still holds.
fn read(path: &Path, text: &str, dialect: Dialect) -> Result<Value, Error> {
    match dialect {
        // An empty file is one this program may write the whole of, and the
        // reader for real JSON has nothing to say about it but "unexpected end
        // of input".
        Dialect::Plain if text.trim().is_empty() => Ok(Value::Object(Map::new())),
        Dialect::Plain => json::parse(text).map_err(|problem| Error::NotRewritable {
            path: path.to_owned(),
            problem,
        }),
        Dialect::Commented => match tree(path, text, dialect)?.value() {
            // A file with nothing in it but comments and space is treated the
            // same way, comments included: they are text, and the value put
            // beside them is what was missing.
            None => Ok(Value::Object(Map::new())),
            Some(value) => {
                refuse_repeats(&value, path)?;
                value.to_serde_value().ok_or_else(|| lossy(path))
            }
        },
    }
}

/// Parses `text` into a tree, naming the file if it will not parse.
fn tree(path: &Path, text: &str, dialect: Dialect) -> Result<CstRootNode, Error> {
    CstRootNode::parse(text, &dialect.options())
        .map_err(|error| unreadable(path, &error.to_string()))
}

/// Refuses a document with the same key twice in one object, anywhere in it.
fn refuse_repeats(node: &CstNode, path: &Path) -> Result<(), Error> {
    if let Some(object) = node.as_object() {
        let mut seen = HashSet::new();
        for property in object.properties() {
            let Some(name) = property.name().and_then(|name| name.decoded_value().ok()) else {
                continue;
            };
            if !seen.insert(name.clone()) {
                return Err(unreadable(
                    path,
                    &format!(
                        "the key \"{name}\" appears twice in one object, and only one of them would survive being written back out"
                    ),
                ));
            }
            if let Some(value) = property.value() {
                refuse_repeats(&value, path)?;
            }
        }
    } else if let Some(array) = node.as_array() {
        for element in array.elements() {
            refuse_repeats(&element, path)?;
        }
    }
    Ok(())
}

/// Takes the unwanted elements out of the array `full[depth..]` leads to, and
/// with them every container that leaves empty.
///
/// Answers with whether `node` is now empty *and* was emptied by this removal,
/// which is what tells the caller above whether to take it away in turn. A
/// container that was already empty answers no, and stays.
fn strip(
    node: &mut Value,
    full: &[&str],
    depth: usize,
    keep: &impl Fn(&Value) -> bool,
    path: &Path,
) -> Result<bool, Error> {
    if depth == full.len() {
        let items = node
            .as_array_mut()
            .ok_or_else(|| conflict(path, full, "an array"))?;
        let before = items.len();
        items.retain(|item| keep(item));
        return Ok(items.len() != before && items.is_empty());
    }
    let entries = node
        .as_object_mut()
        .ok_or_else(|| conflict(path, &full[..depth], "an object"))?;
    let Some(child) = entries.get_mut(full[depth]) else {
        return Ok(false);
    };
    if !strip(child, full, depth + 1, keep, path)? {
        return Ok(false);
    }
    entries.remove(full[depth]);
    Ok(entries.is_empty())
}

/// An array holding `entry`, wrapped in an object for each of `keys`.
fn nested(keys: &[&str], entry: &Value) -> Value {
    let mut value = Value::Array(vec![entry.clone()]);
    for key in keys.iter().rev() {
        value = Value::Object(Map::from_iter([((*key).to_owned(), value)]));
    }
    value
}

/// The tree's spelling of a value.
fn input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(yes) => CstInputValue::Bool(*yes),
        // As written, not as parsed: a number the document spelled `1e+02` goes
        // back in spelled `1e+02`.
        Value::Number(number) => CstInputValue::Number(number.to_string()),
        Value::String(text) => CstInputValue::String(text.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(input).collect()),
        Value::Object(entries) => CstInputValue::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), input(value)))
                .collect(),
        ),
    }
}

/// Whether a container is written on one line.
///
/// Only its own newlines count. A one-line object holding a value that happens
/// to span lines is still a one-line object, and adding to it on a line of its
/// own would look like somebody had reformatted half of it.
fn compact(children: &[CstNode]) -> bool {
    !children.iter().any(CstNode::is_newline)
}

/// The object `keys` leads to.
fn descend<'text, 'ast>(
    object: &'ast AstObject<'text>,
    keys: &[&str],
) -> Option<&'ast AstObject<'text>> {
    let mut at = object;
    for key in keys {
        at = at.get_object(key)?;
    }
    Some(at)
}

/// What a document writes between an object's key and its value.
fn key_separator<'a>(text: &'a str, object: &AstObject<'_>) -> &'a str {
    object.properties.first().map_or(":", |property| {
        &text[property.name.range().end..property.value.range().start]
    })
}

/// What a document writes between one property and the next, after the comma.
///
/// Taken from between the first two, where there are two. Where there is one it
/// is taken from between the opening brace and it, which is where a document
/// that puts a space after its comma has generally put one too.
fn object_delimiter<'a>(text: &'a str, object: &AstObject<'_>) -> &'a str {
    match object.properties.as_slice() {
        [first, second, ..] => after_comma(&text[first.range.end..second.range.start]),
        [only] => &text[object.range.start + 1..only.range.start],
        [] => "",
    }
}

/// What a document writes between one element and the next, after the comma.
fn array_delimiter<'a>(text: &'a str, array: &AstArray<'_>) -> &'a str {
    match array.elements.as_slice() {
        [first, second, ..] => after_comma(&text[first.range().end..second.range().start]),
        [only] => &text[array.range.start + 1..only.range().start],
        [] => "",
    }
}

/// Whatever follows the comma in what separates two entries.
fn after_comma(separator: &str) -> &str {
    separator
        .split_once(',')
        .map_or(separator, |(_, after)| after)
}

/// Writes `value` into the container `range` covers, just inside its closing
/// bracket.
///
/// `last` is where the container's final entry ends, or nothing if it has none.
/// Trailing spaces are stepped over so that `{ "a": 1 }` gains its entry beside
/// the one that is there rather than beyond the space that was keeping the
/// bracket off it.
fn splice(text: &str, range: Range, last: Option<usize>, delimiter: &str, value: &str) -> String {
    let closing = range.end - 1;
    let Some(end) = last else {
        return format!("{}{value}{}", &text[..closing], &text[closing..]);
    };
    match text[end..closing].find(',') {
        // The container already ends in a comma, so the new entry goes in front
        // of it rather than behind, and brings a comma of its own to keep the
        // document written the way it was found.
        Some(offset) => {
            let at = end + offset + 1;
            format!("{}{delimiter}{value},{}", &text[..at], &text[at..])
        }
        None => {
            let at = text[..closing].trim_end_matches([' ', '\t']).len();
            format!("{},{delimiter}{value}{}", &text[..at], &text[at..])
        }
    }
}

/// How a place in a document is named in a message about it.
fn address(at: &[&str]) -> String {
    match at.is_empty() {
        true => "the top level".to_owned(),
        false => at.join("."),
    }
}

/// A document that holds something else where a container has to go.
fn conflict(path: &Path, at: &[&str], needed: &'static str) -> Error {
    Error::Conflict {
        path: path.to_owned(),
        at: address(at),
        needed,
    }
}

/// A document this program will not read.
fn unreadable(path: &Path, reason: &str) -> Error {
    Error::NotRewritable {
        path: path.to_owned(),
        problem: Problem::Unreadable(reason.to_owned()),
    }
}

/// An edit whose text did not turn out to say what the edit meant.
fn lossy(path: &Path) -> Error {
    Error::NotRewritable {
        path: path.to_owned(),
        problem: Problem::Lossy,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::sentinel;

    fn file() -> &'static Path {
        Path::new("/home/someone/.config/agent/settings.json")
    }

    fn entry() -> Value {
        let mut entry = Map::new();
        entry.insert("command".to_owned(), Value::from("agentbus emit"));
        sentinel::mark(&mut entry);
        Value::Object(entry)
    }

    /// Installs the standard entry into `hooks.SessionStart`.
    fn install(text: &str) -> Result<String, Error> {
        let mut document = CstDocument::parse_strict(file(), text)?;
        document.retain(&["hooks", "SessionStart"], |value| {
            !sentinel::is_marked(value)
        })?;
        document.append(&["hooks", "SessionStart"], entry())?;
        Ok(document.render())
    }

    /// Takes it out again.
    fn uninstall(text: &str) -> Result<String, Error> {
        let mut document = CstDocument::parse_strict(file(), text)?;
        document.retain(&["hooks", "SessionStart"], |value| {
            !sentinel::is_marked(value)
        })?;
        Ok(document.render())
    }

    #[test]
    fn everything_outside_the_edit_comes_back_byte_for_byte() {
        let input = concat!(
            "{\r\n",
            "\t\"zeta\" : {\"escaped\":\"\\u0061\", \"number\":1e+02},\r\n",
            "\t\"hooks\" : {\r\n",
            "\t\t\"Notification\" : [{\"matcher\":\"keep\"}]\r\n",
            "\t},\r\n",
            "\t\"alpha\" : 1\r\n",
            "}\r\n\r\n",
        );

        let updated = install(input).unwrap();

        assert!(
            updated.starts_with(concat!(
                "{\r\n",
                "\t\"zeta\" : {\"escaped\":\"\\u0061\", \"number\":1e+02},\r\n",
                "\t\"hooks\" : {\r\n",
                "\t\t\"Notification\" : [{\"matcher\":\"keep\"}],\r\n",
            )),
            "{updated:?}"
        );
        assert!(
            updated.ends_with("\t},\r\n\t\"alpha\" : 1\r\n}\r\n\r\n"),
            "{updated:?}"
        );
        // No line feed anywhere that is not part of a carriage return pair, and
        // no space where the file used a tab.
        assert!(!updated.replace("\r\n", "").contains('\n'), "{updated:?}");
        assert!(!updated.contains("\n  "), "{updated:?}");
    }

    #[test]
    fn a_one_line_document_stays_on_one_line() {
        let written = entry().to_string();
        let cases = [
            (
                r#"{"zeta":{"escaped":"\u0061","n":1e+02},"alpha":1}"#,
                format!(
                    r#"{{"zeta":{{"escaped":"\u0061","n":1e+02}},"alpha":1,"hooks":{{"SessionStart":[{written}]}}}}"#
                ),
            ),
            (
                r#"{"hooks":{"Notification":[1]}, "alpha":1}"#,
                format!(
                    r#"{{"hooks":{{"Notification":[1],"SessionStart":[{written}]}}, "alpha":1}}"#
                ),
            ),
            (
                r#"{"hooks":{"SessionStart":[{"matcher":"keep"}]}}"#,
                format!(r#"{{"hooks":{{"SessionStart":[{{"matcher":"keep"}},{written}]}}}}"#),
            ),
            (
                r#"{ "alpha" : 1 }"#,
                format!(r#"{{ "alpha" : 1, "hooks" : {{"SessionStart":[{written}]}} }}"#),
            ),
            (
                "{}",
                format!(r#"{{"hooks":{{"SessionStart":[{written}]}}}}"#),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(install(input).unwrap(), expected, "from {input}");
        }
    }

    #[test]
    fn a_value_that_spans_lines_does_not_make_its_holder_multiline() {
        let written = entry().to_string();
        let input = "{\"zeta\":{\n  \"x\":1\n},\"alpha\":1}";

        assert_eq!(
            install(input).unwrap(),
            format!(
                "{{\"zeta\":{{\n  \"x\":1\n}},\"alpha\":1,\"hooks\":{{\"SessionStart\":[{written}]}}}}"
            )
        );
    }

    #[test]
    fn a_multiline_document_gets_its_entry_indented_like_the_rest() {
        let input = "{\n  \"hooks\": {\n    \"SessionStart\": [\n      {\"matcher\": \"keep\"}\n    ]\n  }\n}\n";

        let updated = install(input).unwrap();

        assert!(updated.starts_with("{\n  \"hooks\": {\n    \"SessionStart\": [\n"));
        assert!(
            updated.contains("\n      {\"matcher\": \"keep\"},\n      {"),
            "{updated}"
        );
        assert!(updated.ends_with("\n    ]\n  }\n}\n"), "{updated:?}");
    }

    #[test]
    fn installing_what_is_already_there_changes_not_one_byte() {
        let written = entry().to_string();
        let input = format!("{{\"hooks\":{{\"SessionStart\":[{written}]}}}}  \r\n\r\n");

        assert_eq!(install(&input).unwrap(), input);
    }

    #[test]
    fn installing_twice_is_the_same_as_installing_once() {
        for input in [
            "{}",
            "{\n  \"alpha\": 1\n}\n",
            r#"{"hooks":{"SessionStart":[{"matcher":"keep"}]}}"#,
        ] {
            let once = install(input).unwrap();
            assert_eq!(install(&once).unwrap(), once, "from {input}");
        }
    }

    #[test]
    fn taking_out_what_was_put_in_gives_back_the_original_bytes() {
        let corpus = [
            "{}",
            "{\r\n\t\"alpha\" : 1\r\n}\r\n\r\n",
            r#"{"hooks":{"SessionStart":[{"matcher":"keep"}]}}"#,
            "{ \"alpha\" : 1 , \"beta\" : 2 }",
            "{\"unicode\\u00e9\":\"\\ud83d\\ude00\",\"nested\":{\"n\":1e+02}}",
        ];

        for input in corpus {
            let installed = install(input).unwrap();
            assert_ne!(installed, input, "installing did nothing to {input:?}");
            assert_eq!(uninstall(&installed).unwrap(), input, "from {input:?}");
        }
    }

    #[test]
    fn only_the_containers_the_removal_emptied_are_taken_away() {
        // The event array is emptied and goes; so does the hooks object, which
        // held nothing else. The empty object beside it was not ours to tidy.
        let installed = install("{\n  \"spare\": {}\n}\n").unwrap();
        assert_eq!(uninstall(&installed).unwrap(), "{\n  \"spare\": {}\n}\n");

        // An event array that still holds somebody else's entry stays, and so
        // does everything above it.
        let kept = r#"{"hooks":{"SessionStart":[{"matcher":"keep"}]}}"#;
        assert_eq!(uninstall(&install(kept).unwrap()).unwrap(), kept);

        // An event array this program did not empty is left exactly as found.
        let empty = "{\n  \"hooks\": {\n    \"SessionStart\": []\n  }\n}\n";
        assert_eq!(uninstall(empty).unwrap(), empty);
    }

    #[test]
    fn a_container_that_was_already_empty_goes_when_the_removal_empties_it_again() {
        // Nothing on disk tells an uninstall whether the empty object an entry
        // was put into was made by the install or was there before it, so both
        // are treated as the install's own.
        for input in [
            "{\n  \"hooks\": {}\n}\n",
            "{\n  \"hooks\": {\n    \"SessionStart\": []\n  }\n}\n",
        ] {
            let installed = install(input).unwrap();
            assert_eq!(uninstall(&installed).unwrap(), "{}\n", "from {input:?}");
        }
    }

    #[test]
    fn a_document_with_nothing_of_ours_in_it_is_left_alone() {
        for input in ["{}", "{\n  \"alpha\": 1\n}\n", "", "{\"hooks\":{}}"] {
            assert_eq!(uninstall(input).unwrap(), input, "from {input:?}");
        }
    }

    #[test]
    fn a_repeated_key_anywhere_is_refused_and_named() {
        for input in [
            r#"{"alpha": 1, "alpha": 2}"#,
            r#"{"hooks": {"alpha": 1, "alpha": 2}}"#,
            r#"{"hooks": [{"alpha": 1, "alpha": 2}]}"#,
        ] {
            let error = CstDocument::parse_strict(file(), input).unwrap_err();
            assert!(error.to_string().contains("settings.json"), "{error}");
            let Error::NotRewritable {
                problem: Problem::Unreadable(reason),
                ..
            } = &error
            else {
                panic!("{error:?} from {input}");
            };
            assert!(reason.contains("alpha"), "{reason} from {input}");
        }
    }

    #[test]
    fn what_is_not_json_is_refused_by_the_strict_reader() {
        for input in [
            "{\n  // a comment\n  \"alpha\": 1\n}\n",
            "{\"alpha\": 1,}",
            "{'alpha': 1}",
            "not a document at all",
        ] {
            let error = CstDocument::parse_strict(file(), input).unwrap_err();
            assert!(
                matches!(error, Error::NotRewritable { .. }),
                "{error:?} from {input}"
            );
        }
    }

    #[test]
    fn a_refusal_names_the_file_and_leaves_it_as_it_was() {
        let path = Path::new("/home/someone/.config/agent/settings.json");
        let error = CstDocument::parse_strict(path, "{").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("/home/someone/.config/agent/settings.json"),
            "{error}"
        );
    }

    #[test]
    fn a_document_holding_the_wrong_shape_is_refused_and_the_place_named() {
        let mut document = CstDocument::parse_strict(file(), r#"{"hooks": 3}"#).unwrap();
        let error = document
            .append(&["hooks", "SessionStart"], entry())
            .unwrap_err();

        let Error::Conflict { at, needed, .. } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(at, "hooks");
        assert_eq!(*needed, "an object");
        assert_eq!(document.render(), r#"{"hooks": 3}"#);
    }

    #[test]
    fn an_entry_that_is_not_in_an_array_is_refused() {
        let mut document =
            CstDocument::parse_strict(file(), r#"{"hooks": {"SessionStart": {}}}"#).unwrap();
        let error = document
            .append(&["hooks", "SessionStart"], entry())
            .unwrap_err();

        let Error::Conflict { at, needed, .. } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(at, "hooks.SessionStart");
        assert_eq!(*needed, "an array");
    }

    #[test]
    fn a_document_that_is_not_an_object_is_refused() {
        let mut document = CstDocument::parse_strict(file(), "[1, 2]").unwrap();
        let error = document.append(&["hooks"], entry()).unwrap_err();

        let Error::Conflict { at, .. } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(at, "the top level");
    }

    #[test]
    fn comments_and_trailing_commas_survive_an_edit() {
        let input = concat!(
            "{\n",
            "  // Keep this comment.\n",
            "  \"theme\": \"system\",\n",
            "  \"plugin\": [\"already\"],\n",
            "}\n",
        );
        let mut document = CstDocument::parse_jsonc(file(), input).unwrap();
        document
            .append(&["plugin"], Value::from("./hook.js"))
            .unwrap();
        let updated = document.render();

        assert!(updated.contains("// Keep this comment."), "{updated}");
        assert!(updated.starts_with("{\n  // Keep this comment.\n  \"theme\": \"system\",\n"));
        assert!(updated.ends_with(",\n}\n"), "{updated:?}");

        let mut back = CstDocument::parse_jsonc(file(), &updated).unwrap();
        back.retain(&["plugin"], |value| value != "./hook.js")
            .unwrap();
        assert_eq!(back.render(), input);
    }

    #[test]
    fn a_document_that_is_only_a_comment_can_still_be_written_into() {
        let mut document = CstDocument::parse_jsonc(file(), "// nothing here yet\n").unwrap();
        document
            .append(&["plugin"], Value::from("./hook.js"))
            .unwrap();

        let updated = document.render();
        assert!(updated.contains("// nothing here yet"), "{updated:?}");
        assert_eq!(
            CstDocument::parse_jsonc(file(), &updated)
                .unwrap()
                .get(&["plugin"]),
            Some(&json!(["./hook.js"]))
        );
    }

    #[test]
    fn a_document_that_is_only_braces_and_a_comment_keeps_the_comment() {
        let input = "// what this is for\n{}\n";
        let mut document = CstDocument::parse_jsonc(file(), input).unwrap();

        document
            .append(&["plugin"], Value::from("./hook.js"))
            .unwrap();

        assert_eq!(
            document.render(),
            "// what this is for\n{\"plugin\":[\"./hook.js\"]}\n"
        );
    }

    #[test]
    fn a_repeated_key_is_refused_by_the_relaxed_reader_too() {
        let error =
            CstDocument::parse_jsonc(file(), "{\n  // twice\n  \"a\": 1,\n  \"a\": 2,\n}\n")
                .unwrap_err();

        let Error::NotRewritable {
            problem: Problem::Unreadable(reason),
            ..
        } = &error
        else {
            panic!("{error:?}");
        };
        assert!(reason.contains("the key \"a\" appears twice"), "{reason}");
    }

    #[test]
    fn an_edit_the_text_does_not_end_up_agreeing_with_is_refused() {
        // The comma this finds inside a comment is not the trailing comma it
        // takes it for, so the entry would be spliced into the comment. The
        // read-back is what notices, and the document is left as it was.
        let input = r#"{"plugin":["a" /*,*/]}"#;
        let mut document = CstDocument::parse_jsonc(file(), input).unwrap();

        let error = document.append(&["plugin"], Value::from("b")).unwrap_err();

        assert!(
            matches!(
                &error,
                Error::NotRewritable {
                    problem: Problem::Lossy,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("settings.json"), "{error}");
        assert_eq!(document.render(), input);
    }

    #[test]
    fn a_one_line_document_with_a_trailing_comma_keeps_writing_them() {
        let mut document = CstDocument::parse_jsonc(file(), r#"{"plugin":["a",],}"#).unwrap();
        document.append(&["plugin"], Value::from("b")).unwrap();

        assert_eq!(document.render(), r#"{"plugin":["a","b",],}"#);
    }

    #[test]
    fn the_strict_reader_refuses_what_the_relaxed_one_takes() {
        let input = "{\n  // hello\n  \"alpha\": 1,\n}\n";

        assert!(CstDocument::parse_strict(file(), input).is_err());
        assert!(CstDocument::parse_jsonc(file(), input).is_ok());
    }

    #[test]
    fn reading_a_key_path_gives_back_what_is_there_now() {
        let mut document =
            CstDocument::parse_strict(file(), r#"{"hooks":{"SessionStart":[1]}}"#).unwrap();

        assert_eq!(document.get(&["hooks", "SessionStart"]), Some(&json!([1])));
        assert_eq!(document.get(&["hooks", "Missing"]), None);
        assert_eq!(
            document.get(&[]),
            Some(&json!({"hooks":{"SessionStart":[1]}}))
        );

        document
            .append(&["hooks", "SessionStart"], Value::from(2))
            .unwrap();
        assert_eq!(
            document.get(&["hooks", "SessionStart"]),
            Some(&json!([1, 2]))
        );
    }

    #[test]
    fn a_top_level_key_can_be_set_and_taken_away_again() {
        let input = "{\n  \"alpha\": 1\n}\n";
        let mut document = CstDocument::parse_strict(file(), input).unwrap();

        document.set("beta", json!({"on": true})).unwrap();
        let updated = document.render();
        assert!(updated.starts_with("{\n  \"alpha\": 1,\n"), "{updated:?}");
        assert_eq!(document.get(&["beta", "on"]), Some(&Value::Bool(true)));

        document.remove("beta").unwrap();
        assert_eq!(document.render(), input);
    }

    #[test]
    fn setting_a_key_that_is_there_replaces_only_its_value() {
        let mut document =
            CstDocument::parse_strict(file(), "{\n  \"alpha\" : 1,\n  \"beta\" : 2\n}\n").unwrap();

        document.set("alpha", Value::from(9)).unwrap();

        assert_eq!(
            document.render(),
            "{\n  \"alpha\" : 9,\n  \"beta\" : 2\n}\n"
        );
    }

    #[test]
    fn setting_a_key_on_a_one_line_document_leaves_it_on_one_line() {
        let mut document = CstDocument::parse_strict(file(), r#"{"alpha":1}"#).unwrap();

        document.set("beta", Value::from(2)).unwrap();

        assert_eq!(document.render(), r#"{"alpha":1,"beta":2}"#);
    }

    #[test]
    fn taking_away_a_key_that_is_not_there_changes_nothing() {
        let input = "{\n  \"alpha\": 1\n}\n";
        let mut document = CstDocument::parse_strict(file(), input).unwrap();

        document.remove("beta").unwrap();

        assert_eq!(document.render(), input);
    }

    #[test]
    fn a_key_written_with_an_escape_is_read_and_written_as_the_same_key() {
        let mut document =
            CstDocument::parse_strict(file(), "{\"caf\\u00e9\":{\"SessionStart\":[]}}").unwrap();

        document
            .append(&["café", "SessionStart"], Value::from(1))
            .unwrap();

        assert_eq!(document.render(), "{\"caf\\u00e9\":{\"SessionStart\":[1]}}");
    }

    #[test]
    fn a_number_nobody_touched_is_written_back_exactly_as_it_was_spelled() {
        let mut document = CstDocument::parse_strict(
            file(),
            "{\n  \"big\": 1e+02,\n  \"small\": 0.30000000000000004,\n  \"hooks\": {}\n}\n",
        )
        .unwrap();

        document.append(&["hooks", "Stop"], entry()).unwrap();

        let updated = document.render();
        assert!(updated.contains("\"big\": 1e+02,"), "{updated}");
        assert!(
            updated.contains("\"small\": 0.30000000000000004,"),
            "{updated}"
        );
    }

    #[test]
    fn trailing_blank_lines_are_kept() {
        let updated = install("{\n  \"hooks\": {}\n}\n\n\n").unwrap();

        assert!(updated.ends_with("}\n\n\n"), "{updated:?}");
    }

    #[test]
    fn an_empty_file_becomes_a_document_with_the_entry_in_it() {
        for input in ["", "\n\n"] {
            let updated = install(input).unwrap();

            assert_eq!(
                CstDocument::parse_strict(file(), &updated)
                    .unwrap()
                    .get(&["hooks", "SessionStart"]),
                Some(&Value::Array(vec![entry()])),
                "from {input:?}"
            );
            // Taking the entry out again leaves a document rather than an empty
            // file: whether the file is worth keeping at all is a question about
            // who created it, which is not one this can answer.
            let emptied = uninstall(&updated).unwrap();
            assert_eq!(emptied.trim(), "{}", "from {input:?}");
        }
    }
}
