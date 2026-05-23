use std::ops::Mul;

use jiff::{Span, SpanRound, Unit};

/// Energy in watt-hours, signed.
#[expect(unused)]
pub(crate) struct WattHours(i32);

/// Money amount in thousandsths of a dollar (hundredths of a penny).
#[expect(unused)]
pub(crate) struct Millidollars(u32);

#[expect(unused)]
pub(crate) struct Amperes(i32);
#[expect(unused)]
pub(crate) struct Volts(i16);
pub(crate) struct Watts(i32);

impl Mul<Amperes> for Volts {
    type Output = Watts;
    fn mul(self, rhs: Amperes) -> Self::Output {
        Watts(self.0 as i32 * rhs.0)
    }
}

impl Mul<Volts> for Amperes {
    type Output = Watts;
    fn mul(self, volts: Volts) -> Self::Output {
        Watts(self.0 * volts.0 as i32)
    }
}

impl Mul<Watts> for Span {
    type Output = WattHours;
    fn mul(self, watts: Watts) -> Self::Output {
        let round_opts = SpanRound::new().smallest(Unit::Second);
        let span_secs = self.round(round_opts).expect("Round failed").get_seconds() as f64;
        let watt_hours = (watts.0 as f64 * span_secs / 3600.0) as i32;
        WattHours(watt_hours)
    }
}

impl Mul<Span> for Watts {
    type Output = WattHours;
    fn mul(self, span: Span) -> Self::Output {
        let round_opts = SpanRound::new().smallest(Unit::Second);
        let span_secs = span.round(round_opts).expect("Round failed").get_seconds() as f64;
        let watt_hours = (self.0 as f64 * span_secs / 3600.0) as i32;
        WattHours(watt_hours)
    }
}
