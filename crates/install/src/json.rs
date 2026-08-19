//! Reading and writing JSON files that belong to somebody else.
//!
//! These files are hand-edited by the people whose machines they are on, and
//! this program only ever wants to add a few entries to one. That makes the
//! important operation not *write* but *rewrite*, and the rule for a rewrite is
//! that anything not ours comes back out exactly as it went in.
//!
//! Two things stand in the way of that, and both are refused rather than worked
//! around. A document that is not JSON at all cannot be edited without guessing
//! at what the user meant. And a document whose objects repeat a key parses
//! perfectly well — the last value wins — but writing it back out would silently
//! drop the keys that lost, which is a config file quietly changing meaning. A
//! rewrite that would do either bails, and the file is left as it was.

use std::fmt;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

/// The indentation a document is written with when there is nothing to copy.
///
/// Two spaces, because that is what every one of these files is shipped and
/// documented with.
pub const DEFAULT_INDENT: &str = "  ";

/// Why a document cannot be rewritten safely.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    /// It is not JSON this program is willing to read.
    #[error("{0}")]
    Unreadable(String),
    /// It reads, but writing it back out would not produce the same document.
    #[error("writing it back out would not give the same document")]
    Lossy,
}

/// Reads a document, refusing anything that would not survive being written
/// back out.
///
/// The round trip is checked rather than assumed: the value is serialized and
/// read again, and the two must be equal. Formatting is free to differ —
/// whitespace and the spelling of a number carry no meaning — but nothing else
/// may.
pub fn parse(text: &str) -> Result<Value, Problem> {
    let Strict(value) =
        serde_json::from_str(text).map_err(|error| Problem::Unreadable(error.to_string()))?;
    let written = serde_json::to_string(&value).map_err(|_| Problem::Lossy)?;
    let again: Value = serde_json::from_str(&written).map_err(|_| Problem::Lossy)?;
    match again == value {
        true => Ok(value),
        false => Err(Problem::Lossy),
    }
}

/// A string as it is written between the quotation marks of a string literal.
///
/// A JSON one, and a JavaScript one: JavaScript's escapes are a superset of
/// JSON's, so what is right here is right there too, and the one function serves
/// both of the file formats this program generates. Nothing that goes through it
/// can break out of the literal it is being put into, which is the point — the
/// strings being escaped are paths from the machine this is running on.
pub fn escaped(text: &str) -> String {
    let quoted = Value::from(text).to_string();
    quoted[1..quoted.len() - 1].to_owned()
}

/// Writes a document out, indented with `indent` and ending in a newline.
///
/// The newline is unconditional: these files are read by people in editors and
/// diffed by version control, both of which treat a missing one as damage.
pub fn render(value: &Value, indent: &str) -> String {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    value
        .serialize(&mut serializer)
        .expect("writing a JSON value into memory cannot fail");
    let mut text = String::from_utf8(out).expect("serde_json writes UTF-8");
    text.push('\n');
    text
}

/// Guesses what a document is indented with, so that rewriting it does not
/// reformat the whole file.
///
/// The guess is taken from the first line that is indented at all, which in a
/// pretty-printed document is one level deep. It only affects how the result
/// looks, so being wrong about an unusual file costs a diff and nothing more.
pub fn indentation(text: &str) -> &'static str {
    for line in text.lines() {
        let content = line.trim_start_matches([' ', '\t']);
        if content.is_empty() {
            continue;
        }
        return match &line[..line.len() - content.len()] {
            "" => continue,
            lead if lead.starts_with('\t') => "\t",
            lead if lead.len() >= 4 => "    ",
            _ => DEFAULT_INDENT,
        };
    }
    DEFAULT_INDENT
}

/// A JSON value read with repeated object keys refused.
///
/// `serde_json`'s own reader keeps the last of a repeated key, because that is
/// what every other JSON reader does and what the format's users expect. This
/// one exists because keeping the last is only safe when nobody writes the
/// document back out.
struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(Any)
    }
}

