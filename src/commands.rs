//! Command dispatch.
//!
//! Every reply is a single line starting `ok` or `err`, and every `ok` carries
//! the parameters actually achieved rather than the ones requested. A host
//! script can therefore compare what the analyzer saw against what this device
//! says it emitted, with no guessing in between.

use core::fmt::Write as _;

use tester_core::TimingError;
use tester_core::parse::{channel_arg, hex_byte_arg, hz_arg, mask_arg, u32_arg};
use tester_core::pattern::{self, PatternError};

use crate::board::{BUS_WIDTH, MARKER_PIN, SYSCLK_HZ, TIMING};
use crate::bus::{Bus, Mode, PATTERN_CAPACITY, PRELOAD_SAMPLES};
use crate::console::{Reply, Tokens, err, field, ok};
use crate::proto::{MAX_PAYLOAD, Proto, SPI_CYCLES_PER_BIT, UART_CYCLES_PER_BIT};

pub const HELP: &str = concat!(
    "commands:\r\n",
    "  help                      this text\r\n",
    "  id                        firmware, clock and channel count\r\n",
    "  pins                      pin map\r\n",
    "  stop                      stop output, drive all channels low\r\n",
    "  status                    current mode and stall flag\r\n",
    "\r\n",
    "  square <ch> <hz>          square wave on one channel\r\n",
    "  toggle <mask|all> <hz>    square wave on a channel mask\r\n",
    "  count <hz>                free-running 16-bit binary count\r\n",
    "\r\n",
    "  pulse <ch> <hi_ns> <period_ns>\r\n",
    "  glitch <ch> <ticks>       one narrow pulse, ticks x 6.67ns\r\n",
    "  skew <chA> <chB> <ticks>  two rising edges, ticks apart\r\n",
    "  walk <hz> [width]         walking ones\r\n",
    "  walkz <hz> [width]        walking zeros\r\n",
    "  gray <hz> [width]         gray code sweep\r\n",
    "  ramp <hz> [width]         binary count via the pattern engine\r\n",
    "  load <hex> <hex> ...      arbitrary 16-bit samples\r\n",
    "  play <hz> [loop]          play the loaded samples\r\n",
    "\r\n",
    "  uart <baud> <hex...>      8N1 frames on GP17\r\n",
    "  spi <hz> <hex...>         mode 0, MSB first, GP19/20/21\r\n",
    "  i2c <hz> <addr7> <hex...> bit-banged, GP22/GP26\r\n",
    "\r\n",
    "numbers: 42  0xff  0b1010_1010   frequencies: 115200  1M  2k5\r\n",
);

/// Marker pulse width, in state-machine ticks.
const MARKER_TICKS: u32 = 8;

/// Scratch buffer for synthesised patterns.
static mut PATTERN_BUF: [u16; PATTERN_CAPACITY] = [0; PATTERN_CAPACITY];

/// Borrow the pattern scratch buffer.
///
/// Every caller must take this once and pass the resulting slice down, never
/// call it a second time while an earlier borrow is still live. Two `&mut` to
/// the same static existing at once is undefined behaviour even if neither is
/// used again, so the helpers below take `&[u16]` rather than re-borrowing.
fn pattern_buf() -> &'static mut [u16; PATTERN_CAPACITY] {
    // Safety: dispatch runs only on core 0, is not re-entrant, and no interrupt
    // handler touches this buffer, so this is the only reference in existence
    // for as long as the caller holds it.
    unsafe { &mut *core::ptr::addr_of_mut!(PATTERN_BUF) }
}

