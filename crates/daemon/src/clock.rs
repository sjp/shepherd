//! The daemon's clock.
//!
//! Everything in the daemon that wants to know the time asks here, so that the
//! wall clock is read in one place rather than in every module that stamps
//! something. The calendar arithmetic itself is not here: it belongs to the type
//! that defines the format, beside the difference that has to agree with it, and
//! this is the daemon's name for it.
//!
//! What is genuinely the daemon's own is below it — the scatter that keeps two
//! things retrying after the same outage from retrying in step.

use std::time::{SystemTime, UNIX_EPOCH};

use agentbus_protocol::Timestamp;

/// The current time.
pub fn now() -> Timestamp {
    Timestamp::now()
}

/// The instant `millis` milliseconds after the Unix epoch, as a timestamp.
///
/// For the times this daemon reads rather than takes: a process's start time, a
/// file's modification time. See [`Timestamp::from_unix_millis`] for what
/// happens to an instant outside the range a timestamp can spell.
pub fn from_unix_millis(millis: i64) -> Timestamp {
    Timestamp::from_unix_millis(millis)
}

/// A fraction in `0..1` that is different every time it is asked for.
///
/// For spreading retries, which is the only thing in this daemon that wants a
/// number it cannot predict. Taken from the clock rather than from a generator
/// of random numbers, because what is actually wanted is that two things
/// retrying after the same outage do not retry in step, and the sub-second part
/// of the wall clock differs between any two moments that reach it. It also
/// keeps a crate with no other use for randomness from acquiring a dependency
/// for three lines of arithmetic.
pub fn scatter() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    f64::from(nanos) / f64::from(NANOS_PER_SECOND)
}

/// Nanoseconds in a second, as the divisor that turns one into a fraction.
const NANOS_PER_SECOND: u32 = 1_000_000_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_reads_a_plausible_present() {
        // Anything this side of 2020 proves the daemon is reading the clock the
        // protocol's own conversion was tested against; the conversion itself is
        // exercised where it lives.
        assert!(now().as_str() > "2020-01-01T00:00:00.000Z");
        assert_eq!(
            from_unix_millis(1_786_962_721_412).as_str(),
            "2026-08-17T10:32:01.412Z"
        );
    }

    #[test]
    fn the_scatter_stays_inside_the_second_it_came_from() {
        for _ in 0..100 {
            let scattered = scatter();
            assert!((0.0..1.0).contains(&scattered), "{scattered}");
        }
    }
}
