# picolyzer-tester

A Raspberry Pi Pico 2 used as a **known-good digital signal source for testing
logic analyzers** — any of them. It emits precisely timed waveforms and protocol
traffic and reports exactly what it emitted, over a USB serial console. Clip the
analyzer under test onto the pins and compare what it captured against what this
says it sent.

Nothing here is specific to a particular analyzer: it is a signal source with a
scriptable console, useful against a $10 8-channel clone, a Pico running
sigrok, or a bench instrument.

Runs unmodified on **Pico 2 and Pico 2 W**.

## Achieved, not requested

Every command replies with one `ok ...` or `err ...` line carrying the
parameters actually achieved:

```
> square 0 7M
ok mode=toggle mask=0x0001 req_hz=7000000 actual_hz=6999635.435 div=10+183/256
```

7 MHz is not exactly reachable through a 16.8 fixed-point divider, so it says
so. An instrument that silently rounds your request is the easiest way to make a
perfectly good analyzer look broken. The `div` field lets a host recompute the
rate and confirm the number came from the divider that was really programmed
rather than being an echo of the request; `tools/console.py` does that.

Out-of-range arguments are refused, never clamped.

## PIO generates the timing, not the CPU

Every edge on the parallel bus, UART TX and SPI comes out of a PIO state machine
clocked from the system clock, so console traffic and host timing cannot perturb
a waveform. **I2C is the one exception** — it is bit-banged from the CPU, so its
edge placement is approximate. That is fine for exercising a decoder and useless
for measuring one; use the bus for anything timing-critical.

Once started, `toggle` and `count` need no FIFO service at all — each pulls a
single word to seed its mask or counter, then loops on two instructions forever.
So the cases most likely to expose an analyzer's limits are the ones this device
cannot itself get wrong.

Longer patterns are fed by two chained DMA channels, also without the CPU. One
streams words into the TX FIFO paced by the state machine's DREQ; when it
finishes, it chains to a second channel that rewrites the first's read address,
which reloads the count and retriggers it. The loop closes in hardware, so
arbitrary patterns run at the full 150 MSa/s.

Whatever the path, the hardware stall flag is reported:

```
> status
ok mode=pattern-loop samples=256 txstall=no dma=busy proto=idle
```

A missing sample with `txstall=no` is the analyzer's fault; with `txstall=yes`
it is this device's, and the capture should be thrown away. That distinction is
the point of reporting it.

## Pin map

| Pins | Signal | Driven by |
|---|---|---|
| GP0..GP15 | channels 0..15, the 16-bit parallel bus | PIO0 SM0 |
| GP16 | trigger marker, pulsed at the start of every burst | PIO0 SM1 |
| GP17 | UART TX, 8N1 | PIO1 SM0 |
| GP19 / GP20 | SPI SCK / MOSI | PIO1 SM1 |
| GP21 | SPI chip select | CPU |
| GP22 / GP26 | I2C SCL / SDA | CPU (bit-banged) |

The marker starts in the same clock cycle as the data, via PIO's synchronised
group start. A marker not cycle-aligned with the data would silently bias every
timing measurement taken relative to it.

**GP23, GP24, GP25 and GP29 are never touched** — they are the CYW43439
wireless interface on a Pico 2 W. That is also why there is no heartbeat LED:
GP25 is the LED on a Pico 2 but the wireless chip select on a W. One binary
serves both boards.

## Capabilities

At the stock 150 MHz system clock:

| | |
|---|---|
| Timing resolution | 6.667 ns (one system clock) |
| Narrowest pulse | 6.667 ns |
| Pattern buffer | 4096 samples |

The fast end depends entirely on **how** a signal is produced, and the three
paths are not interchangeable:

| Path | Commands | Ceiling | Can it drop samples? |
|---|---|---|---|
| FIFO-free PIO loop | `square`, `toggle`, `count` | 75 MHz | No — nothing to starve |
| Preloaded burst, ≤16 samples | `glitch`, `skew`, short `play` | 150 MSa/s | No — loaded before the clock starts |
| Chained DMA | `walk`, `walkz`, `gray`, `ramp`, `play` | 150 MSa/s | Only for very short loops, below |

The CPU is not in any of these paths, which is what makes the device
trustworthy at the top of its range.

The one caveat is **short looping patterns at high rates**. Closing the DMA loop
costs a chain-and-retrigger round trip, so a loop whose iteration is shorter
than roughly 200 ns drains the FIFO before the reload lands. Measured: at
150 MSa/s a 32-sample loop is clean and a 16-sample one is not; a 4-sample loop
is clean to 50 MSa/s and not at 75. `txstall=yes` reports it every time. For a
short, fast, repeating waveform use `toggle`, which has no loop to close.

The slow end is not sub-hertz — the 16.8 divider bottoms out at 2289
instructions/s, and patterns go slower only by repeating each sample in the
buffer, which the 4096-sample buffer bounds:

| Command | Slowest |
|---|---|
| `square`, `toggle`, `count` | 1145 Hz (divider only, no repetition) |
| `walk`, `walkz` (16 samples) | 9 Hz |
| `gray`, `ramp` (width 8, 256 samples) | 144 Hz |
| `play` | 2289 Sa/s |