pub fn dispatch(bus: &mut Bus, proto: &mut Proto, line: &str) -> Reply {
    if Tokens::truncated(line) {
        return err("too many arguments");
    }
    let t = Tokens::split(line);
    let Some(cmd) = t.get(0) else {
        return err("empty command");
    };

    match cmd {
        "help" => ok(),
        "id" => cmd_id(),
        "pins" => ok(),
        "stop" => {
            bus.stop();
            proto.stop();
            ok()
        }
        "status" => cmd_status(bus, proto),

        "square" => cmd_square(bus, &t),
        "toggle" => cmd_toggle(bus, &t),
        "count" => cmd_count(bus, &t),

        "pulse" => cmd_pulse(bus, &t),
        "glitch" => cmd_glitch(bus, &t),
        "skew" => cmd_skew(bus, &t),
        "walk" => cmd_sweep(bus, &t, Sweep::WalkOnes),
        "walkz" => cmd_sweep(bus, &t, Sweep::WalkZeros),
        "gray" => cmd_sweep(bus, &t, Sweep::Gray),
        "ramp" => cmd_sweep(bus, &t, Sweep::Ramp),
        "load" => cmd_load(bus, &t),
        "play" => cmd_play(bus, &t),

        "uart" => cmd_uart(proto, &t),
        "spi" => cmd_spi(proto, &t),
        "i2c" => cmd_i2c(proto, &t),

        _ => err("unknown command, try `help`"),
    }
}

fn cmd_id() -> Reply {
    let mut r = ok();
    field(
        &mut r,
        "fw",
        format_args!("picolyzer-tester/{}", env!("CARGO_PKG_VERSION")),
    );
    field(&mut r, "sysclk", format_args!("{SYSCLK_HZ}"));
    field(&mut r, "channels", format_args!("{BUS_WIDTH}"));
    field(&mut r, "tick_ps", format_args!("{}", TIMING.tick_ps()));
    field(&mut r, "max_samples", format_args!("{PATTERN_CAPACITY}"));
    // Frequency accuracy is crystal-bound, not something this firmware sets.
    field(&mut r, "xtal_ppm", format_args!("30"));
    r
}

fn cmd_status(bus: &Bus, proto: &Proto) -> Reply {
    let mut r = ok();
    let mode = match bus.mode() {
        Mode::Stopped => "stopped",
        Mode::Toggle => "toggle",
        Mode::Count => "count",
        Mode::PatternOnce => "pattern-once",
        Mode::PatternDma { looping: true } => "pattern-loop",
        Mode::PatternDma { looping: false } => "pattern-dma",
    };
    field(&mut r, "mode", format_args!("{mode}"));
    field(&mut r, "samples", format_args!("{}", bus.pattern_len()));
    field(
        &mut r,
        "txstall",
        format_args!("{}", if bus.stalled() { "yes" } else { "no" }),
    );
    field(
        &mut r,
        "dma",
        format_args!("{}", if bus.dma_busy() { "busy" } else { "idle" }),
    );
    field(
        &mut r,
        "proto",
        format_args!("{}", if proto.busy() { "sending" } else { "idle" }),
    );
    r
}

/// Append the achieved rate to a reply.
///
/// `label` names what the rate refers to, because "the frequency" is ambiguous
/// once sample rates and signal rates differ by a factor of two.
fn report_rate(r: &mut Reply, label: &str, millihz: u64) {
    let _ = write!(r, " {label}={}.{:03}", millihz / 1000, millihz % 1000);
}

fn rate_err(e: TimingError) -> Reply {
    err(e.as_str())
}

fn pattern_err(e: PatternError) -> Reply {
    match e {
        PatternError::TooLong { needed, capacity } => {
            let mut r = err("pattern too long");
            field(&mut r, "needed", format_args!("{needed}"));
            field(&mut r, "capacity", format_args!("{capacity}"));
            r
        }
        PatternError::BadArgument(m) => err(m),
        PatternError::Rate(e) => rate_err(e),
    }
}

// ---------------------------------------------------------------- continuous

fn cmd_square(bus: &mut Bus, t: &Tokens) -> Reply {
    let (Some(ch), Some(hz)) = (t.get(1), t.get(2)) else {
        return err("usage: square <ch> <hz>");
    };
    let Some(ch) = channel_arg(ch, BUS_WIDTH) else {
        return err("channel out of range");
    };
    let Some(hz) = hz_arg(hz) else {
        return err("bad frequency");
    };
    start_toggle(bus, 1u16 << ch, hz)
}

