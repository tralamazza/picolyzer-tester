//! Synthesis of 16-channel sample patterns.
//!
//! Everything the parallel bus emits - square waves, pulses, walking ones, gray
//! codes, glitches, cross-channel skew - is one list of 16-bit samples clocked
//! out at a fixed rate, one sample per state-machine cycle. Collapsing all the
//! waveform types onto a single engine means there is exactly one timing path
//! to get right, and any combination of them is expressible as a single
//! pattern.
//!
//! Sample bit N drives bus channel N.

use crate::timing::{Divisor, Timing, TimingError};

/// Why a pattern could not be produced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PatternError {
    /// The pattern needs more samples than the buffer holds.
    TooLong { needed: usize, capacity: usize },
    /// A width or count argument was outside the legal range.
    BadArgument(&'static str),
    /// The requested rate is unreachable even with sample repetition.
    Rate(TimingError),
}

impl From<TimingError> for PatternError {
    fn from(e: TimingError) -> Self {
        PatternError::Rate(e)
    }
}

/// How a requested per-sample rate is actually realised.
///
/// The PIO divider bottoms out around a few kHz. Slower patterns are produced
/// by writing each logical sample `repeat` times into the buffer and running
/// the state machine faster, which keeps a single 1-cycle-per-sample program
/// covering the whole frequency range instead of needing a second, slower
/// program with different timing characteristics.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RatePlan {
    pub divisor: Divisor,
    /// Buffer entries per logical sample. 1 for everything above the floor.
    pub repeat: u32,
    /// The logical sample rate actually achieved, in milli-hertz.
    pub actual_millihz: u64,
}

/// Work out how to clock a pattern at `sample_rate_hz` logical samples/second.
///
/// `max_repeat` bounds how far below the divider floor we are willing to go,
/// and comes from how much pattern buffer the caller can spare.
pub fn plan_rate(
    timing: &Timing,
    sample_rate_hz: u32,
    max_repeat: u32,
) -> Result<RatePlan, PatternError> {
    if sample_rate_hz == 0 {
        return Err(PatternError::Rate(TimingError::Zero));
    }
    let floor = timing.min_rate_hz();

    let repeat = if sample_rate_hz >= floor {
        1
    } else {
        let needed = floor.div_ceil(sample_rate_hz);
        if needed > max_repeat {
            return Err(PatternError::Rate(TimingError::TooSlow));
        }
        needed
    };

    // Run the state machine `repeat` times faster; each logical sample occupies
    // `repeat` buffer entries, so the logical rate comes back to the request.
    let sm_rate = sample_rate_hz
        .checked_mul(repeat)
        .ok_or(PatternError::Rate(TimingError::TooFast))?;
    let divisor = timing.divisor_for_rate(sm_rate)?;

    Ok(RatePlan {
        divisor,
        repeat,
        actual_millihz: timing.rate_of(divisor).millihz / repeat as u64,
    })
}

/// Writes samples into a caller-owned buffer, expanding each logical sample
/// into `repeat` entries.
pub struct Writer<'a> {
    buf: &'a mut [u16],
    len: usize,
    repeat: u32,
}

impl<'a> Writer<'a> {
    pub fn new(buf: &'a mut [u16], repeat: u32) -> Self {
        Self {
            buf,
            len: 0,
            repeat: repeat.max(1),
        }
    }

    /// Append one logical sample.
    pub fn push(&mut self, sample: u16) -> Result<(), PatternError> {
        let needed = self.len + self.repeat as usize;
        if needed > self.buf.len() {
            return Err(PatternError::TooLong {
                needed,
                capacity: self.buf.len(),
            });
        }
        for _ in 0..self.repeat {
            self.buf[self.len] = sample;
            self.len += 1;
        }
        Ok(())
    }

    /// Append `count` copies of one logical sample.
    pub fn push_n(&mut self, sample: u16, count: u32) -> Result<(), PatternError> {
        for _ in 0..count {
            self.push(sample)?;
        }
        Ok(())
    }

