//! Timestamps on the wire.
//!
//! Every timestamp in this protocol is RFC 3339, UTC, with exactly millisecond
//! precision: `2026-08-17T10:32:01.412Z`. Pinning one shape — rather than
//! accepting the whole of RFC 3339 — is what lets this crate hold timestamps as
//! validated strings and stay free of a datetime dependency, which in turn keeps
//! it free of a clock. Producers that need to *make* a timestamp own their own
//! calendar arithmetic and hand the result to [`Timestamp::parse`], or build one
//! component-wise with [`Timestamp::from_parts`].

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An RFC 3339 UTC timestamp with millisecond precision, e.g.
/// `2026-08-17T10:32:01.412Z`.
///
/// The shape is fixed and fixed-width, so the derived ordering on the inner
/// string is also the chronological ordering.
///
/// Only the *shape* is checked, plus the range of each component. This type does
/// no calendar arithmetic and will accept the 31st of February; that is the price
/// of not depending on a datetime library, and it costs nothing here because the
/// values are produced by machines and only ever compared and displayed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(String);

/// The number of bytes in a well-formed timestamp: `YYYY-MM-DDTHH:MM:SS.mmmZ`.
const LEN: usize = 24;

/// Byte offsets that must hold an ASCII digit.
const DIGITS: [usize; 17] = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18, 20, 21, 22];

/// Byte offsets that must hold a specific ASCII separator.
const SEPARATORS: [(usize, u8); 7] = [
    (4, b'-'),
    (7, b'-'),
    (10, b'T'),
    (13, b':'),
    (16, b':'),
    (19, b'.'),
    (23, b'Z'),
];

impl Timestamp {
    /// Validates `text` and takes ownership of it.
    pub fn parse(text: impl Into<String>) -> Result<Self, TimestampError> {
        let text = text.into();
        validate(&text)?;
        Ok(Self(text))
    }

    /// Formats the components as a timestamp, checking each one's range.
    ///
    /// `second` may be 60 so that a producer relaying a leap second does not have
    /// to decide what to do about it. This is the formatter that guarantees the
    /// shape: whatever it returns parses.
    pub fn from_parts(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        millisecond: u16,
    ) -> Result<Self, TimestampError> {
        let range = |component, value: u32, max| {
            if value > max {
                Err(TimestampError::OutOfRange { component, value })
            } else {
                Ok(())
            }
        };
        range("year", u32::from(year), 9999)?;
        range("month", u32::from(month), 12)?;
        range("day", u32::from(day), 31)?;
        range("hour", u32::from(hour), 23)?;
        range("minute", u32::from(minute), 59)?;
        range("second", u32::from(second), 60)?;
        range("millisecond", u32::from(millisecond), 999)?;
        if month == 0 {
            return Err(TimestampError::OutOfRange {
                component: "month",
                value: 0,
            });
        }
        if day == 0 {
            return Err(TimestampError::OutOfRange {
                component: "day",
                value: 0,
            });
        }
        Ok(Self(format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millisecond:03}Z"
        )))
    }

    /// The timestamp as it appears on the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the timestamp, returning the underlying string.
    pub fn into_string(self) -> String {
        self.0
    }
}