fn cmd_toggle(bus: &mut Bus, t: &Tokens) -> Reply {
    let (Some(mask), Some(hz)) = (t.get(1), t.get(2)) else {
        return err("usage: toggle <mask|all> <hz>");
    };
    let Some(mask) = mask_arg(mask, BUS_WIDTH) else {
        return err("bad mask");
    };
    if mask == 0 {
        return err("mask selects no channels");
    }
    let Some(hz) = hz_arg(hz) else {
        return err("bad frequency");
    };
    start_toggle(bus, mask, hz)
}

fn start_toggle(bus: &mut Bus, mask: u16, hz: u32) -> Reply {
    // The toggle loop is two instructions, so the instruction rate is twice
    // the wave frequency.
    let Some(sm_rate) = hz.checked_mul(2) else {
        return err("frequency too high");
    };
    let divisor = match TIMING.divisor_for_rate(sm_rate) {
        Ok(d) => d,
        Err(e) => return rate_err(e),
    };
    bus.start_toggle(mask, divisor, MARKER_TICKS);

    let mut r = ok();
    field(&mut r, "mode", format_args!("toggle"));
    field(&mut r, "mask", format_args!("{mask:#06x}"));
    field(&mut r, "req_hz", format_args!("{hz}"));
    report_rate(&mut r, "actual_hz", TIMING.rate_of(divisor).millihz / 2);
    field(
        &mut r,
        "div",
        format_args!("{}+{}/256", divisor.int, divisor.frac),
    );
    r
}

fn cmd_count(bus: &mut Bus, t: &Tokens) -> Reply {
    let Some(hz) = t.get(1) else {
        return err("usage: count <hz>");
    };
    let Some(hz) = hz_arg(hz) else {
        return err("bad frequency");
    };
    // Two instructions per emitted code.
    let Some(sm_rate) = hz.checked_mul(2) else {
        return err("rate too high");
    };
    let divisor = match TIMING.divisor_for_rate(sm_rate) {
        Ok(d) => d,
        Err(e) => return rate_err(e),
    };
    bus.start_count(divisor, MARKER_TICKS);

    let mut r = ok();
    field(&mut r, "mode", format_args!("count"));
    field(&mut r, "codes", format_args!("65536"));
    field(&mut r, "req_hz", format_args!("{hz}"));
    report_rate(&mut r, "actual_hz", TIMING.rate_of(divisor).millihz / 2);
    field(
        &mut r,
        "div",
        format_args!("{}+{}/256", divisor.int, divisor.frac),
    );
    r
}

// ------------------------------------------------------------------ one-shot

/// Run a short one-shot pattern at full rate, straight out of the FIFO.
fn play_oneshot(bus: &mut Bus, samples: &[u16], what: &str, ticks: u32) -> Reply {
    let len = samples.len();
    let divisor = match TIMING.divisor_for_rate(TIMING.max_rate_hz()) {
        Ok(d) => d,
        Err(e) => return rate_err(e),
    };
    if let Err(e) = bus.load_pattern(samples) {
        return err(e);
    }
    bus.play_pattern(divisor, false, MARKER_TICKS);

    let mut r = ok();
    field(&mut r, "mode", format_args!("{what}"));
    field(&mut r, "ticks", format_args!("{ticks}"));
    field(
        &mut r,
        "width_ps",
        format_args!("{}", TIMING.ns_of_ticks_ps(ticks)),
    );
    field(&mut r, "samples", format_args!("{len}"));
    field(
        &mut r,
        "preloaded",
        format_args!("{}", if len <= PRELOAD_SAMPLES { "yes" } else { "no" }),
    );
    r
}

