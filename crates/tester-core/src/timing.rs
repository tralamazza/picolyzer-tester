//! PIO clock-divider arithmetic for the RP2350.
//!
//! The PIO clock divider is 16.8 fixed point with a minimum of 1.0, so almost
//! no requested frequency is exactly achievable. Everything here is integer
//! math in units of 1/256 of a divisor, and every result carries the *achieved*
//! rate as well as the register values.
//!
//! Callers must report the achieved rate, never the requested one. Silently
//! rounding a request is the easiest way to make a perfectly good logic
//! analyzer look broken.

/// Divisor in 1/256 units for a divider of exactly 1.0.
const MIN_X256: u64 = 256;
/// Largest divisor the hardware encodes without the `int == 0` special case,
/// i.e. 65535 + 255/256. The `int == 0` encoding means 65536 and is not used
/// here: one extra step at the very bottom of the range is not worth the
/// special case.
const MAX_X256: u64 = 65_535 * 256 + 255;

/// A PIO clock divider register setting.
///
/// The state machine executes one instruction every `int + frac/256` system
/// clock cycles.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Divisor {
    pub int: u16,
    pub frac: u8,
}

impl Divisor {
    fn from_x256(x256: u64) -> Self {
        Self {
            int: (x256 >> 8) as u16,
            frac: (x256 & 0xff) as u8,
        }
    }

    /// The divider expressed in 1/256 units.
    pub const fn x256(&self) -> u64 {
        ((self.int as u64) << 8) | self.frac as u64
    }
}

/// Why a requested rate could not be produced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimingError {
    /// Faster than one state-machine instruction per system clock.
    TooFast,
    /// Slower than the largest divider setting reaches.
    TooSlow,
    /// A rate of zero has no divisor.
    Zero,
}

impl TimingError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            TimingError::TooFast => "rate above sysclk",
            TimingError::TooSlow => "rate below minimum divider rate",
            TimingError::Zero => "rate must be non-zero",
        }
    }
}

/// A state-machine instruction rate, carried in milli-hertz.
///
/// Milli-hertz because at large divisors the whole-Hz rounding error is a
/// meaningful fraction of the rate, and this device is supposed to be the
/// trustworthy side of the measurement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rate {
    pub millihz: u64,
}

impl Rate {
    /// Whole hertz, truncated.
    pub const fn hz(&self) -> u32 {
        (self.millihz / 1000) as u32
    }

    /// The fractional hertz, 0..=999, for display as `hz.milli`.
    pub const fn milli_part(&self) -> u16 {
        (self.millihz % 1000) as u16
    }
}

/// Timing calculations anchored to one system clock frequency.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Timing {
    sysclk_hz: u32,
}

impl Timing {
    pub const fn new(sysclk_hz: u32) -> Self {
        assert!(sysclk_hz > 0);
        Self { sysclk_hz }
    }

    pub const fn sysclk_hz(&self) -> u32 {
        self.sysclk_hz
    }

    /// Fastest instruction rate: one instruction per system clock.
    pub const fn max_rate_hz(&self) -> u32 {
        self.sysclk_hz
    }

    /// Highest rate [`Self::divisor_for_rate`] will accept.
    ///
    /// Slightly above [`Self::max_rate_hz`] because of round-to-nearest: any
    /// request that rounds to a divisor of 1.0 is accepted and achieves
    /// `max_rate_hz`. Derived from `sysclk*256 + rate/2 >= 256*rate`.
    pub const fn max_accepted_rate_hz(&self) -> u32 {
        // rate <= sysclk*256 / 255.5, computed as 512*sysclk/511 to stay integral.
        ((self.sysclk_hz as u64 * 512) / 511) as u32
    }

    /// Slowest instruction rate reachable by the divider alone.
    ///
    /// Rounded up, so this value is itself always achievable. Rates below it
    /// are produced by repeating samples in the pattern buffer instead.
    pub const fn min_rate_hz(&self) -> u32 {
        ((self.sysclk_hz as u64 * 256).div_ceil(MAX_X256)) as u32
    }

    /// Duration of one system clock tick in picoseconds. This is the
    /// resolution of every pulse width the device can produce.
    pub const fn tick_ps(&self) -> u64 {
        1_000_000_000_000 / self.sysclk_hz as u64
    }

    /// Divisor for a requested instruction rate, rounded to nearest.
    ///
    /// Rounding to nearest rather than truncating matters: truncation biases
    /// every frequency in one direction, which shows up as a systematic error
    /// across a frequency sweep.
    ///
    /// Because rounding happens before the range check, a request slightly
    /// *above* `max_rate_hz` is accepted and rounds down to the divisor-1.0
    /// rate rather than being rejected - the same way a request slightly below
    /// it rounds up. This is deliberate: rejecting one side of a rounding
    /// boundary while accepting the other would be arbitrary. The caller
    /// reports the achieved rate either way, so nothing is hidden. See
    /// [`Self::max_accepted_rate_hz`] for the exact cutoff.
    pub fn divisor_for_rate(&self, rate_hz: u32) -> Result<Divisor, TimingError> {
        if rate_hz == 0 {
            return Err(TimingError::Zero);
        }
        let rate = rate_hz as u64;
        let x256 = (self.sysclk_hz as u64 * 256 + rate / 2) / rate;
        if x256 < MIN_X256 {
            return Err(TimingError::TooFast);
        }
        if x256 > MAX_X256 {
            return Err(TimingError::TooSlow);
        }
        Ok(Divisor::from_x256(x256))
    }

    /// The instruction rate a divisor actually produces.
    pub fn rate_of(&self, divisor: Divisor) -> Rate {
        Rate {
            millihz: (self.sysclk_hz as u64 * 256 * 1000) / divisor.x256(),
        }
    }

