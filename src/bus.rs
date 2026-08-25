//! The 16-channel signal engine on PIO0, plus the trigger marker.
//!
//! Three PIO programs share state machine 0, one at a time:
//!
//! * `toggle` - a free-running square wave on an arbitrary channel mask. Two
//!   instructions, no FIFO, so it runs forever at up to sysclk/2 with no CPU or
//!   DMA involvement at all and cannot glitch.
//! * `count`  - a free-running 16-bit binary count. Also two instructions and
//!   no FIFO, so a full 65536-code sweep is exact at any rate.
//! * `pattern`- an arbitrary sample list clocked out one sample per cycle, fed
//!   from the TX FIFO by chained DMA (see [`crate::dma`]), or preloaded into
//!   the FIFO outright when it is short enough and one-shot.
//!
//! The marker on state machine 1 is started in the same cycle as SM0 via
//! `StateMachineGroup::sync`, which is why it has to live on PIO0 too:
//! synchronised start only works within one PIO block.
//!
//! # Why `toggle` and `count` are not just patterns
//!
//! They could be, and DMA now feeds patterns fast enough that they would mostly
//! work. But a pattern still has to flow through the FIFO, and a loop short
//! enough that the DMA chain cannot close in time will drop samples. A
//! dedicated two-instruction program has no loop to close and no FIFO to
//! starve, so the two highest-rate test signals stay exactly the cases this
//! device cannot itself get wrong.

use rp235x_hal as hal;

use hal::pac::PIO0;
use hal::pio::{
    Buffers, InstalledProgram, PIO, PIOBuilder, PIOExt, PinDir, Running, Rx, ShiftDirection,
    StateMachine, Stopped, Tx, UninitStateMachine,
};

use tester_core::Divisor;

use crate::board::{BUS_BASE, BUS_WIDTH, MARKER_PIN};
use crate::dma::PatternDma;

type Sm0 = (PIO0, hal::pio::SM0);
type Sm1 = (PIO0, hal::pio::SM1);

/// Samples held for streamed patterns.
///
/// 4096 u16 samples is 8 KiB out of the RP2350's 520 KiB, and is what bounds
/// how far below the clock-divider floor a pattern can be slowed by sample
/// repetition (see `tester_core::pattern::plan_rate`).
pub const PATTERN_CAPACITY: usize = 4096;

/// Samples that fit in the TX FIFO once it is joined for transmit only.
///
/// 8 words of 2 packed samples. A pattern this short is loaded in full before
/// the state machine starts, so it runs with no refill and therefore no
/// possibility of a stall - which is what makes the narrow-glitch test
/// trustworthy at full rate.
pub const PRELOAD_SAMPLES: usize = 16;

/// What the engine is currently doing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Stopped,
    Toggle,
    Count,
    /// A one-shot pattern preloaded into the FIFO, no DMA involved.
    PatternOnce,
    /// A pattern fed by DMA, optionally looping in hardware.
    PatternDma {
        looping: bool,
    },
}

/// Which PIO programs are installed. They are installed once at start-up and
/// stay installed; only the state machine is reconfigured between commands.
struct Programs {
    toggle: InstalledProgram<PIO0>,
    count: InstalledProgram<PIO0>,
    pattern: InstalledProgram<PIO0>,
    marker: InstalledProgram<PIO0>,
}

enum Sm0State {
    Idle(UninitStateMachine<Sm0>),
    Running {
        sm: StateMachine<Sm0, Running>,
        rx: Rx<Sm0>,
        tx: Tx<Sm0>,
    },
    /// Transient, only observed while swapping states.
    Swapping,
}

enum Sm1State {
    Idle(UninitStateMachine<Sm1>),
    Running {
        sm: StateMachine<Sm1, Running>,
        rx: Rx<Sm1>,
        tx: Tx<Sm1>,
    },
    Swapping,
}

/// The 16-channel engine.
pub struct Bus {
    /// Held for ownership. Programs are installed once in `new` and never
    /// uninstalled, so nothing calls into this again.
    _pio: PIO<PIO0>,
    programs: Programs,
    sm0: Sm0State,
    sm1: Sm1State,
    mode: Mode,

    /// Live words in [`PATTERN_WORDS`].
    word_len: usize,
    /// Whether a stall was seen since the current run started.
    stalled: bool,
    dma: PatternDma,
}

