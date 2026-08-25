# picolyzer-tester

A Raspberry Pi Pico 2 (or Pico 2 W) that turns into a **known-good signal
source for testing logic analyzers** — any of them, from a $10 8-channel clone
to a bench instrument.

It generates precisely timed waveforms and protocol traffic and reports
exactly what it emitted, over a USB serial console. Clip the analyzer under
test onto the pins, capture, compare: every discrepancy is either the
analyzer's fault or this device's, and the replies tell you which.

- **16-channel parallel bus** at up to 150 MSa/s, down to 9 Hz
- **UART, SPI and I2C** traffic to exercise protocol decoders
- **Structured replies** — every command returns `ok ...` or `err ...` with the
  parameters actually achieved, never silently rounded
- One firmware for both boards; nothing to install on the host

## Quick start

You need a Pico 2 or Pico 2 W, a USB cable, jumper wires (with **several
ground wires** — one per few channels), and the logic analyzer you want to
test.

1. Download `picolyzer-tester-v0.5.0.uf2` from the
   [latest release](https://github.com/tralamazza/picolyzer-tester/releases/latest).
2. Hold BOOTSEL while plugging in the Pico, drag the file onto the `RPI-RP2`
   drive. It reboots and appears as a USB serial port.
3. Open the port with any terminal — baud rate is irrelevant on a CDC link:

```sh
picocom /dev/cu.usbmodem00011           # interactive (macOS; /dev/ttyACM0 on Linux)
tools/console.py                        # or, from this repository: a 59-case self-check
tools/console.py "square 0 1M" "status" # one-shot commands
```

## First session

```
picolyzer-tester 0.5.0 - logic analyzer stimulus generator
`help` for commands
> id
ok fw=picolyzer-tester/0.5.0 sysclk=150000000 channels=16 tick_ps=6666 max_samples=4096 xtal_ppm=30
> glitch 0 1
ok mode=glitch ticks=1 width_ps=6666 samples=4 preloaded=yes
> status
ok mode=pattern-once samples=4 txstall=no dma=idle proto=idle
```

`glitch 0 1` puts a single 6.666 ns pulse on GP0. `preloaded=yes` means the
whole burst was in the FIFO before the clock started, so it ran with no refill
and cannot have dropped a sample: if your analyzer misses that pulse, the limit
is on its side. Walk `ticks` up until it catches it.

## What it can generate

The 16-channel bus: free-running square waves, a 16-bit counter, walking
ones/zeros, gray sweeps, binary ramps, pulse trains, cross-channel skew, narrow
glitches, and arbitrary loaded patterns.

Protocols for decoder testing: UART 8N1, SPI mode 0 (MSB first), I2C writes.

```
help                       command list
id                         firmware, clock, channel count, tick resolution
pins                       pin map
stop                       stop output, drive all channels low
status                     current mode, sample count, stall flag

square <ch> <hz>           square wave on one channel
toggle <mask|all> <hz>     square wave on a channel mask
count <hz>                 free-running 16-bit binary count

pulse <ch> <hi_ns> <period_ns>    pulse train
glitch <ch> <ticks>        one narrow pulse, ticks x 6.666 ns
skew <chA> <chB> <ticks>   two rising edges, ticks apart
walk <hz> [width]          walking ones
walkz <hz> [width]         walking zeros
gray <hz> [width]          gray code sweep, one bit changes per step
ramp <hz> [width]          binary count via the pattern engine
load <sample> ...          arbitrary 16-bit samples
play <hz> [loop]           play the loaded samples

uart <baud> <hex...>       8N1 frames
spi <hz> <hex...>          mode 0, MSB first
i2c <hz> <addr7> <hex...>
```

Numbers accept `42`, `0xff`, `0b1010_1010` — decimal unless prefixed, so
`load 10` loads ten and `load 0x10` loads sixteen. The one exception is the
payload of `uart`, `spi` and `i2c`, which is always hex with or without the
prefix: `uart 115200 48 65 6c 6c 6f` sends `Hello`.

Frequencies accept `115200`, `1M`, `2k5` (2500), `1M5` (1500000).

## Pin map and wiring

| Pins | Signal | Driven by |
|---|---|---|
| GP0..GP15 | channels 0..15, the 16-bit parallel bus | PIO0 SM0 |
| GP16 | trigger marker, pulsed at the start of every burst | PIO0 SM1 |
| GP17 | UART TX, 8N1 | PIO1 SM0 |
| GP19 / GP20 | SPI SCK / MOSI | PIO1 SM1 |
| GP21 | SPI chip select | CPU |
| GP22 / GP26 | I2C SCL / SDA | CPU (bit-banged) |

- **Use several grounds.** At 75 MHz, dupont jumpers with a single distant
  ground return ring badly enough to look like a capture fault. One ground per
  few channels, kept short.
- **Do not wire a real I2C slave.** SCL/SDA are push-pull, not open-drain; a
  slave driving SDA would be shorted against this output. The ninth clock of
  each byte is an unasserted ACK slot, so decoders show NAK — that is expected.
- GP23/24/25/29 are never touched: they are the CYW43439 wireless interface on
  a Pico 2 W (and GP25 is the LED on a plain Pico 2 — there is deliberately no
  heartbeat LED). One binary serves both boards.

The marker starts in the same clock cycle as the data, so it is safe to trigger
on. The synchronised group start is necessary but was not sufficient: it aligns
the two state machines' clocks, not the instruction at which each first drives a
pin. See the note under "What to trust".

## What to trust

Every edge on the bus, UART TX and SPI comes out of a PIO state machine
clocked from the 150 MHz system clock. The CPU only parses commands and polls
USB, so console traffic and host timing cannot perturb a waveform. How fast a
signal can go depends on which of three paths produces it:

| Path | Commands | Ceiling | Can it drop samples? |
|---|---|---|---|
| FIFO-free PIO loop | `square`, `toggle`, `count` | 75 MHz | No |
| Preloaded burst, ≤16 samples | `glitch`, `skew`, one-shot `play` | 150 MSa/s | No |
| Chained DMA | `walk`, `walkz`, `gray`, `ramp`, `pulse`, `play` | 150 MSa/s | Only short fast loops |

A missing sample with `txstall=no` is the analyzer's fault; with `txstall=yes`
it is this device's, and the capture should be thrown away. That distinction is
the point of the `status` field.

- Timing resolution is one system clock: 6.666 ns. Frequency accuracy is the
  board's 12 MHz crystal, roughly ±30 ppm — fine for validating an analyzer,
  not a frequency reference.
- Very short loops at high rates (a 4-sample loop above ~50 MSa/s) can drain
  the FIFO before the DMA loop closes; `txstall=yes` reports it. For a short,
  fast, repeating waveform use `toggle`.
- The slow end is not sub-hertz: `square`/`toggle`/`count` bottom out at
  1145 Hz, `walk` at 9 Hz, `gray`/`ramp` (width 8) at 144 Hz, `play` at
  2289 Sa/s. Anything slower is refused, never silently sped up.
- **I2C is bit-banged from the CPU**, so its edge placement is approximate.
  Fine for exercising a decoder, useless for measuring one. Its clock comes
  from a model of the delay loop fitted to bench measurements, not from a
  divider, so `actual_hz` is a prediction: measured within 1% of the report at
  10 k, 50 k, 100 k and 400 kHz, and the ceiling is 617 kHz rather than the
  1 MHz a PIO-driven bus would reach. That model is only valid for the machine
  code it was fitted to, which is why the toolchain is pinned and `just guard`
  fails if the codegen moves.

**Verification status:** 46 host unit tests and 59 hardware checks pass on a
Pico 2 W, covering every command, both ends of the rate range, the DMA
streaming path at full rate, and the error paths.

Measured against a logic analyzer at 200 MSa/s: frequency exact to the 75 MHz
ceiling, including fractional divisors; a one-tick 6.666 ns `glitch` caught
every time; `skew` accurate to within one sample at 1–75 ticks; UART, SPI and
I2C decoding byte-exact. No scope has touched a pin, so rise times and
crosstalk remain unmeasured.

The GP16 marker was found to rise one PIO cycle (6.666 ns) after the data, and
that lag is fixed as of v0.5.1 — an earlier statistical check had missed it.
After the fix the marker rises in the same sample as the data at 25 MSa/s,
measured in both engines: with `walk` at one cycle per sample and with `count`
at two, where the residual would have shown up as a half tick rather than a
whole one. That bounds the remaining offset to well under one 40 ns sample, but
it is not the same as proving it zero; a sub-cycle skew would need a faster
capture than anything used here.

## Testing recipes

A sensible order for validating an analyzer:

```
walk 100k 8         channel mapping (use 16 for all channels; for more than
                    16, probe the bus plus the GP16 marker as a 17th signal)
toggle all 75M      top speed; status must say txstall=no
glitch 0 1          minimum detectable pulse; raise ticks until caught
skew 0 1 3          cross-channel skew resolution
gray 100k 12        single-bit-transition sweep (2^width samples; 12 is the
                    widest that fits the buffer)
uart 115200 48 65 6c 6c 6f     decoder checks
spi 1M de ad be ef
i2c 100k 0x50 00 ff
```

A common 24 MSa/s clone cannot see a 6.666 ns glitch and should not be expected
to. Start at rates your analyzer claims to support, confirm those are clean,
then walk up until it breaks — that boundary is the useful number.

## Building from source

```sh
cargo build --release
cargo run --release    # flashes via picotool (board in BOOTSEL)
```

With [`just`](https://github.com/casey/just) installed, the rest of the
workflow is one command each:

```sh
just check     # fmt, clippy, host unit tests - no hardware needed
just guard     # check the I2C timing model still matches the generated code
just verify    # check, plus the 59 hardware checks over USB
just flash     # build and download over a debug probe
just uf2       # build and package picolyzer-tester-v<version>.uf2
just release minor   # flash + verify, bump, tag, push, reflash, draft the release
```

The toolchain is pinned in `rust-toolchain.toml`. That is not habit: the two
constants in `crates/tester-core/src/i2c_timing.rs` are fitted to the machine
code LLVM emits for the I2C delay loop, so they describe the compiler's output
rather than the chip. `just guard` hashes the instruction bytes of that one
function; if a toolchain or HAL bump reshapes it, the build fails and says to
re-measure SCL on an analyzer before `actual_hz` can be trusted again. Bumping
the pin is a deliberate act. CI also runs clippy on `stable` as a non-blocking
job, so new lints still surface.

Without `just`, the same steps by hand — note the explicit `--target`, since
`.cargo/config.toml` defaults every build to the RP2350:

```sh
cargo clippy --release --bins -- -D warnings
cargo test -p tester-core --target "$(rustc -vV | sed -n 's/^host: //p')"
probe-rs download --chip RP235x \
    target/thumbv8m.main-none-eabihf/release/picolyzer-tester
probe-rs reset --chip RP235x
```

probe-rs names this family `RP235x`; `RP2350` is not a known target.

Design rationale is in [docs/DESIGN.md](docs/DESIGN.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
