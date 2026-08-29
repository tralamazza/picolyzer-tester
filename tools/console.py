#!/usr/bin/env python3
"""Drive the picolyzer-tester USB console from the host.

Deliberately dependency-free: it talks to the CDC device with plain file I/O so
it runs on a stock Python with no pyserial. Baud settings are irrelevant on a
CDC ACM link, so nothing here configures one.

Usage:
    tools/console.py                       # run the built-in self-check
    tools/console.py "square 0 1M" "status"
    tools/console.py --port /dev/cu.usbmodemXXXX "id"
    tools/console.py --help [command]      # per-command help, with examples
"""

import argparse
import glob
import os
import sys
import termios
import time

# The debug probe also enumerates a CDC port; ours is the one whose serial
# number is 0001, which macOS renders as usbmodem00011.
DEFAULT_GLOBS = ["/dev/cu.usbmodem00011", "/dev/cu.usbmodem*"]


def find_port(explicit=None, wait_s=10.0):
    """Locate the board's CDC port, waiting for it to enumerate.

    Resetting the board over a debug probe drops the USB device for a second or
    two, so flashing and then immediately self-checking races the
    re-enumeration. Waiting matters more than it looks: the debug probe presents
    a CDC port of its own, and during that window the fallback glob matches the
    probe instead of the board, which fails every check with a wall of timeouts
    rather than an honest "not found".
    """
    if explicit:
        return explicit

    exact, fallback = DEFAULT_GLOBS[0], DEFAULT_GLOBS[1:]
    # A short grace period when some other CDC device is already present, the
    # full wait when nothing is: a board named differently on another host
    # should not pay ten seconds on every run.
    deadline = time.time() + wait_s
    grace = None
    while True:
        if matches := sorted(glob.glob(exact)):
            return matches[0]
        others = sorted(m for p in fallback for m in glob.glob(p))
        if others:
            grace = time.time() + 1.5 if grace is None else grace
            if time.time() >= grace:
                return others[0]
        elif time.time() >= deadline:
            raise SystemExit("no USB serial device found; is the board plugged in?")
        time.sleep(0.1)


class Console:
    def __init__(self, port, timeout=2.0):
        self.fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        self.timeout = timeout
        # Raw mode: no echo, no line discipline, no CR/LF translation. Without
        # this the tty layer rewrites the bytes and replies come back mangled.
        attrs = termios.tcgetattr(self.fd)
        attrs[0] = 0  # iflag
        attrs[1] = 0  # oflag
        attrs[3] = 0  # lflag
        attrs[6][termios.VMIN] = 0
        attrs[6][termios.VTIME] = 0
        termios.tcsetattr(self.fd, termios.TCSANOW, attrs)
        self.buf = b""
        time.sleep(0.2)
        self.drain()

    def drain(self):
        """Discard anything already queued, e.g. the power-on banner."""
        deadline = time.time() + 0.3
        while time.time() < deadline:
            try:
                if not os.read(self.fd, 4096):
                    time.sleep(0.01)
            except BlockingIOError:
                time.sleep(0.01)
        self.buf = b""

    def send(self, line):
        os.write(self.fd, line.encode() + b"\n")

    def read_lines(self, until_ok=True):
        """Collect output until an ok/err line arrives or the timeout expires."""
        out = []
        deadline = time.time() + self.timeout
        while time.time() < deadline:
            try:
                chunk = os.read(self.fd, 4096)
            except BlockingIOError:
                chunk = b""
            if not chunk:
                time.sleep(0.005)
                continue
            self.buf += chunk
            while b"\n" in self.buf:
                raw, self.buf = self.buf.split(b"\n", 1)
                text = raw.decode("ascii", "replace").strip()
                if text:
                    out.append(text)
                    if until_ok and (text.startswith("ok") or text.startswith("err")):
                        return out
        return out

    def cmd(self, line):
        self.send(line)
        return self.read_lines()

    def close(self):
        os.close(self.fd)


