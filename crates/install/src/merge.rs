//! Putting this program's entries into a JSON file, and taking them out again.
//!
//! Both directions only work out what would change; changing it is a
//! [`Change`], applied later. Nothing here touches the disk, which is what lets
//! the same code answer "what would this do?" and "do it" without the first
//! answer being a description of the second rather than the thing itself.
//!
//! Working it out is also where a file gets refused. A document that cannot be
//! rewritten safely stops the plan before anything has been written, so a run
//! that fails against one agent's config file has not half-installed itself into
//! another's.

use std::path::Path;

use serde_json::{Map, Value};

use crate::change::Change;
use crate::state::{Ownership, State};
use crate::{Error, file, json, sentinel};

/// One entry this program wants present in a document, and where it belongs.
///
/// The path names the object keys leading to an array — `hooks`, then the name
/// of a hook event, say — and the entry is appended to it. Which array, and what
/// goes in it, is each agent's own business; this is the shape they all share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    path: Vec<String>,
    entry: Map<String, Value>,
}

impl Placement {
    /// An entry to be appended to the array at `path`.
    pub fn new(
        path: impl IntoIterator<Item = impl Into<String>>,
        entry: Map<String, Value>,
    ) -> Self {
        Self {
            path: path.into_iter().map(Into::into).collect(),
            entry,
        }
    }
}

/// A key a document carries about itself, rather than about anybody's entries.
///
/// Some agents' hook files open by saying which dialect they are written in.
/// That is not an entry and is never marked as one: a file written here from
/// nothing carries it because the agent that reads the file expects it, and a
/// file that already says something under that key keeps whatever it says —
/// somebody who wrote a different value there meant it.
///
/// It is named on the way out as well as on the way in, because a file holding
/// nothing but what it says about itself is a file holding nothing of the
/// user's. One this program created is then its own to take away, rather than
/// something to leave standing with a single line in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    key: String,
    value: Value,
}

impl Declaration {
    /// A key to be given `value`, wherever it is not already there.
    pub fn new(key: impl Into<String>, value: impl Into<Value>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
}

/// How a place in a document is named in a message about it.
fn address(path: &[String]) -> String {
    match path.is_empty() {
        true => "the top level".to_owned(),
        false => path.join("."),
    }
}

/// Works out what putting `placements` into the document at `path` would do.
///
/// A file that is not there is written from nothing, which is the case worth
/// having: no merge, no user content, nothing that can go wrong. A file that is
/// there has this program's previous entries taken out and the new ones put in,
/// so that an upgrade replaces what an older build wrote instead of adding to
/// it.
pub fn plan_install(path: &Path, placements: &[Placement]) -> Result<Change, Error> {
    plan_install_declaring(path, placements, &[])
}

/// The same, into a document that has to say something about itself before its
/// entries are read at all.
///
/// Each declaration is given to the document only where the document does not
/// already have that key, and before the entries go in, so that a file written
/// from nothing opens the way the agent's own documentation writes it.
pub fn plan_install_declaring(
    path: &Path,
    placements: &[Placement],
    declarations: &[Declaration],
) -> Result<Change, Error> {
    let existing = read(path)?;
    let mut document = match &existing {
        Some(text) => json::parse(text).map_err(|problem| Error::NotRewritable {
            path: path.to_owned(),
            problem,
        })?,
        // A document made from nothing is an object, unless an entry is to go
        // at the very top of it, in which case the document is the array.
        None => match placements.iter().any(|placement| placement.path.is_empty()) {
            true => Value::Array(Vec::new()),
            false => Value::Object(Map::new()),
        },
    };
    sentinel::remove_marked(&mut document);
    for declaration in declarations {
        declare(&mut document, declaration, path)?;
    }
    for placement in placements {
        place(&mut document, placement, path)?;
    }

    let indent = existing
        .as_deref()
        .map_or(json::DEFAULT_INDENT, json::indentation);
    let contents = json::render(&document, indent);
    Ok(match existing {
        Some(text) if text == contents => Change::Keep {
            path: path.to_owned(),
        },
        Some(_) => Change::Rewrite {
            path: path.to_owned(),
            contents,
            executable: false,
        },
        None => Change::Create {
            path: path.to_owned(),
            contents,
            executable: false,
        },
    })
}

/// Works out what taking this program's entries out of the document at `path`
/// would do.
///
/// A file with nothing of ours in it is left alone, including a file that is not
/// there at all. A file emptied by the removal goes away only if this program
/// created it; one it merely added to is written back, empty containers and all
/// removed, and kept.
pub fn plan_uninstall(path: &Path, state: &State) -> Result<Change, Error> {
    plan_uninstall_declaring(path, state, &[])
}

/// The same, out of a document that says something about itself.
///
/// What the document says about itself is left exactly as it is. It only
/// changes the answer to one question: whether what is left once this program's
/// entries are gone is a file with anything in it, and a file whose every
/// remaining key is one of these has nothing in it that anybody asked for.
pub fn plan_uninstall_declaring(
    path: &Path,
    state: &State,
    declarations: &[Declaration],
) -> Result<Change, Error> {
    let Some(text) = read(path)? else {
        return Ok(Change::Keep {
            path: path.to_owned(),
        });
    };
    let mut document = json::parse(&text).map_err(|problem| Error::NotRewritable {
        path: path.to_owned(),
        problem,
    })?;
    if sentinel::remove_marked_and_prune(&mut document) == 0 {
        return Ok(Change::Keep {
            path: path.to_owned(),
        });
    }
    let ours = state.ownership(path) == Some(Ownership::Created);
    if ours && vacant_but_for(&document, declarations) {
        return Ok(Change::Delete {
            path: path.to_owned(),
        });
    }
    Ok(Change::Rewrite {
        path: path.to_owned(),
        contents: json::render(&document, json::indentation(&text)),
        executable: false,
    })
}

/// Appends one marked entry to the array a placement names, making whatever is
/// missing on the way to it.
///
/// A document that holds something else where an object or an array has to go is
/// refused rather than overwritten. It is a config file this program does not
/// understand, and replacing the part it does not understand is how an installer
/// destroys somebody's afternoon.
fn place(document: &mut Value, placement: &Placement, path: &Path) -> Result<(), Error> {
    let conflict = |depth: usize, needed: &'static str| Error::Conflict {
        path: path.to_owned(),
        at: address(&placement.path[..depth]),
        needed,
    };

