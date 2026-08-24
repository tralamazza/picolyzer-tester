//! Protocol traffic generators: UART TX and SPI on PIO1, I2C bit-banged.
//!
//! These exist to exercise an analyzer's protocol *decoders*, which is a
//! different job from the parallel bus. A decoder test cares that the frame
//! structure and bit values are right; the bus is where edge placement is
//! measured. That difference is why I2C being CPU-driven is acceptable here and
//! would not be on the bus.
//!
//! Bytes are pushed into the TX FIFO from the poll loop. Even at 12 Mbaud a
//! byte lasts ~700 ns, which is many thousands of CPU cycles, so the FIFO never
//! runs dry in practice - and if it ever did, the gap lands between bytes,
//! where a decoder sees idle line rather than corrupted data.

use rp235x_hal as hal;

use hal::pac::PIO1;
use hal::pio::{
    Buffers, InstalledProgram, PIO, PIOBuilder, PIOExt, PinDir, Running, Rx, ShiftDirection,
    StateMachine, Tx, UninitStateMachine,
};

use embedded_hal::digital::OutputPin;
use tester_core::Divisor;

use crate::board::{SPI_MOSI_PIN, SPI_SCK_PIN, SioPin, UART_TX_PIN};

type UartSm = (PIO1, hal::pio::SM0);
type SpiSm = (PIO1, hal::pio::SM1);

/// Cycles the UART program spends per bit.
pub const UART_CYCLES_PER_BIT: u32 = 8;
/// Cycles the SPI program spends per bit.
pub const SPI_CYCLES_PER_BIT: u32 = 4;

/// Longest payload accepted in one command.
pub const MAX_PAYLOAD: usize = 32;

enum Slot<SM: hal::pio::ValidStateMachine> {
    Idle(UninitStateMachine<SM>),
    Running {
        sm: StateMachine<SM, Running>,
        rx: Rx<SM>,
        tx: Tx<SM>,
    },
    Swapping,
}

impl<SM: hal::pio::ValidStateMachine> Slot<SM> {
    fn take_idle(&mut self) -> UninitStateMachine<SM> {
        match core::mem::replace(self, Slot::Swapping) {
            Slot::Idle(u) => u,
            _ => unreachable!("callers release before reconfiguring"),
        }
    }

    /// Stop the state machine and leave its pins at the given idle levels.
    ///
    /// The idle levels are not optional: a burst can be interrupted part-way
    /// through a frame, and a UART TX line left low is an endless break
    /// condition rather than an idle line.
    fn release(&mut self, idle: &[(u8, hal::pio::PinState)]) {
        let cur = core::mem::replace(self, Slot::Swapping);
        *self = Slot::Idle(match cur {
            Slot::Idle(u) => u,
            Slot::Running { sm, rx, tx } => {
                let mut stopped = sm.stop();
                stopped.clear_fifos();
                stopped.set_pins(idle.iter().copied());
                let (u, _prog) = stopped.uninit(rx, tx);
                u
            }
            Slot::Swapping => unreachable!("release is not re-entrant"),
        });
    }
}

/// The protocol generators.
pub struct Proto {
    _pio: PIO<PIO1>,
    uart_prog: InstalledProgram<PIO1>,
    spi_prog: InstalledProgram<PIO1>,
    uart: Slot<UartSm>,
    spi: Slot<SpiSm>,

    /// Bytes still to be pushed into whichever FIFO is active.
    payload: [u8; MAX_PAYLOAD],
    len: usize,
    cursor: usize,
    active: Active,

