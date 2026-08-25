//! Board constants and pin map. Works unmodified on Pico 2 and Pico 2 W.
//!
//! Pin assignment is deliberately static. A logic analyzer under test gets
//! clipped onto these pins once and never re-wired, so channel N must always
//! mean the same physical pin.
//!
//! # Pico 2 W compatibility
//!
//! GP23, GP24, GP25 and GP29 are **never touched**. On a plain Pico 2 they are
//! SMPS mode, VBUS sense, the LED, and VSYS sense; on a Pico 2 W the same four
//! pins are the CYW43439 wireless interface (WL_ON, WL_D, WL_CS, WL_CLK).
//! In particular there is no heartbeat LED: GP25 is the LED on a Pico 2 but the
//! wireless chip select on a Pico 2 W, and driving it there would fight the
//! radio. The USB console is the liveness indicator instead, which costs
//! nothing and keeps one binary working on both boards.
//!
//! Everything driven here - GP0..GP17, GP19..GP22 and GP26 - is on the 40-pin
//! header of both boards, in the same places. GP18, GP27 and GP28 are on the
//! header too but are left free.

use rp235x_hal as hal;

use hal::gpio::{
    DynPinId, FunctionPio0, FunctionPio1, OutputDriveStrength, OutputSlewRate, Pin, PullNone,
};

/// External crystal. 12 MHz on both Pico 2 and Pico 2 W.
pub const XTAL_FREQ_HZ: u32 = 12_000_000;

/// System clock produced by `init_clocks_and_plls` on the RP2350.
///
/// Every timing figure this device reports is derived from this constant, so
/// raising the clock is a one-line change here plus a PLL reconfiguration.
pub const SYSCLK_HZ: u32 = 150_000_000;

/// Timing calculator anchored to [`SYSCLK_HZ`].
pub const TIMING: tester_core::Timing = tester_core::Timing::new(SYSCLK_HZ);

/// Width of the parallel channel bus, GP0..GP15.
pub const BUS_WIDTH: u8 = 16;
/// First GPIO of the parallel bus. `OUT` needs a contiguous run starting here.
pub const BUS_BASE: u8 = 0;

/// Trigger marker. Pulsed at the start of every burst so the analyzer under
/// test has something unambiguous to trigger on.
pub const MARKER_PIN: u8 = 16;

/// UART TX, driven by PIO1 SM0.
pub const UART_TX_PIN: u8 = 17;
/// SPI clock, driven as PIO1 side-set.
pub const SPI_SCK_PIN: u8 = 19;
/// SPI data out, driven by PIO1 `OUT`.
pub const SPI_MOSI_PIN: u8 = 20;

/// Pins driven by PIO0: the parallel bus plus the marker.
///
/// The marker has to share PIO0 with the bus because `StateMachineGroup`
/// synchronised start only works between state machines of the same PIO block,
/// and a marker that is not cycle-aligned with the data is useless.
pub type Pio0Pin = Pin<DynPinId, FunctionPio0, PullNone>;

/// Pins driven by PIO1: the protocol generators.
pub type Pio1Pin = Pin<DynPinId, FunctionPio1, PullNone>;

/// Pins driven directly by the CPU: SPI chip select and the I2C pair.
///
/// These are the only signals not clocked by PIO. Chip select is a frame
/// marker, and I2C runs at 100-400 kHz where a decoder cares about protocol
/// structure rather than edge placement, so CPU timing is adequate for both.
/// Nothing whose *timing* is under test is driven this way.
pub type SioPin = Pin<DynPinId, hal::gpio::FunctionSioOutput, PullNone>;