fn cmd_glitch(bus: &mut Bus, t: &Tokens) -> Reply {
    let (Some(ch), Some(ticks)) = (t.get(1), t.get(2)) else {
        return err("usage: glitch <ch> <ticks>");
    };
    let Some(ch) = channel_arg(ch, BUS_WIDTH) else {
        return err("channel out of range");
    };
    let Some(ticks) = u32_arg(ticks) else {
        return err("bad tick count");
    };
    let buf = pattern_buf();
    let len = match pattern::glitch(buf, 1u16 << ch, ticks) {
        Ok(n) => n,
        Err(e) => return pattern_err(e),
    };
    let len = pad_even(buf, len);
    play_oneshot(bus, &buf[..len], "glitch", ticks)
}

fn cmd_skew(bus: &mut Bus, t: &Tokens) -> Reply {
    let (Some(a), Some(b), Some(ticks)) = (t.get(1), t.get(2), t.get(3)) else {
        return err("usage: skew <chA> <chB> <ticks>");
    };
    let (Some(a), Some(b)) = (channel_arg(a, BUS_WIDTH), channel_arg(b, BUS_WIDTH)) else {
        return err("channel out of range");
    };
    if a == b {
        return err("skew needs two different channels");
    }
    let Some(ticks) = u32_arg(ticks) else {
        return err("bad tick count");
    };
    let buf = pattern_buf();
    let len = match pattern::skew(buf, 1u16 << a, 1u16 << b, ticks) {
        Ok(n) => n,
        Err(e) => return pattern_err(e),
    };
    let len = pad_even(buf, len);
    play_oneshot(bus, &buf[..len], "skew", ticks)
}

fn cmd_pulse(bus: &mut Bus, t: &Tokens) -> Reply {
    let (Some(ch), Some(hi), Some(period)) = (t.get(1), t.get(2), t.get(3)) else {
        return err("usage: pulse <ch> <high_ns> <period_ns>");
    };
    let Some(ch) = channel_arg(ch, BUS_WIDTH) else {
        return err("channel out of range");
    };
    let (Some(hi_ns), Some(period_ns)) = (u32_arg(hi), u32_arg(period)) else {
        return err("bad duration");
    };
    let hi_ticks = TIMING.ticks_for_ns(hi_ns);
    let period_ticks = TIMING.ticks_for_ns(period_ns);

    let buf = pattern_buf();
    let len = match pattern::pulse(buf, 1, 1u16 << ch, hi_ticks, period_ticks) {
        Ok(n) => n,
        Err(e) => return pattern_err(e),
    };
    let len = pad_even(buf, len);

    let divisor = match TIMING.divisor_for_rate(TIMING.max_rate_hz()) {
        Ok(d) => d,
        Err(e) => return rate_err(e),
    };
    if let Err(e) = bus.load_pattern(&buf[..len]) {
        return err(e);
    }
    bus.play_pattern(divisor, true, MARKER_TICKS);

    let mut r = ok();
    field(&mut r, "mode", format_args!("pulse"));
    field(
        &mut r,
        "high_ps",
        format_args!("{}", TIMING.ns_of_ticks_ps(hi_ticks)),
    );
    field(
        &mut r,
        "period_ps",
        format_args!("{}", TIMING.ns_of_ticks_ps(period_ticks)),
    );
    field(&mut r, "samples", format_args!("{len}"));
    r
}

// -------------------------------------------------------------------- sweeps

#[derive(Clone, Copy)]
enum Sweep {
    WalkOnes,
    WalkZeros,
    Gray,
    Ramp,
}

impl Sweep {
    fn name(self) -> &'static str {
        match self {
            Sweep::WalkOnes => "walk",
            Sweep::WalkZeros => "walkz",
            Sweep::Gray => "gray",
            Sweep::Ramp => "ramp",
        }
    }

    fn default_width(self) -> u8 {
        match self {
            // A full 16-bit gray or ramp is 65536 samples, far past the buffer,
            // so these default to a width that fits with room for repetition.
            Sweep::Gray | Sweep::Ramp => 8,
            Sweep::WalkOnes | Sweep::WalkZeros => BUS_WIDTH,
        }
    }

    fn build(self, buf: &mut [u16], repeat: u32, width: u8) -> Result<usize, PatternError> {
        match self {
            Sweep::WalkOnes => pattern::walking_ones(buf, repeat, width),
            Sweep::WalkZeros => pattern::walking_zeros(buf, repeat, width),
            Sweep::Gray => pattern::gray(buf, repeat, width),
            Sweep::Ramp => pattern::count(buf, repeat, width),
        }
    }
}

