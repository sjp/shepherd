//! The mark this program puts on everything it writes into somebody else's
//! file.
//!
//! An installer that edits a hand-maintained config file has to be able to find
//! its own work again later, without asking the user to remember what it did and
//! without matching on the content — content changes between versions, and a
//! user is free to edit what they find. So every entry written carries a key
//! that says who wrote it, and upgrading and uninstalling are both "take out
//! everything with this key, leave everything else alone".
//!
//! The version inside the mark is there so that a later build can tell what an
//! earlier one wrote. Ownership is decided by the key alone, never by the
//! version: a build that only removed the versions it knew about would leave
//! the entries of a newer one behind for ever.
//!
//! Not every agent is configured by a document, though. One that reads a script
//! gets a whole file to itself rather than a share of somebody else's, and there
//! is nowhere in it to hang a key — so the same mark goes in a comment on its
//! first line and answers the same question. See [`is_generated`].

use serde_json::{Map, Value};

/// The key that says an entry belongs to this program.
///
/// Underscore-first by the usual convention for a key that is metadata about
/// the document rather than part of it, and named after the program so that a
/// user reading their own config file can tell at a glance what put it there.
pub const KEY: &str = "_agentbus";

/// The key inside the mark that carries its version.
const VERSION_KEY: &str = "v";

/// The shape of the entries this build writes.
pub const VERSION: u64 = 1;

/// Marks an entry as this program's own, in place.
pub fn mark(entry: &mut Map<String, Value>) {
    let mut version = Map::new();
    version.insert(VERSION_KEY.to_owned(), Value::from(VERSION));
    entry.insert(KEY.to_owned(), Value::Object(version));
}

/// Whether `value` is an entry this program wrote.
pub fn is_marked(value: &Value) -> bool {
    value.get(KEY).is_some_and(Value::is_object)
}

/// Whether `text` is a file this program generated whole.
///
/// The same question [`is_marked`] answers, asked where there are no entries to
/// ask it of. An agent whose configuration is a script rather than a document
/// gets a file of its own rather than a share of one, and that file is either
/// this program's to replace and to remove or somebody else's to leave exactly
/// where it is. Nothing but a mark can tell those apart: the name is the same
/// either way, and so is everything a user could have copied out of a
/// documentation page.
///
/// Only the first line is looked at, and it is looked at for the key alone —
/// whatever comment syntax the file is written in goes around it. A mark further
/// down would be one somebody could acquire by accident, and the question here
/// decides whether their file is about to be overwritten.
pub fn is_generated(text: &str) -> bool {
    text.lines().next().is_some_and(|line| line.contains(KEY))
}

/// Takes every entry this program wrote out of `document`, leaving the
/// containers that held them in place, and says how many it found.
///
/// Used when reinstalling, where the containers are about to be written into
/// again. Leaving them alone is what makes a second install a no-op: an entry
/// removed from an array and appended back to it lands where it already was,
/// while a key removed from an object and put back would move to the end and
/// change a file that did not need changing.
pub fn remove_marked(document: &mut Value) -> usize {
    remove(document, false).removed
}

/// Takes every entry this program wrote out of `document`, along with every
/// container the removal emptied, and says how many entries it found.
///
/// Used when uninstalling, where an empty array under a hook name this program
/// introduced is exactly the trace it promised not to leave. A container that
/// was already empty is left alone: this only removes what its own removal
/// emptied.
pub fn remove_marked_and_prune(document: &mut Value) -> usize {
    remove(document, true).removed
}

/// What one pass of the removal did to one value.
struct Removal {
    /// How many marked entries it took out, at any depth.
    removed: usize,
    /// Whether it left a container that had held something empty.
    emptied: bool,
}

/// Removes this program's entries from `value`, optionally taking the
/// containers it empties with them.
fn remove(value: &mut Value, prune: bool) -> Removal {
    let mut removed = 0;
    let emptied = match value {
        Value::Object(entries) => {
            let keys: Vec<String> = entries.keys().cloned().collect();
            for key in keys {
                let Some(entry) = entries.get_mut(&key) else {
                    continue;
                };
                let inner = remove(entry, prune);
                removed += inner.removed;
                if prune && inner.emptied {
                    entries.remove(&key);
                }
            }
            entries.is_empty()
        }
        Value::Array(items) => {
            let mut kept = Vec::with_capacity(items.len());
            for mut item in items.drain(..) {
                if is_marked(&item) {
                    removed += 1;
                    continue;
                }
                let inner = remove(&mut item, prune);
                removed += inner.removed;
                if prune && inner.emptied {
                    continue;
                }
                kept.push(item);
            }
            *items = kept;
            items.is_empty()
        }
        _ => false,
    };
    Removal {
        removed,
        emptied: removed > 0 && emptied,
    }
}

