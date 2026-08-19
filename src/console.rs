//! USB CDC command console: line assembly, tokenizing, and replies.
//!
//! Deliberately dumb and allocation-free. The console runs on the CPU while all
//! signal timing runs in PIO, so nothing here can perturb a waveform.

use core::fmt::Write as _;
use heapless::String;

/// Longest command line accepted. Enough for `bus load` with a good number of
/// samples per line; longer loads are split across lines by the host.
pub const LINE_LEN: usize = 512;
/// Longest single reply.
pub const REPLY_LEN: usize = 256;
/// Most whitespace-separated tokens in one line.
pub const MAX_TOKENS: usize = 64;

/// Assembles bytes arriving from USB into complete lines.
pub struct LineBuffer {
    buf: String<LINE_LEN>,
    /// Set when a line overflowed, so the whole line is rejected instead of
    /// being silently truncated into a different, valid-looking command.
    overflowed: bool,
}

/// What [`LineBuffer::push`] produced for one input byte.
pub enum LineEvent {
    /// Nothing complete yet.
    Pending,
    /// A finished line, ready to execute.
    Line,
    /// The line was too long and has been discarded.
    Overflow,
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LineBuffer {
    pub const fn new() -> Self {
        Self {
            buf: String::new(),
            overflowed: false,
        }
    }

    /// Feed one received byte.
    pub fn push(&mut self, byte: u8) -> LineEvent {
        match byte {
            b'\r' | b'\n' => {
                if self.overflowed {
                    self.reset();
                    LineEvent::Overflow
                } else if self.buf.is_empty() {
                    LineEvent::Pending
                } else {
                    LineEvent::Line
                }
            }
            // Backspace / delete, so an interactive terminal session is usable.
            0x08 | 0x7f => {
                self.buf.pop();
                LineEvent::Pending
            }
            // Ignore anything non-printable; a stray control byte should not
            // corrupt a command.
            b if !(0x20..0x7f).contains(&b) => LineEvent::Pending,
            b => {
                if self.buf.push(b as char).is_err() {
                    self.overflowed = true;
                }
                LineEvent::Pending
            }
        }
    }

    /// The completed line. Only meaningful right after [`LineEvent::Line`].
    pub fn line(&self) -> &str {
        &self.buf
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.overflowed = false;
    }
}

/// A command line split into whitespace-separated tokens.
pub struct Tokens<'a> {
    items: [&'a str; MAX_TOKENS],
    len: usize,
}

impl<'a> Tokens<'a> {
    /// Split a line. Tokens beyond [`MAX_TOKENS`] are dropped, which is
    /// reported as an error by the commands that care.
    pub fn split(line: &'a str) -> Self {
        let mut items = [""; MAX_TOKENS];
        let mut len = 0;
        for tok in line.split_ascii_whitespace() {
            if len == MAX_TOKENS {
                break;
            }
            items[len] = tok;
            len += 1;
        }
        Self { items, len }
    }

    pub fn get(&self, i: usize) -> Option<&'a str> {
        (i < self.len).then(|| self.items[i])
    }

    /// Tokens from `i` onward.
    pub fn rest(&self, i: usize) -> &[&'a str] {
        if i >= self.len {
            &[]
        } else {
            &self.items[i..self.len]
        }
    }

    /// Whether the line had more tokens than [`MAX_TOKENS`].
    pub fn truncated(line: &str) -> bool {
        line.split_ascii_whitespace().count() > MAX_TOKENS
    }
}

/// The response to one command.
///
/// Every reply is `ok ...` or `err ...` on a single line, so a host script can
/// parse results without guessing.
pub type Reply = String<REPLY_LEN>;

/// Start an `ok` reply.
pub fn ok() -> Reply {
    let mut r = Reply::new();
    let _ = r.push_str("ok");
    r
}

/// A complete `err` reply.
pub fn err(msg: &str) -> Reply {
    let mut r = Reply::new();
    let _ = r.push_str("err ");
    let _ = r.push_str(msg);
    r
}

/// Append ` key=value` to a reply, ignoring overflow.
///
/// Overflow is ignored rather than propagated because a truncated diagnostic is
/// still better than no reply at all, and [`REPLY_LEN`] is sized well above
/// anything the command set produces.
pub fn field(reply: &mut Reply, key: &str, value: core::fmt::Arguments<'_>) {
    let _ = write!(reply, " {key}=");
    let _ = reply.write_fmt(value);
}
