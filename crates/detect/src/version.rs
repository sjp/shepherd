//! The version number both manifest families carry.

use std::cmp::Ordering;
use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A dotted-numeric manifest version, such as `2026.06.11.1`.
///
/// The grammar is deliberately smaller than semantic versioning: non-empty
/// segments of ASCII digits, separated by `.`, each fitting in a [`u64`]. There
/// is no pre-release or build metadata, because a manifest has no API to
/// promise — the only question ever asked of two versions is which is newer, and
/// the corpus's `YYYY.MM.DD.N` convention answers it by construction. That
/// convention is not enforced here: a manifest maintained as `1`, `2`, `3` is
/// just as orderable.
///
/// Comparison is segment-wise, with missing trailing segments read as zero, so
/// `1.2` and `1.2.0` are the same version and `1.2.1` is newer than both.
/// Equality follows that ordering rather than the text, which is why the
/// original spelling is kept only for display: two equal versions can still be
/// written differently, and printing back what a manifest actually said is more
/// useful than printing a normalized form nobody wrote.
#[derive(Debug, Clone)]
pub struct ManifestVersion {
    /// The version exactly as it was written.
    text: String,
    /// The same value as numbers, for comparison. Never empty.
    segments: Vec<u64>,
}

impl ManifestVersion {
    /// Parses `value`, rejecting anything outside the grammar above.
    ///
    /// Surrounding whitespace is trimmed; whitespace anywhere else is a
    /// rejection, as is an empty string, an empty segment, a non-digit and a
    /// segment too large for a [`u64`].
    pub fn parse(value: &str) -> Result<Self, InvalidVersion> {
        let text = value.trim();
        if text.is_empty() {
            return Err(InvalidVersion::Empty);
        }
        let mut segments = Vec::new();
        for segment in text.split('.') {
            if segment.is_empty() {
                return Err(InvalidVersion::EmptySegment {
                    version: text.to_owned(),
                });
            }
            if !segment.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(InvalidVersion::NotNumeric {
                    version: text.to_owned(),
                    segment: segment.to_owned(),
                });
            }
            let number = segment
                .parse::<u64>()
                .map_err(|_| InvalidVersion::SegmentTooLarge {
                    version: text.to_owned(),
                    segment: segment.to_owned(),
                })?;
            segments.push(number);
        }
        Ok(Self {
            text: text.to_owned(),
            segments,
        })
    }

    /// The version as it was written.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl fmt::Display for ManifestVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Ord for ManifestVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let length = self.segments.len().max(other.segments.len());
        for index in 0..length {
            let left = self.segments.get(index).copied().unwrap_or(0);
            let right = other.segments.get(index).copied().unwrap_or(0);
            match left.cmp(&right) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for ManifestVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ManifestVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ManifestVersion {}

impl<'de> Deserialize<'de> for ManifestVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(D::Error::custom)
    }
}

impl Serialize for ManifestVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// Why a string is not a [`ManifestVersion`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidVersion {
    /// The value was empty, or only whitespace.
    #[error("version must not be empty")]
    Empty,
    /// Two dots in a row, or a leading or trailing dot.
    #[error("version {version:?} has an empty segment")]
    EmptySegment {
        /// The value that was rejected.
        version: String,
    },
    /// A segment held something other than ASCII digits.
    #[error("version {version:?} has a non-numeric segment {segment:?}")]
    NotNumeric {
        /// The value that was rejected.
        version: String,
        /// The offending segment.
        segment: String,
    },
    /// A segment did not fit in a `u64`.
    #[error("version {version:?} has a segment {segment:?} that is too large")]
    SegmentTooLarge {
        /// The value that was rejected.
        version: String,
        /// The offending segment.
        segment: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: &str) -> ManifestVersion {
        ManifestVersion::parse(value).expect("valid version")
    }

    #[test]
    fn accepts_the_documented_grammar() {
        for good in ["1", "0", "1.2", "2026.06.11.1", "18446744073709551615"] {
            assert!(
                ManifestVersion::parse(good).is_ok(),
                "{good:?} should have been accepted"
            );
        }
    }

    #[test]
    fn keeps_the_spelling_it_was_given() {
        assert_eq!(version("2026.06.11.1").as_str(), "2026.06.11.1");
        assert_eq!(version(" 1.2 ").to_string(), "1.2");
    }

    #[test]
    fn rejects_anything_else() {
        for bad in [
            "",
            "   ",
            "2026.06.alpha",
            "2026..06",
            ".1",
            "1.",
            "1 2",
            "v1.2",
            "1.2.3-rc1",
            "-1",
            "1.-2",
            "18446744073709551616", // one past u64::MAX
        ] {
            assert!(
                ManifestVersion::parse(bad).is_err(),
                "{bad:?} should have been rejected"
            );
        }
    }

    #[test]
    fn names_the_reason_it_rejected() {
        assert_eq!(
            ManifestVersion::parse("").unwrap_err(),
            InvalidVersion::Empty
        );
        assert!(matches!(
            ManifestVersion::parse("2026..06").unwrap_err(),
            InvalidVersion::EmptySegment { .. }
        ));
        assert!(matches!(
            ManifestVersion::parse("2026.06.alpha").unwrap_err(),
            InvalidVersion::NotNumeric { .. }
        ));
        assert!(matches!(
            ManifestVersion::parse("18446744073709551616").unwrap_err(),
            InvalidVersion::SegmentTooLarge { .. }
        ));
    }

    #[test]
    fn orders_segment_wise_padding_with_zero() {
        for (left, right) in [
            ("1.2", "1.2.0"),
            ("1.2.0.0", "1.2"),
            ("1", "1.0.0"),
            ("0", "0.0"),
        ] {
            assert_eq!(version(left), version(right), "{left} should equal {right}");
        }

        for (smaller, larger) in [
            ("1.2", "1.2.1"),
            ("1.2.9", "1.3"),
            ("2026.06.11.1", "2026.06.11.2"),
            ("2026.06.11", "2026.6.12"),
            ("9", "10"),
            ("1.0", "1.0.0.1"),
        ] {
            assert!(
                version(smaller) < version(larger),
                "{smaller} should be older than {larger}"
            );
            assert!(
                version(larger) > version(smaller),
                "{larger} should be newer than {smaller}"
            );
            assert_ne!(version(smaller), version(larger));
        }
    }

    #[test]
    fn ignores_leading_zeros_when_comparing() {
        assert_eq!(version("2026.06.11"), version("2026.6.11"));
        assert!(version("1.02") > version("1.1"));
    }

    #[test]
    fn round_trips_through_toml() {
        #[derive(serde::Deserialize, serde::Serialize)]
        struct Holder {
            version: ManifestVersion,
        }

        let holder: Holder = toml::from_str(r#"version = "2026.06.11.1""#).expect("parses");
        assert_eq!(holder.version.as_str(), "2026.06.11.1");
        assert!(
            toml::to_string(&holder)
                .expect("serializes")
                .contains("2026.06.11.1")
        );
        assert!(toml::from_str::<Holder>(r#"version = "2026.06.alpha""#).is_err());
    }
}
