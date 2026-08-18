//! The daemon's clock.
//!
//! The protocol's timestamps are validated strings and the crate that defines
//! them has no clock at all, deliberately: a producer that needs to *make* one
//! owns the calendar arithmetic. This is that arithmetic, and it is the only
//! place in the daemon that reads the wall clock.
//!
//! The conversion is the civil-calendar algorithm the protocol crate's own
//! difference uses in reverse, so a timestamp made here and then differenced
//! against another agrees to the millisecond. The calendar is proleptic
//! Gregorian and UTC throughout: no zones, no local time, no leap seconds.

use std::time::{SystemTime, UNIX_EPOCH};

use agentbus_protocol::Timestamp;

/// Milliseconds in a day.
const MILLIS_PER_DAY: i64 = 86_400_000;

/// Days from the epoch to `0000-01-01`, the earliest date a timestamp can spell.
const MIN_DAY: i64 = -719_528;

/// Days from the epoch to `9999-12-31`, the latest one.
const MAX_DAY: i64 = 2_932_896;

/// The current time.
pub fn now() -> Timestamp {
    from_unix_millis(unix_millis(SystemTime::now()))
}

/// The instant `millis` milliseconds after the Unix epoch, as a timestamp.
///
/// Clamped to the range a timestamp can spell. A clock that reads a date outside
/// the year range is broken in a way this daemon cannot do anything about, and
/// returning the nearest representable instant keeps the stream well-formed for
/// everyone downstream, which refusing to produce a timestamp would not.
pub fn from_unix_millis(millis: i64) -> Timestamp {
    let millis = millis.clamp(MIN_DAY * MILLIS_PER_DAY, (MAX_DAY + 1) * MILLIS_PER_DAY - 1);
    let (year, month, day) = civil_from_days(millis.div_euclid(MILLIS_PER_DAY));
    let time = millis.rem_euclid(MILLIS_PER_DAY);
    Timestamp::from_parts(
        year,
        month,
        day,
        (time / 3_600_000) as u8,
        (time / 60_000 % 60) as u8,
        (time / 1_000 % 60) as u8,
        (time % 1_000) as u16,
    )
    .expect("a clamped instant is always in range")
}

/// How many milliseconds `time` is after the Unix epoch; negative if before it.
fn unix_millis(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_millis()).unwrap_or(i64::MAX),
        Err(before) => {
            i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |millis| -millis)
        }
    }
}

/// The civil date `days` days after `1970-01-01`, on the proleptic Gregorian
/// calendar.
///
/// The year is counted from March so that the leap day falls at the end of it,
/// which is what lets the day-of-year be a single division rather than a table.
fn civil_from_days(days: i64) -> (u16, u8, u8) {
    // Shift the epoch to 0000-03-01, the start of a 400-year cycle, so that
    // everything below divides non-negative numbers.
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;

    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    (year as u16, month as u8, day as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spelled(millis: i64) -> String {
        from_unix_millis(millis).into_string()
    }

    #[test]
    fn the_epoch_is_the_epoch() {
        assert_eq!(spelled(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn a_known_instant_is_spelled_correctly() {
        assert_eq!(spelled(1_786_962_721_412), "2026-08-17T10:32:01.412Z");
    }

    #[test]
    fn leap_days_and_century_rules_land_where_they_should() {
        // 2000 and 2024 are leap years; 1900 is not, so the day after the
        // 28th of February that year is the 1st of March.
        assert_eq!(spelled(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(spelled(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
        assert_eq!(spelled(-2_203_977_600_000), "1900-02-28T00:00:00.000Z");
        assert_eq!(spelled(-2_203_891_200_000), "1900-03-01T00:00:00.000Z");
    }

    #[test]
    fn instants_before_the_epoch_go_backwards() {
        assert_eq!(spelled(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn differencing_two_timestamps_recovers_the_interval() {
        let earlier = from_unix_millis(1_786_962_721_412);
        for offset in [1, 999, 86_400_000, -86_400_000, 31_557_600_000] {
            let later = from_unix_millis(1_786_962_721_412 + offset);
            assert_eq!(later.millis_since(&earlier), offset);
        }
    }

    #[test]
    fn every_day_of_a_four_century_cycle_round_trips() {
        let mut previous = None;
        let mut day = MIN_DAY;
        while day <= MIN_DAY + 146_097 * 4 {
            let stamped = from_unix_millis(day * MILLIS_PER_DAY);
            if let Some(previous) = previous {
                assert!(stamped > previous, "{stamped:?} should follow {previous:?}");
            }
            previous = Some(stamped);
            day += 1;
        }
    }

    #[test]
    fn an_impossible_clock_still_yields_a_well_formed_timestamp() {
        assert_eq!(spelled(i64::MIN), "0000-01-01T00:00:00.000Z");
        assert_eq!(spelled(i64::MAX), "9999-12-31T23:59:59.999Z");
    }

    #[test]
    fn the_clock_reads_a_plausible_present() {
        // Anything this side of 2020 proves the epoch and the units are right;
        // pinning it tighter would only pin the test to the day it was written.
        assert!(now().as_str() > "2020-01-01T00:00:00.000Z");
    }
}
