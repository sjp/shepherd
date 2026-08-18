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
/// Only the *shape* is checked, plus the range of each component. Nothing
/// validates the calendar, so the 31st of February parses; that is the price of
/// not depending on a datetime library, and it costs nothing here because the
/// values are produced by machines and are only ever compared, differenced and
/// displayed.
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

    /// How many milliseconds passed between `earlier` and `self`. Negative when
    /// `self` is the earlier of the two.
    ///
    /// This is the one piece of arithmetic the type does, and it exists because
    /// deciding that something has gone quiet for too long is a comparison of
    /// two timestamps and nothing more — asking a caller to convert first would
    /// only push the same integer arithmetic somewhere less well tested.
    ///
    /// The calendar is proleptic Gregorian, extended without limit in both
    /// directions, which is what makes this total: a value that parsed but names
    /// no real instant, such as the 31st of February or a leap second, still
    /// yields an answer, and the answer is consistent with every other value.
    /// Two timestamps a leap second apart are therefore one second apart here,
    /// which no consumer of this protocol can tell from an ordinary second.
    pub fn millis_since(&self, earlier: &Self) -> i64 {
        self.epoch_millis() - earlier.epoch_millis()
    }

    /// Milliseconds since 1970-01-01T00:00:00.000Z, on the calendar described by
    /// [`Timestamp::millis_since`]. Private because a difference is the only
    /// meaning this crate needs and the only one that survives a nonsense date.
    fn epoch_millis(&self) -> i64 {
        let digits = self.0.as_bytes();
        let number = |start: usize, len: usize| -> i64 {
            digits[start..start + len]
                .iter()
                .fold(0, |value, digit| value * 10 + i64::from(digit - b'0'))
        };
        let days = days_from_civil(number(0, 4), number(5, 2), number(8, 2));
        let seconds = days * 86_400 + number(11, 2) * 3_600 + number(14, 2) * 60 + number(17, 2);
        seconds * 1_000 + number(20, 3)
    }
}

/// Days between 1970-01-01 and `year-month-day` on the proleptic Gregorian
/// calendar, by Howard Hinnant's `days_from_civil`, whose derivation is at
/// <https://howardhinnant.github.io/date_algorithms.html>. It is shifted so that
/// the era begins on the 1st of March, which is what removes February's special
/// case and leaves an expression with no tables and no branches.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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
    fn differences_are_signed_milliseconds() {
        let parse = |text: &str| Timestamp::parse(text).unwrap();
        let earlier = parse("2026-08-17T10:32:01.412Z");
        let later = parse("2026-08-17T10:34:01.412Z");
        assert_eq!(later.millis_since(&earlier), 120_000);
        assert_eq!(earlier.millis_since(&later), -120_000);
        assert_eq!(earlier.millis_since(&earlier), 0);

        // Across a millisecond, a second, a minute, an hour, a day, a month, a
        // leap day, and a year, in that order.
        for (from, to, millis) in [
            ("2026-08-17T10:32:01.412Z", "2026-08-17T10:32:01.413Z", 1),
            (
                "2026-08-17T10:32:01.412Z",
                "2026-08-17T10:32:02.412Z",
                1_000,
            ),
            (
                "2026-08-17T10:32:01.412Z",
                "2026-08-17T10:33:01.412Z",
                60_000,
            ),
            (
                "2026-08-17T10:32:01.412Z",
                "2026-08-17T11:32:01.412Z",
                3_600_000,
            ),
            (
                "2026-08-17T10:32:01.412Z",
                "2026-08-18T10:32:01.412Z",
                86_400_000,
            ),
            (
                "2026-08-31T00:00:00.000Z",
                "2026-09-01T00:00:00.000Z",
                86_400_000,
            ),
            (
                "2028-02-28T00:00:00.000Z",
                "2028-03-01T00:00:00.000Z",
                2 * 86_400_000,
            ),
            ("2026-12-31T23:59:59.999Z", "2027-01-01T00:00:00.000Z", 1),
        ] {
            assert_eq!(
                parse(to).millis_since(&parse(from)),
                millis,
                "{from} to {to}"
            );
        }

        // 2100 is not a leap year, 2000 was: the century rules are exercised.
        assert_eq!(
            parse("2100-03-01T00:00:00.000Z").millis_since(&parse("2100-02-28T00:00:00.000Z")),
            86_400_000
        );
        assert_eq!(
            parse("2000-03-01T00:00:00.000Z").millis_since(&parse("2000-02-28T00:00:00.000Z")),
            2 * 86_400_000
        );
    }

    #[test]
    fn the_epoch_is_where_it_should_be() {
        let epoch = Timestamp::parse("1970-01-01T00:00:00.000Z").unwrap();
        assert_eq!(
            Timestamp::parse("2026-08-17T10:32:01.412Z")
                .unwrap()
                .millis_since(&epoch),
            1_786_962_721_412
        );
    }

    #[test]
    fn a_difference_of_impossible_dates_is_an_answer_rather_than_a_panic() {
        // Both of these parse, because only the shape is checked. Neither names
        // a real instant, and the only promise is that asking does not panic and
        // that ordering is respected.
        let earlier = Timestamp::parse("2026-02-30T25:61:61.999Z").unwrap();
        let later = Timestamp::parse("2026-02-31T25:61:61.999Z").unwrap();
        assert_eq!(later.millis_since(&earlier), 86_400_000);
        assert!(
            Timestamp::parse("9999-99-99T99:99:99.999Z")
                .unwrap()
                .millis_since(&Timestamp::parse("0000-00-00T00:00:00.000Z").unwrap())
                > 0
        );
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
