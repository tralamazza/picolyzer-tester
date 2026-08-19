#!/usr/bin/env python3
"""Drive the picolyzer-tester USB console from the host.

Deliberately dependency-free: it talks to the CDC device with plain file I/O so
it runs on a stock Python with no pyserial. Baud settings are irrelevant on a
CDC ACM link, so nothing here configures one.

Usage:
    tools/console.py                       # run the built-in self-check
    tools/console.py "square 0 1M" "status"
    tools/console.py --port /dev/cu.usbmodemXXXX "id"
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


def find_port(explicit=None):
    if explicit:
        return explicit
    for pattern in DEFAULT_GLOBS:
        matches = sorted(glob.glob(pattern))
        if matches:
            return matches[0]
    raise SystemExit("no USB serial device found; is the board plugged in?")


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
    ("walk 100k", lambda r: r.startswith("ok") and "width=16" in r, "walking ones"),
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
    ap = argparse.ArgumentParser()
    ap.add_argument("--port")
    ap.add_argument("commands", nargs="*")
    args = ap.parse_args()

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