/// Pattern words, packed two 16-bit samples per word.
///
/// A `static` rather than a field of [`Bus`] because DMA reads it directly and
/// needs an address that is stable for the life of the program. Access is
/// disciplined: only `load_pattern` writes it, and only after `stop` has
/// aborted any transfer that could be reading it.
static mut PATTERN_WORDS: [u32; PATTERN_CAPACITY / 2] = [0; PATTERN_CAPACITY / 2];

/// The pattern word buffer.
///
/// # Safety
///
/// Callers must not hold two references at once. Every use here is a single
/// short-lived borrow on the command-dispatch path, which is not re-entrant and
/// is never touched by an interrupt.
fn pattern_words() -> &'static mut [u32; PATTERN_CAPACITY / 2] {
    unsafe { &mut *core::ptr::addr_of_mut!(PATTERN_WORDS) }
}

impl Bus {
    /// Claim PIO0 and install every program.
    pub fn new(pio0: PIO0, dma: hal::pac::DMA, resets: &mut hal::pac::RESETS) -> Self {
        let dma = PatternDma::new(dma, resets);
        let (mut pio, sm0, sm1, _sm2, _sm3) = pio0.split(resets);

        // Free-running square wave. `x` holds the channel mask, loaded once
        // from the FIFO before the loop; `mov pins, null` writes all zeros.
        // Two cycles per period, so the wave is sysclk / (2 * divisor).
        let toggle = pio::pio_asm!(
            "    pull block",
            "    mov x, osr",
            ".wrap_target",
            "    mov pins, x",
            "    mov pins, null",
            ".wrap",
        );

        // Free-running 16-bit counter.
        //
        // `jmp x--` always decrements and, taken or not, lands on the next
        // instruction, so this is an unconditional 2-cycle loop that never
        // needs the FIFO. `x` counts down while `mov pins, !x` emits its
        // complement, which is an ascending count; when x wraps past zero the
        // low 16 bits keep cycling correctly, so the sweep is seamless.
        let count = pio::pio_asm!(
            "    pull block",
            "    mov x, osr",
            ".wrap_target",
            "    jmp x--, emit",
            "emit:",
            "    mov pins, !x",
            ".wrap",
        );

        // Arbitrary pattern: one sample per cycle, refilled by autopull every
        // two samples.
        let pattern = pio::pio_asm!(".wrap_target", "    out pins, 16", ".wrap",);

        // Trigger marker: a pulse whose width in cycles comes from the FIFO,
        // then back to blocking on `pull` so it fires exactly once per burst.
        let marker = pio::pio_asm!(
            "    pull block",
            "    mov x, osr",
            "    set pins, 1",
            "hold:",
            "    jmp x--, hold",
            "    set pins, 0",
        );

        let programs = Programs {
            toggle: pio.install(&toggle.program).unwrap(),
            count: pio.install(&count.program).unwrap(),
            pattern: pio.install(&pattern.program).unwrap(),
            marker: pio.install(&marker.program).unwrap(),
        };

        // Establish the idle electrical state before anything else can observe
        // the pins.
        //
        // Handing a pin to PIO sets its function select but leaves its
        // direction as input, so without this the 16 channels and the marker
        // float from power-up until the first command. A floating input next to
        // a switching neighbour picks up enough crosstalk to look like real
        // activity, which is a confusing first impression from a device whose
        // whole job is to be the trustworthy side of the measurement.
        //
        // Safety: the master copies stay in `programs` and are never
        // uninstalled, so the shares refer to live instruction memory.
        let sm0 = {
            let prog = unsafe { programs.pattern.share() };
            let (mut sm, rx, tx) = PIOBuilder::from_installed_program(prog)
                .out_pins(BUS_BASE, BUS_WIDTH)
                .build(sm0);
            set_bus_output(&mut sm);
            drive_low(&mut sm);
            let (uninit, _prog) = sm.uninit(rx, tx);
            uninit
        };
        let sm1 = {
            let prog = unsafe { programs.marker.share() };
            let (mut sm, rx, tx) = PIOBuilder::from_installed_program(prog)
                .set_pins(MARKER_PIN, 1)
                .build(sm1);
            sm.set_pindirs([(MARKER_PIN, PinDir::Output)]);
            sm.set_pins([(MARKER_PIN, hal::pio::PinState::Low)]);
            let (uninit, _prog) = sm.uninit(rx, tx);
            uninit
        };

        Self {
            _pio: pio,
            programs,
            sm0: Sm0State::Idle(sm0),
            sm1: Sm1State::Idle(sm1),
            mode: Mode::Stopped,
            word_len: 0,
            stalled: false,
            dma,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether the state machine has stalled waiting for data since the last
    /// start. A stall means the *tester* dropped samples, not the analyzer, so
    /// reporting it is what makes a failed capture attributable.
    pub fn stalled(&self) -> bool {
        self.stalled
    }

    /// Stop everything and leave all 16 channels driven low.
    ///
    /// Driving low rather than releasing matters: a floating bus reads as noise
    /// on a sensitive analyzer and looks like a fault.
    pub fn stop(&mut self) {
        // Abort DMA first: it is the only other thing that reads the pattern
        // buffer or writes the FIFO, so nothing else is safe until it is idle.
        self.dma.stop();

        let sm0 = core::mem::replace(&mut self.sm0, Sm0State::Swapping);
        self.sm0 = Sm0State::Idle(match sm0 {
            Sm0State::Idle(u) => u,
            Sm0State::Running { sm, rx, tx } => {
                let mut stopped = sm.stop();
                stopped.clear_fifos();
                drive_low(&mut stopped);
                let (uninit, _prog) = stopped.uninit(rx, tx);
                uninit
            }
            Sm0State::Swapping => unreachable!("stop() is not re-entrant"),
        });

        let sm1 = core::mem::replace(&mut self.sm1, Sm1State::Swapping);
        self.sm1 = Sm1State::Idle(match sm1 {
            Sm1State::Idle(u) => u,
            Sm1State::Running { sm, rx, tx } => {
                let mut stopped = sm.stop();
                stopped.clear_fifos();
                stopped.set_pins([(MARKER_PIN, hal::pio::PinState::Low)]);
                let (uninit, _prog) = stopped.uninit(rx, tx);
                uninit
            }
            Sm1State::Swapping => unreachable!("stop() is not re-entrant"),
        });

        self.mode = Mode::Stopped;
        // The loaded pattern deliberately survives a stop. `play_pattern` stops
        // before it starts, so clearing the buffer here would make `play`
        // single-use: the pattern it was about to emit would be erased on the
        // way in, and a second `play` would report nothing loaded.
        self.stalled = false;
    }

    /// Start a free-running square wave on `mask`.
    ///
    /// `divisor` sets the instruction rate; the wave is half that, because the
    /// loop is two instructions.
    pub fn start_toggle(&mut self, mask: u16, divisor: Divisor, marker_ticks: u32) {
        self.stop();
        // Safety: the master copy stays in `self.programs` and is never
        // uninstalled, so every share refers to live instruction memory.
        let prog = unsafe { self.programs.toggle.share() };
        let (mut sm, rx, mut tx) = PIOBuilder::from_installed_program(prog)
            .out_pins(BUS_BASE, BUS_WIDTH)
            .clock_divisor_fixed_point(divisor.int, divisor.frac)
            .out_shift_direction(ShiftDirection::Right)
            .build(self.take_sm0());
        set_bus_output(&mut sm);
        // The program pulls the mask once before entering its loop.
        tx.write(mask as u32);
        // Two instructions per period: `mov pins, x` then `mov pins, null`.
        self.launch(sm, rx, tx, marker_ticks, 2, divisor);
        self.mode = Mode::Toggle;
    }

    /// Start a free-running 16-bit binary count.
    pub fn start_count(&mut self, divisor: Divisor, marker_ticks: u32) {
        self.stop();
        // Safety: as in `start_toggle`.
        let prog = unsafe { self.programs.count.share() };
        let (mut sm, rx, mut tx) = PIOBuilder::from_installed_program(prog)
            .out_pins(BUS_BASE, BUS_WIDTH)
            .clock_divisor_fixed_point(divisor.int, divisor.frac)
            .out_shift_direction(ShiftDirection::Right)
            .build(self.take_sm0());
        set_bus_output(&mut sm);
        // Start from all-ones so the emitted complement starts at zero.
        tx.write(0xffff);
        // Two instructions per code: the `jmp x--` loop plus `mov pins, !x`.
        self.launch(sm, rx, tx, marker_ticks, 2, divisor);
        self.mode = Mode::Count;
    }

    /// Load a sample list, packing two 16-bit samples per FIFO word.
    ///
    /// The sample count must be even; `tester_core::pattern` guarantees this by
    /// padding, because autopull refills every 32 bits and an odd tail would
    /// leave half a word to be emitted as a spurious extra sample.
    pub fn load_pattern(&mut self, samples: &[u16]) -> Result<(), &'static str> {
        if samples.is_empty() {
            return Err("pattern is empty");
        }
        if !samples.len().is_multiple_of(2) {
            return Err("pattern length must be even");
        }
        if samples.len() > PATTERN_CAPACITY {
            return Err("pattern exceeds buffer");
        }
        // DMA may still be reading the buffer from a previous run; stopping is
        // what makes writing it here safe.
        self.stop();
        let words = pattern_words();
        // `as_chunks`, not `chunks_exact(2)`: the length is a constant, so this
        // yields `&[u16; 2]` and the indexing below needs no bounds check. The
        // remainder is empty by the even-length check above.
        for (w, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
            // Right shift direction sends the low half first, so sample 2n goes
            // in the low 16 bits and sample 2n+1 in the high half.
            words[w] = pair[0] as u32 | (pair[1] as u32) << 16;
        }
        self.word_len = samples.len() / 2;
        Ok(())
    }

    /// Play the loaded pattern.
    ///
    /// Patterns short enough to preload run entirely out of the FIFO and cannot
    /// stall. Longer ones are refilled from [`Self::service`] in the poll loop,
    /// which caps the sustainable rate; use `toggle` or `count` when a
    /// guaranteed gap-free high-rate signal is what is wanted.
    pub fn play_pattern(&mut self, divisor: Divisor, looping: bool, marker_ticks: u32) {
        self.stop();
        // Safety: as in `start_toggle`.
        let prog = unsafe { self.programs.pattern.share() };
        let (mut sm, rx, mut tx) = PIOBuilder::from_installed_program(prog)
            .out_pins(BUS_BASE, BUS_WIDTH)
            .clock_divisor_fixed_point(divisor.int, divisor.frac)
            .out_shift_direction(ShiftDirection::Right)
            .autopull(true)
            .pull_threshold(32)
            // Give the RX FIFO's storage to TX: 8 words instead of 4, which
            // doubles both the preload ceiling and the streaming headroom.
            .buffers(Buffers::OnlyTx)
            .build(self.take_sm0());
        set_bus_output(&mut sm);

        // TXSTALL is a sticky hardware flag. Without clearing it here, a stall
        // left over from an earlier run would be latched by the first
        // `service` call and reported against this one - the stall report is
        // only worth anything if it cannot produce false positives.
        tx.clear_stalled_flag();

        // A one-shot short enough to sit entirely in the FIFO is loaded up
        // front and needs no DMA at all. That path cannot stall by
        // construction, which is what makes the narrow-glitch test trustworthy,
        // so it is kept rather than folded into the DMA path.
        let preloadable = !looping && self.word_len <= PRELOAD_SAMPLES / 2;
        let mut cursor = 0;
        if preloadable {
            while !tx.is_full() && cursor < self.word_len {
                tx.write(pattern_words()[cursor]);
                cursor += 1;
            }
        }
        let preloaded_all = preloadable && cursor >= self.word_len;

        if !preloaded_all {
            // Safety: `stop` aborted any previous transfer, and the buffer is a
            // static that only `load_pattern` writes - and that stops first.
            unsafe {
                self.dma.start(
                    &pattern_words()[..self.word_len],
                    tx.fifo_address(),
                    looping,
                );
            }
        }

        // `out pins, 16` is one instruction, so one cycle per sample.
        self.launch(sm, rx, tx, marker_ticks, 1, divisor);
        self.stalled = false;
        self.mode = if preloaded_all {
            Mode::PatternOnce
        } else {
            Mode::PatternDma { looping }
        };
    }

    /// Sample the stall flag. Call from the main poll loop.
    ///
    /// DMA does the feeding now, so there is no refill work here and no loop
    /// that could fail to terminate. The previous CPU-refill version looped
    /// until the FIFO reported full, which above a few MSa/s never happened -
    /// the state machine drained it faster than the CPU could fill it, so the
    /// USB poll never ran again and the board hung until power-cycled.
    pub fn service(&mut self) {
        if !matches!(self.mode, Mode::PatternDma { .. }) {
            return;
        }
        let Sm0State::Running { ref mut tx, .. } = self.sm0 else {
            return;
        };
        // Latch it: TXSTALL is a level in hardware, but the question worth
        // answering is "did this run ever starve", which is an edge.
        if tx.has_stalled() {
            self.stalled = true;
            tx.clear_stalled_flag();
        }
    }

    /// Whether the DMA data channel still has transfers outstanding.
    ///
    /// For a one-shot pattern this is how you tell the burst has finished; a
    /// looping one never goes idle by design.
    pub fn dma_busy(&self) -> bool {
        self.dma.busy()
    }

    /// Number of samples currently loaded.
    pub fn pattern_len(&self) -> usize {
        self.word_len * 2
    }

    /// Build and sync-start SM0 together with the marker on SM1.
    ///
    /// `cycles_per_tick` is how many PIO cycles the *data* program spends per
    /// tick the caller reasons about - one output sample for `count` and
    /// `pattern`, one full period for `toggle`. It is needed because the marker
    /// shares the data clock but has its own instruction count, so without it
    /// the same `marker_ticks` produces different widths in different modes.
    fn launch(
        &mut self,
        sm0: StateMachine<Sm0, Stopped>,
        rx0: Rx<Sm0>,
        tx0: Tx<Sm0>,
        marker_ticks: u32,
        cycles_per_tick: u32,
        divisor: Divisor,
    ) {
        // Safety: the master copy stays in `self.programs`.
        let prog = unsafe { self.programs.marker.share() };
        let (mut sm1, rx1, mut tx1) = PIOBuilder::from_installed_program(prog)
            .set_pins(MARKER_PIN, 1)
            .clock_divisor_fixed_point(divisor.int, divisor.frac)
            .out_shift_direction(ShiftDirection::Right)
            .build(self.take_sm1());
        sm1.set_pindirs([(MARKER_PIN, PinDir::Output)]);
        // The marker program holds the pin high for `x + 2` cycles: `jmp x--`
        // runs x+1 times, and the pin only drops at the end of the following
        // `set pins, 0`. Measured on an SLogic16 U3 at 5 MSa/s: 8 written gave
        // 250 samples under `count 100k` (10 cycles of a 200 kHz SM clock) and
        // 500 under `ramp 100k` (10 cycles at 100 kHz). Solving back through
        // both gives the width in ticks the caller asked for.
        let cycles = marker_ticks.max(1).saturating_mul(cycles_per_tick.max(1));
        tx1.write(cycles.saturating_sub(2).max(1));

        // Start both in the same cycle. A marker that is not cycle-aligned with
        // the data is worse than no marker, because it silently biases every
        // timing measurement taken relative to it.
        let (sm0, sm1) = sm0.with(sm1).sync().start().free();

        self.sm0 = Sm0State::Running {
            sm: sm0,
            rx: rx0,
            tx: tx0,
        };
        self.sm1 = Sm1State::Running {
            sm: sm1,
            rx: rx1,
            tx: tx1,
        };
    }

    fn take_sm0(&mut self) -> UninitStateMachine<Sm0> {
        match core::mem::replace(&mut self.sm0, Sm0State::Swapping) {
            Sm0State::Idle(u) => u,
            _ => unreachable!("callers stop() before reconfiguring"),
        }
    }

    fn take_sm1(&mut self) -> UninitStateMachine<Sm1> {
        match core::mem::replace(&mut self.sm1, Sm1State::Swapping) {
            Sm1State::Idle(u) => u,
            _ => unreachable!("callers stop() before reconfiguring"),
        }
    }
}

/// Set all 16 bus pins to outputs.
fn set_bus_output(sm: &mut StateMachine<Sm0, Stopped>) {
    sm.set_pindirs((0..BUS_WIDTH).map(|i| (BUS_BASE + i, PinDir::Output)));
}

/// Drive all 16 bus pins low.
fn drive_low(sm: &mut StateMachine<Sm0, Stopped>) {
    sm.set_pins((0..BUS_WIDTH).map(|i| (BUS_BASE + i, hal::pio::PinState::Low)));
}
