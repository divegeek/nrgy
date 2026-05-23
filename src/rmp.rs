use std::rc::Rc;

use jiff::{
    Timestamp, TimestampRound, Zoned,
    civil::{Date, Time, Weekday},
    tz::TimeZone,
};

use crate::units::{Millidollars, WattHours};

const SECS_PER_QH: i64 = 60 * 15;

/// Time in 15-minute intervals since the epoch.  So given a Jiff timestamp (nanoseconds since the
/// epoch), a [`QuarterHourTimestamp`] is `timestamp / 1_000_000_000 / 60 / 15`.
///
/// Creating a [`QuarterHourTimestamp`] from a [`Timestamp`] rounds down.  If the timestamp is
/// 2:14.999999999, the `QuarterHourTimestamp` will be 2:15
struct QuarterHourTimestamp(i32);

#[expect(dead_code)]
pub(crate) struct ConsumptionRecord {
    /// Start time of interval.  
    time: QuarterHourTimestamp,
    /// Energy consumed during interval
    from_grid: WattHours,
    /// Energy sold during an interval
    to_grid: WattHours,
}

/// Tariff or rate schedule for electricity.  Computes the fee (negative Mills) or credit
/// (positive Mills) for purchase or sale of elecricity for the specified [`ConsumptionRecord`],
/// if applicable.  Returns [`Option::None`] if the tariff doesn't apply.
#[expect(dead_code)]
pub(crate) trait Tariff {
    fn fee(&self, record: ConsumptionRecord) -> Option<Millidollars>;
}

#[expect(dead_code)]
struct TouTariff {
    active_intervals: Rc<[Interval]>,
    tariff: Millidollars,
    credit: Millidollars,
}

impl Tariff for TouTariff {
    fn fee(&self, _record: ConsumptionRecord) -> Option<Millidollars> {
        todo!()
    }
}

struct Interval {
    dates: DateRange,
    times: TimeRange,
    days_of_week: Rc<[Weekday]>,
}

#[expect(dead_code)]
impl Interval {
    fn matches(&self, time: Timestamp) -> bool {
        let zoned = Zoned::new(time, TimeZone::system());
        self.dates.matches(zoned.date())
            && self.times.matches(zoned.time())
            && self.days_of_week.contains(&zoned.weekday())
    }
}

struct DateRange {
    begin: Day,
    end: Day,
}

impl DateRange {
    fn matches(&self, date: Date) -> bool {
        date.month() >= self.begin.month
            && date.month() <= self.end.month
            && date.day() >= self.begin.day
            && date.day() <= self.end.day
    }
}

struct Day {
    day: i8,
    month: i8,
}

#[expect(dead_code)]
struct TimeRange {
    begin: TimeSpec,
    end: TimeSpec,
}

impl TimeRange {
    fn matches(&self, _time: Time) -> bool {
        false
    }
}

#[expect(dead_code)]
struct TimeSpec {
    hour: u8,
    minute: u8,
}

impl From<Timestamp> for QuarterHourTimestamp {
    fn from(value: Timestamp) -> Self {
        Self((trunc_to_quarter_hour(value).as_second() / SECS_PER_QH) as i32)
    }
}

impl From<QuarterHourTimestamp> for Timestamp {
    fn from(value: QuarterHourTimestamp) -> Self {
        Timestamp::from_second(value.0 as i64 * 60 * 15).expect("Out of range")
    }
}

fn trunc_to_quarter_hour(value: Timestamp) -> Timestamp {
    let round_opts = TimestampRound::new()
        .mode(jiff::RoundMode::Floor)
        .smallest(jiff::Unit::Minute)
        .increment(15);
    value.round(round_opts).expect("Infallible")
}