    /// Total buffer entries written.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A square wave on `mask`: one low sample, one high sample.
///
/// The caller sets the sample rate to twice the wanted frequency.
pub fn square(buf: &mut [u16], repeat: u32, mask: u16) -> Result<usize, PatternError> {
    let mut w = Writer::new(buf, repeat);
    w.push(0)?;
    w.push(mask)?;
    Ok(w.len())
}

/// A pulse train: `high` samples asserted, then `period - high` samples idle.
pub fn pulse(
    buf: &mut [u16],
    repeat: u32,
    mask: u16,
    high: u32,
    period: u32,
) -> Result<usize, PatternError> {
    if high == 0 {
        return Err(PatternError::BadArgument("pulse width must be non-zero"));
    }
    if period <= high {
        return Err(PatternError::BadArgument("period must exceed pulse width"));
    }
    let mut w = Writer::new(buf, repeat);
    w.push_n(mask, high)?;
    w.push_n(0, period - high)?;
    Ok(w.len())
}

/// A single burst: `ticks` asserted samples framed by idle samples.
///
/// Used for the narrow-glitch test. The leading and trailing idle samples
/// guarantee both edges are inside the pattern, so the analyzer sees a real
/// pulse rather than a level change at the buffer boundary.
pub fn glitch(buf: &mut [u16], mask: u16, ticks: u32) -> Result<usize, PatternError> {
    if ticks == 0 {
        return Err(PatternError::BadArgument("glitch width must be non-zero"));
    }
    let mut w = Writer::new(buf, 1);
    w.push(0)?;
    w.push_n(mask, ticks)?;
    w.push(0)?;
    Ok(w.len())
}

/// Two channels rising `ticks` apart, to measure cross-channel skew resolution.
///
/// Both fall together at the end, so the analyzer sees one aligned edge and one
/// offset edge in the same capture and any error is attributable.
pub fn skew(buf: &mut [u16], mask_a: u16, mask_b: u16, ticks: u32) -> Result<usize, PatternError> {
    if ticks == 0 {
        return Err(PatternError::BadArgument("skew must be non-zero"));
    }
    let mut w = Writer::new(buf, 1);
    w.push(0)?;
    w.push_n(mask_a, ticks)?;
    w.push_n(mask_a | mask_b, ticks)?;
    w.push(0)?;
    Ok(w.len())
}

/// Walking ones across the low `width` channels.
///
/// The canonical channel-mapping test: if the analyzer shows the walk in the
/// wrong order, its channels are swapped.
pub fn walking_ones(buf: &mut [u16], repeat: u32, width: u8) -> Result<usize, PatternError> {
    if width == 0 || width > 16 {
        return Err(PatternError::BadArgument("width must be 1..=16"));
    }
    let mut w = Writer::new(buf, repeat);
    for bit in 0..width {
        w.push(1u16 << bit)?;
    }
    Ok(w.len())
}

/// Walking zeros: the inverse of [`walking_ones`], which catches stuck-high
/// channels that walking ones alone can miss.
pub fn walking_zeros(buf: &mut [u16], repeat: u32, width: u8) -> Result<usize, PatternError> {
    if width == 0 || width > 16 {
        return Err(PatternError::BadArgument("width must be 1..=16"));
    }
    let all = mask_of_width(width);
    let mut w = Writer::new(buf, repeat);
    for bit in 0..width {
        w.push(all & !(1u16 << bit))?;
    }
    Ok(w.len())
}

/// A full gray-code sequence over the low `width` channels.
///
/// Exactly one channel changes per sample, so any sample the analyzer records
/// with two simultaneous transitions is a sampling error on its side.
pub fn gray(buf: &mut [u16], repeat: u32, width: u8) -> Result<usize, PatternError> {
    if width == 0 || width > 16 {
        return Err(PatternError::BadArgument("width must be 1..=16"));
    }
    let count = 1u32 << width;
    let mut w = Writer::new(buf, repeat);
    for n in 0..count {
        w.push((n ^ (n >> 1)) as u16)?;
    }
    Ok(w.len())
}

/// A full binary count over the low `width` channels.
pub fn count(buf: &mut [u16], repeat: u32, width: u8) -> Result<usize, PatternError> {
    if width == 0 || width > 16 {
        return Err(PatternError::BadArgument("width must be 1..=16"));
    }
    let total = 1u32 << width;
    let mut w = Writer::new(buf, repeat);
    for n in 0..total {
        w.push(n as u16)?;
    }
    Ok(w.len())
}

/// Mask covering the low `width` channels.
pub fn mask_of_width(width: u8) -> u16 {
    if width >= 16 {
        u16::MAX
    } else {
        (1u16 << width) - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RP2350: Timing = Timing::new(150_000_000);

    fn buf() -> [u16; 4096] {
        [0; 4096]
    }

    #[test]
    fn square_is_two_samples() {
        let mut b = buf();
        let n = square(&mut b, 1, 0x0001).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&b[..2], &[0x0000, 0x0001]);
    }

    #[test]
    fn repeat_expands_every_sample_equally() {
        let mut b = buf();
        let n = square(&mut b, 3, 0x0004).unwrap();
        assert_eq!(n, 6);
        assert_eq!(&b[..6], &[0, 0, 0, 4, 4, 4]);
    }

    #[test]
    fn walking_ones_visits_each_channel_once() {
        let mut b = buf();
        let n = walking_ones(&mut b, 1, 16).unwrap();
        assert_eq!(n, 16);
        for (i, s) in b[..16].iter().enumerate() {
            assert_eq!(*s, 1 << i, "sample {i}");
        }
    }

    #[test]
    fn walking_zeros_is_the_complement() {
        let mut b = buf();
        let n = walking_zeros(&mut b, 1, 8).unwrap();
        assert_eq!(n, 8);
        for (i, s) in b[..8].iter().enumerate() {
            assert_eq!(*s, 0xff & !(1 << i), "sample {i}");
        }
    }

    #[test]
    fn gray_changes_exactly_one_bit_per_step_including_the_wrap() {
        let mut b = buf();
        let width = 8u8;
        let n = gray(&mut b, 1, width).unwrap();
        assert_eq!(n, 256);
        for i in 0..n {
            let a = b[i];
            let c = b[(i + 1) % n]; // include the wrap back to the start
            assert_eq!(
                (a ^ c).count_ones(),
                1,
                "step {i}: {a:#06x} -> {c:#06x} changed {} bits",
                (a ^ c).count_ones()
            );
        }
    }

    #[test]
    fn gray_visits_every_code_exactly_once() {
        let mut b = buf();
        let n = gray(&mut b, 1, 8).unwrap();
        let mut seen = [false; 256];
        for s in &b[..n] {
            assert!(!seen[*s as usize], "code {s:#06x} repeated");
            seen[*s as usize] = true;
        }
        assert!(seen.iter().all(|s| *s));
    }

    #[test]
    fn count_is_a_full_ramp() {
        let mut b = buf();
        let n = count(&mut b, 1, 8).unwrap();
        assert_eq!(n, 256);
        for (i, s) in b[..256].iter().enumerate() {
            assert_eq!(*s as usize, i);
        }
    }

    #[test]
    fn glitch_is_framed_by_idle_samples_on_both_sides() {
        let mut b = buf();
        let n = glitch(&mut b, 0x8000, 1).unwrap();
        assert_eq!(n, 3);
        // Both edges must be inside the pattern or it is not a pulse.
        assert_eq!(&b[..3], &[0x0000, 0x8000, 0x0000]);
    }

    #[test]
    fn skew_offsets_the_second_edge_and_aligns_the_fall() {
        let mut b = buf();
        let n = skew(&mut b, 0b01, 0b10, 2).unwrap();
        assert_eq!(n, 6);
        assert_eq!(&b[..6], &[0b00, 0b01, 0b01, 0b11, 0b11, 0b00]);
    }

    #[test]
    fn pulse_rejects_a_period_that_does_not_fit_the_width() {
        let mut b = buf();
        assert!(matches!(
            pulse(&mut b, 1, 1, 5, 5),
            Err(PatternError::BadArgument(_))
        ));
        assert!(matches!(
            pulse(&mut b, 1, 1, 0, 10),
            Err(PatternError::BadArgument(_))
        ));
        assert_eq!(pulse(&mut b, 1, 1, 2, 10).unwrap(), 10);
        assert_eq!(&b[..10], &[1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn overflow_is_reported_with_the_numbers_needed_to_fix_it() {
        let mut small = [0u16; 4];
        match gray(&mut small, 1, 8) {
            Err(PatternError::TooLong { needed, capacity }) => {
                assert_eq!(capacity, 4);
                assert!(needed > capacity);
            }
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    #[test]
    fn rates_above_the_floor_need_no_repetition() {
        let p = plan_rate(&RP2350, 1_000_000, 4096).unwrap();
        assert_eq!(p.repeat, 1);
        assert_eq!(p.actual_millihz / 1000, 1_000_000);
    }

    #[test]
    fn rates_below_the_floor_are_reached_by_repeating_samples() {
        let floor = RP2350.min_rate_hz();
        let target = 10; // 10 Sa/s, far below the divider floor
        let p = plan_rate(&RP2350, target, 4096).unwrap();
        assert!(p.repeat > 1, "should have needed repetition");
        // The state machine itself must run at or above the floor.
        let sm_hz = RP2350.rate_of(p.divisor).hz();
        assert!(sm_hz >= floor, "sm rate {sm_hz} below floor {floor}");
        // And the logical rate must come back to the request.
        let actual_hz = p.actual_millihz as f64 / 1000.0;
        assert!(
            (actual_hz - target as f64).abs() < 0.5,
            "logical rate {actual_hz} != {target}"
        );
    }

    #[test]
    fn a_rate_too_slow_for_the_buffer_is_refused_not_silently_sped_up() {
        // With a repeat budget of 1, nothing below the divider floor is possible.
        assert_eq!(
            plan_rate(&RP2350, 1, 1),
            Err(PatternError::Rate(TimingError::TooSlow))
        );
    }

    #[test]
    fn zero_rate_is_refused() {
        assert_eq!(
            plan_rate(&RP2350, 0, 4096),
            Err(PatternError::Rate(TimingError::Zero))
        );
    }

    #[test]
    fn width_arguments_are_validated() {
        let mut b = buf();
        assert!(matches!(
            gray(&mut b, 1, 0),
            Err(PatternError::BadArgument(_))
        ));
        assert!(matches!(
            walking_ones(&mut b, 1, 17),
            Err(PatternError::BadArgument(_))
        ));
    }

    #[test]
    fn mask_of_width_saturates_at_sixteen() {
        assert_eq!(mask_of_width(1), 0x0001);
        assert_eq!(mask_of_width(8), 0x00ff);
        assert_eq!(mask_of_width(16), 0xffff);
        assert_eq!(mask_of_width(200), 0xffff);
    }
}