fn cmd_sweep(bus: &mut Bus, t: &Tokens, sweep: Sweep) -> Reply {
    let Some(hz) = t.get(1) else {
        return err("usage: <sweep> <hz> [width]");
    };
    let Some(hz) = hz_arg(hz) else {
        return err("bad frequency");
    };
    let width = match t.get(2) {
        None => sweep.default_width(),
        Some(w) => match u32_arg(w) {
            Some(w) if (1..=BUS_WIDTH as u32).contains(&w) => w as u8,
            _ => return err("width must be 1..=16"),
        },
    };

    // Work out the repeat factor first, because it decides how many buffer
    // entries each logical sample consumes.
    let plan = match pattern::plan_rate(&TIMING, hz, (PATTERN_CAPACITY / 2) as u32) {
        Ok(p) => p,
        Err(e) => return pattern_err(e),
    };

    let buf = pattern_buf();
    let len = match sweep.build(buf, plan.repeat, width) {
        Ok(n) => n,
        Err(e) => return pattern_err(e),
    };
    let len = pad_even(buf, len);

    if let Err(e) = bus.load_pattern(&buf[..len]) {
        return err(e);
    }
    bus.play_pattern(plan.divisor, true, MARKER_TICKS);

    let mut r = ok();
    field(&mut r, "mode", format_args!("{}", sweep.name()));
    field(&mut r, "width", format_args!("{width}"));
    field(&mut r, "req_hz", format_args!("{hz}"));
    report_rate(&mut r, "actual_hz", plan.actual_millihz);
    field(&mut r, "repeat", format_args!("{}", plan.repeat));
    field(&mut r, "samples", format_args!("{len}"));
    r
}

// ---------------------------------------------------------------- arbitrary

fn cmd_load(bus: &mut Bus, t: &Tokens) -> Reply {
    let values = t.rest(1);
    if values.is_empty() {
        return err("usage: load <sample> [sample ...]");
    }
    let buf = pattern_buf();
    if values.len() > buf.len() {
        return err("too many samples for one line");
    }
    for (i, v) in values.iter().enumerate() {
        match u32_arg(v).and_then(|n| u16::try_from(n).ok()) {
            Some(s) => buf[i] = s,
            None => {
                let mut r = err("bad sample");
                field(&mut r, "index", format_args!("{i}"));
                field(&mut r, "token", format_args!("{v}"));
                return r;
            }
        }
    }
    let len = pad_even(buf, values.len());
    if let Err(e) = bus.load_pattern(&buf[..len]) {
        return err(e);
    }

    let mut r = ok();
    field(&mut r, "loaded", format_args!("{}", values.len()));
    field(&mut r, "padded_to", format_args!("{len}"));
    r
}

fn cmd_play(bus: &mut Bus, t: &Tokens) -> Reply {
    if bus.pattern_len() == 0 {
        return err("no pattern loaded, use `load` first");
    }
    let Some(hz) = t.get(1) else {
        return err("usage: play <hz> [loop]");
    };
    let Some(hz) = hz_arg(hz) else {
        return err("bad frequency");
    };
    let looping = match t.get(2) {
        None => false,
        Some("loop") => true,
        Some(_) => return err("second argument must be `loop`"),
    };
    // A loaded pattern is already expanded, so no repetition budget here: the
    // caller asked for these exact samples.
    let divisor = match TIMING.divisor_for_rate(hz) {
        Ok(d) => d,
        Err(e) => return rate_err(e),
    };
    let samples = bus.pattern_len();
    bus.play_pattern(divisor, looping, MARKER_TICKS);

    let mut r = ok();
    field(&mut r, "mode", format_args!("play"));
    field(&mut r, "req_hz", format_args!("{hz}"));
    report_rate(&mut r, "actual_sps", TIMING.rate_of(divisor).millihz);
    field(&mut r, "samples", format_args!("{samples}"));
    field(&mut r, "loop", format_args!("{looping}"));
    r
}

