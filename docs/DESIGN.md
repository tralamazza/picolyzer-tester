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

Anything slower is refused with `err rate below minimum divider rate` or
`err pattern too long`, never silently sped up.

The one caveat at the fast end is **short looping patterns at high rates**.
Closing the DMA loop costs a chain-and-retrigger round trip, so a loop whose
iteration is shorter than roughly 200 ns drains the FIFO before the reload
lands. Measured: at 150 MSa/s a 32-sample loop is clean and a 16-sample one is
not; a 4-sample loop is clean to 50 MSa/s and not at 75. `txstall=yes` reports
it every time. For a short, fast, repeating waveform use `toggle`, which has
no loop to close.

## Pin choices

GP23, GP24, GP25 and GP29 are never touched: on a plain Pico 2 they are SMPS
mode, VBUS sense, the LED and VSYS sense; on a Pico 2 W the same four pins are
the CYW43439 wireless interface. In particular there is no heartbeat LED
(GP25 is the LED on a Pico 2 but the wireless chip select on a W), which is
what keeps one binary working on both boards. Everything the firmware does use
— GP0..GP22 and GP26..GP28 — is on the 40-pin header of both boards.

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