Anything slower is refused with `err rate below minimum divider rate` or
`err pattern too long`, never silently sped up. Wider sweeps hit the buffer
sooner: a full 16-bit `gray` is 65536 samples and does not fit at all.

Frequency accuracy is bounded by the board's 12 MHz crystal, roughly ±30 ppm.
Fine for validating an analyzer; this is not a frequency reference.

## Matching the analyzer you have

Channel count is the one thing that really varies. The bus is 16 channels on
GP0..GP15, and the sweep commands take a `width` so patterns fit whatever is in
front of you:

```
walk 100k 8       walking ones across GP0..GP7 only, for an 8-channel analyzer
walk 100k 16      all 16 channels, the channel-mapping test
toggle 0xff 1M    square wave on the low 8 channels together
gray 100k 12      widest gray sweep that fits the buffer (4096 samples)
```

`gray` and `ramp` cost 2^width samples, so 12 is the ceiling; `walk` costs one
sample per channel and goes to 16.

For an analyzer with more than 16 channels, probe the 16 and use the marker on
GP16 as a 17th known signal. For one with fewer, `width` keeps the unused
channels quiet rather than leaving them switching.

Sample rate matters too: a common 24 MSa/s clone cannot see a 6.7 ns glitch and
should not be expected to. Start at rates it claims to support, confirm those
are clean, then walk up until it breaks — that boundary is the useful number.

## Commands

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
glitch <ch> <ticks>        one narrow pulse, ticks x 6.67 ns
skew <chA> <chB> <ticks>   two rising edges, ticks apart
walk <hz> [width]          walking ones
walkz <hz> [width]         walking zeros
gray <hz> [width]          gray code sweep, one bit changes per step
ramp <hz> [width]          binary count via the pattern engine
load <hex> <hex> ...       arbitrary 16-bit samples
play <hz> [loop]           play the loaded samples

uart <baud> <hex...>       8N1 frames
spi <hz> <hex...>          mode 0, MSB first
i2c <hz> <addr7> <hex...>
```

Numbers accept `42`, `0xff`, `0b1010_1010`. Frequencies accept `115200`, `1M`,
`2k5` (2500), `1M5` (1500000).

## Build and flash

```sh
cargo fmt --all -- --check
cargo clippy --release --bins -- -D warnings
cargo test -p tester-core --target aarch64-apple-darwin
cargo build --release
```

probe-rs names the family `RP235x`; `RP2350` is not a known target and fails
with an unhelpful "could not determine chip":

```sh
probe-rs download --chip RP235x \
    target/thumbv8m.main-none-eabihf/release/picolyzer-tester
probe-rs reset --chip RP235x
```

Or `cargo run --release` to flash via picotool with the board in BOOTSEL. Then
talk to it — baud rate is irrelevant on a CDC link:

```sh
tools/console.py                        # hardware self-check, 55 cases
tools/console.py "square 0 1M" "status" # one-shot commands
picocom /dev/cu.usbmodem00011           # or interactively
```

## Status

Verified on a Pico 2 W over a Raspberry Pi Debug Probe: 37 host unit tests and
55 hardware checks pass, covering every command, the achieved-rate arithmetic
cross-checked against the reported divider, both ends of the rate range, the DMA
streaming path at full rate, every example in this README, and the error paths.

**The electrical side is unverified.** No scope or analyzer has touched a pin,
so edge placement, rise times, crosstalk, and the cycle-alignment of the trigger
marker are all unmeasured. Everything confirmed so far is the command surface
and the arithmetic behind it.

## Bench notes

- **Use several grounds.** At 75 MHz, dupont jumpers with a single distant
  ground return ring badly enough to look like a capture fault. One ground per
  few channels, kept short.
- **Do not wire a real I2C slave.** SCL and SDA are push-pull, not open-drain,
  because this device is the only driver on the bus; a slave driving SDA would
  be shorted against this output. The ninth clock of each byte is an unasserted
  ACK slot, so decoders show NAK — that is expected.
- **RP2350 erratum E9** latches GPIO *inputs* that have pull-downs enabled. This
  firmware only drives outputs and disables the input buffers, so it is
  unaffected — but an RP2350-based analyzer under test may exhibit it. Worth
  knowing before blaming the tester.
- **Panics halt silently.** `panic-halt` spins and there is no RTT logger to
  print to. Dropping defmt/RTT was deliberate: USB is this device's only
  interface and every command already returns a structured reply, so a second
  channel that needs a probe buys little. It also avoids a foot-gun — RTT's
  blocking mode is armed by the *host*, and the flag persists in RAM after the
  host detaches, so ending a `probe-rs` session without resetting leaves log
  writes able to spin once the 1 KiB buffer fills. That would stall the USB
  poll loop, and USB is the only way to talk to the device.

## Worked example

Find the analyzer's true minimum detectable pulse width:

```
> glitch 0 1
ok mode=glitch ticks=1 width_ps=6666 samples=4 preloaded=yes
```

`preloaded=yes` means the whole burst was in the FIFO before the clock started,
so it ran with no refill and cannot have stalled. If the analyzer misses this
pulse, that is a real limit on its side. Walk `ticks` up until it catches it.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
