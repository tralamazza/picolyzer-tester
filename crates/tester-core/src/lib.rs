//! Pure logic for picolyzer-tester: timing arithmetic, argument parsing, and
//! waveform synthesis.
//!
//! None of this touches hardware. It lives in its own crate so it can be
//! unit-tested on the host - the firmware crate is `no_main` and cannot host a
//! test harness. Everything that *can* be wrong in a way a bench measurement
//! would struggle to catch (divider rounding, gray-code adjacency, argument
//! coercion) belongs here, behind tests.

#![cfg_attr(not(test), no_std)]

pub mod parse;
pub mod pattern;
pub mod timing;

pub use timing::{Divisor, Rate, Timing, TimingError};