/// Reads whatever the document holds at this position.
struct Any;

impl<'de> Visitor<'de> for Any {
    type Value = Strict;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Strict, E> {
        Ok(Strict(value.into()))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Strict, E> {
        Ok(Strict(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Strict, E> {
        Ok(Strict(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Strict, E> {
        Ok(Strict(
            Number::from_f64(value).map_or(Value::Null, Value::Number),
        ))
    }

    fn visit_str<E>(self, value: &str) -> Result<Strict, E> {
        Ok(Strict(value.into()))
    }

    fn visit_string<E>(self, value: String) -> Result<Strict, E> {
        Ok(Strict(value.into()))
    }

    fn visit_unit<E>(self) -> Result<Strict, E> {
        Ok(Strict(Value::Null))
    }

    fn visit_none<E>(self) -> Result<Strict, E> {
        Ok(Strict(Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Strict, D::Error>
    where
        D: Deserializer<'de>,
    {
        Deserialize::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Strict, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut items = Vec::new();
        while let Some(Strict(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Strict(Value::Array(items)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Strict, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let Strict(value) = map.next_value()?;
            if entries.contains_key(&key) {
                return Err(de::Error::custom(format!(
                    "the key \"{key}\" appears twice in one object, and only one of them would survive being written back out"
                )));
            }
            entries.insert(key, value);
        }
        Ok(Strict(Value::Object(entries)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_document_reads() {
        let value = parse(r#"{"hooks": {"Stop": [1, 2]}, "n": 1.5}"#).unwrap();
        assert_eq!(value["hooks"]["Stop"][1], Value::from(2));
        assert_eq!(value["n"], Value::from(1.5));
    }

    #[test]
    fn something_that_is_not_json_is_refused_by_name() {
        let problem = parse("{not json at all}").unwrap_err();
        assert!(matches!(problem, Problem::Unreadable(_)), "{problem:?}");
    }

    #[test]
    fn a_repeated_key_is_refused_and_the_key_is_named() {
        let problem = parse(r#"{"hooks": 1, "hooks": 2}"#).unwrap_err();
        let Problem::Unreadable(reason) = &problem else {
            panic!("{problem:?}");
        };
        assert!(reason.contains("hooks"), "{reason:?}");
    }

    #[test]
    fn a_repeated_key_deep_in_the_document_is_refused_too() {
        assert!(parse(r#"{"a": [{"b": 1, "b": 2}]}"#).is_err());
        assert!(parse(r#"{"a": {"b": {"c": 1, "c": 1}}}"#).is_err());
    }

    #[test]
    fn keys_keep_the_order_they_were_written_in() {
        let text = r#"{"z": 1, "a": 2, "m": 3}"#;
        assert_eq!(
            render(&parse(text).unwrap(), DEFAULT_INDENT),
            "{\n  \"z\": 1,\n  \"a\": 2,\n  \"m\": 3\n}\n"
        );
    }

    #[test]
    fn what_is_written_always_ends_in_one_newline() {
        let rendered = render(&parse(r#"{"a": 1}"#).unwrap(), DEFAULT_INDENT);
        assert!(rendered.ends_with("}\n"), "{rendered:?}");
    }

    #[test]
    fn indentation_is_copied_from_the_document() {
        assert_eq!(indentation("{\n  \"a\": 1\n}\n"), "  ");
        assert_eq!(indentation("{\n    \"a\": 1\n}\n"), "    ");
        assert_eq!(indentation("{\n\t\"a\": 1\n}\n"), "\t");
    }

    #[test]
    fn a_document_with_nothing_to_copy_gets_the_default() {
        assert_eq!(indentation(r#"{"a": 1}"#), DEFAULT_INDENT);
        assert_eq!(indentation(""), DEFAULT_INDENT);
        assert_eq!(indentation("{\n\n  \n}"), DEFAULT_INDENT);
    }
}