/// Pad a pattern to an even sample count.
///
/// Autopull refills every 32 bits, i.e. every two samples, so an odd tail would
/// leave half a word in the OSR to be clocked out as a spurious extra sample.
/// The pad repeats the last sample, which extends a level rather than adding an
/// edge - the one padding that cannot invent a transition the caller did not
/// ask for.
fn pad_even(buf: &mut [u16], len: usize) -> usize {
    if len.is_multiple_of(2) || len >= buf.len() {
        return len;
    }
    buf[len] = buf[len - 1];
    len + 1
}

// ----------------------------------------------------------------- protocols

/// What was wrong with a payload byte list.
///
/// Deliberately small: returning a formatted `Reply` here would put a 260-byte
/// value in the `Err` arm of every payload parse.
#[derive(Clone, Copy)]
enum PayloadError {
    Empty,
    TooLong,
    BadByte { index: usize },
}

/// Collect a hex byte payload from the tail of a command line.
fn payload<'a>(tokens: &[&str], out: &'a mut [u8; MAX_PAYLOAD]) -> Result<&'a [u8], PayloadError> {
    if tokens.is_empty() {
        return Err(PayloadError::Empty);
    }
    if tokens.len() > MAX_PAYLOAD {
        return Err(PayloadError::TooLong);
    }
    for (i, tok) in tokens.iter().enumerate() {
        match hex_byte_arg(tok) {
            Some(b) => out[i] = b,
            None => return Err(PayloadError::BadByte { index: i }),
        }
    }
    Ok(&out[..tokens.len()])
}

fn payload_err(e: PayloadError, tokens: &[&str]) -> Reply {
    match e {
        PayloadError::Empty => err("no payload bytes"),
        PayloadError::TooLong => {
            let mut r = err("payload too long");
            field(&mut r, "max", format_args!("{MAX_PAYLOAD}"));
            field(&mut r, "given", format_args!("{}", tokens.len()));
            r
        }
        PayloadError::BadByte { index } => {
            let mut r = err("bad payload byte");
            field(&mut r, "index", format_args!("{index}"));
            field(&mut r, "token", format_args!("{}", tokens[index]));
            r
        }
    }
}

fn cmd_uart(proto: &mut Proto, t: &Tokens) -> Reply {
    let Some(baud) = t.get(1) else {
        return err("usage: uart <baud> <hex> [hex ...]");
    };
    let Some(baud) = hz_arg(baud) else {
        return err("bad baud rate");
    };
    let Some(sm_rate) = baud.checked_mul(UART_CYCLES_PER_BIT) else {
        return err("baud rate too high");
    };
    let divisor = match TIMING.divisor_for_rate(sm_rate) {
        Ok(d) => d,
        Err(e) => return rate_err(e),
    };

    let mut buf = [0u8; MAX_PAYLOAD];
    let bytes = match payload(t.rest(2), &mut buf) {
        Ok(b) => b,
        Err(e) => return payload_err(e, t.rest(2)),
    };
    let n = bytes.len();
    if let Err(e) = proto.uart_send(bytes, divisor) {
        return err(e);
    }

    let mut r = ok();
    field(&mut r, "mode", format_args!("uart"));
    field(&mut r, "format", format_args!("8N1"));
    field(&mut r, "req_baud", format_args!("{baud}"));
    report_rate(
        &mut r,
        "actual_baud",
        TIMING.rate_of(divisor).millihz / UART_CYCLES_PER_BIT as u64,
    );
    field(
        &mut r,
        "div",
        format_args!("{}+{}/256", divisor.int, divisor.frac),
    );
    field(&mut r, "bytes", format_args!("{n}"));
    r
}

