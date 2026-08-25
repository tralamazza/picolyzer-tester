# Measurements

What has actually been put on an analyzer, what the numbers were, and what
remains unmeasured. Claims in the README that are not backed by something here
should be treated as untested.

Instrument: a Sipeed SLogic16 U3 driven by sigrok, 5 to 800 MSa/s depending on
channel count. No oscilloscope has touched a pin, so **rise times, overshoot and
crosstalk are entirely unmeasured** — every result below is a threshold
crossing seen by a digital sampler, which cannot distinguish a clean edge from a
ringing one that happens to cross at the right moment.

## Bus timing

| Property | Result |
|---|---|
| Frequency | Exact to the 75 MHz ceiling, fractional divisors included. 65.084746 MHz commanded read 65.0845 MHz. |
| Timebase | Agrees with the analyzer to −2.0 ppm. |
| Minimum pulse | The one-tick 6.666 ns `glitch` caught every time at 200 MSa/s. At 800 MSa/s the same width was checked with `pulse` (a repeating train, so no trigger is needed): 250 pulses, none missed, reading 5.33 samples as predicted. |
| `skew` | Within one sample at 1–75 ticks. Commanded 1/5/20/75 ticks read 1/7/27/100 samples at 200 MSa/s against 1.33/6.67/26.66/99.99 predicted. |
| Inter-channel skew | 0.43 ns spread across GP0–GP7 driven by one state machine, 501 edges each at 400 MSa/s. |
| Dropped samples | None. A 1 M-sample `gray` run gave 4998/4998 single-bit steps. |
| Protocols | UART, SPI and I2C decode byte-exact. |

`count` shows brief intermediate codes during multi-bit transitions. Those are
real on the wire, not lost data — the code sequence itself is always correct.
Use `gray` when that matters.

At 800 MSa/s the analyzer never became the limiting factor: the shortest pulse
this firmware can emit is still comfortably resolved, so the floor measured
there is the stimulus, not the instrument.

## Trigger marker alignment

The marker on GP16 exists so a capture can be aligned to the start of a burst,
which is only worth anything if it is aligned to the data itself.

It was documented as cycle-aligned from v0.3.0, on the strength of the PIO
synchronised group start plus a check at 200 MSa/s that found the marker and the
data rising in the same sample 13 times in 14 — statistically indistinguishable
from two pins driven by the *same* state machine, which split across a sample
boundary at the same rate.

That check was wrong, and instructively so. The real offset was one PIO cycle,
6.666 ns, against a 5 ns sample period: a lag that size can only show up as the
boundary-split noise already present. **Agreement at the noise floor is not
evidence of zero** — a near-zero effect and a small effect look identical to a
test whose resolution is the effect size.

What resolved it was measuring in units the lag scales with. `walk` runs one PIO
cycle per sample and `count` runs two, so a fixed one-cycle offset reads as a
whole bus tick in one engine and half a tick in the other — a signature sampling
jitter cannot produce. Measured −25 samples under `walk` and +12 under `count`
at 25 MSa/s.

The cause was in this firmware, not the analyzer. The synchronised group start
aligns the state machines' clocks, not the instruction at which each first
drives a pin, and the marker program spent a `mov x, osr` getting to its `set
pins, 1`. Fixed in v0.5.1 by raising the pin one instruction earlier.

After the fix, at 25 MSa/s: offset 0 under `walk`, and 25 — one full bus tick —
under `count`. Width re-checked at 5 MSa/s, 400 samples in both engines for the
8 ticks requested. That bounds any residual offset well under one 40 ns sample
but does not prove it zero; a sub-cycle skew would need a faster capture than
any used here.

## I2C timing model

I2C is bit-banged from the CPU, so its edge placement is approximate — fine for
exercising a decoder, useless for measuring one.

Its clock does not come from a divider. `actual_hz` is a *prediction* from a
model of the delay loop fitted to bench measurements, verified within 1% of the
report at 10 k, 50 k, 100 k and 400 kHz. The ceiling is 617 kHz, where a
PIO-driven bus would reach 1 MHz.

The model's two constants live in `crates/tester-core/src/i2c_timing.rs` and are
fitted to the machine code LLVM emits for the delay loop — they describe the
compiler's output, not the chip. This is why `rust-toolchain.toml` pins the
toolchain rather than tracking stable. `just guard` hashes the instruction bytes
of that one function and fails the build if a toolchain or HAL bump reshapes it,
because at that point SCL has to be re-measured before `actual_hz` means
anything again. Bumping the pin is a deliberate act, not a routine one.

## Analyzer defects found along the way

Testing an analyzer with a known-good source cuts both ways: several of these
were bugs in the instrument's driver, not in the firmware. Fixes for the
SLogic16 U3 live on a `sipeed-slogic-fixes` branch of libsigrok.

- **Threshold DAC mapping.** The driver assumed a 6.66 V full scale through the
  origin. The real transfer function has a smaller slope and a non-zero
  intercept, putting a requested 1.5 V at roughly 1.19 V. Calibrated against a
  lab supply over 1.0–3.3 V; linearity degrades above ~4 V and true full scale
  remains unmeasured.
- **Triggered captures below 16 channels.** Two unit mismatches between raw wire
  bytes and post-demux bytes, both masked at 16 channels, which made
  captureratio land at the wrong place or never trigger at all.
- **Soft-trigger bandwidth ceiling.** Trigger processing costs about 4x
  throughput headroom, so armed captures need to stay at or below ~100 MB/s even
  though the hardware sustains 400 MB/s untriggered. Not a bug — a limit worth
  knowing, since exceeding it silently returns no trigger.
- **Highs are stretched by 0.2–0.3 ns**, and by ~0.5 ns at the 6.666 ns floor.
  Across the pulse sweep every mean width sat above prediction (5.73 vs 5.33,
  16.25 vs 16.00, 80.23 vs 79.99) while the period stayed exactly 800 samples,
  so the lows absorb it — consistent with a threshold not centred on the swing.
  Only matters when measuring duty cycle near the resolution floor.

The general lesson is the one this project is built around: when a source and an
instrument disagree, the instrument is a suspect too.