SYSCLK_HZ = 150_000_000


def fields(reply):
    """Parse the `key=value` pairs out of a reply line."""
    out = {}
    for token in reply.split():
        if "=" in token:
            k, v = token.split("=", 1)
            out[k] = v
    return out


def within_ppm(reply, key, target, ppm):
    """Whether a reported rate is within `ppm` of `target`."""
    f = fields(reply)
    if key not in f:
        return False
    actual = float(f[key])
    return abs(actual - target) <= target * ppm / 1e6


def rate_matches_divisor(reply, key, cycles_per_period):
    """Recompute the rate from the reported divisor and check it agrees.

    This is the check that actually matters: it proves the number the device
    prints is derived from the divisor it programmed, rather than being an echo
    of the request dressed up as a measurement.
    """
    f = fields(reply)
    if key not in f or "div" not in f:
        return False
    whole, frac = f["div"].split("+")
    x256 = int(whole) * 256 + int(frac.split("/")[0])
    expected = SYSCLK_HZ * 256 * 1000 // x256 // cycles_per_period
    reported = round(float(f[key]) * 1000)
    # Allow a hair of slack for the integer division order.
    return abs(expected - reported) <= 2


# (command, predicate on the final reply line, description)
CHECKS = [
    ("id", lambda r: r.startswith("ok") and "sysclk=150000000" in r, "identity and clock"),
    ("help", lambda r: r == "ok", "help text prints"),
    ("pins", lambda r: r == "ok", "pin map prints"),
    ("stop", lambda r: r.startswith("ok"), "stop is accepted"),
    ("status", lambda r: "mode=stopped" in r and "txstall=no" in r, "idle status"),
    # The headline claim: 7 MHz is unreachable, so it must not claim 7 MHz. The
    # expected value is recomputed from the divisor the device reports rather
    # than hardcoded, so this checks the device's own arithmetic instead of my
    # memory of it.
    ("square 0 7M", lambda r: rate_matches_divisor(r, "actual_hz", cycles_per_period=2)
     and "req_hz=7000000" in r and "actual_hz=7000000.000" not in r,
     "reports achieved rate, not requested"),
    ("square 0 1M", lambda r: "actual_hz=1000000.000" in r, "exact rate is exact"),
    ("toggle all 75M", lambda r: "actual_hz=75000000.000" in r, "top speed, all channels"),
    ("count 1M", lambda r: r.startswith("ok") and "codes=65536" in r, "16-bit counter"),
    ("glitch 0 1", lambda r: "width_ps=6666" in r and "preloaded=yes" in r,
     "one-tick glitch runs from the FIFO"),
    ("skew 0 1 3", lambda r: r.startswith("ok") and "ticks=3" in r, "cross-channel skew"),
    ("pulse 0 100 1000", lambda r: r.startswith("ok") and "samples=150" in r, "pulse train"),
    ("walk 100k", lambda r: r.startswith("ok") and "width=16" in r, "walking ones"),
    ("walkz 100k", lambda r: r.startswith("ok") and "width=16" in r and "samples=16" in r,
     "walking zeros"),
    ("gray 100k", lambda r: r.startswith("ok") and "samples=256" in r, "gray sweep"),
    ("ramp 100k", lambda r: r.startswith("ok"), "binary ramp"),
    ("load 0x0001 0x0002 0x0004", lambda r: "loaded=3" in r and "padded_to=4" in r,
     "odd sample count is padded"),
    ("play 1M", lambda r: r.startswith("ok") and "samples=4" in r, "play loaded pattern"),
    # Regression: `stop` used to clear the pattern buffer, and `play_pattern`
    # stops before it starts - so playing a pattern erased it and a second
    # `play` reported "no pattern loaded".
    ("status", lambda r: "samples=4" in r, "pattern survives being played"),
    ("play 1M", lambda r: r.startswith("ok") and "samples=4" in r, "play is repeatable"),
    ("play 150M", lambda r: "actual_sps=150000000.000" in r,
     "short burst runs at the full 150 MSa/s"),

    # Regression: `service` used to loop until the TX FIFO reported full. Above
    # a few MSa/s the state machine drains it faster than the CPU fills it, so
    # that condition never came true, the USB poll never ran again, and the
    # board hung until power-cycled. Reaching the reply after this command is
    # the whole assertion.
    ("gray 150M", lambda r: r.startswith("ok"), "full-rate stream does not hang the device"),
    ("id", lambda r: r.startswith("ok"), "console still responds afterwards"),

    # Chained DMA replaced CPU refill, lifting streaming from ~100 kSa/s to the
    # full sample rate. A stall anywhere here means the DMA loop regressed.
    ("gray 200k", lambda r: r.startswith("ok"), "200 kSa/s stream"),
    ("status", lambda r: "txstall=no" in r, "200 kSa/s is clean (was the old ceiling)"),
    ("gray 150M", lambda r: r.startswith("ok"), "full-rate stream"),
    ("status", lambda r: "txstall=no" in r and "dma=busy" in r,
     "150 MSa/s streams with no dropped samples"),
    ("walk 9", lambda r: "samples=4080" in r, "longest pattern, slowest rate"),
    ("status", lambda r: "txstall=no" in r, "4080-sample loop is clean"),

    # The documented short-loop caveat: closing the DMA loop costs a
    # chain-and-retrigger, so a loop iteration under ~200 ns cannot keep up.
    ("load 0x0 0x1 0x2 0x3", lambda r: "loaded=4" in r, "4-sample pattern"),
    ("play 50M loop", lambda r: r.startswith("ok"), "4-sample loop at 50 MSa/s"),
    ("status", lambda r: "txstall=no" in r, "short loop is fine at 50 MSa/s"),
    ("play 150M loop", lambda r: r.startswith("ok"), "4-sample loop at 150 MSa/s"),
    ("status", lambda r: "txstall=yes" in r,
     "short loop cannot close in time at 150 MSa/s, and says so"),

    # Preloaded bursts and FIFO-free loops must never report a stall, at any
    # rate - that is the property the whole design leans on.
    ("glitch 0 1", lambda r: "preloaded=yes" in r, "full-rate glitch"),
    ("status", lambda r: "txstall=no" in r, "preloaded burst cannot stall"),
    ("toggle all 75M", lambda r: r.startswith("ok"), "75 MHz on all channels"),
    ("status", lambda r: "txstall=no" in r, "FIFO-free loop cannot stall"),

    # Slow-end boundaries, exactly as documented in the README table. Each pair
    # is the lowest accepted rate and the highest refused one.
    ("play 2288", lambda r: r.startswith("err"), "play floor: 2288 refused"),
    ("square 0 1145", lambda r: r.startswith("ok"), "square floor: 1145 accepted"),
    ("square 0 1144", lambda r: r.startswith("err"), "square floor: 1144 refused"),
    ("count 1144", lambda r: r.startswith("err"), "count floor: 1144 refused"),
    ("walk 9", lambda r: r.startswith("ok") and "repeat=255" in r,
     "walk floor: 9 Hz accepted"),
    ("walk 8", lambda r: r.startswith("err") and "capacity=4096" in r,
     "walk floor: 8 Hz refused, buffer-bound"),
    ("gray 144", lambda r: r.startswith("ok") and "samples=4096" in r,
     "gray floor: 144 Hz accepted"),
    ("gray 143", lambda r: r.startswith("err"), "gray floor: 143 Hz refused"),

    # Width handling, as documented under "Matching the analyzer you have".
    ("walk 100k 8", lambda r: "width=8" in r and "samples=8" in r,
     "8-channel walk for a narrower analyzer"),
    ("walk 100k 16", lambda r: "width=16" in r and "samples=16" in r, "16-channel walk"),
    ("gray 100k 12", lambda r: "samples=4096" in r, "widest gray sweep that fits"),
    ("gray 100k 13", lambda r: r.startswith("err"), "13-bit gray does not fit, and says so"),
    ("toggle 0xff 1M", lambda r: "mask=0x00ff" in r, "square wave on the low 8 channels"),
    ("uart 115200 48 65 6c 6c 6f",
     lambda r: rate_matches_divisor(r, "actual_baud", cycles_per_period=8)
     and within_ppm(r, "actual_baud", 115200, 100) and "bytes=5" in r,
     "UART frames, baud within 100 ppm of request"),
    ("spi 1M de ad be ef",
     lambda r: rate_matches_divisor(r, "actual_hz", cycles_per_period=4)
     and "actual_hz=1000000.000" in r and "bytes=4" in r,
     "SPI frames"),
    ("i2c 100k 0x50 00 ff", lambda r: "addr=0x50" in r and "bytes=2" in r, "I2C transaction"),
    # Error paths must reject rather than coerce.
    ("square 16 1M", lambda r: r.startswith("err"), "channel bounds are enforced"),
    ("square 0 200M", lambda r: r.startswith("err"), "rate above sysclk is refused"),
    ("square 0 abc", lambda r: r.startswith("err"), "bad frequency is refused"),
    ("nonsense", lambda r: r.startswith("err"), "unknown command is refused"),
    ("stop", lambda r: r.startswith("ok"), "final stop"),
]


