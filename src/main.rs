//! picolyzer-tester: a Raspberry Pi Pico 2 used as a known-good digital signal
//! source for validating a logic analyzer.
//!
//! It emits precisely timed waveforms and reports exactly what it emitted; you
//! point the analyzer under test at the pins and compare. A USB CDC serial
//! console selects and parameterises the patterns.
//!
//! The central design rule is that **the CPU never generates timing**. Every
//! edge comes out of a PIO state machine clocked from the system clock. The
//! CPU only parses commands and polls USB, so console traffic, USB interrupts
//! and host timing cannot perturb a waveform. Where the CPU does have to keep
//! up - refilling the FIFO for long streamed patterns - the hardware stall flag
//! is reported, so a shortfall on this side is never mistaken for one on the
//! analyzer's side.
//!
//! Runs unmodified on Pico 2 and Pico 2 W; see [`board`] for why.

#![no_std]
#![no_main]

use rp235x_hal as hal;

use usb_device::class_prelude::UsbBusAllocator;
use usb_device::prelude::{StringDescriptors, UsbDeviceBuilder, UsbVidPid};
use usbd_serial::SerialPort;

use panic_halt as _;

mod board;
mod bus;
mod commands;
mod console;
mod dma;
mod proto;

use board::{SignalPins, XTAL_FREQ_HZ};
use bus::Bus;
use console::{LineBuffer, LineEvent};
use proto::Proto;

/// Tell the Boot ROM about our application.
#[unsafe(link_section = ".start_block")]
#[used]
pub static IMAGE_DEF: hal::block::ImageDef = hal::block::ImageDef::secure_exe();

const BANNER: &str = concat!(
    "picolyzer-tester ",
    env!("CARGO_PKG_VERSION"),
    " - logic analyzer stimulus generator\r\n`help` for commands\r\n"
);

#[hal::entry]
fn main() -> ! {
    let mut pac = hal::pac::Peripherals::take().unwrap();
    let mut watchdog = hal::Watchdog::new(pac.WATCHDOG);

    // 150 MHz system clock, 48 MHz USB clock from the separate USB PLL.
    let clocks = hal::clocks::init_clocks_and_plls(
        XTAL_FREQ_HZ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    let sio = hal::Sio::new(pac.SIO);
    let pins = hal::gpio::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    // The PIO-driven pins are held for the lifetime of the program: these
    // handles own the GPIO function select, and PIO addresses the pins by
    // number rather than through them. The CPU-driven ones move into `Proto`,
    // which actually toggles them.
    let signals = SignalPins::new(pins);

    let mut bus = Bus::new(pac.PIO0, pac.DMA, &mut pac.RESETS);
    let mut proto = Proto::new(
        pac.PIO1,
        &mut pac.RESETS,
        signals.spi_cs,
        signals.i2c_scl,
        signals.i2c_sda,
    );
    // Start from a defined state: all channels driven low, nothing running.
    bus.stop();
    proto.stop();

    let usb_bus = UsbBusAllocator::new(hal::usb::UsbBus::new(
        pac.USB,
        pac.USB_DPRAM,
        clocks.usb_clock,
        true,
        &mut pac.RESETS,
    ));
    let mut serial = SerialPort::new(&usb_bus);
    let mut usb_dev = UsbDeviceBuilder::new(&usb_bus, UsbVidPid(0x16c0, 0x27dd))
        .strings(&[StringDescriptors::default()
            .manufacturer("picolyzer-tester")
            .product("Logic Analyzer Test Source")
            .serial_number("0001")])
        .unwrap()
        .device_class(usbd_serial::USB_CLASS_CDC)
        .build();

    let mut line = LineBuffer::new();
    let mut greeted = false;

    loop {
        // Poll USB first and unconditionally. Everything else in this loop is
        // best-effort; missing a USB poll window is what makes a device
        // enumerate unreliably.
        let has_data = usb_dev.poll(&mut [&mut serial]);

        // Keep any streamed pattern fed. Non-blocking - it writes only while
        // the FIFO has room.
        bus.service();
        proto.service();

        if !greeted && usb_dev.state() == usb_device::device::UsbDeviceState::Configured {
            // Only greet once the host has configured us, otherwise the write
            // is dropped and the banner never appears.
            if write_all(&mut serial, BANNER.as_bytes()) {
                greeted = true;
            }
        }

        if !has_data {
            continue;
        }

        let mut buf = [0u8; 64];
        let Ok(count) = serial.read(&mut buf) else {
            continue;
        };

        for &b in &buf[..count] {
            match line.push(b) {
                LineEvent::Pending => {}
                LineEvent::Overflow => {
                    write_all(&mut serial, b"err line too long\r\n");
                }
                LineEvent::Line => {
                    let cmd = first_word(line.line());
                    // Multi-line output (help, pin map) goes out before the
                    // single-line status, so a script can still parse the last
                    // line as the result.
                    if let Some(text) = commands::long_output(cmd) {
                        write_all(&mut serial, text.as_bytes());
                    }
                    let reply = commands::dispatch(&mut bus, &mut proto, line.line());
                    write_all(&mut serial, reply.as_bytes());
                    write_all(&mut serial, b"\r\n");
                    line.reset();
                }
            }
        }
    }
}

/// First whitespace-separated word of a line.
fn first_word(line: &str) -> &str {
    line.split_ascii_whitespace().next().unwrap_or("")
}

/// Write a whole buffer, giving up if the host is not draining.
///
/// Returns whether everything was written. The bounded retry matters: blocking
/// here until the host reads would stall pattern streaming, and this device's
/// job is to keep the signal clean even when the console is neglected.
fn write_all(serial: &mut SerialPort<hal::usb::UsbBus>, mut data: &[u8]) -> bool {
    let mut attempts = 0u32;
    while !data.is_empty() {
        match serial.write(data) {
            Ok(0) | Err(_) => {
                attempts += 1;
                if attempts > 10_000 {
                    return false;
                }
            }
            Ok(n) => {
                data = &data[n..];
                attempts = 0;
            }
        }
    }
    true
}

/// Program metadata for `picotool info`.
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [hal::binary_info::EntryAddr; 5] = [
    hal::binary_info::rp_cargo_bin_name!(),
    hal::binary_info::rp_cargo_version!(),
    hal::binary_info::rp_program_description!(
        c"Logic analyzer stimulus generator: 16-channel patterns over a USB serial console"
    ),
    hal::binary_info::rp_cargo_homepage_url!(),
    hal::binary_info::rp_program_build_attribute!(),
];