    cs: SioPin,
    scl: SioPin,
    sda: SioPin,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Active {
    None,
    Uart,
    Spi,
}

impl Proto {
    pub fn new(
        pio1: PIO1,
        resets: &mut hal::pac::RESETS,
        cs: SioPin,
        scl: SioPin,
        sda: SioPin,
    ) -> Self {
        let (mut pio, sm0, sm1, _sm2, _sm3) = pio1.split(resets);

        // 8N1 UART transmit, 8 cycles per bit. Side-set drives the line high
        // while stalled on `pull`, which is what makes the idle level correct
        // between frames rather than only during them.
        let uart = pio::pio_asm!(
            ".side_set 1 opt",
            "    pull       side 1 [7]", // stop bit / idle while waiting
            "    set x, 7   side 0 [7]", // start bit, preload bit counter
            "bitloop:",
            "    out pins, 1",
            "    jmp x-- bitloop [6]",
        );

        // SPI mode 0, MSB first, 4 cycles per bit: data changes on the falling
        // edge and is stable across the rising edge, which is what a mode-0
        // decoder samples on.
        let spi = pio::pio_asm!(
            ".side_set 1",
            ".wrap_target",
            "    out pins, 1  side 0 [1]",
            "    nop          side 1 [1]",
            ".wrap",
        );

        let uart_prog = pio.install(&uart.program).unwrap();
        let spi_prog = pio.install(&spi.program).unwrap();

        // Drive the PIO-owned protocol pins to their idle levels before anything
        // observes them. Handing a pin to PIO leaves its direction as input, so
        // otherwise UART TX floats from power-up - and a floating TX line makes
        // a decoder report framing errors that have nothing to do with the
        // frames this device later sends.
        //
        // Safety: the master copies live in `uart_prog`/`spi_prog` and are never
        // uninstalled, so the shares refer to live instruction memory.
        let sm0 = {
            let prog = unsafe { uart_prog.share() };
            let (mut sm, rx, tx) = PIOBuilder::from_installed_program(prog)
                .set_pins(UART_TX_PIN, 1)
                .build(sm0);
            sm.set_pindirs([(UART_TX_PIN, PinDir::Output)]);
            // UART idles high; low would look like an endless break condition.
            sm.set_pins([(UART_TX_PIN, hal::pio::PinState::High)]);
            let (uninit, _prog) = sm.uninit(rx, tx);
            uninit
        };
        let sm1 = {
            let prog = unsafe { spi_prog.share() };
            let (mut sm, rx, tx) = PIOBuilder::from_installed_program(prog)
                .set_pins(SPI_SCK_PIN, 1)
                .build(sm1);
            sm.set_pindirs([
                (SPI_SCK_PIN, PinDir::Output),
                (SPI_MOSI_PIN, PinDir::Output),
            ]);
            // Mode 0 idles the clock low.
            sm.set_pins([
                (SPI_SCK_PIN, hal::pio::PinState::Low),
                (SPI_MOSI_PIN, hal::pio::PinState::Low),
            ]);
            let (uninit, _prog) = sm.uninit(rx, tx);
            uninit
        };

        Self {
            uart_prog,
            spi_prog,
            _pio: pio,
            uart: Slot::Idle(sm0),
            spi: Slot::Idle(sm1),
            payload: [0; MAX_PAYLOAD],
            len: 0,
            cursor: 0,
            active: Active::None,
            cs,
            scl,
            sda,
        }
    }

    /// Stop any protocol output and return every line to its idle level.
    pub fn stop(&mut self) {
        use hal::pio::PinState;
        self.uart.release(&[(UART_TX_PIN, PinState::High)]);
        self.spi
            .release(&[(SPI_SCK_PIN, PinState::Low), (SPI_MOSI_PIN, PinState::Low)]);
        self.active = Active::None;
        self.len = 0;
        self.cursor = 0;
        let _ = self.cs.set_high();
        let _ = self.scl.set_high();
        let _ = self.sda.set_high();
    }

    /// Whether a burst is still being fed out.
    pub fn busy(&self) -> bool {
        self.active != Active::None && self.cursor < self.len
    }

    /// Send bytes as 8N1 UART frames.
    pub fn uart_send(&mut self, bytes: &[u8], divisor: Divisor) -> Result<(), &'static str> {
        if bytes.is_empty() {
            return Err("no bytes to send");
        }
        if bytes.len() > MAX_PAYLOAD {
            return Err("payload too long");
        }
        self.stop();

        // Safety: the master copy lives in `self.uart_prog` and is never
        // uninstalled, so the share always refers to live instruction memory.
        let prog = unsafe { self.uart_prog.share() };
        let (mut sm, rx, tx) = PIOBuilder::from_installed_program(prog)
            .out_pins(UART_TX_PIN, 1)
            .side_set_pin_base(UART_TX_PIN)
            .clock_divisor_fixed_point(divisor.int, divisor.frac)
            // UART is little-endian on the wire: LSB first.
            .out_shift_direction(ShiftDirection::Right)
            .autopull(false)
            .buffers(Buffers::OnlyTx)
            .build(self.uart.take_idle());
        sm.set_pindirs([(UART_TX_PIN, PinDir::Output)]);

        self.payload[..bytes.len()].copy_from_slice(bytes);
        self.len = bytes.len();
        self.cursor = 0;
        self.active = Active::Uart;

        let sm = sm.start();
        self.uart = Slot::Running { sm, rx, tx };
        self.service();
        Ok(())
    }

    /// Send bytes over SPI mode 0, MSB first, with chip select asserted.
    pub fn spi_send(&mut self, bytes: &[u8], divisor: Divisor) -> Result<(), &'static str> {
        if bytes.is_empty() {
            return Err("no bytes to send");
        }
        if bytes.len() > MAX_PAYLOAD {
            return Err("payload too long");
        }
        self.stop();