    /// System clock ticks closest to a duration in nanoseconds.
    ///
    /// Saturates at 1 tick: a zero-width pulse is not a thing the hardware can
    /// emit, and rounding a sub-tick request down to nothing would silently
    /// drop the edge the caller asked for.
    pub fn ticks_for_ns(&self, ns: u32) -> u32 {
        let ps = ns as u64 * 1000;
        let ticks = (ps + self.tick_ps() / 2) / self.tick_ps();
        ticks.max(1).min(u32::MAX as u64) as u32
    }

    /// Duration of `ticks` system clocks, in picoseconds.
    pub const fn ns_of_ticks_ps(&self, ticks: u32) -> u64 {
        ticks as u64 * self.tick_ps()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RP2350: Timing = Timing::new(150_000_000);

    #[test]
    fn full_speed_is_divisor_one() {
        let d = RP2350.divisor_for_rate(150_000_000).unwrap();
        assert_eq!(d, Divisor { int: 1, frac: 0 });
        assert_eq!(RP2350.rate_of(d).hz(), 150_000_000);
    }

    #[test]
    fn well_above_sysclk_is_rejected_not_clamped() {
        assert_eq!(RP2350.divisor_for_rate(u32::MAX), Err(TimingError::TooFast));
        assert_eq!(
            RP2350.divisor_for_rate(300_000_000),
            Err(TimingError::TooFast)
        );
    }

    #[test]
    fn rounding_boundary_above_sysclk_is_exact_and_honest() {
        // Round-to-nearest means requests a hair above sysclk still land on
        // divisor 1.0 rather than being rejected. Pin the exact cutoff so this
        // stays a decision and not an accident.
        let cutoff = RP2350.max_accepted_rate_hz();
        assert_eq!(cutoff, 150_293_542);

        let d = RP2350.divisor_for_rate(cutoff).unwrap();
        assert_eq!(d, Divisor { int: 1, frac: 0 });
        // Crucially: it reports what it really does, not what was asked.
        assert_eq!(RP2350.rate_of(d).hz(), RP2350.max_rate_hz());

        assert_eq!(
            RP2350.divisor_for_rate(cutoff + 1),
            Err(TimingError::TooFast)
        );
    }

    #[test]
    fn zero_is_rejected() {
        assert_eq!(RP2350.divisor_for_rate(0), Err(TimingError::Zero));
    }

    #[test]
    fn unreachable_rate_reports_what_it_actually_does() {
        // 150e6 / 7e6 = 21.42857..., nearest 1/256 step is 21 + 110/256.
        let d = RP2350.divisor_for_rate(7_000_000).unwrap();
        assert_eq!(d, Divisor { int: 21, frac: 110 });
        let actual = RP2350.rate_of(d);
        assert_ne!(actual.hz(), 7_000_000);
        // Within 0.05% of the request, but not equal - and we say so.
        let err = (actual.hz() as i64 - 7_000_000).unsigned_abs();
        assert!(err < 7_000_000 / 2000, "error {err} Hz too large");
    }

    #[test]
    fn exactly_representable_rates_are_exact() {
        // Any integer divisor of 150 MHz must come back exact.
        for div in [1u32, 2, 3, 5, 10, 100, 1000, 15_000] {
            let rate = 150_000_000 / div;
            let d = RP2350.divisor_for_rate(rate).unwrap();
            assert_eq!(d.frac, 0, "divisor {div} should be integral");
            assert_eq!(RP2350.rate_of(d).hz(), rate, "divisor {div}");
        }
    }

    #[test]
    fn min_rate_is_achievable_and_below_it_is_rejected() {
        let min = RP2350.min_rate_hz();
        assert!(RP2350.divisor_for_rate(min).is_ok());
        assert_eq!(RP2350.divisor_for_rate(min - 1), Err(TimingError::TooSlow));
    }

    #[test]
    fn rounding_is_nearest_not_truncating() {
        // Sweep a range and check the achieved rate is never further from the
        // request than half a divider step - i.e. no directional bias.
        for rate in (1_000_000..2_000_000).step_by(9_973) {
            let d = RP2350.divisor_for_rate(rate).unwrap();
            let ideal_x256 = 150_000_000f64 * 256.0 / rate as f64;
            let err = (d.x256() as f64 - ideal_x256).abs();
            assert!(err <= 0.5 + 1e-9, "rate {rate}: x256 error {err}");
        }
    }

    #[test]
    fn tick_is_six_point_six_nanoseconds() {
        assert_eq!(RP2350.tick_ps(), 6_666);
    }

    #[test]
    fn ns_to_ticks_rounds_to_nearest() {
        assert_eq!(RP2350.ticks_for_ns(0), 1, "never round an edge away");
        assert_eq!(RP2350.ticks_for_ns(7), 1);
        assert_eq!(RP2350.ticks_for_ns(10), 2); // 10ns / 6.666ns = 1.5 -> 2
        assert_eq!(RP2350.ticks_for_ns(100), 15);
        assert_eq!(RP2350.ticks_for_ns(1000), 150);
    }

    #[test]
    fn rate_milli_part_splits_correctly() {
        let r = Rate { millihz: 1_234_567 };
        assert_eq!(r.hz(), 1234);
        assert_eq!(r.milli_part(), 567);
    }

    #[test]
    fn works_for_an_overclocked_sysclk_too() {
        // The whole point of parameterising on sysclk: raising the clock later
        // must not need any other change.
        let oc = Timing::new(300_000_000);
        assert_eq!(oc.tick_ps(), 3_333);
        let d = oc.divisor_for_rate(300_000_000).unwrap();
        assert_eq!(d, Divisor { int: 1, frac: 0 });
    }
}