    let mut at = document;
    for (depth, key) in placement.path.iter().enumerate() {
        let entries = at
            .as_object_mut()
            .ok_or_else(|| conflict(depth, "an object"))?;
        // Everything on the way to the entry is an object; the thing it is
        // appended to is an array.
        let leaf = depth + 1 == placement.path.len();
        at = entries.entry(key.clone()).or_insert_with(|| match leaf {
            true => Value::Array(Vec::new()),
            false => Value::Object(Map::new()),
        });
    }
    let items = at
        .as_array_mut()
        .ok_or_else(|| conflict(placement.path.len(), "an array"))?;

    let mut entry = placement.entry.clone();
    sentinel::mark(&mut entry);
    items.push(Value::Object(entry));
    Ok(())
}

/// Gives a document a key it says about itself, unless it says something there
/// already.
///
/// A document that is not an object at all is refused rather than replaced, for
/// the same reason a misplaced entry is: whatever is there is a file this
/// program does not understand, and the part it does not understand is the part
/// it has no business rewriting.
fn declare(document: &mut Value, declaration: &Declaration, path: &Path) -> Result<(), Error> {
    document
        .as_object_mut()
        .ok_or_else(|| Error::Conflict {
            path: path.to_owned(),
            at: address(&[]),
            needed: "an object",
        })?
        .entry(declaration.key.clone())
        .or_insert_with(|| declaration.value.clone());
    Ok(())
}

/// Whether a document holds nothing but what it says about itself.
///
/// Only the keys at the top are treated that way, because that is where a
/// document says such things. Everything else is asked the ordinary question.
fn vacant_but_for(document: &Value, declarations: &[Declaration]) -> bool {
    let Value::Object(entries) = document else {
        return sentinel::is_vacant(document);
    };
    entries.iter().all(|(key, value)| {
        declarations.iter().any(|declared| &declared.key == key) || sentinel::is_vacant(value)
    })
}

/// Reads a file that may not be there, naming it if that fails.
fn read(path: &Path) -> Result<Option<String>, Error> {
    file::read(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })
}