        // Safety: as in `uart_send`.
        let prog = unsafe { self.spi_prog.share() };
        let (mut sm, rx, tx) = PIOBuilder::from_installed_program(prog)
            .out_pins(SPI_MOSI_PIN, 1)
            .side_set_pin_base(SPI_SCK_PIN)
            .clock_divisor_fixed_point(divisor.int, divisor.frac)
            // SPI is MSB first.
            .out_shift_direction(ShiftDirection::Left)
            .autopull(true)
            .pull_threshold(8)
            .buffers(Buffers::OnlyTx)
            .build(self.spi.take_idle());
        sm.set_pindirs([
            (SPI_MOSI_PIN, PinDir::Output),
            (SPI_SCK_PIN, PinDir::Output),
        ]);

        self.payload[..bytes.len()].copy_from_slice(bytes);
        self.len = bytes.len();
        self.cursor = 0;
        self.active = Active::Spi;

        let _ = self.cs.set_low();
        let sm = sm.start();
        self.spi = Slot::Running { sm, rx, tx };
        self.service();
        Ok(())
    }

    /// Push queued bytes into the active FIFO. Call from the poll loop.
    pub fn service(&mut self) {
        match self.active {
            Active::None => {}
            Active::Uart => {
                if let Slot::Running { ref mut tx, .. } = self.uart {
                    while self.cursor < self.len && !tx.is_full() {
                        // The program pulls a whole word and shifts out 8 bits.
                        tx.write(self.payload[self.cursor] as u32);
                        self.cursor += 1;
                    }
                }
            }
            Active::Spi => {
                if let Slot::Running { ref mut tx, .. } = self.spi {
                    while self.cursor < self.len && !tx.is_full() {
                        // Left shift takes the MSB first, so the byte sits in
                        // the top of the word.
                        tx.write((self.payload[self.cursor] as u32) << 24);
                        self.cursor += 1;
                    }
                }
            }
        }
    }

    /// Emit an I2C transaction by bit-banging.
    ///
    /// Push-pull rather than open-drain: this device is the only driver on the
    /// bus, so there is nothing to contend with and the edges come out cleaner.
    /// **Do not wire a real I2C slave to these pins** - a slave that drives SDA
    /// would be shorted against this output. The generated traffic is for a
    /// decoder to read, not for a device to answer.
    ///
    /// The ninth clock of every byte is an unasserted ACK slot, because there
    /// is no slave to assert it. Decoders show it as NAK; that is expected and
    /// does not mean the frame is malformed.
    /// Returns the achieved SCL frequency in milli-hertz, which is not the
    /// requested one: see [`tester_core::i2c_timing`].
    pub fn i2c_send(&mut self, addr: u8, bytes: &[u8], hz: u32) -> Result<u64, &'static str> {
        if addr > 0x7f {
            return Err("address must be 7-bit");
        }
        if bytes.len() > MAX_PAYLOAD {
            return Err("payload too long");
        }
        // The delay loop is not one cycle per count and each bit pays a fixed
        // GPIO cost, so the quarter-bit count comes from a fitted model rather
        // than from sysclk/hz/4. See `tester_core::i2c_timing`.
        let plan = match tester_core::i2c_timing::plan(crate::board::SYSCLK_HZ, hz) {
            Ok(p) => p,
            Err(e) => return Err(e.as_str()),
        };
        self.stop();

        let mut w = I2cWire {
            scl: &mut self.scl,
            sda: &mut self.sda,
            quarter: plan.quarter,
        };

        w.start();
        // 7-bit address followed by the R/W bit, which is always 0 (write).
        w.byte(addr << 1);
        for b in bytes {
            w.byte(*b);
        }
        w.stop_cond();
        Ok(plan.achieved_millihz)
    }
}

/// Bit-banged I2C line driver.
struct I2cWire<'a> {
    scl: &'a mut SioPin,
    sda: &'a mut SioPin,
    quarter: u32,
}

impl I2cWire<'_> {
    fn wait(&self) {
        hal::arch::delay(self.quarter);
    }

    fn set(&mut self, scl_high: bool, sda_high: bool) {
        let _ = if scl_high {
            self.scl.set_high()
        } else {
            self.scl.set_low()
        };
        let _ = if sda_high {
            self.sda.set_high()
        } else {
            self.sda.set_low()
        };
    }

    /// START: SDA falls while SCL is high.
    fn start(&mut self) {
        self.set(true, true);
        self.wait();
        self.set(true, false);
        self.wait();
        self.set(false, false);
        self.wait();
    }

    /// STOP: SDA rises while SCL is high.
    fn stop_cond(&mut self) {
        self.set(false, false);
        self.wait();
        self.set(true, false);
        self.wait();
        self.set(true, true);
        self.wait();
    }

    fn bit(&mut self, value: bool) {
        // Change SDA while SCL is low, then clock it.
        self.set(false, value);
        self.wait();
        self.set(true, value);
        self.wait();
        self.wait();
        self.set(false, value);
        self.wait();
    }

    fn byte(&mut self, value: u8) {
        for i in (0..8).rev() {
            self.bit(value & (1 << i) != 0);
        }
        // ACK slot: release SDA and clock once. Nothing pulls it low.
        self.bit(true);
    }
}