/// Whether a document holds nothing at all.
///
/// Answered through the containers rather than at the top level, because a
/// document this program created and has just emptied reads as `{}` only once
/// the containers it made along the way are counted as empty too.
pub fn is_vacant(document: &Value) -> bool {
    match document {
        Value::Null => true,
        Value::Object(entries) => entries.values().all(is_vacant),
        Value::Array(items) => items.iter().all(is_vacant),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn ours() -> Value {
        let mut entry = Map::new();
        entry.insert("command".to_owned(), Value::from("agentbus emit"));
        mark(&mut entry);
        Value::Object(entry)
    }

    #[test]
    fn a_marked_entry_is_recognized_and_an_unmarked_one_is_not() {
        assert!(is_marked(&ours()));
        assert!(!is_marked(&document(r#"{"command": "something-else"}"#)));
        assert!(!is_marked(&Value::from("a string cannot be marked")));
    }

    #[test]
    fn a_generated_file_is_recognized_by_its_first_line_and_only_by_that() {
        assert!(is_generated("// _agentbus generated\nconst x = 1;\n"));
        assert!(is_generated(
            "# _agentbus, in whatever a comment looks like here"
        ));
        assert!(!is_generated("const x = 1;\n// _agentbus\n"));
        assert!(!is_generated("// something a user wrote\n"));
        assert!(!is_generated(""));
    }

    #[test]
    fn the_mark_says_which_build_wrote_it() {
        assert_eq!(ours()[KEY][VERSION_KEY], Value::from(VERSION));
    }

    #[test]
    fn a_mark_of_another_version_is_still_ours() {
        let entry = document(r#"{"_agentbus": {"v": 99}, "command": "x"}"#);
        assert!(is_marked(&entry));
    }

    #[test]
    fn only_marked_entries_are_removed() {
        let mut doc = document(r#"{"hooks": {"Stop": [{"command": "theirs"}]}}"#);
        doc["hooks"]["Stop"].as_array_mut().unwrap().push(ours());

        assert_eq!(remove_marked(&mut doc), 1);
        assert_eq!(
            doc,
            document(r#"{"hooks": {"Stop": [{"command": "theirs"}]}}"#)
        );
    }

    #[test]
    fn removing_without_pruning_leaves_the_containers_alone() {
        let mut doc = document(r#"{"hooks": {"Stop": []}}"#);
        doc["hooks"]["Stop"].as_array_mut().unwrap().push(ours());

        assert_eq!(remove_marked(&mut doc), 1);
        assert_eq!(doc, document(r#"{"hooks": {"Stop": []}}"#));
    }

    #[test]
    fn pruning_takes_away_the_containers_the_removal_emptied() {
        let mut doc = document(r#"{"hooks": {"Stop": []}, "theirs": 1}"#);
        doc["hooks"]["Stop"].as_array_mut().unwrap().push(ours());

        assert_eq!(remove_marked_and_prune(&mut doc), 1);
        assert_eq!(doc, document(r#"{"theirs": 1}"#));
    }

    #[test]
    fn pruning_keeps_a_container_that_was_already_empty() {
        let mut doc = document(r#"{"hooks": {"Stop": []}}"#);

        assert_eq!(remove_marked_and_prune(&mut doc), 0);
        assert_eq!(doc, document(r#"{"hooks": {"Stop": []}}"#));
    }

    #[test]
    fn pruning_keeps_a_container_that_still_holds_something_of_theirs() {
        let mut doc = document(r#"{"hooks": {"Stop": [{"command": "theirs"}]}}"#);
        doc["hooks"]["Stop"].as_array_mut().unwrap().push(ours());

        assert_eq!(remove_marked_and_prune(&mut doc), 1);
        assert_eq!(
            doc,
            document(r#"{"hooks": {"Stop": [{"command": "theirs"}]}}"#)
        );
    }

    #[test]
    fn a_document_of_empty_containers_holds_nothing() {
        assert!(is_vacant(&document("{}")));
        assert!(is_vacant(&document(r#"{"hooks": {}}"#)));
        assert!(is_vacant(&document(r#"{"hooks": {"Stop": []}}"#)));
        assert!(!is_vacant(&document(r#"{"hooks": {"Stop": [1]}}"#)));
        assert!(!is_vacant(&document(r#"{"theirs": false}"#)));
    }
}