fn cmd_spi(proto: &mut Proto, t: &Tokens) -> Reply {
    let Some(hz) = t.get(1) else {
        return err("usage: spi <hz> <hex> [hex ...]");
    };
    let Some(hz) = hz_arg(hz) else {
        return err("bad clock frequency");
    };
    let Some(sm_rate) = hz.checked_mul(SPI_CYCLES_PER_BIT) else {
        return err("clock too high");
    };
    let divisor = match TIMING.divisor_for_rate(sm_rate) {
        Ok(d) => d,
        Err(e) => return rate_err(e),
    };

    let mut buf = [0u8; MAX_PAYLOAD];
    let bytes = match payload(t.rest(2), &mut buf) {
        Ok(b) => b,
        Err(e) => return payload_err(e, t.rest(2)),
    };
    let n = bytes.len();
    if let Err(e) = proto.spi_send(bytes, divisor) {
        return err(e);
    }

    let mut r = ok();
    field(&mut r, "mode", format_args!("spi"));
    field(&mut r, "spi_mode", format_args!("0"));
    field(&mut r, "bit_order", format_args!("msb"));
    field(&mut r, "req_hz", format_args!("{hz}"));
    report_rate(
        &mut r,
        "actual_hz",
        TIMING.rate_of(divisor).millihz / SPI_CYCLES_PER_BIT as u64,
    );
    field(
        &mut r,
        "div",
        format_args!("{}+{}/256", divisor.int, divisor.frac),
    );
    field(&mut r, "bytes", format_args!("{n}"));
    r
}

fn cmd_i2c(proto: &mut Proto, t: &Tokens) -> Reply {
    let (Some(hz), Some(addr)) = (t.get(1), t.get(2)) else {
        return err("usage: i2c <hz> <addr7> <hex> [hex ...]");
    };
    let Some(hz) = hz_arg(hz) else {
        return err("bad clock frequency");
    };
    let Some(addr) = u32_arg(addr).and_then(|a| u8::try_from(a).ok()) else {
        return err("bad address");
    };

    let mut buf = [0u8; MAX_PAYLOAD];
    let bytes = match payload(t.rest(3), &mut buf) {
        Ok(b) => b,
        Err(e) => return payload_err(e, t.rest(3)),
    };
    let n = bytes.len();
    if let Err(e) = proto.i2c_send(addr, bytes, hz) {
        return err(e);
    }

    let mut r = ok();
    field(&mut r, "mode", format_args!("i2c"));
    field(&mut r, "addr", format_args!("{addr:#04x}"));
    field(&mut r, "rw", format_args!("write"));
    field(&mut r, "req_hz", format_args!("{hz}"));
    field(&mut r, "bytes", format_args!("{n}"));
    // The transaction is already complete: i2c_send blocks, because at 100 kHz
    // a short frame is well under a millisecond and a synchronous reply is
    // simpler to script against than a completion poll.
    field(&mut r, "ack", format_args!("none-no-slave"));
    r
}

pub const PIN_MAP: &str = concat!(
    "pin map (identical on Pico 2 and Pico 2 W):\r\n",
    "  GP0..GP15  channels 0..15, the 16-bit bus\r\n",
    "  GP16       trigger marker, pulsed at the start of every burst\r\n",
    "  GP17       UART TX, 8N1, PIO-clocked\r\n",
    "  GP19/20/21 SPI SCK/MOSI (PIO-clocked) and CS (CPU-driven)\r\n",
    "  GP22, GP26 I2C SCL/SDA, CPU bit-banged, push-pull - no slaves!\r\n",
    "  GND        use several - one ground per few channels\r\n",
    "\r\n",
    "GP23/24/25/29 are untouched: they are the wireless interface on a Pico 2 W.\r\n",
);

/// Multi-line text for commands whose output does not fit one reply line.
pub fn long_output(cmd: &str) -> Option<&'static str> {
    match cmd {
        "help" => Some(HELP),
        "pins" => Some(PIN_MAP),
        _ => None,
    }
}

const _: () = assert!(MARKER_PIN == 16, "help text names GP16 as the marker");
