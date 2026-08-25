//! How long the window takes to draw, measured while it is drawing.
//!
//! A terminal that cannot keep up with a process printing at full speed is a
//! terminal nobody will use, and whether this one can is not a question to be
//! answered by watching the screen and forming an impression. So every frame is
//! timed, the times are rolled up once a period, and the result is both logged
//! and put on screen — so that a person judging whether this is fast enough is
//! reading numbers taken while they were looking at it.
//!
//! # What is being timed
//!
//! From the moment the view begins building its elements to just after the
//! frame containing them has been rendered: laying the elements out, painting
//! them, and handing the result to the platform. It does not include waiting for
//! the display to show it, which is not observable from here, and it does not
//! include the time the window spends idle between frames — a still screen draws
//! nothing at all, and averaging that in would make an idle window look fast.
//!
//! The rate is therefore a rate of frames *asked for*. Nothing here draws faster
//! than it is asked to, so a rate at the ceiling means the window kept up with
//! everything it was given, and one below it means it did not.
//!
//! The part of a frame which is this application's own — reading the grid and
//! building elements out of it — is timed separately and reported alongside.
//! The two numbers answer different questions when a frame turns out to be
//! expensive: one asks whether the code drawing a terminal is too slow, and the
//! other whether the machinery underneath it is.

use std::fmt;
use std::time::{Duration, Instant};

use tracing::info;

/// How long one report covers.
///
/// A second, because that is the unit the number is read in: a person watching
/// a scrolling window and a line saying how many frames went by in the last
/// second are looking at the same second.
pub const PERIOD: Duration = Duration::from_secs(1);

/// The frames drawn so far, and what the last period's worth came to.
#[derive(Debug)]
pub struct Frames {
    period: Duration,
    /// When the period being accumulated began — `None` until the first frame,
    /// so that a window which sat unopened for a minute does not report that
    /// minute as one very slow period.
    since: Option<Instant>,
    drawn: u32,
    spent: Duration,
    worst: Duration,
    building: Duration,
    last: Option<Report>,
}

/// One period's worth of frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// How many frames were drawn.
    pub drawn: u32,
    /// How long they were drawn over.
    pub over: Duration,
    /// The mean time one of them took.
    pub mean: Duration,
    /// The longest one of them took.
    pub worst: Duration,
    /// The mean time spent building one of them, of the mean above.
    pub building: Duration,
}

impl Report {
    /// Frames per second, over the period this covers.
    pub fn rate(&self) -> f64 {
        let over = self.over.as_secs_f64();
        if over <= 0.0 {
            return 0.0;
        }
        f64::from(self.drawn) / over
    }
}

impl fmt::Display for Report {
    /// The one line a person reads to know whether this is fast enough: how
    /// many frames a second, and what a frame cost when it cost the most.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:.0} fps · {:.1} ms mean ({:.1} ms building) · {:.1} ms worst",
            self.rate(),
            self.mean.as_secs_f64() * 1000.0,
            self.building.as_secs_f64() * 1000.0,
            self.worst.as_secs_f64() * 1000.0,
        )
    }
}

impl Frames {
    /// A record that reports every [`PERIOD`].
    pub fn new() -> Self {
        Self::reporting_every(PERIOD)
    }

    /// A record that reports every `period` instead.
    pub fn reporting_every(period: Duration) -> Self {
        Self {
            period,
            since: None,
            drawn: 0,
            spent: Duration::ZERO,
            worst: Duration::ZERO,
            building: Duration::ZERO,
            last: None,
        }
    }

    /// Takes in how long the view spent building one frame's elements.
    ///
    /// Said separately from the frame it belongs to because it is said at a
    /// different moment: building is over when the view hands its elements back,
    /// and the frame is not over until they have been drawn.
    pub fn built(&mut self, took: Duration) {
        self.building += took;
    }

    /// Takes in one frame, which began at `began` and has just been rendered.
    ///
    /// Called from the callback the window runs after a frame, with the moment
    /// the view started building that frame's elements.
    pub fn drew(&mut self, began: Instant, now: Instant) {
        let took = now.saturating_duration_since(began);
        self.drawn = self.drawn.saturating_add(1);
        self.spent += took;
        self.worst = self.worst.max(took);

        let since = *self.since.get_or_insert(began);
        let over = now.saturating_duration_since(since);
        if over >= self.period {
            self.roll_up(over, now);
        }
    }

    /// The last period's worth, or `None` until a period has gone by.
    pub fn last(&self) -> Option<Report> {
        self.last
    }

    /// Closes off the period and starts another.
    fn roll_up(&mut self, over: Duration, now: Instant) {
        let report = Report {
            drawn: self.drawn,
            over,
            mean: self.spent.checked_div(self.drawn).unwrap_or_default(),
            worst: self.worst,
            building: self.building.checked_div(self.drawn).unwrap_or_default(),
        };
        info!(
            frames = report.drawn,
            mean_ms = report.mean.as_secs_f64() * 1000.0,
            building_ms = report.building.as_secs_f64() * 1000.0,
            worst_ms = report.worst.as_secs_f64() * 1000.0,
            rate = report.rate(),
            "drew a second's worth of frames"
        );
        self.last = Some(report);
        self.since = Some(now);
        self.drawn = 0;
        self.spent = Duration::ZERO;
        self.worst = Duration::ZERO;
        self.building = Duration::ZERO;
    }
}

impl Default for Frames {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_reported_until_a_period_has_gone_by() {
        let mut frames = Frames::new();
        let began = Instant::now();
        frames.drew(began, began + Duration::from_millis(5));
        assert_eq!(frames.last(), None);
    }

    #[test]
    fn a_period_reports_the_frames_it_held() {
        let mut frames = Frames::reporting_every(Duration::from_millis(100));
        let start = Instant::now();
        // Three frames, 10ms apart, of which 1ms each was spent building.
        frames.built(Duration::from_millis(1));
        frames.drew(start, start + Duration::from_millis(2));
        frames.built(Duration::from_millis(1));
        frames.drew(
            start + Duration::from_millis(10),
            start + Duration::from_millis(16),
        );
        frames.built(Duration::from_millis(1));
        frames.drew(
            start + Duration::from_millis(20),
            start + Duration::from_millis(124),
        );

        let report = frames.last().expect("the period is over");
        assert_eq!(report.drawn, 3);
        assert_eq!(report.building, Duration::from_millis(1));
        assert_eq!(report.worst, Duration::from_millis(104));
        // The period is measured from the first frame, not from whenever this
        // was built.
        assert_eq!(report.over, Duration::from_millis(124));
    }

    #[test]
    fn a_period_starts_again_once_it_has_been_reported() {
        let mut frames = Frames::reporting_every(Duration::from_millis(10));
        let start = Instant::now();
        frames.drew(start, start + Duration::from_millis(20));
        frames.drew(
            start + Duration::from_millis(21),
            start + Duration::from_millis(22),
        );
        assert_eq!(
            frames.last().expect("the first period is over").drawn,
            1,
            "the second period's frame is not counted in the first period's report"
        );
    }

    #[test]
    fn a_rate_is_frames_over_the_time_they_were_drawn_in() {
        let report = Report {
            drawn: 30,
            over: Duration::from_millis(500),
            mean: Duration::from_millis(4),
            worst: Duration::from_millis(9),
            building: Duration::from_millis(1),
        };
        assert!((report.rate() - 60.0).abs() < f64::EPSILON);
        assert_eq!(
            report.to_string(),
            "60 fps · 4.0 ms mean (1.0 ms building) · 9.0 ms worst"
        );
    }
}