# Command reference, printed by `--help`: name, usage, description,
# (command, reply) example pairs, and notes. The replies are real captures
# from v0.5.1, so the examples show exactly what the board prints.
COMMANDS = [
    (
        "help", "",
        "print the command list from the device itself",
        [("help",
          "commands:\n"
          "help                      this text\n"
          "id                        firmware, clock and channel count\n"
          "pins                      pin map\n"
          "stop                      stop output, drive all channels low\n"
          "status                    current mode and stall flag\n"
          "square <ch> <hz>          square wave on one channel\n"
          "toggle <mask|all> <hz>    square wave on a channel mask\n"
          "count <hz>                free-running 16-bit binary count\n"
          "pulse <ch> <hi_ns> <period_ns>\n"
          "glitch <ch> <ticks>       one narrow pulse, ticks x 6.666ns\n"
          "skew <chA> <chB> <ticks>  two rising edges, ticks apart\n"
          "walk <hz> [width]         walking ones\n"
          "walkz <hz> [width]        walking zeros\n"
          "gray <hz> [width]         gray code sweep\n"
          "ramp <hz> [width]         binary count via the pattern engine\n"
          "load <hex> <hex> ...      arbitrary 16-bit samples\n"
          "play <hz> [loop]          play the loaded samples\n"
          "uart <baud> <hex...>      8N1 frames on GP17\n"
          "spi <hz> <hex...>         mode 0, MSB first, GP19/20/21\n"
          "i2c <hz> <addr7> <hex...> bit-banged, GP22/GP26\n"
          "examples:\n"
          "square 0 1M                 1 MHz square wave on channel 0\n"
          "toggle all 75M              75 MHz on all 16 channels\n"
          "count 1M                    16-bit counter at 1 MHz\n"
          "glitch 0 1                  one 6.666 ns pulse on channel 0\n"
          "skew 0 1 3                  rising edges ~20 ns apart on GP0/GP1\n"
          "pulse 0 100 1000            100 ns high, 1000 ns period, channel 0\n"
          "walk 100k 8                 walking ones on the low 8 channels\n"
          "gray 100k 12                widest gray sweep that fits the buffer\n"
          "load 0x0001 0x0002 0x0004   load three 16-bit samples\n"
          "play 1M loop                play them looping at 1 MSa/s\n"
          "uart 115200 48 65 6c 6c 6f  sends \"Hello\" at 115200 baud\n"
          "spi 1M de ad be ef          4 bytes at 1 MHz\n"
          "i2c 100k 0x50 00 ff         write 00 ff to address 0x50\n"
          "full reference with example replies: tools/console.py --help\n"
          "numbers: 42  0xff  0b1010_1010   frequencies: 115200  1M  2k5\n"
          "ok")],
        "multi-line output, closed by a bare `ok`",
    ),
    (
        "id", "",
        "firmware version, system clock, channel count, tick resolution",
        [("id",
          "ok fw=picolyzer-tester/0.5.1 sysclk=150000000 channels=16 "
          "tick_ps=6666 max_samples=4096 xtal_ppm=30")],
        "tick_ps is the timing resolution: a 150 MHz clock is 6.666 ns per tick",
    ),
    (
        "pins", "",
        "pin map: which GPIO each signal comes out on",
        [("pins",
          "pin map (identical on Pico 2 and Pico 2 W):\n"
          "GP0..GP15  channels 0..15, the 16-bit bus\n"
          "GP16       trigger marker, pulsed at the start of every burst\n"
          "GP17       UART TX, 8N1, PIO-clocked\n"
          "GP19/20/21 SPI SCK/MOSI (PIO-clocked) and CS (CPU-driven)\n"
          "GP22, GP26 I2C SCL/SDA, CPU bit-banged, push-pull - no slaves!\n"
          "GND        use several - one ground per few channels\n"
          "GP23/24/25/29 are untouched: they are the wireless interface on a Pico 2 W.\n"
          "ok")],
        "multi-line output, closed by a bare `ok`",
    ),
    (
        "stop", "",
        "stop output, drive all channels low",
        [("stop", "ok")],
        "does not clear the pattern buffer: `status` still reports the last sample count",
    ),
    (
        "status", "",
        "current mode, sample count, stall flag, DMA and protocol state",
        [("status", "ok mode=stopped samples=4 txstall=no dma=idle proto=idle")],
        "txstall=yes means the device dropped samples in the last run - discard that "
        "capture. `play` reports ok first; the stall shows up here, on the next status",
    ),
    (
        "square", "<ch> <hz>",
        "square wave on one channel",
        [
            ("square 0 1M",
             "ok mode=toggle mask=0x0001 req_hz=1000000 actual_hz=1000000.000 "
             "div=75+0/256"),
            ("square 0 7M",
             "ok mode=toggle mask=0x0001 req_hz=7000000 actual_hz=6999635.435 "
             "div=10+183/256"),
        ],
        "reports the achieved rate, never the request. 1145 Hz to 75 MHz; below that "
        "`err rate below minimum divider rate`, above `err rate above sysclk`",
    ),
    (
        "toggle", "<mask|all> <hz>",
        "square wave on a channel mask; `all` is every channel",
        [
            ("toggle all 75M",
             "ok mode=toggle mask=0xffff req_hz=75000000 actual_hz=75000000.000 "
             "div=1+0/256"),
            ("toggle 0xff 1M",
             "ok mode=toggle mask=0x00ff req_hz=1000000 actual_hz=1000000.000 "
             "div=75+0/256"),
        ],
        "a FIFO-free PIO loop, so it cannot drop samples. `toggle 0 1M` -> "
        "`err mask selects no channels`. 1145 Hz to 75 MHz",
    ),
    (
        "count", "<hz>",
        "free-running 16-bit binary count on all channels, wrapping at 65536",
        [("count 1M",
          "ok mode=count codes=65536 req_hz=1000000 actual_hz=1000000.000 "
          "div=75+0/256")],
        "same rate bounds as `square`",
    ),
    (
        "pulse", "<ch> <hi_ns> <period_ns>",
        "repeating pulse train on one channel, durations in nanoseconds",
        [("pulse 0 100 1000",
          "ok mode=pulse high_ps=99990 period_ps=999900 samples=150")],
        "durations snap to the 6.666 ns tick: 100 ns becomes high_ps=99990",
    ),
    (
        "glitch", "<ch> <ticks>",
        "one narrow pulse, ticks x 6.666 ns",
        [("glitch 0 1",
          "ok mode=glitch ticks=1 width_ps=6666 samples=4 preloaded=yes")],
        "preloaded=yes: the whole burst was in the FIFO before the clock started, so it "
        "cannot have dropped a sample. If the analyzer misses it, the limit is on its side",
    ),
    (
        "skew", "<chA> <chB> <ticks>",
        "two rising edges, ticks apart, on two different channels",
        [("skew 0 1 3", "ok mode=skew ticks=3 width_ps=19998 samples=8 preloaded=yes")],
        "same channel twice -> `err skew needs two different channels`",
    ),
    (
        "walk", "<hz> [width]",
        "walking ones: one high bit sweeping across the bus",
        [("walk 100k",
          "ok mode=walk width=16 req_hz=100000 actual_hz=100000.000 repeat=1 "
          "samples=16")],
        "width defaults to 16; `walk 100k 8` for a narrower analyzer. Reaches down to "
        "9 Hz across all 16 channels (repeat=255, samples=4080); slower is refused as "
        "`err pattern too long` - the buffer, not the clock, is the bound",
    ),
    (
        "walkz", "<hz> [width]",
        "walking zeros: the same sweep, inverted",
        [("walkz 100k",
          "ok mode=walkz width=16 req_hz=100000 actual_hz=100000.000 repeat=1 "
          "samples=16")],
        "same bounds as `walk`",
    ),
    (
        "gray", "<hz> [width]",
        "gray-code sweep: exactly one bit changes per step",
        [
            ("gray 100k",
             "ok mode=gray width=8 req_hz=100000 actual_hz=100000.000 repeat=1 "
             "samples=256"),
            ("gray 100k 12",
             "ok mode=gray width=12 req_hz=100000 actual_hz=100000.000 repeat=1 "
             "samples=4096"),
        ],
        "width defaults to 8: a 16-bit sweep is 65536 samples and does not fit. 12 is "
        "the widest that fits the 4096-sample buffer; 13 -> `err pattern too long "
        "needed=4097 capacity=4096`. Lowest rate 144 Hz",
    ),
    (
        "ramp", "<hz> [width]",
        "binary count via the pattern engine",
        [("ramp 100k",
          "ok mode=ramp width=8 req_hz=100000 actual_hz=100000.000 repeat=1 "
          "samples=256")],
        "same buffer limits as `gray`",
    ),
    (
        "load", "<sample> [sample ...]",
        "load arbitrary 16-bit samples into the pattern buffer",
        [("load 0x0001 0x0002 0x0004", "ok loaded=3 padded_to=4")],
        "samples are 16-bit integers: hex, decimal or binary. Odd counts are padded to "
        "even by repeating the last sample. A new `load` replaces the previous one",
    ),
    (
        "play", "<hz> [loop]",
        "play the loaded samples, once or looping",
        [
            ("play 1M",
             "ok mode=play req_hz=1000000 actual_sps=1000000.000 samples=4 loop=false"),
            ("play 150M loop",
             "ok mode=play req_hz=150000000 actual_sps=150000000.000 samples=4 "
             "loop=true"),
        ],
        "needs `load` first: on a fresh boot, `err no pattern loaded, use `load` "
        "first`. The rate is actual_sps, samples per second, from 2289 Sa/s up; below "
        "that `err rate below minimum divider rate`. A short loop above ~50 MSa/s may "
        "drop samples - the next `status` then reports txstall=yes",
    ),
    (
        "uart", "<baud> <hex> [hex ...]",
        "UART 8N1 frames on GP17; payload bytes are always hex",
        [("uart 115200 48 65 6c 6c 6f",
          "ok mode=uart format=8N1 req_baud=115200 actual_baud=115199.078 "
          "div=162+195/256 bytes=5")],
        "sends `Hello`. Up to 32 bytes per command",
    ),
    (
        "spi", "<hz> <hex> [hex ...]",
        "SPI mode 0, MSB first, on GP19/20/21; payload bytes are always hex",
        [("spi 1M de ad be ef",
          "ok mode=spi spi_mode=0 bit_order=msb req_hz=1000000 "
          "actual_hz=1000000.000 div=37+128/256 bytes=4")],
        "up to 32 bytes per command",
    ),
    (
        "i2c", "<hz> <addr7> <hex> [hex ...]",
        "I2C write on GP22/GP26, bit-banged by the CPU",
        [("i2c 100k 0x50 00 ff",
          "ok mode=i2c addr=0x50 rw=write req_hz=100000 actual_hz=99601.593 "
          "bytes=2 ack=none-no-slave")],
        "push-pull, not open-drain: never wire a real I2C slave. Edge timing is "
        "approximate - actual_hz is a model, not a divider readback - so this exercises "
        "decoders but is not a timing reference. ack=none-no-slave is expected",
    ),
]