fn validate(text: &str) -> Result<(), TimestampError> {
    let bytes = text.as_bytes();
    if bytes.len() != LEN {
        return Err(TimestampError::Malformed(text.to_owned()));
    }
    for offset in DIGITS {
        if !bytes[offset].is_ascii_digit() {
            return Err(TimestampError::Malformed(text.to_owned()));
        }
    }
    for (offset, expected) in SEPARATORS {
        if bytes[offset] != expected {
            return Err(TimestampError::Malformed(text.to_owned()));
        }
    }
    Ok(())
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Timestamp {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for Timestamp {
    type Err = TimestampError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl TryFrom<String> for Timestamp {
    type Error = TimestampError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(text)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Deserialization validates, so a `Timestamp` that exists is well-formed
/// wherever it came from. This is deliberately *not* one of the forward
/// compatibility escape hatches: the shape is part of version 1 of the protocol,
/// and a line that breaks it is a producer bug rather than a future extension.
impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(text).map_err(serde::de::Error::custom)
    }
}

/// Why a string is not a well-formed [`Timestamp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimestampError {
    /// The string does not have the shape `YYYY-MM-DDTHH:MM:SS.mmmZ`.
    Malformed(String),
    /// A component was outside the range the calendar allows for it.
    OutOfRange {
        /// The name of the component, for the message.
        component: &'static str,
        /// The value that was rejected.
        value: u32,
    },
}

impl fmt::Display for TimestampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(text) => write!(
                f,
                "timestamp {text:?} is not of the form YYYY-MM-DDTHH:MM:SS.mmmZ"
            ),
            Self::OutOfRange { component, value } => {
                write!(f, "timestamp {component} {value} is out of range")
            }
        }
    }
}

impl Error for TimestampError {}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "2026-08-17T10:32:01.412Z";

    #[test]
    fn accepts_the_documented_shape() {
        assert_eq!(Timestamp::parse(GOOD).unwrap().as_str(), GOOD);
    }

    #[test]
    fn rejects_anything_else() {
        for bad in [
            "",
            "2026-08-17T10:32:01Z",          // no milliseconds
            "2026-08-17T10:32:01.412345Z",   // microseconds
            "2026-08-17T10:32:01.412+00:00", // offset rather than Z
            "2026-08-17T10:32:01.412z",      // lower-case zone
            "2026-08-17 10:32:01.412Z",      // space rather than T
            "2026/08/17T10:32:01.412Z",      // wrong date separator
            "202-08-17T10:32:01.4127Z",      // right length, wrong shape
            "2026-08-17T10:32:01.412Z ",     // trailing space
        ] {
            assert!(
                Timestamp::parse(bad).is_err(),
                "{bad:?} should have been rejected"
            );
        }
    }

    #[test]
    fn formats_from_parts() {
        let ts = Timestamp::from_parts(2026, 8, 17, 10, 32, 1, 412).unwrap();
        assert_eq!(ts.as_str(), GOOD);
        assert_eq!(
            Timestamp::from_parts(7, 1, 2, 3, 4, 5, 6).unwrap().as_str(),
            "0007-01-02T03:04:05.006Z"
        );
        assert!(Timestamp::from_parts(2026, 13, 17, 10, 32, 1, 412).is_err());
        assert!(Timestamp::from_parts(2026, 0, 17, 10, 32, 1, 412).is_err());
        assert!(Timestamp::from_parts(2026, 8, 0, 10, 32, 1, 412).is_err());
        assert!(Timestamp::from_parts(2026, 8, 17, 24, 32, 1, 412).is_err());
        assert!(Timestamp::from_parts(2026, 8, 17, 10, 60, 1, 412).is_err());
        assert!(Timestamp::from_parts(2026, 8, 17, 10, 32, 61, 412).is_err());
        assert!(Timestamp::from_parts(2026, 8, 17, 10, 32, 1, 1000).is_err());
        // A leap second is representable rather than rejected.
        assert!(Timestamp::from_parts(2026, 12, 31, 23, 59, 60, 0).is_ok());
    }

    #[test]
    fn orders_chronologically() {
        let earlier = Timestamp::parse("2026-08-17T10:32:01.412Z").unwrap();
        let later = Timestamp::parse("2026-08-17T10:32:01.413Z").unwrap();
        assert!(earlier < later);
    }

    #[test]
    fn round_trips_through_serde() {
        let json = format!("\"{GOOD}\"");
        let ts: Timestamp = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&ts).unwrap(), json);
    }

    #[test]
    fn deserialization_rejects_a_bad_shape() {
        assert!(serde_json::from_str::<Timestamp>("\"2026-08-17T10:32:01Z\"").is_err());
    }
}