/// Configure a pad for clean fast edges.
///
/// 12 mA drive and fast slew keep rise times short at the top of the frequency
/// range; disabling the input buffer removes the pad from any input path, which
/// also keeps us clear of RP2350 erratum E9 (input latch-up with pull-downs).
fn configure_pad<I, F, P>(pin: &mut Pin<I, F, P>)
where
    I: hal::gpio::PinId,
    F: hal::gpio::Function,
    P: hal::gpio::PullType,
{
    pin.set_drive_strength(OutputDriveStrength::TwelveMilliAmps);
    pin.set_slew_rate(OutputSlewRate::Fast);
    pin.set_schmitt_enabled(false);
}

/// Take a bank0 pin, hand it to PIO0 and set its pad up for fast switching.
macro_rules! into_pio0 {
    ($pin:expr) => {{
        let mut p = $pin
            .into_pull_type::<PullNone>()
            .into_function::<FunctionPio0>();
        configure_pad(&mut p);
        p.into_dyn_pin()
    }};
}

/// Take a bank0 pin, hand it to PIO1 and set its pad up for fast switching.
macro_rules! into_pio1 {
    ($pin:expr) => {{
        let mut p = $pin
            .into_pull_type::<PullNone>()
            .into_function::<FunctionPio1>();
        configure_pad(&mut p);
        p.into_dyn_pin()
    }};
}

/// Take a bank0 pin as a CPU-driven push-pull output, starting high.
///
/// High is the idle level for all three signals that use this: SPI chip select
/// is active-low, and I2C idles with both lines released.
macro_rules! into_sio_high {
    ($pin:expr) => {{
        let mut p = $pin.into_pull_type::<PullNone>().into_push_pull_output();
        configure_pad(&mut p);
        let _ = <_ as embedded_hal::digital::OutputPin>::set_high(&mut p);
        p.into_dyn_pin()
    }};
}

/// Every pin this device drives, already in its final function.
///
/// The PIO-driven fields are never read. That is the point: a `Pin` in a PIO
/// function *is* the GPIO function select, so holding the handle is what keeps
/// the pin routed to PIO. PIO addresses pins by number, so there is nothing to
/// call on them. Dropping them would hand the pins back to SIO mid-waveform.
#[allow(
    dead_code,
    reason = "PIO-driven pins are RAII handles for the function select"
)]
pub struct SignalPins {
    pub bus: [Pio0Pin; BUS_WIDTH as usize],
    pub marker: Pio0Pin,
    pub uart_tx: Pio1Pin,
    pub spi_sck: Pio1Pin,
    pub spi_mosi: Pio1Pin,
    pub spi_cs: SioPin,
    pub i2c_scl: SioPin,
    pub i2c_sda: SioPin,
}

impl SignalPins {
    /// Claim every signal pin and put it in its final function.
    ///
    /// Note what is *not* claimed: GP23/24/25/29 are left completely alone so
    /// this is one binary for Pico 2 and Pico 2 W. See the module docs.
    pub fn new(pins: hal::gpio::Pins) -> Self {
        Self {
            bus: [
                into_pio0!(pins.gpio0),
                into_pio0!(pins.gpio1),
                into_pio0!(pins.gpio2),
                into_pio0!(pins.gpio3),
                into_pio0!(pins.gpio4),
                into_pio0!(pins.gpio5),
                into_pio0!(pins.gpio6),
                into_pio0!(pins.gpio7),
                into_pio0!(pins.gpio8),
                into_pio0!(pins.gpio9),
                into_pio0!(pins.gpio10),
                into_pio0!(pins.gpio11),
                into_pio0!(pins.gpio12),
                into_pio0!(pins.gpio13),
                into_pio0!(pins.gpio14),
                into_pio0!(pins.gpio15),
            ],
            marker: into_pio0!(pins.gpio16),
            uart_tx: into_pio1!(pins.gpio17),
            spi_sck: into_pio1!(pins.gpio19),
            spi_mosi: into_pio1!(pins.gpio20),
            spi_cs: into_sio_high!(pins.gpio21),
            i2c_scl: into_sio_high!(pins.gpio22),
            i2c_sda: into_sio_high!(pins.gpio26),
        }
    }
}