NUMBERS = """
Numbers and rates

Integers are decimal unless prefixed: 42, 0xff, 0b1010_1010, 1_000_000.
`load 10` loads ten; `load 0x10` loads sixteen.

Payload bytes of uart, spi and i2c are always hex, with or without the 0x
prefix: `uart 115200 48 65 6c 6c 6f` sends "Hello".

Frequencies take k/M suffixes: 115200, 1M, 2k5 (2500), 1M5 (1500000).
"""


def print_reference(names):
    """Print the offline command reference, filtered by `names` if given."""
    known = {name for name, *_ in COMMANDS}
    unknown = [n for n in names if n not in known and n != "numbers"]
    if unknown:
        for n in unknown:
            print(f"unknown command: {n}", file=sys.stderr)
        print("run `--help` alone to list every command", file=sys.stderr)
        return 2
    if not names or names == ["numbers"]:
        print(NUMBERS.strip())
        print()
    for name, usage, description, examples, notes in COMMANDS:
        if names and name not in names:
            continue
        print(f"{name} {usage}".strip())
        print(f"    {description}")
        for cmd, reply in examples:
            print(f'    tools/console.py "{cmd}"')
            for line in reply.splitlines():
                print(f"      {line}")
        if notes:
            print(f"    {notes}")
        print()
    return 0


