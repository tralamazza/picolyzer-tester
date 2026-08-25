# Design notes

The deep end. The README covers usage; this explains the choices behind it.

## Achieved, not requested

Every command replies with one `ok ...` or `err ...` line carrying the
parameters actually achieved:

```
> square 0 7M
ok mode=toggle mask=0x0001 req_hz=7000000 actual_hz=6999635.435 div=10+183/256
```

7 MHz is not exactly reachable through a 16.8 fixed-point divider, so it says
so. An instrument that silently rounds your request is the easiest way to make
a perfectly good analyzer look broken. The `div` field lets a host recompute
the rate and confirm the number came from the divider that was really
programmed rather than being an echo of the request; `tools/console.py` does
that. Out-of-range arguments are refused, never clamped.

## PIO generates the timing, not the CPU

Every edge on the parallel bus, UART TX and SPI comes out of a PIO state
machine clocked from the system clock, so console traffic and host timing
cannot perturb a waveform. **I2C is the one exception** — it is bit-banged from
the CPU, so its edge placement is approximate. That is fine for exercising a
decoder and useless for measuring one.

Once started, `toggle` and `count` need no FIFO service at all — each pulls a
single word to seed its mask or counter, then loops on two instructions
forever. They could have been patterns, but a pattern still has to flow through
the FIFO, and a loop short enough that the DMA chain cannot close in time will
drop samples. A dedicated two-instruction program has no loop to close and no
FIFO to starve, so the two highest-rate test signals stay exactly the cases
this device cannot itself get wrong.

Longer patterns are fed by two chained DMA channels, also without the CPU. One
streams words into the TX FIFO paced by the state machine's DREQ; when it
finishes, it chains to a second channel that rewrites the first's read address,
which reloads the count and retriggers it. The loop closes in hardware, so
arbitrary patterns run at the full 150 MSa/s. The DMA registers are written
directly against the PAC rather than through rp-hal's wrapper, whose
`double_buffer` re-arms from software and would reintroduce the very CPU
dependency being removed.

The trigger marker on GP16 shares PIO0 with the bus because `StateMachineGroup`
synchronised start only works between state machines of the same PIO block —
and a marker that is not cycle-aligned with the data would silently bias every
timing measurement taken relative to it.

## Rate arithmetic

The PIO clock divider is 16.8 fixed point with a minimum of 1.0, so almost no
requested frequency is exactly achievable. All timing math is integer
arithmetic in units of 1/256 of a divisor, rounding to nearest — truncation
would bias every frequency in one direction, which shows up as a systematic
error across a sweep.

The divider bottoms out at 65535 + 255/256, i.e. 2289 instructions/s at a
150 MHz system clock. Slower patterns repeat each sample in the buffer, which
the 4096-sample buffer bounds:

| Command | Slowest |
|---|---|
| `square`, `toggle`, `count` | 1145 Hz (divider only, no repetition) |
| `walk`, `walkz` (16 samples) | 9 Hz |
| `gray`, `ramp` (width 8, 256 samples) | 144 Hz |
| `play` | 2289 Sa/s |

Narrower patterns therefore reach lower, since they spend fewer of the 4096
samples per pass: `walk` at width 2 runs down to 2 Hz, and only there does the
divider itself finally become the binding constraint.

Anything slower is refused with `err rate below minimum divider rate` or
`err pattern too long`, never silently sped up — and which of the two you get
tells you whether to lower the width or give up on the rate.

The one caveat at the fast end is **short looping patterns at high rates**.
Closing the DMA loop costs a chain-and-retrigger round trip, and a loop short
enough that the reload cannot land before the FIFO empties drops samples.
Measured, by loop length against play rate:

| Loop | Highest clean rate |
|---|---|
| 4 samples | 50 MSa/s |
| 8 samples | 75 MSa/s |
| 16 samples | 125 MSa/s |
| 32 samples and up | 150 MSa/s — no stall at any rate |

The discriminator is *not* loop duration, which is the obvious guess and is
wrong: a 4-sample loop is clean at 80 ns per iteration while a 16-sample loop
stalls at 107 ns. Length and rate both matter and the mechanism behind the exact
boundary has not been pinned down, so the table is measurements rather than a
model — and the boundary sits somewhere between adjacent columns, not on them.

`txstall=yes` reports it every time, so a capture is never silently wrong. Two
ways around it: use `toggle`, which has no loop to close, or pad the pattern —
the same 4-sample waveform written out eight times to fill 32 samples runs clean
at the full 150 MSa/s.

## Pin choices

GP23, GP24, GP25 and GP29 are never touched: on a plain Pico 2 they are SMPS
mode, VBUS sense, the LED and VSYS sense; on a Pico 2 W the same four pins are
the CYW43439 wireless interface. In particular there is no heartbeat LED
(GP25 is the LED on a Pico 2 but the wireless chip select on a W), which is
what keeps one binary working on both boards. Everything the firmware does
drive — GP0..GP17, GP19..GP22 and GP26 — is on the 40-pin header of both
boards; GP18, GP27 and GP28 are there too but left free.

## No RTT, on purpose

`panic-halt` spins and there is no RTT logger to print to. USB is this device's
only interface and every command already returns a structured reply, so a
second channel that needs a probe buys little. It also avoids a foot-gun:
RTT's blocking mode is armed by the *host*, and the flag persists in RAM after
the host detaches, so ending a `probe-rs` session without resetting leaves log
writes able to spin once the 1 KiB buffer fills. That would stall the USB poll
loop, and USB is the only way to talk to the device.

## RP2350 erratum E9

E9 latches GPIO *inputs* that have pull-downs enabled. This firmware only
drives outputs and disables the input buffers, so it is unaffected — but an
RP2350-based analyzer under test may exhibit it. Worth knowing before blaming
the tester.
