//! Bit-bang timing for the CPU-driven I2C generator.
//!
//! Unlike everything else in this project, I2C edges come from the CPU
//! spinning in [`cortex_m::asm::delay`] rather than from a PIO divider. That
//! loop is *not* cycle-per-count: it executes `subs`/`bne` per iteration, which
//! costs three cycles on the Cortex-M33, and each bit also pays a fixed cost
//! for the GPIO writes around the delays. Computing the delay count as if one
//! count were one cycle - which this project did until it was measured - makes
//! the bus run three times slower than requested, and says nothing about it.
//!
//! The model below was fitted to a logic-analyzer capture of SCL on a Pico 2
//! at 150 MHz, decoding the clock directly:
//!
//! | requested | measured before | model |
//! |---|---|---|
//! | 10 kHz | 3.32 kHz | 10.00 kHz |
//! | 50 kHz | 16.38 kHz | 49.97 kHz |
//! | 100 kHz | 32.18 kHz | 100.0 kHz |
//! | 400 kHz | 117.85 kHz | 399.1 kHz |
//!
//! Two constants were solved from the 10 kHz and 100 kHz points and then
//! checked against the other two, predicting them to within 0.22%.
//!
//! These are timing constants for *generated* code, so they are only as stable
//! as the compiler output around the delay loop. That is exactly why
//! [`Plan::achieved_millihz`] exists: the console reports what the model says
//! was emitted, and a bench measurement can contradict it.

/// CPU cycles per quarter-bit delay count, scaled by 1000.
///
/// A bit spends four delay counts and the loop costs ~3.009 cycles each
/// (`subs` plus a taken `bne` on the Cortex-M33), so 12.036 per quarter.
/// Integer-scaled because this crate does no floating point.
const CYCLES_PER_QUARTER_X1000: u64 = 12_036;

/// Fixed CPU cycles per bit, outside the delay loop.
///
/// The GPIO writes in a bit period plus loop entry and exit. Measured as 230.6.
const FIXED_CYCLES_PER_BIT: u64 = 231;

/// Total cycles for one bit period at a given quarter count.
///
/// Both constants were fitted to a capture of the *optimised* build. They are
/// properties of the generated code, not of the chip, and this crate is built
/// with fat LTO - so an unrelated change elsewhere can shift them. That is why
/// [`plan`] returns an achieved rate for the console to report rather than
/// letting callers assume the request was met.
const fn bit_cycles(quarter: u32) -> u64 {
    (CYCLES_PER_QUARTER_X1000 * quarter as u64) / 1000 + FIXED_CYCLES_PER_BIT
}

/// Why a requested I2C clock cannot be produced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum I2cRateError {
    /// Faster than the bit-bang loop can toggle, even with no delay at all.
    TooFast,
    /// A clock of zero has no period.
    Zero,
}

impl I2cRateError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            I2cRateError::TooFast => "rate above i2c bit-bang maximum",
            I2cRateError::Zero => "rate must be non-zero",
        }
    }
}

/// A quarter-bit delay count and the bus clock it actually produces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Plan {
    /// Value to hand to the delay loop for each quarter-bit wait.
    pub quarter: u32,
    /// Achieved SCL frequency in milli-hertz.
    pub achieved_millihz: u64,
}

/// Plan an I2C bit-bang clock for `sysclk_hz`.
///
/// Returns the smallest quarter count whose achieved rate does not exceed the
/// request, so the bus is never driven faster than asked - overshooting is the
/// direction that breaks real slaves.
pub const fn plan(sysclk_hz: u32, hz: u32) -> Result<Plan, I2cRateError> {
    if hz == 0 {
        return Err(I2cRateError::Zero);
    }

    let want_cycles = sysclk_hz as u64 / hz as u64;
    // Equality is the fastest achievable case, exactly `quarter == 1`, so it
    // is accepted; anything shorter would need a fractional delay count.
    if want_cycles < bit_cycles(1) {
        return Err(I2cRateError::TooFast);
    }

    // Round up, so the period is never shorter than requested.
    let numerator = (want_cycles - FIXED_CYCLES_PER_BIT) * 1000;
    let quarter = numerator.div_ceil(CYCLES_PER_QUARTER_X1000);

    // want_cycles > bit_cycles(1) guarantees quarter >= 1, and the u32 cast is
    // safe because want_cycles is bounded by sysclk_hz.
    let quarter = quarter as u32;

    Ok(Plan {
        quarter,
        achieved_millihz: (sysclk_hz as u64 * 1000) / bit_cycles(quarter),
    })
}