def self_check(con):
    failures = 0
    for cmd, predicate, description in CHECKS:
        lines = con.cmd(cmd)
        reply = lines[-1] if lines else "<no reply>"
        good = bool(lines) and predicate(reply)
        print(f"{'PASS' if good else 'FAIL'}  {cmd:<28} {description}")
        if not good:
            print(f"      reply: {reply}")
            failures += 1
    print()
    print(f"{len(CHECKS) - failures}/{len(CHECKS)} checks passed")
    return failures


def main():
    ap = argparse.ArgumentParser(
        add_help=False,
        description="Drive the picolyzer-tester USB console from the host.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "Each argument is one command line sent to the board, so quote any\n"
            "command that contains spaces:\n"
            "    tools/console.py \"uart 115200 48 65 6c 6c 6f\" \"status\"\n"
            "With no arguments, the built-in self-check runs."
        ),
    )
    ap.add_argument(
        "-h", "--help", nargs="?", const="", metavar="CMD",
        help="per-command help with examples; no board needed",
    )
    ap.add_argument("--port")
    ap.add_argument("commands", nargs="*")
    args = ap.parse_args()

    if args.help is not None:
        if not args.help:
            print(ap.format_help().rstrip())
            print()
        return print_reference([args.help] if args.help else [])

    port = find_port(args.port)
    print(f"# port: {port}", file=sys.stderr)
    con = Console(port)
    try:
        if not args.commands:
            return 1 if self_check(con) else 0
        for cmd in args.commands:
            for line in con.cmd(cmd):
                print(line)
        return 0
    finally:
        con.close()


if __name__ == "__main__":
    sys.exit(main())