/// Fastest rate [`plan`] will accept, in whole hertz.
pub const fn max_rate_hz(sysclk_hz: u32) -> u32 {
    (sysclk_hz as u64 / bit_cycles(1)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSCLK: u32 = 150_000_000;

    /// The achieved rate must never exceed the request: too fast is what
    /// breaks a real bus, too slow only makes it sluggish.
    #[test]
    fn never_overshoots_the_request() {
        for hz in [1_000, 10_000, 50_000, 100_000, 400_000, 600_000] {
            let p = plan(SYSCLK, hz).unwrap();
            assert!(
                p.achieved_millihz <= hz as u64 * 1000,
                "{hz} Hz overshot: {} millihz",
                p.achieved_millihz
            );
        }
    }

    /// The whole point of the fix: the standard rates land close, where the
    /// old one-cycle-per-count assumption was 3x slow.
    #[test]
    fn standard_rates_land_within_one_percent() {
        for hz in [10_000, 100_000, 400_000] {
            let p = plan(SYSCLK, hz).unwrap();
            let err = (hz as f64 * 1000.0 - p.achieved_millihz as f64) / (hz as f64 * 1000.0);
            assert!(err < 0.01, "{hz} Hz off by {:.3}%", err * 100.0);
        }
    }

    /// Regression against the measured hardware: 100 kHz used to ask for
    /// quarter=375 and deliver 32 kHz. The corrected count is near a third of
    /// that, because the delay loop costs three cycles per count.
    #[test]
    fn hundred_khz_matches_the_bench_measurement() {
        let p = plan(SYSCLK, 100_000).unwrap();
        assert_eq!(p.quarter, 106);
        // (12036*106)/1000 + 231 = 1506 cycles -> 99601.6 Hz
        assert_eq!(p.achieved_millihz, 99_601_593);
    }

    #[test]
    fn rejects_zero() {
        assert_eq!(plan(SYSCLK, 0), Err(I2cRateError::Zero));
    }

    #[test]
    fn rejects_rates_above_the_bit_bang_ceiling() {
        let max = max_rate_hz(SYSCLK);
        assert_eq!(max, 617_283);
        assert!(plan(SYSCLK, max).is_ok());
        assert_eq!(plan(SYSCLK, max * 2), Err(I2cRateError::TooFast));
    }

    /// Refused, not clamped - the project's rule for every other command.
    #[test]
    fn does_not_clamp_out_of_range_requests() {
        assert!(plan(SYSCLK, 5_000_000).is_err());
    }

    #[test]
    fn achieved_rate_is_consistent_with_the_quarter_count() {
        // Recomputing the rate from the returned count must give the same
        // answer, or the console would be reporting a number the hardware
        // never produced.
        for hz in [1_000, 37_000, 100_000, 600_000] {
            let p = plan(SYSCLK, hz).unwrap();
            let expect = (SYSCLK as u64 * 1000) / ((12_036 * p.quarter as u64) / 1000 + 231);
            assert_eq!(p.achieved_millihz, expect, "at {hz} Hz");
        }
    }

    #[test]
    fn slow_rates_stay_exact_enough() {
        let p = plan(SYSCLK, 1_000).unwrap();
        assert!((p.achieved_millihz as i64 - 1_000_000).abs() < 1_000);
    }

    /// Parameterised on sysclk like the PIO timing, so an overclock needs no
    /// other change.
    #[test]
    fn scales_with_sysclk() {
        let a = plan(150_000_000, 100_000).unwrap();
        let b = plan(300_000_000, 100_000).unwrap();
        assert!(b.quarter > a.quarter);
        let err = (b.achieved_millihz as i64 - 100_000_000).abs();
        assert!(err < 1_000_000, "300 MHz sysclk off by {err} millihz");
    }
}
